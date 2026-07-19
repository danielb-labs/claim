//! The `claim-hub` binary: boot the hub, or mint a scoped read token.
//!
//! A thin entry point over the [`claim_hub`] library. Two invocations:
//!
//! - `claim-hub [--config <path>]` — the default: install tracing and hand off to
//!   [`claim_hub::run`], which loads the config (defaulting to `hub.toml` in the working
//!   directory when no flag is given), opens the database, and serves.
//! - `claim-hub mint-token [--scope read] [--name <label>]` — mint a hub-minted scoped read
//!   token for the IdP-less floor. It prints the raw token **once** (the operator hands it
//!   to a client) and the `[[read_auth.tokens]]` config snippet holding only its **hash** to
//!   paste into the config. The hub never stores the raw token; a leaked config yields only
//!   the hash.
//!
//! Every real concern lives in the library so it is testable in-process; this file is
//! argument handling and the boot report. A boot failure — an invalid config, a database
//! that will not open, an address that will not bind, or a read-auth policy that would open
//! reads by accident — is reported to stderr and exits non-zero, naming the problem (the
//! item's "fail loudly" requirement). It never degrades to a silent or partial serve.

use std::path::PathBuf;
use std::process::ExitCode;

use claim_hub::scope::Scope;
use claim_hub::token;

/// The multi-threaded tokio runtime axum serves on. `flavor = "multi_thread"` so the
/// hub handles concurrent reads (WAL SQLite serves concurrent readers against one
/// writer — HUB-IMPLEMENTATION.md §1.4).
#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // The `mint-token` subcommand runs before tracing is installed and before any async
    // work: it is a pure, synchronous, one-shot utility that must not emit `info` boot
    // lines around a secret it is printing.
    let mut raw_args = std::env::args().skip(1);
    if raw_args.next().as_deref() == Some("mint-token") {
        return mint_token(std::env::args().skip(2).collect());
    }

    claim_hub::init_tracing();

    let config_arg = match parse_args() {
        Ok(arg) => arg,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("usage: claim-hub [--config <path>]");
            eprintln!("       claim-hub mint-token [--scope read] [--name <label>]");
            return ExitCode::FAILURE;
        }
    };

    match claim_hub::run(config_arg).await {
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

/// Parse the one supported serve flag, `--config <path>`, returning `Some(path)` when the
/// operator named a config and `None` when they did not.
///
/// The presence of the flag is load-bearing downstream, not just the resolved path: a
/// missing file at the operator-named path is a loud error, while a missing file at the
/// default path is the ordinary empty-volume boot ([`claim_hub::run`] applies that rule
/// via [`claim_hub::config::ConfigSource`]). Hand-rolled rather than pulling in an
/// argument parser: the hub binary takes one path and nothing else, so a dependency would
/// be more surface than it earns. An unrecognized argument or a `--config` with no value
/// is a usage error, named.
fn parse_args() -> Result<Option<PathBuf>, String> {
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
    Ok(config_path)
}

/// Mint a hub-minted scoped read token and print the raw secret plus its config snippet.
///
/// The raw token is printed **once** to stdout — the operator copies it to the client and
/// never sees it again (the hub stores only its hash). The `[[read_auth.tokens]]` snippet,
/// carrying the `sha256:` hash and the granted scopes, goes to the config file. Defaults to
/// the `read` scope (the only scope any v1 route requires) with an empty name; `--scope` and
/// `--name` override.
fn mint_token(args: Vec<String>) -> ExitCode {
    let opts = match parse_mint_args(args) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("usage: claim-hub mint-token [--scope read] [--scope act] [--name <label>]");
            return ExitCode::FAILURE;
        }
    };
    let minted = match token::mint() {
        Ok(minted) => minted,
        Err(reason) => {
            eprintln!("error: could not mint a token: {reason}");
            return ExitCode::FAILURE;
        }
    };
    let scopes: Vec<&str> = opts.scopes.iter().map(|s| s.as_str()).collect();
    let scopes_toml = scopes
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // The raw token is emitted exactly here and nowhere else: not logged, not stored.
    println!("Minted a scoped read token. Give the RAW token to the client — it is shown ONCE:");
    println!();
    println!("  {}", minted.raw());
    println!();
    println!("Add this to the hub's config; it stores only the hash, never the token:");
    println!();
    println!("  [[read_auth.tokens]]");
    if !opts.name.is_empty() {
        println!("  name = \"{}\"", opts.name);
    }
    println!("  scopes = [{scopes_toml}]");
    println!("  hash = \"{}\"", minted.config_hash());
    ExitCode::SUCCESS
}

/// The parsed options for `mint-token`: the scopes to grant and an optional label.
struct MintOptions {
    scopes: Vec<Scope>,
    name: String,
}

/// Parse `mint-token`'s flags. `--scope` may repeat; absent, it defaults to `read`. An
/// unrecognized scope word or flag is a named usage error.
fn parse_mint_args(args: Vec<String>) -> Result<MintOptions, String> {
    let mut scopes = Vec::new();
    let mut name = String::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--scope" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "`--scope` needs a value (`read` or `act`)".to_owned())?;
                let scope = Scope::parse(&value)
                    .ok_or_else(|| format!("unknown scope `{value}` (expected `read` or `act`)"))?;
                scopes.push(scope);
            }
            "--name" => {
                name = iter
                    .next()
                    .ok_or_else(|| "`--name` needs a value".to_owned())?;
            }
            other => return Err(format!("unrecognized argument `{other}`")),
        }
    }
    if scopes.is_empty() {
        scopes.push(Scope::Read);
    }
    Ok(MintOptions { scopes, name })
}
