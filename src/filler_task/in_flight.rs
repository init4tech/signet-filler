use crate::ChainTokenPair;
use alloy::primitives::{B256, U256};
use signet_constants::SignetSystemConstants;
use signet_types::SignedOrder;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

/// Tracks orders that have been submitted in fill bundles but whose Permit2 nonce has not yet
/// been observed consumed on-chain. Used to:
///
/// 1. Avoid re-submitting the same order across overlapping target-block windows. With
///    `target_blocks > 1`, a single polling cycle submits a bundle targeting blocks
///    `[N, N+target_blocks)` and the next cycle starts before block `N` has produced - without
///    tracking we'd score and submit the same order again, racing two host-chain transactions
///    for the same Permit2 `(owner, nonce)` slot. Whichever lands second reverts when Permit2
///    finds the bit already set, wasting host-chain gas.
/// 2. Earmark per-cycle token budgets so other orders selected in the same cycle don't greenlight
///    against funds already committed to an in-flight bundle.
///
/// Entries are inserted with a caller-supplied TTL covering the full window during which the
/// submitted bundle could still land (i.e. up to its Permit2 deadline). [`Self::live_orders`]
/// and [`Self::earmarks`] retain unexpired entries as part of their read pass, so callers
/// always see a TTL-consistent view and the inner map stays bounded as long as one of those
/// methods is called every cycle. Entries can also be cleared early via
/// [`InFlightTracker::clear`] once a nonce-consumed check confirms the bundle has landed,
/// freeing the earmarked budget for the next cycle.
#[derive(Debug)]
pub(super) struct InFlightTracker {
    inner: Mutex<HashMap<B256, Entry>>,
    ttl: Duration,
}

#[derive(Debug)]
struct Entry {
    /// Submitted order, retained so the FillerTask can reconcile by passing it to
    /// [`signet_orders::permit2::is_order_nonce_consumed`] each cycle.
    order: Arc<SignedOrder>,
    expires_at: Instant,
    earmarks: Vec<(ChainTokenPair, U256)>,
}

impl InFlightTracker {
    /// Create a new tracker with the given TTL.
    pub(super) fn new(ttl: Duration) -> Self {
        Self { inner: Mutex::new(HashMap::new()), ttl }
    }

    /// Records each order in `orders` as in-flight, all sharing a TTL from now. The caller passes
    /// owned (cloned) orders so the originals can still be moved into the submission call. Outputs
    /// whose chain is neither the host nor the rollup are dropped from the per-order earmarks,
    /// matching the [`super::WorkingMap`] coverage so every earmark has a corresponding budget
    /// entry to decrement. Taking a batch lets the caller acquire the inner lock once per
    /// submitted bundle.
    pub(super) fn track(&self, orders: Vec<SignedOrder>, constants: &SignetSystemConstants) {
        let ru_chain_id = constants.ru_chain_id();
        let host_chain_id = constants.host_chain_id();
        let expires_at = Instant::now() + self.ttl;
        let mut guard = self.inner.lock().unwrap();
        for order in orders {
            let earmarks = order
                .outputs()
                .iter()
                .filter_map(|output| {
                    let chain_id = u64::from(output.chainId);
                    (chain_id == ru_chain_id || chain_id == host_chain_id)
                        .then(|| (ChainTokenPair::new(chain_id, output.token), output.amount))
                })
                .collect();
            let order_hash = *order.order_hash();
            guard.insert(order_hash, Entry { order: Arc::new(order), expires_at, earmarks });
        }
    }

    /// Returns every unexpired tracked in-flight order, dropping any expired entries from the
    /// inner map as part of the same pass.
    pub(super) fn live_orders(&self) -> Vec<Arc<SignedOrder>> {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
        let mut live = Vec::with_capacity(guard.len());
        guard.retain(|_, entry| {
            if entry.expires_at > now {
                live.push(entry.order.clone());
                true
            } else {
                false
            }
        });
        live
    }

    /// Removes the in-flight entry for `order_hash`, freeing any earmarked budget for subsequent
    /// cycles. No-op if not tracked.
    pub(super) fn clear(&self, order_hash: &B256) {
        self.inner.lock().unwrap().remove(order_hash);
    }

    /// Removes the in-flight entries for the given hashes in a single lock acquisition. No-op for
    /// hashes that aren't tracked.
    pub(super) fn clear_many(&self, order_hashes: &[B256]) {
        let mut guard = self.inner.lock().unwrap();
        for order_hash in order_hashes {
            guard.remove(order_hash);
        }
    }

    /// Returns `true` if `order_hash` is currently tracked as in-flight and unexpired.
    pub(super) fn is_in_flight(&self, order_hash: &B256) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.get(order_hash).is_some_and(|entry| entry.expires_at > Instant::now())
    }

    /// Returns the sum of earmarked amounts across unexpired tracked in-flight entries, grouped by
    /// [`ChainTokenPair`]. Drops any expired entries from the inner map as part of the same pass.
    pub(super) fn earmarks(&self) -> HashMap<ChainTokenPair, U256> {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
        let mut totals = HashMap::<ChainTokenPair, U256>::new();
        guard.retain(|_, entry| {
            if entry.expires_at > now {
                for (chain_token, amount) in &entry.earmarks {
                    let total = totals.entry(*chain_token).or_default();
                    *total = total.saturating_add(*amount);
                }
                true
            } else {
                false
            }
        });
        totals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, Bytes};
    use signet_constants::test_utils::{HOST_CHAIN_ID, RU_CHAIN_ID, TEST_SYS};
    use signet_zenith::RollupOrders::{
        Output, Permit2Batch, PermitBatchTransferFrom, TokenPermissions,
    };

    /// Sentinel chain ID for outputs we expect the tracker to ignore. Static-asserted to be
    /// distinct from the known chain IDs in `TEST_SYS`, so a future bump to `RU_CHAIN_ID` /
    /// `HOST_CHAIN_ID` can't silently turn this into a "known" chain.
    const OTHER_CHAIN_ID: u64 = 9_999;
    const _: () = assert!(OTHER_CHAIN_ID != RU_CHAIN_ID && OTHER_CHAIN_ID != HOST_CHAIN_ID);

    const TOKEN_X: Address = Address::repeat_byte(0xAA);
    const TOKEN_Y: Address = Address::repeat_byte(0xBB);

    fn output(chain_id: u64, token: Address, amount: u64) -> Output {
        Output {
            token,
            amount: U256::from(amount),
            recipient: Address::ZERO,
            chainId: chain_id as u32,
        }
    }

    fn order(nonce: u64, outputs: Vec<Output>) -> SignedOrder {
        SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions { token: TOKEN_X, amount: U256::from(1) }],
                    nonce: U256::from(nonce),
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            outputs,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn marks_and_reports_in_flight_until_ttl_expires() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));
        let order = order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)]);
        let order_hash = *order.order_hash();

        assert!(!tracker.is_in_flight(&order_hash));

        tracker.track(vec![order], &TEST_SYS);
        assert!(tracker.is_in_flight(&order_hash));

        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(tracker.is_in_flight(&order_hash));

        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!tracker.is_in_flight(&order_hash));
    }

    #[tokio::test]
    async fn clear_removes_entry() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));
        let order = order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)]);
        let order_hash = *order.order_hash();

        tracker.track(vec![order], &TEST_SYS);
        tracker.clear(&order_hash);

        assert!(!tracker.is_in_flight(&order_hash));
        assert!(tracker.earmarks().is_empty());
    }

    #[tokio::test]
    async fn clear_many_removes_listed_entries_and_ignores_unknown_hashes() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));
        let first = order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)]);
        let second = order(2, vec![output(RU_CHAIN_ID, TOKEN_X, 200)]);
        let kept = order(3, vec![output(RU_CHAIN_ID, TOKEN_X, 50)]);
        let first_hash = *first.order_hash();
        let second_hash = *second.order_hash();
        let kept_hash = *kept.order_hash();
        let unknown_hash = B256::repeat_byte(0xFF);

        tracker.track(vec![first, second, kept], &TEST_SYS);

        tracker.clear_many(&[first_hash, second_hash, unknown_hash]);

        assert!(!tracker.is_in_flight(&first_hash));
        assert!(!tracker.is_in_flight(&second_hash));
        assert!(tracker.is_in_flight(&kept_hash));
        // Only the kept order's earmark remains.
        let pair_x = ChainTokenPair::new(RU_CHAIN_ID, TOKEN_X);
        assert_eq!(tracker.earmarks()[&pair_x], U256::from(50));
    }

    #[tokio::test]
    async fn earmarks_sum_outputs_across_orders_and_skip_unknown_chains() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));

        tracker.track(
            vec![
                order(
                    1,
                    vec![
                        output(RU_CHAIN_ID, TOKEN_X, 100),
                        output(HOST_CHAIN_ID, TOKEN_Y, 50),
                        // Output on an unknown chain should be ignored.
                        output(OTHER_CHAIN_ID, TOKEN_X, 999),
                    ],
                ),
                order(2, vec![output(RU_CHAIN_ID, TOKEN_X, 300)]),
            ],
            &TEST_SYS,
        );

        let earmarks = tracker.earmarks();
        assert_eq!(earmarks.len(), 2);
        let pair_x = ChainTokenPair::new(RU_CHAIN_ID, TOKEN_X);
        let pair_y = ChainTokenPair::new(HOST_CHAIN_ID, TOKEN_Y);
        assert_eq!(earmarks[&pair_x], U256::from(400));
        assert_eq!(earmarks[&pair_y], U256::from(50));
    }

    #[tokio::test(start_paused = true)]
    async fn earmarks_excludes_expired_entries() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));

        tracker.track(vec![order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)])], &TEST_SYS);

        tokio::time::advance(Duration::from_secs(11)).await;

        tracker.track(vec![order(2, vec![output(RU_CHAIN_ID, TOKEN_X, 50)])], &TEST_SYS);

        // The expired first order is dropped from the read pass; only the second contributes.
        let pair_x = ChainTokenPair::new(RU_CHAIN_ID, TOKEN_X);
        let earmarks = tracker.earmarks();
        assert_eq!(earmarks.len(), 1);
        assert_eq!(earmarks[&pair_x], U256::from(50));
    }

    #[tokio::test(start_paused = true)]
    async fn live_orders_excludes_expired_entries() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));

        let expired = order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)]);
        tracker.track(vec![expired], &TEST_SYS);

        tokio::time::advance(Duration::from_secs(11)).await;

        let live = order(2, vec![output(RU_CHAIN_ID, TOKEN_X, 50)]);
        let live_hash = *live.order_hash();
        tracker.track(vec![live], &TEST_SYS);

        let returned = tracker.live_orders();
        assert_eq!(returned.len(), 1);
        assert_eq!(*returned[0].order_hash(), live_hash);
    }

    #[tokio::test]
    async fn track_with_empty_orders_is_a_noop() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));
        tracker.track(vec![], &TEST_SYS);

        assert!(tracker.live_orders().is_empty());
        assert!(tracker.earmarks().is_empty());
    }

    /// `live_orders` must hand back every tracked order. The reconcile loop iterates this list
    /// straight into `is_order_nonce_consumed`, so any regression that drops or substitutes
    /// entries would silently leave landed orders earmarked until their TTL lapsed.
    #[tokio::test]
    async fn live_orders_returns_every_tracked_order() {
        let tracker = InFlightTracker::new(Duration::from_secs(10));
        let first = order(1, vec![output(RU_CHAIN_ID, TOKEN_X, 100)]);
        let second = order(2, vec![output(RU_CHAIN_ID, TOKEN_X, 200)]);
        let first_hash = *first.order_hash();
        let second_hash = *second.order_hash();

        tracker.track(vec![first, second], &TEST_SYS);

        let live: HashMap<B256, _> =
            tracker.live_orders().into_iter().map(|order| (*order.order_hash(), order)).collect();
        assert_eq!(live.len(), 2);
        assert!(live.contains_key(&first_hash));
        assert!(live.contains_key(&second_hash));
    }
}
