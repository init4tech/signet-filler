use crate::{
    Config,
    pricing::{PricingClient, StaticPricingClient},
};
use alloy::{
    network::EthereumWallet,
    primitives::{Address, B256, U256, address},
    providers::{
        RootProvider,
        fillers::{FillProvider, JoinFill, WalletFiller},
        utils::JoinedRecommendedFillers,
    },
    sol,
};
use eyre::{Context, Report, Result, bail};
use futures_util::{TryStreamExt, future::join_all};
use init4_bin_base::{
    deps::tracing::{debug, error, info, instrument, trace, warn},
    utils::signer::LocalOrAws,
};
use lru::LruCache;
use signet_orders::{FeePolicySubmitter, FillerError, FillerOptions};
use signet_tx_cache::TxCache;
use signet_types::SignedOrder;
use std::{
    num::NonZeroUsize,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    select,
    time::{Duration, Instant, MissedTickBehavior},
    try_join,
};
use tokio_util::sync::CancellationToken;

mod initialization;

// Minimal Permit2 binding for querying the nonce bitmap.
sol! {
    #[sol(rpc)]
    interface IPermit2 {
        function nonceBitmap(address owner, uint256 wordPos) external view returns (uint256);
    }
}
const PERMIT2: Address = address!("000000000022D473030F116dDEE9F6B43aC78BA3");
const FILLED_ORDERS_CACHE_SIZE: NonZeroUsize = NonZeroUsize::new(10240).unwrap();

type FillProviderType =
    FillProvider<JoinFill<JoinedRecommendedFillers, WalletFiller<EthereumWallet>>, RootProvider>;
type Filler = signet_orders::Filler<
    LocalOrAws,
    TxCache,
    FeePolicySubmitter<FillProviderType, FillProviderType, TxCache>,
>;

/// Order filler service that submits fill bundles shortly before each block boundary.
pub struct FillerTask {
    filler: Filler,
    pricing_client: StaticPricingClient,
    filled_orders: Mutex<LruCache<B256, ()>>,
    block_lead_duration: Duration,
    slot_duration: u64,
    start_timestamp: u64,
    cancellation_token: CancellationToken,
}

impl FillerTask {
    /// Create a new filler from configuration.
    #[instrument(skip_all)]
    pub async fn initialize(
        config: &Config,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let constants = config.constants();

        let signer = initialization::connect_signer(config.signer()).await?;
        let wallet = EthereumWallet::from(signer.clone());

        let (host_provider, ru_provider, tx_cache) = select! {
            biased;
            _ = cancellation_token.cancelled() => {
                debug!("cancelling initialization");
                bail!("initialization cancelled");
            }
            result = async {
                try_join!(
                    initialization::connect_to_host_provider(config.host_rpc(), wallet.clone()),
                    initialization::connect_to_rollup_provider(config.ru_rpc(), wallet),
                    initialization::connect_to_tx_cache(constants.environment().transaction_cache())
                )
            } => result.wrap_err("initialization failure")?,
        };

        let submitter = FeePolicySubmitter::new(
            ru_provider,
            host_provider,
            tx_cache.clone(),
            constants.system().clone(),
        );

        // FillerOptions has two optional fields:
        // - `nonce`: Permit2 nonce for fill signatures. When None (default), a fresh nonce is
        //   generated from the current timestamp in microseconds for each fill.
        // - `deadline_offset`: Seconds from now until the Permit2 signature expires. When `None`,
        //   defaults to 12 seconds. A longer offset allows the same signed fill to be resubmitted
        //   for multiple consecutive blocks (useful for multi-block bundle submission, not yet
        //   implemented).
        let filler = Filler::new(
            signer,
            tx_cache,
            submitter,
            constants.system().clone(),
            FillerOptions::new(),
        );

        let pricing_client = StaticPricingClient::new(
            U256::from(config.min_profit_threshold_wei()),
            config.gas_estimate_per_order(),
            config.gas_price_gwei(),
        );

        let host = constants.system().host();
        let block_lead_duration = config.block_lead_duration();
        let slot_duration = host.slot_duration();
        let start_timestamp = host.start_timestamp();

        let filled_orders = Mutex::new(LruCache::new(FILLED_ORDERS_CACHE_SIZE));

        Ok(Self {
            filler,
            pricing_client,
            filled_orders,
            block_lead_duration,
            slot_duration,
            start_timestamp,
            cancellation_token,
        })
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
            "starting filler task"
        );

        let slot_duration = Duration::from_secs(self.slot_duration);
        let first_tick = self.submission_anchor_instant();
        let mut interval = tokio::time::interval_at(first_tick, slot_duration);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            select! {
                biased;
                _ = self.cancellation_token.cancelled() => {
                    debug!("filler task cancelled");
                    break;
                }
                _ = interval.tick() => {
                    if let Err(e) = self.process_orders().await {
                        error!(error = %e, "error processing orders");
                    }
                }
            }
        }
    }

    #[instrument(parent = None, skip(self))]
    async fn process_orders(&self) -> Result<()> {
        let mut orders_count = 0usize;

        // Stream: skip orders known to be filled (LRU cache), then filter for profitability.
        let candidates: Vec<SignedOrder> = self
            .filler
            .get_orders()
            .inspect_ok(|_| {
                orders_count += 1;
            })
            .try_filter_map(|order| self.not_in_filled_cache(order))
            .try_filter_map(|order| self.profitable(order))
            .try_collect()
            .await
            .wrap_err("failed to fetch orders")?;

        if candidates.is_empty() {
            if orders_count == 0 {
                info!("no orders fetched from transaction cache");
            } else {
                info!(orders_count, "no profitable, unfilled orders found");
            }
            return Ok(());
        }

        // Check Permit2 nonces concurrently for all profitable candidates.
        let candidates_len = candidates.len();
        let orders_to_fill: Vec<SignedOrder> =
            join_all(candidates.into_iter().map(|order| self.unfilled(order)))
                .await
                .into_iter()
                .flatten()
                .collect();

        if orders_to_fill.is_empty() {
            info!(orders_count, candidates_len, "no unfilled orders after nonce check");
            return Ok(());
        }

        let orders_in_bundle = orders_to_fill.len();
        match self.filler.fill(orders_to_fill).await {
            Ok(response) => {
                info!(
                    bundle_id = %response.id,
                    orders_in_bundle,
                    "successfully submitted fill bundle"
                );
            }
            Err(error) => warn!(%error, orders_in_bundle, "failed to fill orders"),
        }

        Ok(())
    }

    /// Returns an [`Instant`] corresponding to the very first submission anchor:
    /// `start_timestamp - block_lead_duration`. This will typically be far in the past, but that's
    /// intentional — [`tokio::time::interval_at`] with [`MissedTickBehavior::Skip`] fast-forwards
    /// over all elapsed ticks and fires at the next one that falls in the future.
    fn submission_anchor_instant(&self) -> Instant {
        let anchor =
            UNIX_EPOCH + Duration::from_secs(self.start_timestamp) - self.block_lead_duration;
        let now_system = SystemTime::now();
        let now_instant = Instant::now();
        let elapsed =
            now_system.duration_since(anchor).expect("system clock before first submission anchor");
        now_instant - elapsed
    }

    /// Returns `Ok(Some(order))` if the order is not held in our local cache of known filled
    /// orders, or `Ok(None)` if not.
    ///
    /// Never returns `Err`, but this signature suits usage in `try_filter_map`.
    async fn not_in_filled_cache(
        &self,
        order: SignedOrder,
    ) -> Result<Option<SignedOrder>, FillerError> {
        let cached = self.filled_orders.lock().unwrap().contains(order.order_hash());
        if cached {
            trace!(order_hash = %order.order_hash(), "skipping cached filled order");
            Ok(None)
        } else {
            Ok(Some(order))
        }
    }

    /// Returns `Ok(Some(order))` if the order is profitable, otherwise returns `Ok(None)`.
    ///
    /// Never returns `Err`, but this signature suits usage in `try_filter_map`.
    async fn profitable(&self, order: SignedOrder) -> Result<Option<SignedOrder>, FillerError> {
        match self.pricing_client.is_profitable(&order).await {
            Ok(true) => {
                trace!(order_hash = %order.order_hash(), "order is profitable");
                Ok(Some(order))
            }
            Ok(false) => {
                trace!(order_hash = %order.order_hash(), "order is not profitable");
                Ok(None)
            }
            Err(error) => {
                warn!(
                    order_hash = %order.order_hash(),
                    error = %error,
                    "failed to check profitability"
                );
                Ok(None)
            }
        }
    }

    /// Returns `Some(order)` if its Permit2 nonce has not been consumed on the rollup chain,
    /// indicating it is still available to fill. Increments `unfilled_count` for each unfilled
    /// order.
    async fn unfilled(&self, order: SignedOrder) -> Option<SignedOrder> {
        let owner = order.permit().owner;
        let nonce = order.permit().permit.nonce;
        let word_pos = nonce >> 8;
        let bit_pos = nonce & U256::from(0xFF);

        let permit2 = IPermit2::new(PERMIT2, self.filler.submitter().ru_provider());
        let is_filled = match permit2.nonceBitmap(owner, word_pos).call().await {
            Ok(bitmap) => (bitmap >> bit_pos) & U256::from(1) != U256::ZERO,
            Err(error) => {
                warn!(
                    order_hash = %order.order_hash(),
                    %error,
                    "failed to check Permit2 nonce bitmap, assuming not filled"
                );
                return Some(order);
            }
        };

        if is_filled {
            trace!(order_hash = %order.order_hash(), "order already filled");
            self.filled_orders.lock().unwrap().put(*order.order_hash(), ());
            None
        } else {
            trace!(order_hash = %order.order_hash(), "order unfilled");
            Some(order)
        }
    }
}
