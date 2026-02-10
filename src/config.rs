use alloy::rpc::client::BuiltInConnectionString;
use eyre::{Result, WrapErr};
use init4_bin_base::utils::{
    from_env::FromEnv,
    provider::{ProviderConfig, PubSubConfig},
    signer::LocalOrAwsConfig,
};
use itertools::Itertools;
use signet_constants::SignetConstants;
use std::time::Duration;

const CHAIN_NAME_VAR: &str = "SIGNET_FILLER_CHAIN_NAME";
const HOST_RPC_VAR: &str = "SIGNET_FILLER_HOST_RPC_URL";
const RU_RPC_VAR: &str = "SIGNET_FILLER_ROLLUP_RPC_URL";

const DEFAULT_CHAIN_NAME: &str = "parmigiana";
const DEFAULT_HOST_RPC: &str = "https://host-rpc.parmigiana.signet.sh";
const DEFAULT_RU_RPC: &str = "wss://rpc.parmigiana.signet.sh";
const DEFAULT_BLOCK_LEAD_DURATION: Duration = Duration::from_secs(2);
const DEFAULT_MIN_PROFIT_THRESHOLD: u64 = 100;
const DEFAULT_GAS_ESTIMATE_PER_ORDER: u64 = 150_000;
const DEFAULT_GAS_PRICE_GWEI: u64 = 1;
const DEFAULT_HEALTHCHECK_PORT: u16 = 8080;

/// Internal configuration loaded directly from environment variables.
#[derive(Debug, FromEnv)]
struct ConfigInner {
    #[from_env(
        var = "SIGNET_FILLER_CHAIN_NAME",
        desc = "Signet chain name [default: parmigiana]",
        optional
    )]
    chain_name: Option<String>,

    #[from_env(
        var = "SIGNET_FILLER_HOST_RPC_URL",
        desc =
            "URL for Host RPC node. This MUST be a valid HTTP or WS URL, starting with http://, \
            https://, ws:// or wss:// [default: https://host-rpc.parmigiana.signet.sh]"
        optional
    )]
    host_rpc: Option<String>,

    #[from_env(
        var = "SIGNET_FILLER_ROLLUP_RPC_URL",
        desc =
            "URL for Rollup RPC node. This MUST be a valid WS url starting with ws:// or wss://. \
            Http providers are not supported [default: wss://rpc.parmigiana.signet.sh]"
        optional
    )]
    ru_rpc: Option<String>,

    #[from_env(
        var = "SIGNET_FILLER_BLOCK_LEAD_DURATION_MS",
        desc = "How far before each block boundary to submit fill bundles, in milliseconds [default: 2000]",
        optional
    )]
    block_lead_duration_ms: Option<u64>,

    #[from_env(
        var = "SIGNET_FILLER_MIN_PROFIT_THRESHOLD_WEI",
        desc = "Minimum profit threshold in wei [default: 100]",
        optional
    )]
    min_profit_threshold_wei: Option<u64>,

    #[from_env(
        var = "SIGNET_FILLER_GAS_ESTIMATE_PER_ORDER",
        desc = "Estimated gas per order fill [default: 150000]",
        optional
    )]
    gas_estimate_per_order: Option<u64>,

    #[from_env(
        var = "SIGNET_FILLER_GAS_PRICE_GWEI",
        desc = "Assumed gas price in gwei for cost estimation [default: 1]",
        optional
    )]
    gas_price_gwei: Option<u64>,

    #[from_env(
        var = "SIGNET_FILLER_HEALTHCHECK_PORT",
        desc = "Port for the healthcheck HTTP server [default: 8080]",
        optional
    )]
    healthcheck_port: Option<u16>,

    signer: LocalOrAwsConfig,
}

/// Configuration for the Signet Filler service.
///
/// Load from environment variables using [`config_from_env`]. Use `--help` to see the full list of
/// supported environment variables.
#[derive(Debug)]
pub struct Config {
    chain_name: String,
    host_rpc: ProviderConfig,
    ru_rpc: PubSubConfig,
    block_lead_duration: Duration,
    min_profit_threshold_wei: u64,
    gas_estimate_per_order: u64,
    gas_price_gwei: u64,
    healthcheck_port: u16,
    signer: LocalOrAwsConfig,
    constants: SignetConstants,
}

impl Config {
    /// The Signet chain name (e.g. "parmigiana").
    pub fn chain_name(&self) -> &str {
        &self.chain_name
    }

    /// URL for Host RPC node.
    pub const fn host_rpc(&self) -> &ProviderConfig {
        &self.host_rpc
    }

    /// URL for the Rollup RPC node.
    pub const fn ru_rpc(&self) -> &PubSubConfig {
        &self.ru_rpc
    }

    /// How far before each block boundary to submit fill bundles.
    pub const fn block_lead_duration(&self) -> Duration {
        self.block_lead_duration
    }

    /// Minimum profit threshold in wei for filling orders.
    pub const fn min_profit_threshold_wei(&self) -> u64 {
        self.min_profit_threshold_wei
    }

    /// Estimated gas per order fill for cost calculations.
    pub const fn gas_estimate_per_order(&self) -> u64 {
        self.gas_estimate_per_order
    }

    /// Assumed gas price in gwei for cost estimation.
    pub const fn gas_price_gwei(&self) -> u64 {
        self.gas_price_gwei
    }

    /// Port for the healthcheck HTTP server.
    pub const fn healthcheck_port(&self) -> u16 {
        self.healthcheck_port
    }

    /// Signer configuration for transaction signing.
    pub const fn signer(&self) -> &LocalOrAwsConfig {
        &self.signer
    }

    /// Chain-specific constants derived from the chain name.
    pub const fn constants(&self) -> &SignetConstants {
        &self.constants
    }

    fn from_env() -> Result<Self> {
        let ConfigInner {
            chain_name,
            host_rpc,
            ru_rpc,
            block_lead_duration_ms,
            min_profit_threshold_wei,
            gas_estimate_per_order,
            gas_price_gwei,
            healthcheck_port,
            signer,
        } = ConfigInner::from_env()?;
        let chain_name = chain_name.unwrap_or(DEFAULT_CHAIN_NAME.to_string());
        let constants =
            chain_name.parse().wrap_err_with(|| format!("invalid value for {CHAIN_NAME_VAR}"))?;

        let host_rpc = ProviderConfig::new(
            host_rpc
                .unwrap_or(DEFAULT_HOST_RPC.to_string())
                .parse()
                .wrap_err_with(|| format!("failed to parse {HOST_RPC_VAR}"))?,
        );
        let ru_rpc = PubSubConfig::try_from(
            ru_rpc
                .unwrap_or(DEFAULT_RU_RPC.to_string())
                .parse::<BuiltInConnectionString>()
                .wrap_err_with(|| format!("failed to parse {RU_RPC_VAR}"))?,
        )
        .wrap_err_with(|| format!("{RU_RPC_VAR} must start with ws:// or wss://"))?;
        let block_lead_duration = block_lead_duration_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_BLOCK_LEAD_DURATION);
        let min_profit_threshold_wei =
            min_profit_threshold_wei.unwrap_or(DEFAULT_MIN_PROFIT_THRESHOLD);
        let gas_estimate_per_order =
            gas_estimate_per_order.unwrap_or(DEFAULT_GAS_ESTIMATE_PER_ORDER);
        let gas_price_gwei = gas_price_gwei.unwrap_or(DEFAULT_GAS_PRICE_GWEI);
        let healthcheck_port = healthcheck_port.unwrap_or(DEFAULT_HEALTHCHECK_PORT);

        Ok(Config {
            chain_name,
            host_rpc,
            ru_rpc,
            block_lead_duration,
            min_profit_threshold_wei,
            gas_estimate_per_order,
            gas_price_gwei,
            healthcheck_port,
            signer,
            constants,
        })
    }
}

/// Get a list of the env vars used to configure the app.
pub fn env_var_info() -> String {
    let inventory = ConfigInner::inventory();
    let max_width = inventory.iter().map(|env_item| env_item.var.len()).max().unwrap_or(0);
    inventory
        .iter()
        .map(|env_item| {
            format!(
                "  {:width$}  {}{}",
                env_item.var,
                env_item.description,
                if env_item.optional { " [optional]" } else { "" },
                width = max_width
            )
        })
        .join("\n")
}

/// Load configuration from environment variables.
pub fn config_from_env() -> Result<Config> {
    Config::from_env()
        .wrap_err("failed to configure filler (run with '--help' to see all required env vars)")
}
