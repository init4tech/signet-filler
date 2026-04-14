use crate::{
    AllowanceCache, FillProviderType, FillerContext, FixedPricingClient, FixedPricingError, metrics,
};
use alloy::{primitives::B256, signers::Signer};
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
    block_lead_duration: Duration,
    slot_duration: u64,
    host_start_timestamp: u64,
    app_start_instant: Instant,
    cancellation_token: CancellationToken,
}

impl FillerTask {
    /// Create a new filler from configuration.
    pub fn new(context: &FillerContext) -> Result<Self> {
        let submitter = FeePolicySubmitter::new(
            context.ru_provider().clone(),
            context.host_provider().clone(),
            context.tx_cache().clone(),
            context.constants().system().clone(),
        );

        // FillerOptions has two optional fields:
        // - `nonce`: Permit2 nonce for fill signatures. When None (default), a fresh nonce is
        //   generated from the current timestamp in microseconds for each fill.
        // - `deadline_offset`: Seconds from now until the Permit2 signature expires. When `None`,
        //   defaults to 12 seconds. A longer offset allows the same signed fill to be resubmitted
        //   for multiple consecutive blocks (useful for multi-block bundle submission, not yet
        //   implemented).
        let filler = Filler::new(
            context.signer().clone(),
            context.tx_cache().clone(),
            submitter,
            context.constants().system().clone(),
            FillerOptions::new(),
        );

        let pricing_client = FixedPricingClient::new(
            context.constants().system(),
            context.chain_name(),
            context.max_loss_percent(),
        );

        Ok(Self {
            filler,
            pricing_client,
            allowance_cache: context.allowance_cache().clone(),
            filled_orders: Mutex::new(LruCache::new(FILLED_ORDERS_CACHE_SIZE)),
            block_lead_duration: context.block_lead_duration(),
            slot_duration: context.constants().system().host().slot_duration(),
            host_start_timestamp: context.constants().system().host().start_timestamp(),
            app_start_instant: context.app_start_instant(),
            cancellation_token: context.cancellation_token().clone(),
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

        self.submit_bundle(orders_to_fill).await;
        Ok(())
    }

    /// Fetches orders from the tx cache, filters out known-filled orders, scores by profitability,
    /// and returns candidates sorted most-profitable-first.
    #[instrument(skip_all)]
    async fn fetch_and_score_orders(&self) -> Result<Vec<(i128, SignedOrder)>> {
        let mut orders_count = 0_u64;

        let orders: Vec<SignedOrder> = self
            .filler
            .get_orders()
            .inspect_ok(|_| {
                orders_count += 1;
            })
            .try_filter_map(|order| self.not_in_filled_cache(order))
            .try_collect()
            .await
            .inspect_err(|_| metrics::record_fetch_order_error())
            .wrap_err("failed to fetch orders")?;

        metrics::record_orders_fetched(orders_count);

        if orders.is_empty() {
            if orders_count == 0 {
                info!("no orders fetched from transaction cache");
            } else {
                info!(orders_count, "all orders already filled (cached)");
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

        scored.sort_by(|a, b| b.0.cmp(&a.0));
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

    /// Submits a fill bundle and records metrics.
    #[instrument(skip_all, fields(orders_in_bundle = orders_to_fill.len()))]
    async fn submit_bundle(&self, orders_to_fill: Vec<SignedOrder>) {
        let orders_in_bundle = orders_to_fill.len();
        match self.filler.fill(orders_to_fill, 1).await {
            Ok(responses) => {
                info!(
                    bundle_ids = ?responses.iter().map(|response| response.id).collect::<Vec<_>>(),
                    orders_in_bundle,
                    "successfully submitted fill bundle"
                );
                metrics::record_bundle(metrics::SubmissionResult::Success);
                metrics::record_orders_in_bundle(orders_in_bundle as u64);
                metrics::record_orders_per_bundle(orders_in_bundle as f64);
            }
            Err(error) => {
                warn!(%error, orders_in_bundle, "failed to fill orders");
                metrics::record_bundle(metrics::SubmissionResult::Failure);
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

    /// Returns `Ok(Some(order))` if the order is not held in our local cache of known filled
    /// orders, or `Ok(None)` if it is.
    ///
    /// Never returns `Err`, but this signature suits usage in `try_filter_map`.
    async fn not_in_filled_cache(
        &self,
        order: SignedOrder,
    ) -> Result<Option<SignedOrder>, FillerError> {
        let cached = self.filled_orders.lock().unwrap().contains(order.order_hash());
        if cached {
            trace!(order_hash = %order.order_hash(), "skipping cached filled order");
            metrics::record_order_skipped(metrics::OrderSkippedReason::AlreadyFilled);
            Ok(None)
        } else {
            Ok(Some(order))
        }
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
