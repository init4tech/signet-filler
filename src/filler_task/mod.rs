use crate::{
    AllowanceCache, FillProviderType, FillerContext, FixedPricingClient, FixedPricingError, metrics,
};
use alloy::{primitives::B256, signers::Signer};
use eyre::{Context, Report, Result, bail};
use futures_util::{TryStreamExt, future::join_all};
use init4_bin_base::{
    deps::tracing::{Instrument, debug, error, info, info_span, instrument, trace, warn},
    utils::signer::LocalOrAws,
};
use lru::LruCache;
use signet_orders::{
    FeePolicySubmitter, FillerOptions, OrderStreamExt, stream::predicates::not_expired_at,
};
use signet_tx_cache::TxCache;
use signet_types::SignedOrder;
use std::{
    cmp::Reverse,
    collections::HashSet,
    num::NonZeroUsize,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    select,
    time::{Duration, Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

mod preflight;
use preflight::WorkingMap;

const FILLED_ORDERS_CACHE_SIZE: NonZeroUsize = NonZeroUsize::new(10240).unwrap();
/// Safety margin added to the Permit2 deadline to cover signing/network latency and clock drift
/// between the filler and the host chain.
const DEADLINE_DRIFT_BUFFER_SECS: u64 = 5;

type Filler = signet_orders::Filler<
    LocalOrAws,
    TxCache,
    FeePolicySubmitter<FillProviderType, FillProviderType, TxCache>,
>;

/// Order filler service that submits fill bundles shortly before each block boundary.
#[derive(Debug)]
pub struct FillerTask {
    filler: Filler,
    pricing_client: FixedPricingClient,
    allowance_cache: AllowanceCache,
    filled_orders: Mutex<LruCache<B256, ()>>,
    target_blocks: u8,
    max_orders_per_bundle: Option<NonZeroUsize>,
    block_lead_duration: Duration,
    slot_duration: u64,
    host_start_timestamp: u64,
    app_start_instant: Instant,
    cancellation_token: CancellationToken,
}

impl FillerTask {
    /// Create a new filler from configuration.
    pub fn new(context: &FillerContext) -> Self {
        let submitter = FeePolicySubmitter::new(
            context.ru_provider().clone(),
            context.host_provider().clone(),
            context.tx_cache().clone(),
            context.constants().system().clone(),
        );

        let target_blocks = context.target_blocks();
        let slot_duration = context.constants().system().host().slot_duration();
        let block_lead_duration = context.block_lead_duration();
        // Keep the Permit2 signature valid from signing time (`block_lead_duration` before block N)
        // through the end of block N+target_blocks-1, with an extra buffer for signing/network
        // latency and clock drift.
        let deadline_offset = block_lead_duration.as_secs()
            + u64::from(target_blocks) * slot_duration
            + DEADLINE_DRIFT_BUFFER_SECS;
        let filler = Filler::new(
            context.signer().clone(),
            context.tx_cache().clone(),
            submitter,
            context.constants().system().clone(),
            FillerOptions::new().with_deadline_offset(deadline_offset),
        );

        let pricing_client = FixedPricingClient::new(
            context.constants().system(),
            context.chain_name(),
            context.max_loss_percent(),
        );

        Self {
            filler,
            pricing_client,
            allowance_cache: context.allowance_cache().clone(),
            filled_orders: Mutex::new(LruCache::new(FILLED_ORDERS_CACHE_SIZE)),
            target_blocks,
            max_orders_per_bundle: context.max_orders_per_bundle(),
            block_lead_duration,
            slot_duration,
            host_start_timestamp: context.constants().system().host().start_timestamp(),
            app_start_instant: context.app_start_instant(),
            cancellation_token: context.cancellation_token().clone(),
        }
    }

    /// Run the filler task to completion.
    ///
    /// Spawns the filler loop as a tokio task and supervises it, returning `Ok(())` on graceful
    /// cancellation or an error if the task exits unexpectedly.
    pub async fn run(self) -> Result<()> {
        let cancellation_token = self.cancellation_token.clone();
        let result = tokio::spawn(self.run_loop()).await;
        if cancellation_token.is_cancelled() {
            return Ok(());
        }
        cancellation_token.cancel();
        match result {
            Ok(()) => bail!("filler task exited without cancellation"),
            Err(error) if error.is_panic() => {
                Err(Report::new(error).wrap_err("panic in filler task"))
            }
            Err(_) => bail!("filler task cancelled unexpectedly"),
        }
    }

    async fn run_loop(self) {
        info!(
            slot_duration_secs = self.slot_duration,
            block_lead_duration_ms = %self.block_lead_duration.as_millis(),
            target_blocks_count = self.target_blocks,
            "starting filler task"
        );

        let slot_duration = Duration::from_secs(self.slot_duration);
        let staleness_threshold = Duration::from_millis(100).max(self.block_lead_duration / 4);
        let first_tick = self.submission_anchor_instant();
        let mut interval = tokio::time::interval_at(first_tick, slot_duration);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // The first tick fires immediately with a large elapsed time; consume it.
        interval.tick().await;

        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => {
                    debug!("filler task cancelled");
                    break;
                }
                ticked_at = interval.tick() => {
                    metrics::record_uptime(self.app_start_instant.elapsed());
                    let staleness = ticked_at.elapsed();
                    if staleness > staleness_threshold {
                        warn!(
                            staleness_ms = %staleness.as_millis(),
                            "missed processing window, skipping cycle"
                        );
                        metrics::record_missed_window();
                        continue;
                    }
                    if let Err(error) = self.process_orders().await {
                        error!(%error, "error processing orders");
                    }
                }
            }
        }
    }

    #[instrument(skip(self))]
    async fn process_orders(&self) -> Result<()> {
        let _cycle_guard = metrics::CycleGuard::new();

        let scored = self.fetch_and_score_orders().await?;
        if scored.is_empty() {
            return Ok(());
        }

        let orders_to_fill = self.select_fillable_orders(scored).await;
        if orders_to_fill.is_empty() {
            info!("no fillable orders after budget and nonce checks");
            return Ok(());
        }

        self.submit_bundles(orders_to_fill).await;
        Ok(())
    }

    /// Fetches orders from the tx cache, filters out known-filled orders, scores by profitability,
    /// and returns candidates sorted most-profitable-first.
    #[instrument(skip_all)]
    async fn fetch_and_score_orders(&self) -> Result<Vec<(i128, SignedOrder)>> {
        let mut orders_count = 0_u64;
        let mut orders_after_expiry_filter = 0_u64;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock set before UNIX epoch")
            .as_secs();
        // Subtract the drift buffer for symmetry with the sign-side deadline, so an order whose
        // deadline is within the buffer of the first target block isn't prematurely dropped here
        // while the sign path would still have accepted it.
        let earliest_fill_timestamp =
            (now + self.block_lead_duration.as_secs()).saturating_sub(DEADLINE_DRIFT_BUFFER_SECS);

        let not_expired = not_expired_at(move || earliest_fill_timestamp);
        let not_expired_with_metric = move |order: &SignedOrder| {
            let kept = not_expired(order);
            if !kept {
                trace!(
                    order_hash = %order.order_hash(),
                    deadline = %order.permit().permit.deadline,
                    earliest_fill_timestamp,
                    "skipping expired order"
                );
                metrics::record_order_skipped(metrics::OrderSkippedReason::Expired);
            }
            kept
        };
        let filled_orders = &self.filled_orders;
        let not_in_filled_cache = move |order: &SignedOrder| {
            let cached = filled_orders.lock().unwrap().contains(order.order_hash());
            if cached {
                trace!(order_hash = %order.order_hash(), "skipping cached filled order");
                metrics::record_order_skipped(metrics::OrderSkippedReason::AlreadyFilled);
                false
            } else {
                true
            }
        };

        let orders: Vec<SignedOrder> = self
            .filler
            .get_orders()
            .inspect_ok(|_| orders_count += 1)
            .filter_orders(not_expired_with_metric)
            .inspect_ok(|_| orders_after_expiry_filter += 1)
            .filter_orders(not_in_filled_cache)
            .try_collect()
            .await
            .inspect_err(|_| metrics::record_fetch_order_error())
            .wrap_err("failed to fetch orders")?;

        metrics::record_orders_fetched(orders_count);

        if orders.is_empty() {
            if orders_count == 0 {
                info!("no orders fetched from transaction cache");
            } else {
                info!(
                    orders_count,
                    expired = orders_count - orders_after_expiry_filter,
                    already_filled = orders_after_expiry_filter,
                    "all fetched orders filtered out"
                );
            }
            return Ok(Vec::new());
        }

        let mut scored: Vec<(i128, SignedOrder)> = orders
            .into_iter()
            .filter_map(|order| match self.pricing_client.profitability(&order) {
                Ok(Some(margin)) => Some((margin, order)),
                Ok(None) => {
                    trace!(order_hash = %order.order_hash(), "order exceeds max loss threshold");
                    metrics::record_order_skipped(metrics::OrderSkippedReason::ExceedsMaxLoss);
                    None
                }
                Err(FixedPricingError::UnknownToken(token)) => {
                    warn!(order_hash = %order.order_hash(), %token, "unknown token, skipping");
                    metrics::record_order_skipped(metrics::OrderSkippedReason::UnknownToken);
                    None
                }
                Err(error) => {
                    warn!(order_hash = %order.order_hash(), %error, "profitability check failed");
                    metrics::record_pricing_error();
                    None
                }
            })
            .collect();

        if scored.is_empty() {
            info!(orders_count, "no profitable orders");
            return Ok(Vec::new());
        }

        scored.sort_by_key(|entry| Reverse(entry.0));
        Ok(scored)
    }

    /// Builds a per-cycle budget map and checks Permit2 nonces, then selects orders that pass both
    /// budget and nonce checks in profitability order.
    #[instrument(skip_all, fields(scored_len = scored.len()))]
    async fn select_fillable_orders(&self, scored: Vec<(i128, SignedOrder)>) -> Vec<SignedOrder> {
        let (mut working_map, filled_hashes) = tokio::join!(
            WorkingMap::build(
                &scored,
                self.filler.signer().address(),
                self.filler.submitter().ru_provider(),
                self.filler.submitter().host_provider(),
                self.filler.constants(),
                &self.allowance_cache,
            ),
            async {
                join_all(scored.iter().map(|(_margin, order)| self.check_filled(order)))
                    .await
                    .into_iter()
                    .flatten()
                    .collect::<HashSet<B256>>()
            },
        );

        let mut orders_to_fill = Vec::new();
        for (_margin, order) in scored {
            if filled_hashes.contains(order.order_hash()) {
                continue;
            }

            if !working_map.can_fill(&order) {
                trace!(
                    order_hash = %order.order_hash(),
                    "insufficient filler balance or allowance, skipping"
                );
                metrics::record_order_skipped(
                    metrics::OrderSkippedReason::InsufficientFillerBalance,
                );
                continue;
            }

            working_map.accept_order(&order);
            orders_to_fill.push(order);
        }

        orders_to_fill
    }

    /// Chunks orders by `max_orders_per_bundle` and submits each chunk sequentially so the most
    /// profitable chunk acquires the lowest nonce via `CachedNonceManager`. Relies on the builder
    /// ordering a sender's txs by nonce within a block for profitability ordering to hold.
    ///
    /// Stops submitting on the first chunk that fails to submit: `CachedNonceManager` advances its
    /// cached nonce on every call regardless of submission outcome, so if chunk K fails the
    /// builder sees a nonce gap at K and chunks K+1.. cannot land anyway - continuing would just
    /// waste RPC calls and muddle metrics.
    #[instrument(skip_all, fields(orders_to_fill = orders_to_fill.len()))]
    async fn submit_bundles(&self, orders_to_fill: Vec<SignedOrder>) {
        debug_assert!(!orders_to_fill.is_empty(), "orders_to_fill is empty");
        let chunks = chunk_orders(orders_to_fill, self.max_orders_per_bundle);
        let chunk_count = chunks.len();
        if self.max_orders_per_bundle.is_some() {
            metrics::record_chunks_per_cycle(chunk_count as f64);
        }
        let mut successful_chunks = 0_usize;
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            let span = info_span!(
                "submit_one_bundle",
                orders_in_bundle = chunk.len(),
                chunk_index = chunk_index,
                chunk_count = chunk_count,
            );
            if !self.submit_one_bundle(chunk).instrument(span).await {
                break;
            }
            successful_chunks += 1;
        }
        if successful_chunks > 0 && successful_chunks < chunk_count {
            warn!(
                successful_chunks,
                chunk_count,
                "stopped submitting after a chunk failed; remaining chunks skipped to avoid \
                 creating a nonce gap the builder cannot fill"
            );
        }
    }

    /// Submits a single fill bundle and records metrics. Returns `true` on success.
    async fn submit_one_bundle(&self, orders: Vec<SignedOrder>) -> bool {
        debug_assert!(!orders.is_empty(), "orders is empty");
        let orders_in_bundle = orders.len();
        // Record attempted-bundle size regardless of submission outcome so the histogram and
        // counter reflect what we tried to fill, not just what succeeded. Success / failure is
        // carried by the `BUNDLES` counter's `result` label.
        metrics::record_orders_in_bundle(orders_in_bundle as u64);
        metrics::record_orders_per_bundle(orders_in_bundle as f64);
        match self.filler.fill(orders, self.target_blocks).await {
            Ok(responses) => {
                info!(
                    bundle_ids = ?responses.iter().map(|response| response.id).collect::<Vec<_>>(),
                    orders_in_bundle,
                    "successfully submitted fill bundle"
                );
                metrics::record_bundle(metrics::SubmissionResult::Success);
                true
            }
            Err(error) => {
                warn!(%error, orders_in_bundle, "failed to fill orders");
                metrics::record_bundle(metrics::SubmissionResult::Failure);
                false
            }
        }
    }

    /// Returns an [`Instant`] corresponding to the very first submission anchor:
    /// `host_start_timestamp - block_lead_duration`. This will typically be far in the past, but
    /// that's intentional - [`tokio::time::interval_at`] with [`MissedTickBehavior::Skip`]
    /// fast-forwards over all elapsed ticks and fires at the next one that falls in the future.
    fn submission_anchor_instant(&self) -> Instant {
        let anchor =
            UNIX_EPOCH + Duration::from_secs(self.host_start_timestamp) - self.block_lead_duration;
        let now_system = SystemTime::now();
        let now_instant = Instant::now();
        let elapsed =
            now_system.duration_since(anchor).expect("system clock before first submission anchor");
        now_instant - elapsed
    }

    /// Checks whether the order's Permit2 nonce has been consumed on the rollup chain. Returns
    /// `Some(order_hash)` if filled, `None` if unfilled or on RPC error.
    async fn check_filled(&self, order: &SignedOrder) -> Option<B256> {
        let is_filled = match signet_orders::permit2::is_order_nonce_consumed(
            self.filler.submitter().ru_provider(),
            order,
        )
        .await
        {
            Ok(consumed) => consumed,
            Err(error) => {
                warn!(
                    order_hash = %order.order_hash(),
                    %error,
                    "failed to check Permit2 nonce bitmap, assuming not filled"
                );
                metrics::record_nonce_check_error();
                return None;
            }
        };

        if is_filled {
            trace!(order_hash = %order.order_hash(), "order already filled");
            self.filled_orders.lock().unwrap().put(*order.order_hash(), ());
            metrics::record_order_skipped(metrics::OrderSkippedReason::AlreadyFilled);
            Some(*order.order_hash())
        } else {
            None
        }
    }
}

/// Splits `orders` into chunks of at most `cap` while preserving order. Returns a single chunk
/// containing all orders when `cap` is `None` or when `orders.len() <= cap`.
fn chunk_orders(mut orders: Vec<SignedOrder>, cap: Option<NonZeroUsize>) -> Vec<Vec<SignedOrder>> {
    debug_assert!(!orders.is_empty(), "orders is empty");
    // Uses `split_off` rather than `Vec::chunks().map(<to_vec>)` to avoid cloning each
    // `SignedOrder` (which carries a `Vec<TokenPermissions>` and `Vec<Output>`).
    let Some(cap) = cap.map(NonZeroUsize::get) else {
        return vec![orders];
    };
    let mut chunks = Vec::with_capacity(orders.len().div_ceil(cap));
    while orders.len() > cap {
        let rest = orders.split_off(cap);
        chunks.push(orders);
        orders = rest;
    }
    chunks.push(orders);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, Bytes, U256};
    use signet_zenith::RollupOrders::{Permit2Batch, PermitBatchTransferFrom, TokenPermissions};

    fn build_order(id: u64) -> SignedOrder {
        SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions {
                        token: Address::ZERO,
                        amount: U256::from(1u64),
                    }],
                    nonce: U256::from(id),
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            vec![],
        )
    }

    // `build_order`'s `id` becomes the Permit2 nonce, used here purely as a distinguishing id so
    // the chunking tests can assert on order identity.
    fn distinguishable_orders(count: u64) -> Vec<SignedOrder> {
        (0..count).map(build_order).collect()
    }

    fn ids(orders: &[SignedOrder]) -> Vec<u64> {
        orders.iter().map(|order| order.permit().permit.nonce.saturating_to()).collect()
    }

    #[test]
    fn chunk_orders_returns_single_chunk_when_cap_unset() {
        let chunks = chunk_orders(distinguishable_orders(12), None);
        assert_eq!(chunks.len(), 1);
        assert_eq!(ids(&chunks[0]), (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_orders_returns_single_chunk_when_within_cap() {
        let chunks = chunk_orders(distinguishable_orders(3), NonZeroUsize::new(5));
        assert_eq!(chunks.len(), 1);
        assert_eq!(ids(&chunks[0]), (0..3).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_orders_splits_into_equal_chunks_and_preserves_order() {
        let chunks = chunk_orders(distinguishable_orders(10), NonZeroUsize::new(5));
        assert_eq!(chunks.len(), 2);
        assert_eq!(ids(&chunks[0]), (0..5).collect::<Vec<_>>());
        assert_eq!(ids(&chunks[1]), (5..10).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_orders_leaves_short_final_chunk_for_uneven_split() {
        let chunks = chunk_orders(distinguishable_orders(7), NonZeroUsize::new(3));
        assert_eq!(chunks.len(), 3);
        assert_eq!(ids(&chunks[0]), vec![0, 1, 2]);
        assert_eq!(ids(&chunks[1]), vec![3, 4, 5]);
        assert_eq!(ids(&chunks[2]), vec![6]);
    }

    // Boundary test: the split loop uses `>` rather than `>=`, so with `orders.len() == cap` we
    // expect a single chunk holding every order.
    #[test]
    fn chunk_orders_returns_single_chunk_when_count_equals_cap() {
        let chunks = chunk_orders(distinguishable_orders(5), NonZeroUsize::new(5));
        assert_eq!(chunks.len(), 1);
        assert_eq!(ids(&chunks[0]), (0..5).collect::<Vec<_>>());
    }

    #[test]
    fn chunk_orders_cap_of_one_gives_one_order_per_chunk() {
        let chunks = chunk_orders(distinguishable_orders(3), NonZeroUsize::new(1));
        assert_eq!(chunks.len(), 3);
        assert_eq!(ids(&chunks[0]), vec![0]);
        assert_eq!(ids(&chunks[1]), vec![1]);
        assert_eq!(ids(&chunks[2]), vec![2]);
    }
}
