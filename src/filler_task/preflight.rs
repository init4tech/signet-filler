use crate::{AllowanceCache, ChainTokenPair, metrics};
use alloy::{
    primitives::{Address, U256},
    providers::Provider,
    sol,
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use init4_bin_base::deps::tracing::{debug, instrument, warn};
use signet_constants::{NATIVE_TOKEN_ADDRESS, SignetSystemConstants};
use signet_types::SignedOrder;
use std::collections::{HashMap, HashSet};

sol! {
    /// Minimal ERC20 interface for balance queries.
    #[sol(rpc)]
    interface IERC20Balance {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Token budget tracker for a single processing cycle. Initialized from fresh balance queries and
/// a snapshot of the allowance cache, then decremented as orders are accepted.
#[derive(Debug)]
struct TokenBudget {
    balance: U256,
    allowance: U256,
}

impl TokenBudget {
    /// Whether this budget can cover the given output amount.
    fn can_cover(&self, amount: U256) -> bool {
        self.balance >= amount && self.allowance >= amount
    }

    /// Decrement the budget by the given amount. Allowance is only decremented when it is not
    /// `U256::MAX`, matching ERC20 implementations that treat max approval as infinite.
    fn decrement(&mut self, amount: U256) {
        self.balance = self.balance.saturating_sub(amount);
        if self.allowance != U256::MAX {
            self.allowance = self.allowance.saturating_sub(amount);
        }
    }
}

/// Per-cycle token budget map. Tracks remaining filler balances and allowances, decremented as
/// orders are accepted into a bundle.
///
/// Built fresh each cycle from on-chain balance queries and cached Permit2 allowances.
#[derive(Debug)]
pub(super) struct WorkingMap {
    inner: HashMap<ChainTokenPair, TokenBudget>,
}

impl WorkingMap {
    /// Builds a working map for the output tokens needed by the given candidates. Balances are
    /// queried fresh from the chain; allowances are copied from the background-refreshed cache.
    #[instrument(skip_all, name = "build_working_map", fields(candidates_len = candidates.len()))]
    pub(super) async fn build(
        candidates: &[(i128, SignedOrder)],
        filler_address: Address,
        ru_provider: &super::FillProviderType,
        host_provider: &super::FillProviderType,
        constants: &SignetSystemConstants,
        allowance_cache: &AllowanceCache,
    ) -> Self {
        let ru_chain_id = constants.ru_chain_id();
        let host_chain_id = constants.host_chain_id();

        // Collect unique tokens from all candidate outputs on known chains.
        let tokens_needed: HashSet<ChainTokenPair> = candidates
            .iter()
            .flat_map(|(_margin, order)| order.outputs())
            .filter_map(|output| {
                let output_chain_id = u64::from(output.chainId);
                (output_chain_id == ru_chain_id || output_chain_id == host_chain_id)
                    .then(|| ChainTokenPair::new(output_chain_id, output.token))
            })
            .collect();

        // Get the filler's allowance for each token from the allowance cache.
        let allowances: HashMap<ChainTokenPair, U256> = tokens_needed
            .into_iter()
            .map(|chain_token| {
                // Native tokens don't need Permit2 allowance.
                let allowance = if chain_token.token() == NATIVE_TOKEN_ADDRESS {
                    U256::MAX
                } else {
                    allowance_cache.get(&chain_token).unwrap_or_else(|| {
                        debug!(%chain_token, "no cached allowance, assuming zero");
                        U256::ZERO
                    })
                };
                (chain_token, allowance)
            })
            .collect();

        // Query fresh balances concurrently, pairing each with its cached allowance.
        let inner = allowances
            .into_iter()
            .map(|(chain_token, allowance)| async move {
                let provider =
                    if chain_token.chain_id() == ru_chain_id { ru_provider } else { host_provider };
                let balance = if chain_token.token() == NATIVE_TOKEN_ADDRESS {
                    provider.get_balance(filler_address).await.map_err(|error| {
                        warn!(%error, %chain_token, "failed to query native balance");
                        metrics::record_preflight_query_error(metrics::PreflightQuery::Balance);
                    })
                } else {
                    let contract = IERC20Balance::new(chain_token.token(), provider);
                    let balance_call = contract.balanceOf(filler_address);
                    balance_call.call().await.map_err(|error| {
                        warn!(%error, %chain_token, "failed to query token balance");
                        metrics::record_preflight_query_error(metrics::PreflightQuery::Balance);
                    })
                };
                balance.ok().map(|balance| (chain_token, TokenBudget { balance, allowance }))
            })
            .collect::<FuturesUnordered<_>>()
            .filter_map(|result| async { result })
            .collect()
            .await;

        Self { inner }
    }

    /// Whether all outputs of an order can be covered by the current budgets. Aggregates multiple
    /// outputs for the same token before checking.
    pub(super) fn can_fill(&self, order: &SignedOrder) -> bool {
        let totals = order.outputs().iter().fold(
            HashMap::<ChainTokenPair, U256>::new(),
            |mut map, output| {
                let key = ChainTokenPair::new(u64::from(output.chainId), output.token);
                let total = map.entry(key).or_default();
                *total = total.saturating_add(output.amount);
                map
            },
        );
        totals.iter().all(|(key, &total)| match self.inner.get(key) {
            Some(budget) => budget.can_cover(total),
            None => {
                warn!(%key, %total, "no budget entry for token, allowing order through");
                true
            }
        })
    }

    /// Decrements budgets for all outputs of an accepted order.
    pub(super) fn accept_order(&mut self, order: &SignedOrder) {
        for output in order.outputs() {
            let key = ChainTokenPair::new(u64::from(output.chainId), output.token);
            if let Some(budget) = self.inner.get_mut(&key) {
                budget.decrement(output.amount)
            }
        }
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (ChainTokenPair, U256, U256)>) -> Self {
        let inner = entries
            .into_iter()
            .map(|(token, balance, allowance)| (token, TokenBudget { balance, allowance }))
            .collect();
        Self { inner }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Bytes;
    use signet_zenith::RollupOrders::{
        Output, Permit2Batch, PermitBatchTransferFrom, TokenPermissions,
    };

    const CHAIN_A: u64 = 1;
    const CHAIN_B: u64 = 2;
    const TOKEN_X: Address = Address::repeat_byte(0xAA);
    const TOKEN_Y: Address = Address::repeat_byte(0xBB);

    fn order_with_outputs(outputs: Vec<Output>) -> SignedOrder {
        SignedOrder::new(
            Permit2Batch {
                permit: PermitBatchTransferFrom {
                    permitted: vec![TokenPermissions { token: TOKEN_X, amount: U256::from(1) }],
                    nonce: U256::ZERO,
                    deadline: U256::ZERO,
                },
                owner: Address::ZERO,
                signature: Bytes::from([0; 65]),
            },
            outputs,
        )
    }

    fn output(chain_id: u32, token: Address, amount: u64) -> Output {
        Output { token, amount: U256::from(amount), recipient: Address::ZERO, chainId: chain_id }
    }

    // -- TokenBudget --

    #[test]
    fn can_cover_within_both_limits() {
        let budget = TokenBudget { balance: U256::from(100), allowance: U256::from(100) };
        assert!(budget.can_cover(U256::from(100)));
        assert!(budget.can_cover(U256::from(50)));
    }

    #[test]
    fn can_cover_exceeds_balance() {
        let budget = TokenBudget { balance: U256::from(99), allowance: U256::from(200) };
        assert!(!budget.can_cover(U256::from(100)));
    }

    #[test]
    fn can_cover_exceeds_allowance() {
        let budget = TokenBudget { balance: U256::from(200), allowance: U256::from(99) };
        assert!(!budget.can_cover(U256::from(100)));
    }

    #[test]
    fn decrement_reduces_both() {
        let mut budget = TokenBudget { balance: U256::from(100), allowance: U256::from(80) };
        budget.decrement(U256::from(30));
        assert_eq!(budget.balance, U256::from(70));
        assert_eq!(budget.allowance, U256::from(50));
    }

    #[test]
    fn decrement_skips_max_allowance() {
        let mut budget = TokenBudget { balance: U256::from(100), allowance: U256::MAX };
        budget.decrement(U256::from(40));
        assert_eq!(budget.balance, U256::from(60));
        assert_eq!(budget.allowance, U256::MAX);
    }

    // -- WorkingMap::can_fill --

    #[test]
    fn can_fill_single_output_within_budget() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let map = WorkingMap::from_entries([(pair, U256::from(1000), U256::from(1000))]);
        let order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 500)]);
        assert!(map.can_fill(&order));
    }

    #[test]
    fn can_fill_max_allowance_only_checks_balance() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let map = WorkingMap::from_entries([(pair, U256::from(500), U256::MAX)]);
        let order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 500)]);
        assert!(map.can_fill(&order));

        let over_balance = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 501)]);
        assert!(!map.can_fill(&over_balance));
    }

    #[test]
    fn can_fill_single_output_exceeds_budget() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let map = WorkingMap::from_entries([(pair, U256::from(100), U256::from(1000))]);
        let order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 200)]);
        assert!(!map.can_fill(&order));
    }

    #[test]
    fn can_fill_aggregates_same_token_outputs() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let map = WorkingMap::from_entries([(pair, U256::from(150), U256::from(150))]);
        // Two outputs of 80 each = 160 total, exceeds 150 budget.
        let order = order_with_outputs(vec![
            output(CHAIN_A as u32, TOKEN_X, 80),
            output(CHAIN_A as u32, TOKEN_X, 80),
        ]);
        assert!(!map.can_fill(&order));
    }

    #[test]
    fn can_fill_multiple_tokens_both_covered() {
        let pair_x = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let pair_y = ChainTokenPair::new(CHAIN_B, TOKEN_Y);
        let map = WorkingMap::from_entries([
            (pair_x, U256::from(100), U256::from(100)),
            (pair_y, U256::from(200), U256::from(200)),
        ]);
        let order = order_with_outputs(vec![
            output(CHAIN_A as u32, TOKEN_X, 50),
            output(CHAIN_B as u32, TOKEN_Y, 150),
        ]);
        assert!(map.can_fill(&order));
    }

    #[test]
    fn can_fill_one_token_short_fails_whole_order() {
        let pair_x = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let pair_y = ChainTokenPair::new(CHAIN_B, TOKEN_Y);
        let map = WorkingMap::from_entries([
            (pair_x, U256::from(100), U256::from(100)),
            (pair_y, U256::from(10), U256::from(200)),
        ]);
        let order = order_with_outputs(vec![
            output(CHAIN_A as u32, TOKEN_X, 50),
            output(CHAIN_B as u32, TOKEN_Y, 150),
        ]);
        assert!(!map.can_fill(&order));
    }

    #[test]
    fn can_fill_unknown_token_passes_through() {
        let map = WorkingMap::from_entries([]);
        let order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 100)]);
        assert!(map.can_fill(&order));
    }

    /// When a balance query fails during `WorkingMap::build`, the token is dropped from the map.
    /// An order requiring that token then passes `can_fill` because the missing-entry path returns
    /// `true`. This test documents that behavior - a partial RPC failure silently disables the
    /// budget check for the affected token.
    #[test]
    fn can_fill_missing_budget_from_failed_query_allows_order_through() {
        let pair_x = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let map = WorkingMap::from_entries([(pair_x, U256::from(1000), U256::from(1000))]);

        // Order requires both TOKEN_X (has budget) and TOKEN_Y (missing from map).
        let order = order_with_outputs(vec![
            output(CHAIN_A as u32, TOKEN_X, 500),
            output(CHAIN_B as u32, TOKEN_Y, 500),
        ]);

        // TOKEN_Y has no budget entry, so the order passes through despite potentially being
        // unfillable for that token.
        assert!(map.can_fill(&order));
    }

    // -- WorkingMap::accept_order --

    #[test]
    fn accept_order_decrements_budget() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let mut map = WorkingMap::from_entries([(pair, U256::from(1000), U256::from(1000))]);
        let order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 300)]);

        map.accept_order(&order);

        // Budget should now be 700 - next order of 800 should fail.
        let big_order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 800)]);
        assert!(!map.can_fill(&big_order));

        // But 700 should still pass.
        let exact_order = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 700)]);
        assert!(map.can_fill(&exact_order));
    }

    /// Simulates the `select_fillable_orders` loop: orders sorted most-profitable-first are
    /// checked against the budget and accepted in order until the budget is exhausted.
    #[test]
    fn budget_exhaustion_accepts_orders_in_profitability_order() {
        let pair = ChainTokenPair::new(CHAIN_A, TOKEN_X);
        let mut map = WorkingMap::from_entries([(pair, U256::from(500), U256::from(500))]);

        // Three orders sorted by descending profitability (margins are for illustration only -
        // the WorkingMap doesn't see margins, just output amounts).
        let best = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 300)]);
        let mid = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 200)]);
        let worst = order_with_outputs(vec![output(CHAIN_A as u32, TOKEN_X, 100)]);

        let orders = [best, mid, worst];
        let mut accepted = Vec::new();
        for order in &orders {
            if map.can_fill(order) {
                map.accept_order(order);
                accepted.push(order);
            }
        }

        // Budget of 500: best (300) + mid (200) = 500 exactly, worst is rejected.
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].order_hash(), orders[0].order_hash());
        assert_eq!(accepted[1].order_hash(), orders[1].order_hash());
    }
}
