//! The hub's configuration: one TOML file plus environment overrides.
//!
//! A hub is configured by one file (HUB-IMPLEMENTATION.md §1.12): the listen
//! address, the database path, and — as later items fill them — the connected
//! stores, the OIDC trust anchor, per-hub `hub:` overrides, and the read-auth
//! policy. This item ships the sections that exist and leaves the rest as typed
//! stubs, so a later item extends the struct rather than reshaping it, and an
//! operator's file keeps working across those additions.
//!
//! Config is an input to `derive()` (HUB-IMPLEMENTATION.md §1.5): its hash keys the
//! deriver's memo, so a config change invalidates derived answers like any other
//! input change. The deriver and its memo are hub-06's; this struct is the shape
//! they and the shell read.
//!
//! Loading is deliberately loud (invariant #6, CLAUDE.md's error-message rule): a
//! missing file, unreadable bytes, or a malformed field fails with a message that
//! names the file and the offending field, never a silent default that would let a
//! typo'd address or path pass unnoticed.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The hub's parsed configuration.
///
/// `#[serde(deny_unknown_fields)]` so a stray or misspelled top-level key is a loud
/// parse error naming it, not a silently ignored line — a config a hub half-honors
/// is exactly the quiet drift this product exists to kill. Later items add fields
/// here (connected stores, OIDC trust, `hub:` overrides, read-auth policy); adding a
/// field with a `#[serde(default)]` keeps an existing operator's file valid.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the hub binds its HTTP listener. Defaults to `127.0.0.1:8080` — a
    /// loopback bind, so an unconfigured hub is not exposed beyond the host until an
    /// operator opts into a wider address.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,

    /// The SQLite database file the ledger and registry live in (`claim-hub-store`
    /// creates and migrates it on first open). Defaults to `hub.db` in the process's
    /// working directory. This is the one file the customer owns: export is `cp`,
    /// delete is `rm` (HUB.md §1).
    #[serde(default = "default_database_path")]
    pub database: PathBuf,

    /// Connected git stores the registry mirrors and syncs. Empty in this shell;
    /// registry sync (hub-05) reads it. Present so an operator's file can already
    /// carry the section a later item consumes.
    #[serde(default)]
    pub stores: Vec<StoreConfig>,

    /// OIDC trust for the ingest gate (hub-04): the allowed repositories and the
    /// audience the hub verifies a producer's token against. `None` until ingest
    /// lands; the shell has no ingest route to guard.
    #[serde(default)]
    pub oidc: Option<OidcConfig>,

    /// Per-hub overrides of a claim's `hub:` hints (hub-06). Empty in this shell; the
    /// deriver reads it to override a store's declared cadence for this environment.
    #[serde(default)]
    pub hub_overrides: HubOverrides,

    /// The read-auth policy for the API and MCP surfaces (hub-13). Defaults to the
    /// secure default (authed-everything); the shell serves only `/status`, which is
    /// unauthenticated health, so nothing enforces this yet.
    #[serde(default)]
    pub read_auth: ReadAuthConfig,
}

/// One connected git store, as the config names it. A stub for hub-05: the fields
/// registry sync needs (mirror url, branch) are added there. `url` is the minimum a
/// present entry must carry, so an operator's `[[stores]]` block is not empty.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    /// The store's git remote (e.g. `https://github.com/acme/payments`). Registry
    /// sync (hub-05) mirrors and fetches this.
    pub url: String,
}

/// The OIDC trust anchor for the ingest gate. A stub for hub-04; the gate verifies a
/// producer's token's `aud` against `audience` and its `repository` against
/// `repositories`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// The audience the hub identifies itself as — the value a producer's token's
    /// `aud` claim must equal for the hub to accept it (HUB-IMPLEMENTATION.md §1.7).
    pub audience: String,
    /// The connected repositories a producer's token's `repository` claim must be one
    /// of. Empty means no repository is trusted yet.
    #[serde(default)]
    pub repositories: Vec<String>,
}

/// Per-hub overrides of claims' `hub:` cadence hints (hub-06). A newtype over the map
/// so it is one config field the deriver reads, not a bare collection; empty by
/// default, meaning the store's declared cadence stands.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct HubOverrides(pub std::collections::BTreeMap<String, String>);

/// The read-auth policy for the hub's read surfaces (hub-13).
///
/// The v1 default is secure-by-default: authed-everything, with the scoped-token
/// floor for IdP-less self-hosters (HUB-IMPLEMENTATION.md §4.5, decision 5). Open
/// reads are an explicit opt-in for a trusted private network. The shell serves only
/// unauthenticated `/status`, so this is carried but not yet enforced. The derived
/// `Default` gives `open_reads = false` — authed-everything — which is exactly the
/// secure default, so the safe policy is the one an absent `[read_auth]` section gets.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAuthConfig {
    /// When `true`, read surfaces are open (no bearer required) — the private-network
    /// opt-in. Defaults to `false`: authed-everything.
    #[serde(default)]
    pub open_reads: bool,
}

/// The default listen address: loopback on port 8080, so an unconfigured hub is not
/// reachable off the host.
fn default_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8080))
}

/// The default database file: `hub.db` beside the process's working directory.
fn default_database_path() -> PathBuf {
    PathBuf::from("hub.db")
}

impl Config {
    /// Parse a config from a TOML string, mapping a syntax or field error to a
    /// message that names the offending field.
    ///
    /// `toml`'s own error already reports the field name, the source line, and a caret
    /// under the bad value (`invalid socket address syntax` beneath the offending
    /// `listen = "..."`), so a malformed `listen` or `database` is actionable without
    /// the caller re-deriving what went wrong. This is the file-independent half of
    /// loading; [`Config::load`] adds the filename.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Read and parse the config at `path`, naming the file in every failure.
    ///
    /// Both failure modes name the file (CLAUDE.md's error-message rule): an
    /// unreadable or missing file reports the path and the OS reason, and a malformed
    /// file reports the path *and* points at the offending line — `toml`'s error
    /// carries the field name, the source line, and a caret under the bad value. A
    /// bad `listen` reads, over several lines:
    ///
    /// ```text
    /// config `/etc/hub.toml`: TOML parse error at line 1, column 10
    ///   |
    /// 1 | listen = "not-an-address"
    ///   |          ^^^^^^^^^^^^^^^^
    /// invalid socket address syntax
    /// ```
    ///
    /// — never a bare "invalid config".
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|source| anyhow::anyhow!("config `{}`: {source}", path.display()))?;
        Self::from_toml(&text)
            .map_err(|source| anyhow::anyhow!("config `{}`: {source}", path.display()))
    }

    /// Read the config at `path` and apply environment-variable overrides on top.
    ///
    /// The file is the base; a small set of `CLAIM_HUB_*` environment variables
    /// override individual fields after parsing (HUB-IMPLEMENTATION.md §1.12: "one
    /// TOML file plus environment overrides"), so a deployment can point at a shared
    /// file and vary the bind address or database path per instance without editing
    /// it. An override that fails to parse is loud, naming the variable and the value
    /// — an operator's typo in an env var degrades the same way a bad file field
    /// does, never silently.
    ///
    /// Recognized overrides:
    /// - `CLAIM_HUB_LISTEN` — the listen [`SocketAddr`], e.g. `0.0.0.0:8080`.
    /// - `CLAIM_HUB_DATABASE` — the database file path.
    pub fn load_with_env(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut config = Self::load(path)?;
        config.apply_env(&EnvVars::from_process())?;
        Ok(config)
    }

    /// Apply the environment overrides in `env`, failing loudly on a malformed value.
    ///
    /// Split from process-environment reading so it is testable without mutating the
    /// process's real environment (which would make tests order-dependent — CLAUDE.md's
    /// determinism rule). Only the fields with a recognized variable are touched; an
    /// unset variable leaves the file's value in place.
    fn apply_env(&mut self, env: &EnvVars) -> anyhow::Result<()> {
        if let Some(listen) = &env.listen {
            self.listen = listen
                .parse()
                .map_err(|source| anyhow::anyhow!("CLAIM_HUB_LISTEN=`{listen}`: {source}"))?;
        }
        if let Some(database) = &env.database {
            self.database = PathBuf::from(database);
        }
        Ok(())
    }
}

/// The recognized configuration environment variables, read once.
///
/// A plain struct rather than direct `std::env::var` calls in [`Config::apply_env`],
/// so the override logic takes its input as a value and stays deterministic under
/// test — a test constructs an [`EnvVars`] directly instead of setting process-wide
/// state that would leak between tests.
#[derive(Debug, Default)]
struct EnvVars {
    listen: Option<String>,
    database: Option<String>,
}

impl EnvVars {
    /// Read the recognized overrides from the process environment. An unset or
    /// non-UTF-8 variable is treated as absent (the file's value stands).
    fn from_process() -> Self {
        Self {
            listen: std::env::var("CLAIM_HUB_LISTEN").ok(),
            database: std::env::var("CLAIM_HUB_DATABASE").ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_uses_defaults() {
        // An empty file is valid: every field has a default, so a hub boots from the
        // smallest possible config against an empty directory.
        let config = Config::from_toml("").unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(config.database, PathBuf::from("hub.db"));
        assert!(config.stores.is_empty());
        assert!(config.oidc.is_none());
        assert!(!config.read_auth.open_reads);
    }

    #[test]
    fn explicit_fields_override_defaults() {
        let config = Config::from_toml(
            r#"
            listen = "0.0.0.0:9000"
            database = "/var/lib/hub/hub.db"
            "#,
        )
        .unwrap();
        assert_eq!(config.listen, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(config.database, PathBuf::from("/var/lib/hub/hub.db"));
    }

    #[test]
    fn a_malformed_field_names_that_field() {
        // A bad `listen` value must be actionable: the message names the field, not a
        // bare "invalid config".
        let err = Config::from_toml(r#"listen = "not-an-address""#).unwrap_err();
        assert!(err.to_string().contains("listen"), "names the field: {err}");
    }

    #[test]
    fn an_unknown_top_level_key_is_rejected_naming_it() {
        // A misspelled or stray key is a loud error, never a silently ignored line.
        let err = Config::from_toml(r#"databse = "hub.db""#).unwrap_err();
        assert!(err.to_string().contains("databse"), "names the key: {err}");
    }

    #[test]
    fn load_names_the_file_when_it_is_missing() {
        let err = Config::load("/no/such/hub/config.toml").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("/no/such/hub/config.toml"),
            "names the file: {message}"
        );
    }

    #[test]
    fn load_names_both_file_and_field_on_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.toml");
        std::fs::write(&path, r#"listen = "not-an-address""#).unwrap();
        let err = Config::load(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("hub.toml"), "names the file: {message}");
        assert!(message.contains("listen"), "names the field: {message}");
    }

    #[test]
    fn an_env_override_replaces_the_files_field() {
        let mut config = Config::from_toml(r#"listen = "127.0.0.1:8080""#).unwrap();
        let env = EnvVars {
            listen: Some("0.0.0.0:9999".to_owned()),
            database: Some("/data/hub.db".to_owned()),
        };
        config.apply_env(&env).unwrap();
        assert_eq!(config.listen, "0.0.0.0:9999".parse().unwrap());
        assert_eq!(config.database, PathBuf::from("/data/hub.db"));
    }

    #[test]
    fn an_unset_env_override_leaves_the_files_field() {
        let mut config = Config::from_toml(r#"listen = "127.0.0.1:8080""#).unwrap();
        config.apply_env(&EnvVars::default()).unwrap();
        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn a_malformed_env_override_names_the_variable() {
        // A typo in the env var degrades the same way a bad file field does: loud,
        // naming what to fix.
        let mut config = Config::from_toml("").unwrap();
        let env = EnvVars {
            listen: Some("not-an-address".to_owned()),
            database: None,
        };
        let err = config.apply_env(&env).unwrap_err();
        assert!(
            err.to_string().contains("CLAIM_HUB_LISTEN"),
            "names the variable: {err}"
        );
    }

    #[test]
    fn later_sections_parse_when_present() {
        // An operator can already write the sections later items consume, and they
        // deserialize into the typed stubs rather than erroring as unknown.
        let config = Config::from_toml(
            r#"
            [[stores]]
            url = "https://github.com/acme/payments"

            [oidc]
            audience = "https://hub.acme.example"
            repositories = ["acme/payments"]

            [read_auth]
            open_reads = true

            [hub_overrides]
            "payments/libfoo-pin" = "max-age: 30d"
            "#,
        )
        .unwrap();
        assert_eq!(config.stores.len(), 1);
        assert_eq!(config.stores[0].url, "https://github.com/acme/payments");
        let oidc = config.oidc.expect("oidc section present");
        assert_eq!(oidc.audience, "https://hub.acme.example");
        assert_eq!(oidc.repositories, vec!["acme/payments".to_owned()]);
        assert!(config.read_auth.open_reads);
        assert_eq!(
            config
                .hub_overrides
                .0
                .get("payments/libfoo-pin")
                .map(String::as_str),
            Some("max-age: 30d")
        );
    }
}
