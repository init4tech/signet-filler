use crate::{
    AllowanceCache, ChainTokenPair, Config, FillProviderType,
    metrics::{self, ConnectionTarget},
};
use alloy::{
    network::EthereumWallet,
    providers::{Provider, ProviderBuilder},
    rpc::client::BuiltInConnectionString,
    signers::Signer,
    transports::{RpcError, TransportErrorKind},
};
use backon::{ExponentialBuilder, Retryable};
use core::fmt::{self, Display, Formatter};
use eyre::{Context, Result, bail};
use init4_bin_base::{
    deps::tracing::{debug, info, instrument, warn},
    utils::{
        provider::{ProviderConfig, PubSubConfig},
        signer::{LocalOrAws, LocalOrAwsConfig},
    },
};
use signet_constants::SignetConstants;
use signet_tx_cache::TxCache;
use std::sync::{
    LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use tokio::time::{Duration, Instant};
use tokio::{select, try_join};
use tokio_util::sync::CancellationToken;

/// Shared infrastructure created during initialization, used to construct the filler and
/// allowance refresh tasks.
#[derive(Debug)]
pub struct FillerContext {
    config: Config,
    cancellation_token: CancellationToken,
    app_start_instant: Instant,
    signer: LocalOrAws,
    host_provider: FillProviderType,
    ru_provider: FillProviderType,
    tx_cache: TxCache,
    allowance_cache: AllowanceCache,
}

impl FillerContext {
    /// Connect to providers, signer, and transaction cache, returning the shared context.
    #[instrument(skip_all, name = "initialize_filler_context")]
    pub async fn initialize(config: Config, cancellation_token: CancellationToken) -> Result<Self> {
        let app_start_instant = Instant::now();
        LazyLock::force(&metrics::DESCRIPTIONS);
        ChainTokenPair::init_token_names(config.constants().system());

        if config.max_loss_percent() > 100 {
            bail!(
                "invalid config value for max loss percent ({}), must be in range [0, 100]",
                config.max_loss_percent()
            );
        }

        let signer = connect_signer(config.signer()).await?;
        let wallet = EthereumWallet::from(signer.clone());

        let (host_provider, ru_provider, tx_cache) = select! {
            biased;
            _ = cancellation_token.cancelled() => {
                debug!("cancelling initialization");
                bail!("initialization cancelled");
            }
            result = async {
                try_join!(
                    connect_to_host_provider(config.host_rpc(), wallet.clone()),
                    connect_to_rollup_provider(config.ru_rpc(), wallet),
                    connect_to_tx_cache(config.constants().environment().transaction_cache())
                )
            } => result.wrap_err("initialization failure")?,
        };
        let allowance_cache = AllowanceCache::new();

        Ok(Self {
            config,
            cancellation_token,
            app_start_instant,
            signer,
            host_provider,
            ru_provider,
            tx_cache,
            allowance_cache,
        })
    }

    pub(crate) const fn app_start_instant(&self) -> Instant {
        self.app_start_instant
    }

    pub(crate) const fn constants(&self) -> &SignetConstants {
        self.config.constants()
    }

    pub(crate) const fn chain_name(&self) -> &str {
        self.config.chain_name()
    }

    pub(crate) const fn block_lead_duration(&self) -> Duration {
        self.config.block_lead_duration()
    }

    pub(crate) const fn max_loss_percent(&self) -> u8 {
        self.config.max_loss_percent()
    }

    pub(crate) const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    pub(crate) const fn signer(&self) -> &LocalOrAws {
        &self.signer
    }

    pub(crate) const fn host_provider(&self) -> &FillProviderType {
        &self.host_provider
    }

    pub(crate) const fn ru_provider(&self) -> &FillProviderType {
        &self.ru_provider
    }

    pub(crate) const fn tx_cache(&self) -> &TxCache {
        &self.tx_cache
    }

    pub(crate) const fn allowance_cache(&self) -> &AllowanceCache {
        &self.allowance_cache
    }

    /// The port for the healthcheck HTTP server.
    pub const fn healthcheck_port(&self) -> u16 {
        self.config.healthcheck_port()
    }
}

#[instrument(skip_all)]
async fn connect_signer(config: &LocalOrAwsConfig) -> Result<LocalOrAws> {
    debug!("connecting to signer");
    let signer = config.connect().await?;
    info!(signer_address = %signer.address(), "connected to signer");
    Ok(signer)
}

#[instrument(skip_all, fields(url = %DisplayUrl::from(config)))]
async fn connect_to_host_provider(
    config: &ProviderConfig,
    wallet: EthereumWallet,
) -> Result<FillProviderType> {
    connect_provider_with_retry(ConnectionTarget::HostProvider, || async {
        let provider = config.connect().await?;
        provider.get_chain_id().await?;
        Ok(ProviderBuilder::new().wallet(wallet.clone()).connect_provider(provider))
    })
    .await
}

#[instrument(skip_all, fields(url = %DisplayUrl::from(config)))]
async fn connect_to_rollup_provider(
    config: &PubSubConfig,
    wallet: EthereumWallet,
) -> Result<FillProviderType> {
    connect_provider_with_retry(ConnectionTarget::RollupProvider, || async {
        let provider = config.connect().await?;
        provider.get_chain_id().await?;
        Ok(ProviderBuilder::new().wallet(wallet.clone()).connect_provider(provider))
    })
    .await
}

#[instrument]
async fn connect_to_tx_cache(url: &str) -> Result<TxCache> {
    let tx_cache = TxCache::new_from_string(url)
        .wrap_err_with(|| format!("failed to parse transaction cache url '{url}'"))?;

    let orders_url = tx_cache
        .url()
        .join("orders")
        .wrap_err("failed to construct transaction cache orders URL")?;

    let attempt = AtomicUsize::new(1);

    let check_connection = || async {
        tx_cache.client().head(orders_url.clone()).send().await?.error_for_status()?;
        Ok(())
    };

    debug!("connecting to transaction cache");

    check_connection
        .retry(backoff())
        .when(is_transient_reqwest_error)
        .notify(|error, duration| {
            metrics::record_connection_attempt(ConnectionTarget::TxCache);
            warn!(
                error = %error,
                attempt = attempt.fetch_add(1, Ordering::Relaxed),
                retry_in_ms = duration.as_millis(),
                "transient error connecting to transaction cache"
            );
        })
        .await
        .wrap_err("failed to connect to transaction cache")?;

    info!("connected to transaction cache");
    Ok(tx_cache)
}

const fn backoff() -> ExponentialBuilder {
    ExponentialBuilder::new()
        .with_factor(1.5)
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(Duration::from_secs(10))
        .without_max_times()
}

async fn connect_provider_with_retry<F, Fut, T>(
    target: ConnectionTarget,
    connect_fn: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RpcError<TransportErrorKind>>>,
{
    let name = target.as_str();
    debug!(provider = %name, "connecting");

    let attempt = AtomicUsize::new(1);

    let result = connect_fn
        .retry(backoff())
        .when(is_transient_transport_error)
        .notify(|error, duration| {
            metrics::record_connection_attempt(target);
            warn!(
                error = ?error,
                provider = %name,
                attempt = attempt.fetch_add(1, Ordering::Relaxed),
                retry_in_ms = duration.as_millis(),
                "transient error connecting"
            );
        })
        .await
        .wrap_err_with(|| format!("failed to connect to {name}"))?;

    info!(provider = %name, "connected");
    Ok(result)
}

fn is_transient_transport_error(err: &RpcError<TransportErrorKind>) -> bool {
    match err {
        RpcError::ErrorResp(error) => error.is_retry_err(),
        RpcError::NullResp
        | RpcError::UnsupportedFeature(_)
        | RpcError::LocalUsageError(_)
        | RpcError::SerError(_)
        | RpcError::DeserError { .. } => false,
        RpcError::Transport(error_kind) => error_kind.is_retry_err(),
    }
}

fn is_transient_reqwest_error(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() || err.is_request() {
        return true;
    }
    if let Some(status) = err.status() {
        return status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    }
    false
}

enum DisplayUrl<'a> {
    Url(&'a str),
    Ipc(std::path::Display<'a>),
    Unknown,
}

impl<'a> Display for DisplayUrl<'a> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DisplayUrl::Url(url) => write!(formatter, "{url}"),
            DisplayUrl::Ipc(path) => write!(formatter, "{path}"),
            DisplayUrl::Unknown => write!(formatter, "unknown"),
        }
    }
}

impl<'a> From<&'a BuiltInConnectionString> for DisplayUrl<'a> {
    fn from(conn: &'a BuiltInConnectionString) -> Self {
        match conn {
            BuiltInConnectionString::Http(url) | BuiltInConnectionString::Ws(url, _) => {
                DisplayUrl::Url(url.as_str())
            }
            BuiltInConnectionString::Ipc(path) => DisplayUrl::Ipc(path.display()),
            _ => DisplayUrl::Unknown,
        }
    }
}

impl<'a> From<&'a ProviderConfig> for DisplayUrl<'a> {
    fn from(config: &'a ProviderConfig) -> Self {
        DisplayUrl::from(config.connection_string())
    }
}

impl<'a> From<&'a PubSubConfig> for DisplayUrl<'a> {
    fn from(config: &'a PubSubConfig) -> Self {
        DisplayUrl::from(config.connection_string())
    }
}
