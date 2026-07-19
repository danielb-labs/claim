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

pub mod api;
pub mod app;
pub mod config;
pub mod ingest;
pub mod oidc;
pub mod status;

pub use app::{build_app, AppState};
pub use config::Config;
pub use status::Status;

use anyhow::Context;
use claim_hub_store::SqliteStore;
use std::sync::Arc;

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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
/// (HUB-IMPLEMENTATION.md §1.13). It maps the `[deriver]` section onto the read API's
/// freshness config, so the read surface ages claims per the operator's window. When the
/// config carries an `[oidc]` section it builds the ingest gate's verifier, so
/// `POST /ingest` is mounted; with no OIDC config the hub serves reads only and exposes no
/// write path. It then binds `config.listen` and serves the router. A database that will
/// not open, a malformed `[deriver]` window, an OIDC trust anchor that cannot initialize,
/// or an address that will not bind is a loud boot failure naming the fault — a hub that
/// cannot serve says so rather than exiting silently.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let store = SqliteStore::open(&config.database).await.with_context(|| {
        format!(
            "opening the hub database at `{}`",
            config.database.display()
        )
    })?;
    let deriver_config = config
        .deriver_config()
        .context("configuring the read API's freshness windows")?;
    let verifier = build_verifier(&config).context("configuring the ingest gate (OIDC)")?;
    if verifier.is_none() {
        tracing::warn!(
            "no [oidc] config: the ingest gate is not mounted; the hub serves reads only"
        );
    }
    let app = build_app(AppState::new(store, verifier).with_deriver_config(deriver_config));
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

/// Build the ingest gate's OIDC verifier from config, or `None` when no `[oidc]` section
/// is present.
///
/// The verifier trusts the GitHub Actions issuer, the configured audience, and the
/// connected-store repositories, resolving keys through an [`oidc::HttpJwksSource`] over
/// the live GitHub Actions JWKS endpoint. Building the HTTP client can fail (a TLS-backend
/// fault); that is a loud boot error, not a silent read-only degrade. A hub with no
/// `[oidc]` section returns `None` — no ingest route is mounted.
///
/// # Errors
///
/// Returns an error if the JWKS HTTP source cannot be constructed, or if the configured
/// `audience` (or the issuer) is empty — an empty audience would make audience pinning
/// vacuous, so it is refused loudly at boot rather than silently accepting any token's
/// `aud`.
fn build_verifier(config: &Config) -> anyhow::Result<Option<oidc::SharedVerifier>> {
    let Some(oidc_config) = &config.oidc else {
        return Ok(None);
    };
    // An empty audience or issuer would hollow out the pinning `verify` relies on, so
    // refuse it at boot. The issuer is a compile-time constant (checked as belt-and-
    // suspenders against a future edit), the audience is operator-supplied config.
    anyhow::ensure!(
        !oidc::GITHUB_ACTIONS_ISSUER.is_empty(),
        "the OIDC issuer is empty; the ingest gate cannot pin the issuer"
    );
    anyhow::ensure!(
        !oidc_config.audience.trim().is_empty(),
        "config `[oidc].audience` is empty; set it to the hub's identifier so the ingest \
         gate can pin a token's audience (an empty audience would accept any token's `aud`)"
    );
    let source = oidc::HttpJwksSource::new(oidc::GITHUB_ACTIONS_JWKS_URL)
        .map_err(|e| anyhow::anyhow!("building the JWKS HTTP client: {e}"))?;
    // The connected-store repositories a token's `repository` claim must be one of. The
    // config names stores as `github.com/owner/repo`; the token names `owner/repo`, so a
    // config store id has its `github.com/` prefix stripped to match. A store id without
    // that prefix is passed through unchanged (a non-GitHub forge, a later addition).
    let repositories = oidc_config
        .repositories
        .iter()
        .map(|store| {
            store
                .strip_prefix("github.com/")
                .unwrap_or(store)
                .to_owned()
        })
        .collect::<Vec<_>>();
    let verifier = oidc::OidcVerifier::new(
        oidc::GITHUB_ACTIONS_ISSUER,
        &oidc_config.audience,
        repositories,
        source,
    );
    Ok(Some(Arc::new(verifier)))
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

#[cfg(test)]
mod tests {
    use super::*;

    // `SharedVerifier` (an `Arc<dyn TokenVerifier>`) is not `Debug`, so these assert on
    // the outcome via `match`/`is_ok` rather than `unwrap`/`unwrap_err`, which would need
    // the `Ok` type to be `Debug`.

    #[test]
    fn no_oidc_section_builds_no_verifier() {
        // A hub with no `[oidc]` config has no ingest gate, so no verifier — the route
        // is unmounted, reads still serve.
        let config = Config::from_toml("").unwrap();
        match build_verifier(&config) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("no [oidc] should yield no verifier"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn a_valid_oidc_section_builds_a_verifier() {
        let config = Config::from_toml(
            "[oidc]\naudience = \"https://hub.acme.example\"\nrepositories = [\"acme/payments\"]\n",
        )
        .unwrap();
        match build_verifier(&config) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("a valid [oidc] should yield a verifier"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn an_empty_audience_fails_the_boot_loudly() {
        // An empty audience would make audience pinning vacuous (any token's `aud` would
        // pass), so boot refuses it naming the field, rather than standing up a gate that
        // accepts anything.
        let config = Config::from_toml("[oidc]\naudience = \"\"\n").unwrap();
        let Err(err) = build_verifier(&config) else {
            panic!("an empty audience must fail the boot");
        };
        assert!(
            err.to_string().contains("audience"),
            "the boot error names the empty audience: {err}"
        );
    }

    #[test]
    fn a_whitespace_only_audience_is_also_refused() {
        let config = Config::from_toml("[oidc]\naudience = \"   \"\n").unwrap();
        assert!(build_verifier(&config).is_err());
    }
}
