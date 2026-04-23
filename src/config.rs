use alloy::rpc::client::BuiltInConnectionString;
use eyre::{Result, WrapErr, bail};
use init4_bin_base::utils::{
    from_env::FromEnv,
    provider::{ProviderConfig, PubSubConfig},
    signer::LocalOrAwsConfig,
};
use itertools::Itertools;
use signet_constants::SignetConstants;
use std::{num::NonZeroUsize, time::Duration};

const CHAIN_NAME_VAR: &str = "SIGNET_FILLER_CHAIN_NAME";
const HOST_RPC_VAR: &str = "SIGNET_FILLER_HOST_RPC_URL";
const RU_RPC_VAR: &str = "SIGNET_FILLER_ROLLUP_RPC_URL";
const MAX_LOSS_PERCENT_VAR: &str = "SIGNET_FILLER_MAX_LOSS_PERCENT";
const TARGET_BLOCKS_VAR: &str = "SIGNET_FILLER_TARGET_BLOCKS";
const MAX_ORDERS_PER_BUNDLE_VAR: &str = "SIGNET_FILLER_MAX_ORDERS_PER_BUNDLE";

const DEFAULT_CHAIN_NAME: &str = "parmigiana";
const DEFAULT_HOST_RPC: &str = "https://host-rpc.parmigiana.signet.sh";
const DEFAULT_RU_RPC: &str = "wss://rpc.parmigiana.signet.sh";
const DEFAULT_BLOCK_LEAD_DURATION: Duration = Duration::from_secs(2);
const DEFAULT_MAX_LOSS_PERCENT: u8 = 10;
const DEFAULT_HEALTHCHECK_PORT: u16 = 8080;
const DEFAULT_TARGET_BLOCKS: u8 = 5;
/// Caps `target_blocks` to avoid wasting resources on redundant inclusion attempts once a bundle
/// has either landed or become clearly stale.
const MAX_TARGET_BLOCKS: u8 = 10;

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
        desc = "URL for Host RPC node. This MUST be a valid HTTP or WS URL, starting with http://, \
            https://, ws:// or wss:// [default: https://host-rpc.parmigiana.signet.sh]",
        optional
    )]
    host_rpc: Option<String>,

    #[from_env(
        var = "SIGNET_FILLER_ROLLUP_RPC_URL",
        desc = "URL for Rollup RPC node. This MUST be a valid WS url starting with ws:// or \
            wss://. Http providers are not supported [default: wss://rpc.parmigiana.signet.sh]",
        optional
    )]
    ru_rpc: Option<String>,

    #[from_env(
        var = "SIGNET_FILLER_BLOCK_LEAD_DURATION_MS",
        desc = "How far before each block boundary to submit fill bundles, in milliseconds \
            [default: 2000]",
        optional
    )]
    block_lead_duration_ms: Option<u64>,

    #[from_env(
        var = "SIGNET_FILLER_MAX_LOSS_PERCENT",
        desc = "Maximum acceptable loss percent for order pricing, 0-100 [default: 10]",
        optional
    )]
    max_loss_percent: Option<u8>,

    #[from_env(
        var = "SIGNET_FILLER_HEALTHCHECK_PORT",
        desc = "Port for the healthcheck HTTP server [default: 8080]",
        optional
    )]
    healthcheck_port: Option<u16>,

    #[from_env(
        var = "SIGNET_FILLER_TARGET_BLOCKS",
        desc = "Number of consecutive blocks to target per fill bundle, 1-10 [default: 5]",
        optional
    )]
    target_blocks: Option<u8>,

    #[from_env(
        var = "SIGNET_FILLER_MAX_ORDERS_PER_BUNDLE",
        desc = "Maximum number of orders to include in a single fill bundle. Must be greater \
            than 0 when set [default: unset, no cap]",
        optional
    )]
    max_orders_per_bundle: Option<usize>,

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
    max_loss_percent: u8,
    healthcheck_port: u16,
    target_blocks: u8,
    max_orders_per_bundle: Option<NonZeroUsize>,
    signer: LocalOrAwsConfig,
    constants: SignetConstants,
}

impl Config {
    /// The Signet chain name (e.g. "parmigiana").
    pub const fn chain_name(&self) -> &str {
        self.chain_name.as_str()
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

    /// Maximum acceptable loss percentage (0-100) for order pricing.
    pub const fn max_loss_percent(&self) -> u8 {
        self.max_loss_percent
    }

    /// Port for the healthcheck HTTP server.
    pub const fn healthcheck_port(&self) -> u16 {
        self.healthcheck_port
    }

    /// Number of consecutive blocks to target per fill bundle.
    pub const fn target_blocks(&self) -> u8 {
        self.target_blocks
    }

    /// Maximum number of orders to include in a single fill bundle, or `None` for no cap.
    pub const fn max_orders_per_bundle(&self) -> Option<NonZeroUsize> {
        self.max_orders_per_bundle
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
            max_loss_percent,
            healthcheck_port,
            target_blocks,
            max_orders_per_bundle,
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
        let max_loss_percent = max_loss_percent.unwrap_or(DEFAULT_MAX_LOSS_PERCENT);
        if max_loss_percent > 100 {
            bail!(
                "{MAX_LOSS_PERCENT_VAR} must be between 0 and 100 inclusive \
                 (got {max_loss_percent})"
            );
        }
        let healthcheck_port = healthcheck_port.unwrap_or(DEFAULT_HEALTHCHECK_PORT);
        let target_blocks = target_blocks.unwrap_or(DEFAULT_TARGET_BLOCKS);
        if !(1..=MAX_TARGET_BLOCKS).contains(&target_blocks) {
            bail!(
                "{TARGET_BLOCKS_VAR} must be between 1 and {MAX_TARGET_BLOCKS} inclusive \
                 (got {target_blocks})"
            );
        }
        if max_orders_per_bundle == Some(0) {
            bail!("{MAX_ORDERS_PER_BUNDLE_VAR} must be greater than 0");
        }
        let max_orders_per_bundle =
            max_orders_per_bundle.map(|v| NonZeroUsize::new(v).expect("already checked non-zero"));

        Ok(Config {
            chain_name,
            host_rpc,
            ru_rpc,
            block_lead_duration,
            max_loss_percent,
            healthcheck_port,
            target_blocks,
            max_orders_per_bundle,
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
