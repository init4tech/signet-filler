#![recursion_limit = "256"]

use init4_bin_base::deps::tracing::debug;
use signet_filler::{
    AllowanceRefreshTask, FillerContext, FillerTask, config_from_env, env_var_info,
    serve_healthcheck,
};
use tokio::join;

fn should_print_help() -> bool {
    std::env::args().any(|arg| {
        let lowercase_arg = arg.to_ascii_lowercase();
        lowercase_arg == "-h" || lowercase_arg == "--help"
    })
}

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    let env_vars = env_var_info();
    println!(
        r#"Signet filler service v{version}

Run with no args. The process will run until it receives a SIGTERM or SIGINT signal.

Configuration is via the following environment variables:
{env_vars}
"#
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> eyre::Result<()> {
    if should_print_help() {
        print_help();
        return Ok(());
    }

    #[expect(deprecated, reason = "ticketed to be fixed in ENG-1990")]
    let _guard = init4_bin_base::init4();
    let config = config_from_env()?;
    debug!(chain = %config.constants().environment().rollup_name(), "starting filler");

    let cancellation_token = signet_filler::handle_signals()?;
    let context = match FillerContext::initialize(config, cancellation_token.clone()).await {
        Ok(context) => context,
        Err(_) if cancellation_token.is_cancelled() => return Ok(()),
        Err(error) => return Err(error),
    };

    let filler_task = FillerTask::new(&context)?;
    let allowance_task = AllowanceRefreshTask::initialize(&context).await;
    let healthcheck_port = context.healthcheck_port();

    let (filler_result, _, server_result) = join!(
        filler_task.run(),
        allowance_task.run(),
        serve_healthcheck(healthcheck_port, cancellation_token),
    );
    filler_result?;
    server_result
}
