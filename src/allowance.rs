use crate::FillerContext;
use crate::{ChainTokenPair, FillProviderType, IERC20, KnownToken, metrics};
use alloy::primitives::{Address, U256};
use alloy::signers::Signer;
use core::fmt::{self, Display, Formatter};
use futures_util::future::join_all;
use init4_bin_base::deps::tracing::{debug, info, instrument, trace, warn};
use signet_orders::permit2::PERMIT2;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::{select, time::Duration};
use tokio_util::sync::CancellationToken;

/// How often the background task refreshes cached Permit2 allowances.
const REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// Cached Permit2 allowances for known tokens, shared between the background refresh task and the
/// per-cycle balance filter.
#[derive(Debug, Clone)]
pub(crate) struct AllowanceCache {
    inner: Arc<RwLock<HashMap<ChainTokenPair, U256>>>,
}

impl AllowanceCache {
    /// Create an empty cache.
    pub(crate) fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())) }
    }

    /// Read the cached allowance for a token.
    pub(crate) fn get(&self, key: &ChainTokenPair) -> Option<U256> {
        self.inner.read().unwrap().get(key).copied()
    }

    /// Merge successfully queried allowances into the cache. Tokens whose queries failed retain
    /// their previous cached value rather than falling to zero on a transient RPC error.
    fn update(&self, new_entries: HashMap<ChainTokenPair, U256>) {
        self.inner.write().unwrap().extend(new_entries);
    }
}

/// Per-token outcome of the Permit2 allowance query. The error is wrapped as an [`eyre::Report`]
/// so the success value and the failure reason can flow together through the refresh pipeline
/// while preserving the underlying alloy error chain for log output.
#[derive(Debug)]
struct AllowanceQueryResult {
    chain_token: ChainTokenPair,
    outcome: eyre::Result<U256>,
}

/// Wrapper for logging an allowance value: prints the decimal representation, except for
/// [`U256::MAX`] which is rendered as the sentinel `U256::MAX` since the raw decimal is a 78-digit
/// number that offers no extra information and obscures the "unlimited approval" meaning.
struct DisplayAllowance(U256);

impl Display for DisplayAllowance {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if self.0 == U256::MAX {
            formatter.write_str("U256::MAX (treated as unlimited, doesn't auto decrement)")
        } else {
            write!(formatter, "{}", self.0)
        }
    }
}

/// Background task that periodically refreshes Permit2 allowances for all known ERC20 tokens.
#[derive(Debug)]
pub struct AllowanceRefreshTask {
    cache: AllowanceCache,
    ru_provider: FillProviderType,
    host_provider: FillProviderType,
    filler_address: Address,
    tokens: [ChainTokenPair; 6],
    ru_chain_id: u64,
    cancellation_token: CancellationToken,
}

impl AllowanceRefreshTask {
    /// Create a new allowance refresh task and perform an initial refresh so the cache is warm.
    #[instrument(skip_all, name = "initialize_allowance_refresh_task")]
    pub async fn initialize(context: &FillerContext) -> Self {
        let ru_chain_id = context.constants().system().ru_chain_id();
        let tokens = KnownToken::ERC20.map(|known| known.resolve(context.constants().system()));

        let cache = context.allowance_cache().clone();
        let ru_provider = context.ru_provider().clone();
        let host_provider = context.host_provider().clone();
        let filler_address = context.signer().address();
        let cancellation_token = context.cancellation_token().clone();

        let task = Self {
            cache,
            ru_provider,
            host_provider,
            filler_address,
            tokens,
            ru_chain_id,
            cancellation_token,
        };
        select! {
            biased;
            _ = context.cancellation_token().cancelled() => {
                debug!("allowance refresh task initialization cancelled");
            }
            _ = task.startup_refresh() => {}
        }
        task
    }

    /// Run the periodic refresh loop.
    pub async fn run(self) {
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        // Consume the immediate first tick; the caller awaits `initialize` before spawning this.
        interval.tick().await;

        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => {
                    debug!("allowance refresh task cancelled");
                    break;
                }
                _ = interval.tick() => {
                    self.refresh().await;
                }
            }
        }
    }

    /// Queries Permit2 allowances for all known tokens, updates the cache, and logs a compact
    /// summary for the periodic refresh path.
    #[instrument(skip_all, fields(token_count = self.tokens.len()))]
    async fn refresh(&self) {
        let results = self.query_and_cache().await;
        for AllowanceQueryResult { chain_token, outcome } in results {
            outcome
                .map(|allowance| {
                    trace!(
                        %chain_token,
                        allowance = %DisplayAllowance(allowance),
                        "refreshed Permit2 allowance",
                    );
                })
                .unwrap_or_else(|error| {
                    metrics::record_preflight_query_error(metrics::PreflightQuery::Allowance);
                    warn!(
                        %chain_token,
                        error = format!("{error:#}"),
                        "failed to refresh Permit2 allowance",
                    );
                });
        }
        info!("allowance cache refreshed");
    }

    /// Same query + cache update as [`Self::refresh`], but logs one info line per token and a
    /// summary warning if no known token has a non-zero allowance. Mirrors the startup balance
    /// report in `initialization.rs` so operators see both funding and Permit2 approvals surfaced
    /// the same way at deploy time.
    #[instrument(skip_all)]
    async fn startup_refresh(&self) {
        let results = self.query_and_cache().await;
        let total = results.len();
        let mut non_zero_count = 0_usize;
        let mut error_count = 0_usize;
        for AllowanceQueryResult { chain_token, outcome } in results {
            outcome
                .map(|allowance| {
                    if !allowance.is_zero() {
                        non_zero_count += 1;
                    }
                    info!(
                        %chain_token,
                        allowance = %DisplayAllowance(allowance),
                        "startup allowance",
                    );
                })
                .unwrap_or_else(|error| {
                    error_count += 1;
                    metrics::record_preflight_query_error(metrics::PreflightQuery::Allowance);
                    warn!(
                        %chain_token,
                        error = format!("{error:#}"),
                        "failed to query startup allowance",
                    );
                });
        }
        if total > 0 && error_count == total {
            warn!(
                "all startup allowance queries failed; could not determine Permit2 approval state"
            );
        } else if non_zero_count == 0 {
            warn!(
                error_count,
                "no non-zero allowance found among successfully-queried tokens; check Permit2 approvals"
            );
        }
    }

    /// Queries Permit2 allowances for all known tokens concurrently, merges every successful
    /// result into the cache, and returns the full per-token results (including errors) for the
    /// caller to log. Failed queries are filtered out before [`AllowanceCache::update`] is called,
    /// so a token whose query fails this round retains its previously cached value rather than
    /// being clobbered.
    async fn query_and_cache(&self) -> Vec<AllowanceQueryResult> {
        let results: Vec<AllowanceQueryResult> =
            join_all(self.tokens.iter().map(|chain_token| async move {
                let provider = if chain_token.chain_id() == self.ru_chain_id {
                    &self.ru_provider
                } else {
                    &self.host_provider
                };
                let contract = IERC20::new(chain_token.token(), provider);
                let outcome = contract
                    .allowance(self.filler_address, PERMIT2)
                    .call()
                    .await
                    .map_err(eyre::Report::from);
                AllowanceQueryResult { chain_token: *chain_token, outcome }
            }))
            .await;

        let entries: HashMap<ChainTokenPair, U256> = results
            .iter()
            .filter_map(|entry| {
                entry.outcome.as_ref().ok().map(|allowance| (entry.chain_token, *allowance))
            })
            .collect();
        self.cache.update(entries);

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    const CHAIN: u64 = 1;
    const TOKEN_A: Address = Address::repeat_byte(0xAA);
    const TOKEN_B: Address = Address::repeat_byte(0xBB);

    #[test]
    fn get_on_empty_cache_returns_none() {
        let cache = AllowanceCache::new();
        assert_eq!(cache.get(&ChainTokenPair::new(CHAIN, TOKEN_A)), None);
    }

    #[test]
    fn update_then_get_returns_value() {
        let cache = AllowanceCache::new();
        let key = ChainTokenPair::new(CHAIN, TOKEN_A);
        cache.update(HashMap::from([(key, U256::from(42))]));
        assert_eq!(cache.get(&key), Some(U256::from(42)));
    }

    #[test]
    fn update_preserves_existing_entries() {
        let cache = AllowanceCache::new();
        let key_a = ChainTokenPair::new(CHAIN, TOKEN_A);
        let key_b = ChainTokenPair::new(CHAIN, TOKEN_B);

        cache.update(HashMap::from([(key_a, U256::from(10))]));
        cache.update(HashMap::from([(key_b, U256::from(20))]));

        assert_eq!(cache.get(&key_a), Some(U256::from(10)));
        assert_eq!(cache.get(&key_b), Some(U256::from(20)));
    }

    #[test]
    fn update_overwrites_existing_key() {
        let cache = AllowanceCache::new();
        let key = ChainTokenPair::new(CHAIN, TOKEN_A);

        cache.update(HashMap::from([(key, U256::from(10))]));
        cache.update(HashMap::from([(key, U256::from(99))]));

        assert_eq!(cache.get(&key), Some(U256::from(99)));
    }
}
