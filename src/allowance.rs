use crate::FillerContext;
use crate::{ChainTokenPair, FillProviderType, metrics};
use alloy::signers::Signer;
use alloy::{
    primitives::{Address, U256},
    sol,
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use init4_bin_base::deps::tracing::{debug, info, instrument, trace, warn};
use signet_orders::permit2::PERMIT2;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::{select, time::Duration};
use tokio_util::sync::CancellationToken;

sol! {
    /// Minimal ERC20 interface for allowance queries.
    #[sol(rpc)]
    interface IERC20Allowance {
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

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
        let tokens = ChainTokenPair::known_erc20_tokens(context.constants().system())
            .map(|(pair, _name)| pair);

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
            _ = task.refresh() => {}
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

    /// Queries Permit2 allowances for all known tokens and updates the cache.
    #[instrument(skip_all, fields(token_count = self.tokens.len()))]
    async fn refresh(&self) {
        let entries = self
            .tokens
            .iter()
            .map(|chain_token| async move {
                let provider = if chain_token.chain_id() == self.ru_chain_id {
                    &self.ru_provider
                } else {
                    &self.host_provider
                };
                let contract = IERC20Allowance::new(chain_token.token(), provider);
                let allowance_call = contract.allowance(self.filler_address, PERMIT2);
                match allowance_call.call().await {
                    Ok(allowance) => {
                        trace!(%chain_token, %allowance, "refreshed Permit2 allowance");
                        Some((*chain_token, allowance))
                    }
                    Err(error) => {
                        warn!(%error, %chain_token, "failed to refresh Permit2 allowance");
                        metrics::record_preflight_query_error(metrics::PreflightQuery::Allowance);
                        None
                    }
                }
            })
            .collect::<FuturesUnordered<_>>()
            .filter_map(|result| async move { result })
            .collect()
            .await;

        self.cache.update(entries);
        info!("allowance cache refreshed");
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
