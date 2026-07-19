//! The hub — the axum application shell (`claim-hub`).
//!
//! This crate is the binary that hosts the hub: the axum app the later components
//! mount into, the TOML-plus-environment config, `tracing` wiring, `/status`, and
//! boot (HUB-IMPLEMENTATION.md §4.3). It is the hub's front door, kept a thin shell
//! over `claim-hub-core` (the domain) and `claim-hub-store` (the storage seam), the
//! same layering the CLI keeps over `claim-core`/`claim-store`.
//!
//! What this shell ships:
//!
//! - [`Config`] — one TOML file plus `CLAIM_HUB_*` environment overrides, parsed
//!   with a message that names the file and the offending field on any failure.
//! - [`build_app`] — the axum [`axum::Router`] assembled from tower layers, with
//!   `/status` mounted and the mount points for ingest, the API, MCP, and the UI
//!   named for the later items.
//! - [`Status`] / [`app::AppState`] — the machine-readable position endpoint and the
//!   shared state it reads the store through.
//! - [`serve`] and [`run`] — open (creating if absent) the SQLite database via
//!   `claim-hub-store` and serve, and the top-level boot the binary calls.
//!
//! What is deliberately **not** here: the ingest route (hub-04), registry sync
//! (hub-05), the deriver-backed API (hub-08), MCP (hub-09), the UI (hub-10), and
//! auth (hub-13). The router names each mount point; this item is only the shell.
//!
//! The app is assembled ([`build_app`]) separately from being bound and served
//! ([`serve`]), so a test drives the whole router in-process via
//! [`tower::ServiceExt::oneshot`] with no bound port and no network.

pub mod app;
pub mod config;
pub mod status;

pub use app::{build_app, AppState};
pub use config::Config;
pub use status::Status;

use anyhow::Context;
use claim_hub_store::SqliteStore;

/// Install the tracing subscriber the binary logs through.
///
/// Structured spans, not `println` (CLAUDE.md): a request carries span context
/// through the stack via the router's [`tower_http::trace::TraceLayer`], and this is
/// where those spans are formatted and filtered. Verbosity follows `RUST_LOG` (via
/// the env filter), defaulting to `info` for the hub so a quiet operator still sees
/// boot and error lines. Idempotent-safe for a binary (installed once at boot);
/// calling it twice is an error the caller ignores, since a test that installs its
/// own subscriber must win.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,claim_hub=info"));
    // `try_init` returns Err if a subscriber is already set (e.g. a test's). The
    // binary calls this once at boot; ignoring the Err keeps a double-init from
    // aborting rather than masking a real fault.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Open the store from `config`, assemble the app, and serve until the process stops.
///
/// This is the boot path: it opens (creating if absent) the SQLite database at
/// `config.database` via [`SqliteStore::open`], which runs the embedded migrations,
/// so a hub pointed at an empty directory stands up its own schema on first boot
/// (HUB-IMPLEMENTATION.md §1.13). It then binds `config.listen` and serves the
/// router. A database that will not open, or an address that will not bind, is a
/// loud boot failure naming the path or address — a hub that cannot serve says so
/// rather than exiting silently.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let store = SqliteStore::open(&config.database).await.with_context(|| {
        format!(
            "opening the hub database at `{}`",
            config.database.display()
        )
    })?;
    let app = build_app(AppState { store });
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding the hub listener on `{}`", config.listen))?;
    let bound = listener.local_addr().unwrap_or(config.listen);
    tracing::info!(%bound, database = %config.database.display(), "hub listening");
    axum::serve(listener, app)
        .await
        .context("serving the hub")?;
    Ok(())
}

/// Boot the hub from the config file at `config_path`: load config, wire tracing,
/// and serve.
///
/// The top-level entry the binary's `main` calls. A missing or invalid config fails
/// loudly here, naming the file and the field (via [`Config::load_with_env`]), before
/// anything binds — the "fail loudly, name the problem" requirement of the item.
/// Environment variables (`CLAIM_HUB_LISTEN`, `CLAIM_HUB_DATABASE`) override the
/// file's fields.
pub async fn run(config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load_with_env(config_path)?;
    serve(config).await
}

/// The default config path the binary reads when none is given on the command line.
///
/// `hub.toml` in the working directory: the self-host default, so `claim-hub` in a
/// data directory finds its config with no argument.
pub const DEFAULT_CONFIG_PATH: &str = "hub.toml";
