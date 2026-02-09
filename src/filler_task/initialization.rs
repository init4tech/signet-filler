use super::FillProviderType;
use crate::metrics::{self, ConnectionTarget};
use alloy::{
    network::EthereumWallet,
    providers::{Provider, ProviderBuilder},
    rpc::client::BuiltInConnectionString,
    signers::Signer,
    transports::{RpcError, TransportErrorKind},
};
use backon::{ExponentialBuilder, Retryable};
use core::fmt::{self, Display, Formatter};
use eyre::{Context, Result};
use init4_bin_base::{
    deps::tracing::{debug, info, instrument, warn},
    utils::{
        provider::{ProviderConfig, PubSubConfig},
        signer::{LocalOrAws, LocalOrAwsConfig},
    },
};
use signet_tx_cache::TxCache;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::Duration;

#[instrument(skip_all)]
pub(super) async fn connect_signer(config: &LocalOrAwsConfig) -> Result<LocalOrAws> {
    debug!("connecting to signer");
    let signer = config.connect().await?;
    info!(signer_address = %signer.address(), "connected to signer");
    Ok(signer)
}

#[instrument(skip_all, fields(url = %DisplayUrl::from(config)))]
pub(super) async fn connect_to_host_provider(
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
pub(super) async fn connect_to_rollup_provider(
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
pub(super) async fn connect_to_tx_cache(url: &str) -> Result<TxCache> {
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

fn backoff() -> ExponentialBuilder {
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
