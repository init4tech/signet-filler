use crate::{
    Config,
    pricing::{PricingClient, StaticPricingClient},
};
use alloy::{
    network::EthereumWallet,
    primitives::U256,
    providers::{
        RootProvider,
        fillers::{FillProvider, JoinFill, WalletFiller},
        utils::JoinedRecommendedFillers,
    },
};
use eyre::{Context, Report, Result, bail};
use futures_util::TryStreamExt;
use init4_bin_base::{
    deps::tracing::{debug, error, info, instrument, trace, warn},
    utils::signer::LocalOrAws,
};
use signet_orders::{FeePolicySubmitter, FillerOptions};
use signet_tx_cache::TxCache;
use signet_types::SignedOrder;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    select,
    time::{Duration, Instant, MissedTickBehavior},
    try_join,
};
use tokio_util::sync::CancellationToken;

mod initialization;

type FillProviderType =
    FillProvider<JoinFill<JoinedRecommendedFillers, WalletFiller<EthereumWallet>>, RootProvider>;
type Filler = signet_orders::Filler<
    LocalOrAws,
    TxCache,
    FeePolicySubmitter<FillProviderType, FillProviderType, TxCache>,
>;

/// Order filler service that submits fill bundles shortly before each block
/// boundary.
pub struct FillerTask {
    filler: Filler,
    pricing_client: StaticPricingClient,
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

        Ok(Self {
            filler,
            pricing_client,
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
        let orders_count = AtomicUsize::new(0);
        let profitable_orders: Vec<SignedOrder> = self
            .filler
            .get_orders()
            .inspect_ok(|_| {
                orders_count.fetch_add(1, Ordering::Relaxed);
            })
            .try_filter_map(|order| async { Ok(self.profitable(order).await) })
            .try_collect()
            .await
            .wrap_err("failed to fetch orders")?;

        if profitable_orders.is_empty() {
            let count = orders_count.load(Ordering::Relaxed);
            if count == 0 {
                info!("no orders fetched from transaction cache");
            } else {
                info!(
                    orders_count = %count,
                    "fetched orders from transaction cache, but no profitable orders found"
                );
            }
            return Ok(());
        }

        let orders_in_bundle = profitable_orders.len();
        match self.filler.fill(profitable_orders).await {
            Ok(response) => {
                info!(
                    bundle_id = %response.id,
                    orders_in_bundle,
                    "successfully submitted fill bundle"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    orders_in_bundle,
                    "failed to fill orders"
                );
            }
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

    /// Returns `Some(order)` if the order is profitable, otherwise returns None.
    async fn profitable(&self, order: SignedOrder) -> Option<SignedOrder> {
        match self.pricing_client.is_profitable(&order).await {
            Ok(true) => {
                trace!(order_hash = %order.order_hash(), "order is profitable");
                Some(order)
            }
            Ok(false) => {
                trace!(order_hash = %order.order_hash(), "order is not profitable");
                None
            }
            Err(error) => {
                warn!(
                    order_hash = %order.order_hash(),
                    error = %error,
                    "failed to check profitability"
                );
                None
            }
        }
    }
}
