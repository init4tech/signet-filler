use axum::{
    Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use eyre::{Report, Result, WrapErr, bail};
use init4_bin_base::deps::tracing::debug;
use std::net::SocketAddr;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

async fn return_404() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

async fn return_200() -> Response {
    (StatusCode::OK, "ok").into_response()
}

/// Serve a `/healthcheck` endpoint on the given port until cancelled or failure.
///
/// Returns `Ok(())` on graceful cancellation or an error if the server exits
/// unexpectedly.
pub async fn serve_healthcheck(port: u16, cancellation_token: CancellationToken) -> Result<()> {
    let handle = do_serve_healthcheck(port, cancellation_token.clone());
    let result = handle.await;
    if cancellation_token.is_cancelled() {
        return Ok(());
    }
    cancellation_token.cancel();
    match result {
        Ok(Ok(())) => bail!("healthcheck server exited without cancellation"),
        Ok(error) => error,
        Err(error) if error.is_panic() => {
            Err(Report::new(error).wrap_err("panic in healthcheck server"))
        }
        Err(_) => bail!("healthcheck server task cancelled unexpectedly"),
    }
}

fn do_serve_healthcheck(port: u16, cancel_token: CancellationToken) -> JoinHandle<Result<()>> {
    let router = Router::new().route("/healthcheck", get(return_200)).fallback(return_404);
    let socket_address = SocketAddr::from(([0, 0, 0, 0], port));
    tokio::spawn(async move {
        let listener = TcpListener::bind(socket_address)
            .await
            .wrap_err_with(|| format!("failed to bind to healthcheck address on port {port}"))?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                cancel_token.cancelled().await;
                debug!("healthcheck service cancelled");
            })
            .await
            .wrap_err("failed serving healthcheck")
    })
}
