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
pub mod authlayer;
pub mod config;
pub mod http;
pub mod ingest;
pub mod mcp;
pub mod metadata;
pub mod oidc;
pub mod readauth;
pub mod router;
pub mod scope;
pub mod status;
pub mod token;
pub mod ui;

pub use app::{build_app, AppState};
pub use config::Config;
pub use router::{spawn_router_tick, Router, RouterView};
pub use status::Status;

use anyhow::Context;
use authlayer::AuthLayerState;
use claim_hub_store::SqliteStore;
use readauth::{ReadAuthPolicy, ReadTokenVerifier};
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

/// Open the store from `config`, assemble the app, spawn the router tick, and serve until
/// the process stops.
///
/// This is the boot path: it opens (creating if absent) the SQLite database at
/// `config.database` via [`SqliteStore::open`], which runs the embedded migrations,
/// so a hub pointed at an empty directory stands up its own schema on first boot
/// (HUB-IMPLEMENTATION.md §1.13). It maps the `[deriver]` section onto the read API's
/// freshness config, so the read surface ages claims per the operator's window. When the
/// config carries an `[oidc]` section it builds the ingest gate's verifier, so
/// `POST /ingest` is mounted; with no OIDC config the hub serves reads only and exposes no
/// write path.
///
/// It points the router at the mirror directory beside the database (where registry sync
/// keeps its per-store mirrors) so `GET /api/nags` resolves owners from CODEOWNERS locally,
/// and it spawns the **router tick** — the one recurring task (HUB-IMPLEMENTATION.md §1.8)
/// that wakes the router to notice clock-crossing staleness with no new verdict. It then
/// binds `config.listen` and serves. A database that will not open, a malformed `[deriver]`
/// window, an OIDC trust anchor that cannot initialize, or an address that will not bind is
/// a loud boot failure naming the fault — a hub that cannot serve says so rather than
/// exiting silently.
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
    // The mirror directory registry sync writes to sits beside the database, so the router
    // reads CODEOWNERS from the same local mirrors — no forge call at fire time (invariant #3).
    let data_dir = config
        .database
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mirror_root = router::mirror_root_for(&data_dir);
    // Resolve the read-auth policy, which enforces secure-by-default: a hub that is not
    // explicitly opened and has no read authenticator fails here, loudly, rather than
    // serving open reads by accident (§4.5 decision 5). Resolved before the tick spawns so a
    // misconfigured hub fails the boot before doing any work.
    let read_auth = build_read_auth(&config).context("configuring read authentication")?;
    let state = AppState::new(store, verifier)
        .with_deriver_config(deriver_config)
        .with_mirror_root(mirror_root)
        .with_read_auth(read_auth);

    // Spawn the router tick: it re-derives on a cadence and fires a nag once per new
    // transition, so a claim aging into stale by the clock is noticed without a new verdict.
    // Detached; it keeps running until the process stops.
    let _tick = spawn_router_tick(state.router(), config.router_period(), |result| {
        if let Err(error) = result {
            tracing::error!(%error, "a router pass failed; retrying next tick");
        }
    });

    let app = build_app(state);
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

/// Resolve the read-auth policy from config into the layer state the app enforces.
///
/// This is where secure-by-default lives (HUB-IMPLEMENTATION.md §4.5, decision 5): it
/// resolves the `[read_auth]` section into a [`ReadAuthPolicy`], which **refuses to build**
/// when authed-everything is in force with no authenticator — so a hub with a bare or
/// absent `[read_auth]` fails the boot loudly rather than silently serving open reads. The
/// only way to open reads is `open_reads = true`.
///
/// When an `[read_auth.issuer]` is configured it builds the IdP bearer-JWT verifier over an
/// [`oidc::HttpJwksSource`] against the issuer's JWKS. The metadata pointer is the
/// root-relative RFC 9728 well-known path, so the hub need not know its own external
/// hostname (which it typically cannot behind a proxy).
///
/// # Errors
///
/// Returns an error when the JWKS HTTP client cannot be built, when a configured issuer has
/// an empty issuer/audience/JWKS URL (which would hollow the pinning), or when the resolved
/// policy is the insecure no-authenticator default or holds a malformed token — every one a
/// loud boot failure, never a silent open.
fn build_read_auth(config: &Config) -> anyhow::Result<Arc<AuthLayerState>> {
    let read_auth = &config.read_auth;
    let (verifier, resource) = match &read_auth.issuer {
        Some(issuer) => {
            anyhow::ensure!(
                !issuer.issuer.trim().is_empty(),
                "config `[read_auth.issuer].issuer` is empty; set it to the IdP's issuer URL \
                 so read tokens can be pinned to it"
            );
            anyhow::ensure!(
                !issuer.audience.trim().is_empty(),
                "config `[read_auth.issuer].audience` is empty; set it to the hub's identifier \
                 so a read token's `aud` can be pinned (an empty audience would accept any \
                 token's `aud`)"
            );
            anyhow::ensure!(
                !issuer.jwks_url.trim().is_empty(),
                "config `[read_auth.issuer].jwks_url` is empty; set it to the IdP's JWKS \
                 endpoint so the hub can verify a read token's signature"
            );
            let source = oidc::HttpJwksSource::new(&issuer.jwks_url)
                .map_err(|e| anyhow::anyhow!("building the read-auth JWKS HTTP client: {e}"))?;
            let verifier: readauth::SharedReadVerifier = Arc::new(ReadTokenVerifier::new(
                issuer.issuer.clone(),
                issuer.audience.clone(),
                source,
            ));
            (Some(verifier), issuer.audience.clone())
        }
        // No IdP: the resource identifier for the metadata document falls back to the ingest
        // audience if set, else the hub's default identifier — a self-describing `resource`
        // even on the scoped-token floor.
        None => (None, resource_identifier(config)),
    };
    let issuer_url = read_auth.issuer.as_ref().map(|i| i.issuer.clone());
    let policy = ReadAuthPolicy::resolve(read_auth.open_reads, verifier, read_auth.tokens.clone())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let metadata = metadata::ResourceMetadata::new(resource, issuer_url);
    Ok(Arc::new(AuthLayerState::new(
        policy,
        metadata,
        metadata::METADATA_PATH.to_owned(),
    )))
}

/// The RFC 9728 `resource` identifier for a hub with no read IdP — its own audience if the
/// ingest gate declares one, else a generic placeholder.
///
/// The metadata document's `resource` names the protected resource. With an IdP it is the
/// read audience; without one there is no token-`aud` to pin, so this is purely descriptive
/// — the ingest `[oidc].audience` (the hub's identifier) if present, else a literal marker
/// so the field is never an empty string.
fn resource_identifier(config: &Config) -> String {
    config
        .oidc
        .as_ref()
        .map(|o| o.audience.clone())
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "claim-hub".to_owned())
}

/// Boot the hub, resolving its config from `config_path`: load config, wire tracing,
/// and serve.
///
/// `config_path` is `Some(path)` when the operator named a config with `--config`, and
/// `None` when they did not — in which case the default [`DEFAULT_CONFIG_PATH`] applies.
/// The top-level entry the binary's `main` calls. An invalid config fails loudly here,
/// naming the file and the field (via [`Config::load_with_env`]), before anything binds
/// — the "fail loudly, name the problem" requirement of the item. Environment variables
/// (`CLAIM_HUB_LISTEN`, `CLAIM_HUB_DATABASE`) override the file's fields.
///
/// A **missing** file behaves differently by source: an absent *default* `hub.toml`
/// yields an empty config so the env overrides alone can boot a container off an empty
/// volume (HUB-IMPLEMENTATION.md §1.13); an absent *named* `--config` path is a loud
/// error, since a mistyped config name must never silently become defaults.
pub async fn run(config_path: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let default_path;
    let source = match &config_path {
        Some(path) => config::ConfigSource::Explicit(path),
        None => {
            default_path = std::path::PathBuf::from(DEFAULT_CONFIG_PATH);
            config::ConfigSource::Default(&default_path)
        }
    };
    let config = Config::load_with_env(source)?;
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

    #[test]
    fn read_auth_default_config_fails_the_boot_loudly() {
        // Secure-by-default at the boot seam: an empty config is authed-everything with no
        // authenticator, which `build_read_auth` refuses — naming the fix — rather than
        // standing up a hub that silently serves open reads (§4.5 decision 5).
        let config = Config::from_toml("").unwrap();
        let Err(err) = build_read_auth(&config) else {
            panic!("a no-authenticator authed config must fail the boot");
        };
        let message = err.to_string();
        assert!(
            message.contains("open_reads"),
            "the error names the open-reads opt-in as one fix: {message}"
        );
    }

    #[test]
    fn read_auth_open_reads_optin_boots() {
        let config = Config::from_toml("[read_auth]\nopen_reads = true\n").unwrap();
        assert!(
            build_read_auth(&config).is_ok(),
            "the explicit open-reads opt-in resolves"
        );
    }

    #[test]
    fn read_auth_with_a_scoped_token_boots() {
        let config = Config::from_toml(
            "[[read_auth.tokens]]\nscopes = [\"read\"]\n\
             hash = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n",
        )
        .unwrap();
        assert!(
            build_read_auth(&config).is_ok(),
            "a scoped-token floor resolves an authed-everything hub"
        );
    }

    #[test]
    fn read_auth_with_an_issuer_boots_and_serves_metadata_with_the_authorization_server() {
        let config = Config::from_toml(
            "[read_auth.issuer]\nissuer = \"https://idp.example\"\n\
             audience = \"https://hub.example\"\n\
             jwks_url = \"https://idp.example/jwks\"\n",
        )
        .unwrap();
        let auth = build_read_auth(&config).expect("an issuer resolves");
        // The metadata document points a client at the configured IdP.
        assert_eq!(auth.metadata().resource, "https://hub.example");
        assert_eq!(
            auth.metadata().authorization_servers,
            vec!["https://idp.example"]
        );
    }

    #[test]
    fn read_auth_with_an_empty_issuer_url_fails_the_boot() {
        let config = Config::from_toml(
            "[read_auth.issuer]\nissuer = \"\"\naudience = \"https://hub.example\"\n\
             jwks_url = \"https://idp.example/jwks\"\n",
        )
        .unwrap();
        let Err(err) = build_read_auth(&config) else {
            panic!("an empty issuer URL must fail the boot");
        };
        assert!(err.to_string().contains("issuer"), "names the field: {err}");
    }

    #[test]
    fn read_auth_with_a_malformed_token_fails_the_boot_by_name() {
        let config = Config::from_toml(
            "[[read_auth.tokens]]\nname = \"broken\"\nscopes = [\"read\"]\n\
             hash = \"not-a-hash\"\n",
        )
        .unwrap();
        let Err(err) = build_read_auth(&config) else {
            panic!("a malformed token must fail the boot");
        };
        assert!(
            err.to_string().contains("broken"),
            "names the offending token: {err}"
        );
    }
}
