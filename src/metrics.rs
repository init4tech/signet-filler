use core::time::Duration;
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use std::sync::LazyLock;

// Metric names
const UPTIME_SECONDS: &str = "signet.filler.uptime_seconds";
const CYCLES: &str = "signet.filler.cycles";
const ORDERS_FETCHED: &str = "signet.filler.orders_fetched";
const ORDERS_SKIPPED: &str = "signet.filler.orders_skipped";
const ORDERS_IN_BUNDLES: &str = "signet.filler.orders_in_bundles";
const BUNDLES: &str = "signet.filler.bundles";
const NONCE_CHECK_ERRORS: &str = "signet.filler.nonce_check_errors";
const PRICING_ERRORS: &str = "signet.filler.pricing_errors";
const FETCH_ORDER_ERRORS: &str = "signet.filler.fetch_order_errors";
const CONNECTION_RETRY_ATTEMPTS: &str = "signet.filler.connection_retry_attempts";
const MISSED_WINDOWS: &str = "signet.filler.missed_windows";
const CYCLE_DURATION_SECONDS: &str = "signet.filler.cycle_duration_seconds";
const ORDERS_PER_BUNDLE: &str = "signet.filler.orders_per_bundle";

/// Force evaluation to register all metric descriptions with the exporter.
pub(crate) static DESCRIPTIONS: LazyLock<()> = LazyLock::new(|| {
    describe_gauge!(UPTIME_SECONDS, "Seconds since signet-filler started");
    describe_counter!(CYCLES, "Processing cycles completed");
    describe_counter!(ORDERS_FETCHED, "Orders fetched from tx cache");
    describe_counter!(
        ORDERS_SKIPPED,
        "Orders skipped (label: reason = already-filled / not-profitable)"
    );
    describe_counter!(ORDERS_IN_BUNDLES, "Orders included in submitted fill bundles");
    describe_counter!(BUNDLES, "Bundle submissions (label: result = success / failure)");
    describe_counter!(NONCE_CHECK_ERRORS, "Errors querying Permit2 nonce bitmap");
    describe_counter!(PRICING_ERRORS, "Errors during profitability evaluation");
    describe_counter!(FETCH_ORDER_ERRORS, "Errors fetching orders from tx cache");
    describe_counter!(
        CONNECTION_RETRY_ATTEMPTS,
        "Connection retry attempts during initialization (label: target = host-provider / \
        rollup-provider / transaction-cache)"
    );
    describe_counter!(
        MISSED_WINDOWS,
        "Processing cycles skipped because the processing window was missed"
    );
    describe_histogram!(CYCLE_DURATION_SECONDS, "Duration of each processing cycle");
    describe_histogram!(ORDERS_PER_BUNDLE, "Orders in submitted bundles");
});

pub(crate) enum OrderSkippedReason {
    AlreadyFilled,
    NotProfitable,
}

impl OrderSkippedReason {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            OrderSkippedReason::AlreadyFilled => "already-filled",
            OrderSkippedReason::NotProfitable => "not-profitable",
        }
    }
}

pub(crate) enum SubmissionResult {
    Success,
    Failure,
}

impl SubmissionResult {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            SubmissionResult::Success => "success",
            SubmissionResult::Failure => "failure",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ConnectionTarget {
    HostProvider,
    RollupProvider,
    TxCache,
}

impl ConnectionTarget {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            ConnectionTarget::HostProvider => "host-provider",
            ConnectionTarget::RollupProvider => "rollup-provider",
            ConnectionTarget::TxCache => "transaction-cache",
        }
    }
}

/// Record uptime gauge.
pub(crate) fn record_uptime(elapsed: Duration) {
    gauge!(UPTIME_SECONDS).set(elapsed.as_secs_f64());
}

/// Increment the cycle counter.
pub(crate) fn record_cycle() {
    counter!(CYCLES).increment(1);
}

/// Record how many orders were fetched from the tx cache.
pub(crate) fn record_orders_fetched(count: u64) {
    counter!(ORDERS_FETCHED).increment(count);
}

/// Record an order skipped for the given reason.
pub(crate) fn record_order_skipped(reason: OrderSkippedReason) {
    counter!(ORDERS_SKIPPED, "reason" => reason.as_str()).increment(1);
}

/// Record orders included in a submitted bundle.
pub(crate) fn record_orders_in_bundle(count: u64) {
    counter!(ORDERS_IN_BUNDLES).increment(count);
}

/// Record a bundle submission result.
pub(crate) fn record_bundle(result: SubmissionResult) {
    counter!(BUNDLES, "result" => result.as_str()).increment(1);
}

/// Record a Permit2 nonce check error.
pub(crate) fn record_nonce_check_error() {
    counter!(NONCE_CHECK_ERRORS).increment(1);
}

/// Record a pricing evaluation error.
pub(crate) fn record_pricing_error() {
    counter!(PRICING_ERRORS).increment(1);
}

/// Record an error fetching orders from the tx cache.
pub(crate) fn record_fetch_order_error() {
    counter!(FETCH_ORDER_ERRORS).increment(1);
}

/// Record a connection retry attempt for the given target.
pub(crate) fn record_connection_attempt(target: ConnectionTarget) {
    counter!(CONNECTION_RETRY_ATTEMPTS, "target" => target.as_str()).increment(1);
}

/// Record a missed processing window.
pub fn record_missed_window() {
    counter!(MISSED_WINDOWS).increment(1);
}

/// Record the duration of a processing cycle.
pub(crate) fn record_cycle_duration(elapsed: Duration) {
    histogram!(CYCLE_DURATION_SECONDS).record(elapsed.as_secs_f64());
}

/// Record the number of orders in a submitted bundle.
pub(crate) fn record_orders_per_bundle(count: f64) {
    histogram!(ORDERS_PER_BUNDLE).record(count);
}
