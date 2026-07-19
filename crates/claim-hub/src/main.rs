//! The `claim-hub` binary: boot the hub from a config file and serve.
//!
//! A thin entry point over the [`claim_hub`] library. It reads the config path
//! (`--config <path>`, defaulting to `hub.toml` in the working directory), installs
//! the tracing subscriber, and hands off to [`claim_hub::run`], which loads the
//! config, opens the database, and serves. Every real concern lives in the library
//! so it is testable in-process; this file is argument handling and the boot report.
//!
//! A boot failure — a missing or invalid config, a database that will not open, an
//! address that will not bind — is reported to stderr and exits non-zero, naming the
//! problem (the item's "fail loudly" requirement). It never degrades to a silent or
//! partial serve.

use std::path::PathBuf;
use std::process::ExitCode;

use claim_hub::DEFAULT_CONFIG_PATH;

/// The multi-threaded tokio runtime axum serves on. `flavor = "multi_thread"` so the
/// hub handles concurrent reads (WAL SQLite serves concurrent readers against one
/// writer — HUB-IMPLEMENTATION.md §1.4).
#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    claim_hub::init_tracing();

    let config_path = match parse_args() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("usage: claim-hub [--config <path>]");
            return ExitCode::FAILURE;
        }
    };

    match claim_hub::run(&config_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The full cause chain, so a lower layer's specific detail (the field a
            // config parse faulted on, the OS reason a bind failed) is not swallowed
            // by the outer context.
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the one supported flag, `--config <path>`, returning the config path (the
/// default when the flag is absent).
///
/// Hand-rolled rather than pulling in an argument parser: the hub binary takes one
/// path and nothing else, so a dependency would be more surface than it earns. An
/// unrecognized argument or a `--config` with no value is a usage error, named.
fn parse_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = args
                    .next()
                    .ok_or_else(|| "`--config` needs a path argument".to_owned())?;
                config_path = Some(PathBuf::from(value));
            }
            other => {
                return Err(format!("unrecognized argument `{other}`"));
            }
        }
    }
    Ok(config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)))
}
