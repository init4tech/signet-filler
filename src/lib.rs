use eyre::{Result, WrapErr};
use init4_bin_base::deps::tracing::{debug, info};
use tokio::{
    select,
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;

mod config;
pub use config::{Config, config_from_env, env_var_info};

mod filler_task;
pub use filler_task::FillerTask;

mod metrics;

pub mod pricing;

mod service;
pub use service::serve_healthcheck;

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
