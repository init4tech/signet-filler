use init4_bin_base::deps::tracing::debug;
use signet_filler::{FillerTask, config_from_env, env_var_info};

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

    let _guard = init4_bin_base::init4();
    let config = config_from_env()?;
    debug!(chain = %config.constants().environment().rollup_name(), "starting filler");

    let cancellation_token = signet_filler::handle_signals()?;
    let filler_task = match FillerTask::initialize(&config, cancellation_token.clone()).await {
        Ok(task) => task,
        Err(_) if cancellation_token.is_cancelled() => return Ok(()),
        Err(error) => return Err(error),
    };

    match filler_task.spawn().await {
        Err(error) if error.is_panic() => {
            Err(eyre::Report::new(error).wrap_err("panic running filler task"))
        }
        Err(_) | Ok(_) => Ok(()),
    }
}
