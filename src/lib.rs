//! Signet filler service for Parmigiana testnet

#![warn(
    missing_copy_implementations,
    missing_debug_implementations,
    missing_docs,
    unreachable_pub,
    clippy::missing_const_for_fn,
    rustdoc::all
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![recursion_limit = "256"]

use alloy::{
    network::EthereumWallet,
    providers::{
        RootProvider,
        fillers::{FillProvider, JoinFill, WalletFiller},
        utils::JoinedRecommendedFillers,
    },
};
use eyre::{Result, WrapErr};
use init4_bin_base::deps::tracing::{debug, info};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

mod chain_token_pair;
pub(crate) use chain_token_pair::ChainTokenPair;

mod config;
pub use config::{Config, config_from_env, env_var_info};

mod allowance;
pub(crate) use allowance::AllowanceCache;
pub use allowance::AllowanceRefreshTask;

mod filler_task;
pub use filler_task::FillerTask;

mod metrics;

mod fixed_pricing_client;
use fixed_pricing_client::{FixedPricingClient, FixedPricingError};

mod initialization;
pub use initialization::FillerContext;

mod service;
pub use service::serve_healthcheck;

pub(crate) type FillProviderType =
    FillProvider<JoinFill<JoinedRecommendedFillers, WalletFiller<EthereumWallet>>, RootProvider>;

/// Register signal handlers for graceful shutdown, returning a
/// [`CancellationToken`] that is cancelled on SIGINT or SIGTERM.
pub fn handle_signals() -> Result<CancellationToken> {
    let cancellation_token = CancellationToken::new();

    let mut sigint =
        signal(SignalKind::interrupt()).wrap_err("failed to register SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).wrap_err("failed to register SIGTERM handler")?;

    tokio::spawn({
        let cancel_token = cancellation_token.clone();
        async move {
            select! {
                _ = sigint.recv() => {
                    info!("received SIGINT, shutting down");
                }
                _ = sigterm.recv() => {
                    info!("received SIGTERM, shutting down");
                }
            }
            cancel_token.cancel();
        }
    });

    debug!("ready to handle SIGINT or SIGTERM");
    Ok(cancellation_token)
}
