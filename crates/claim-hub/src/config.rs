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
//! Loading is deliberately loud (invariant #6, CLAUDE.md's error-message rule):
//! unreadable bytes or a malformed field fails with a message that names the file and
//! the offending field, never a silent default that would let a typo'd address or path
//! pass unnoticed. The one deliberate exception is a *missing* file at the **default**
//! path: [`Config::load_with_env`] treats it as an empty config (all defaults), so a
//! container booted from an empty volume with only `CLAIM_HUB_*` env overrides stands
//! up (HUB-IMPLEMENTATION.md §1.13). A missing file at an **explicit** `--config` path
//! stays a loud error — a user who names a config and mistypes it must never get silent
//! defaults.

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

    /// The freshness knobs the deriver reads: a hub-wide default and override for a
    /// claim's `hub.max-age` (HUB-IMPLEMENTATION.md §1.5). Mapped onto the deriver's
    /// [`DeriverConfig`](claim_hub_core::DeriverConfig) via [`Config::deriver_config`].
    #[serde(default)]
    pub deriver: DeriverConfigToml,

    /// The read-auth policy for the API and MCP surfaces (hub-13). Defaults to the
    /// secure default (authed-everything); the shell serves only `/status`, which is
    /// unauthenticated health, so nothing enforces this yet.
    #[serde(default)]
    pub read_auth: ReadAuthConfig,
}

/// Where a config path came from, so loading knows whether a missing file is a fatal
/// typo or the expected empty-volume case.
///
/// The distinction is load-bearing (invariant #6): a missing file at the **default**
/// path is the ordinary first-boot state a container hits against an empty volume, so it
/// falls back to [`Config::default`] and lets `CLAIM_HUB_*` env overrides drive the boot;
/// a missing file at a path the operator **named** with `--config` is a typo that must
/// fail loudly, never silently substitute defaults.
#[derive(Debug, Clone, Copy)]
pub enum ConfigSource<'a> {
    /// The operator named this path with `--config`. A missing file here is a loud error.
    Explicit(&'a Path),
    /// No `--config` was given; this is [`DEFAULT_CONFIG_PATH`](crate::DEFAULT_CONFIG_PATH).
    /// A missing file here falls back to [`Config::default`].
    Default(&'a Path),
}

/// The empty-config defaults: every field's `#[serde(default)]`, so
/// `Config::default()` is exactly the config an empty TOML file parses to.
///
/// Kept in lockstep with the serde defaults by *going through* the parser
/// (`from_toml("")`) rather than duplicating each field's default — a new field with a
/// `#[serde(default)]` is picked up here for free, and a field added without one makes
/// this panic loudly at first use rather than drifting silently from the parsed shape.
impl Default for Config {
    fn default() -> Self {
        Self::from_toml("").expect("the empty config parses via every field's serde default")
    }
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
/// reads are an explicit opt-in for a trusted private network.
///
/// **Secure by default is not enforced by this struct's `Default`** — the derived default
/// (`open_reads = false`, no issuer, no tokens) is *authed-everything with no
/// authenticator*, which cannot serve anyone. That is deliberate: the boot path
/// ([`crate::serve`]) resolves this config into a [`ReadAuthPolicy`](crate::readauth::ReadAuthPolicy),
/// and resolution **fails loudly** on that state rather than silently opening reads. So a
/// hub with a bare `[read_auth]` (or none) does not boot until the operator either
/// configures an authenticator or explicitly opts into open reads — the "open by accident"
/// regression is impossible.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAuthConfig {
    /// When `true`, read surfaces are open (no bearer required) — the private-network
    /// opt-in. Defaults to `false`: authed-everything. This is the *only* way to serve
    /// reads with no authentication.
    #[serde(default)]
    pub open_reads: bool,

    /// The OAuth 2.1 IdP the hub validates read bearer JWTs against. `None` when the hub
    /// runs on the scoped-token floor alone (or open reads).
    #[serde(default)]
    pub issuer: Option<ReadIssuerConfig>,

    /// The hub-minted scoped tokens (the IdP-less floor), each stored as a hash. Empty when
    /// the hub relies on an IdP or serves open reads. A present entry with no scopes or a
    /// malformed hash is a loud boot error (invariant #6), never a silent dead token.
    #[serde(default)]
    pub tokens: Vec<crate::token::ScopedToken>,
}

/// The OAuth 2.1 read IdP trust anchor: the issuer whose JWKS signs read bearer tokens.
///
/// Distinct from the ingest `[oidc]` anchor (which trusts GitHub Actions to attest a
/// *producer*): this is the customer's own IdP acting as the authorization server for
/// *reads*. The hub validates a read token's `iss` against `issuer`, its `aud` against
/// `audience` (the hub's own identifier — the RFC 9728 `resource`), and its signature
/// against the JWKS at `jwks_url`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadIssuerConfig {
    /// The IdP issuer URL a read token's `iss` claim must equal, and the value published as
    /// the RFC 9728 `authorization_servers` entry so a client discovers where to get a token.
    pub issuer: String,
    /// The hub's own identifier a read token's `aud` claim must equal — the RFC 9728
    /// `resource`. This is what stops a token minted for another service from being replayed
    /// against the hub's reads.
    pub audience: String,
    /// The JWKS endpoint the hub fetches the issuer's signing keys from (cached, refreshed
    /// on an unknown key id, rate-limited — the same machinery as the ingest gate).
    pub jwks_url: String,
}

/// The deriver's freshness knobs, as the TOML file spells them.
///
/// The deriver ([`claim_hub_core::derive`]) needs a hub-wide default and override for a
/// claim's `hub.max-age` (HUB-IMPLEMENTATION.md §1.5): `default_max_age` applies to a
/// claim that declares no `max-age`, and `max_age_override` wins over a claim's own
/// hint (the hub operator's word on cadence is final). Both are `<N>d` day-count strings
/// — the same spelling a claim file uses — parsed into [`claim_core::Days`] by
/// [`Config::deriver_config`], so a malformed value is a loud config error naming the
/// field, never a silent fallback. Absent both, no window applies and a passing claim
/// stays fresh forever (the CLI's stance: absent a window, the hub does not invent one).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeriverConfigToml {
    /// The freshness window for a claim declaring no `hub.max-age`, as `<N>d`. `None`
    /// leaves such a claim un-aged by the clock.
    #[serde(default)]
    pub default_max_age: Option<String>,
    /// A hub-wide override applied to every claim regardless of its own `hub.max-age`,
    /// as `<N>d`. `None` lets each claim's own hint (then the default) govern.
    #[serde(default)]
    pub max_age_override: Option<String>,
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

/// Parse an optional `<N>d` day-count config value into [`claim_core::Days`], naming the
/// config field on failure.
///
/// `None` in, `None` out (the field was absent). A present-but-malformed value is a loud
/// error framed with `field` so the operator sees which knob to fix — the deriver never
/// falls back to a silent default when the operator wrote something the parser rejects.
fn parse_days_field(field: &str, value: Option<&str>) -> anyhow::Result<Option<claim_core::Days>> {
    value
        .map(|raw| {
            raw.parse::<claim_core::Days>()
                .map_err(|source| anyhow::anyhow!("config `{field}`: {source}"))
        })
        .transpose()
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

    /// Read the config at `source` and apply environment-variable overrides on top.
    ///
    /// The file is the base; a small set of `CLAIM_HUB_*` environment variables
    /// override individual fields after parsing (HUB-IMPLEMENTATION.md §1.12: "one
    /// TOML file plus environment overrides"), so a deployment can point at a shared
    /// file and vary the bind address or database path per instance without editing
    /// it. An override that fails to parse is loud, naming the variable and the value
    /// — an operator's typo in an env var degrades the same way a bad file field
    /// does, never silently.
    ///
    /// A **missing file at the default path** ([`ConfigSource::Default`]) is not an
    /// error: it yields an empty [`Config::default`] before the env overrides apply, so
    /// a container booted from an empty volume with only `CLAIM_HUB_LISTEN` /
    /// `CLAIM_HUB_DATABASE` set stands up (HUB-IMPLEMENTATION.md §1.13). A missing file
    /// at an **explicit** path ([`ConfigSource::Explicit`], from `--config`) stays a
    /// loud error — a user who names a config and mistypes it must not get silent
    /// defaults. A file that exists but is unreadable or malformed is a loud error in
    /// either case; only `NotFound` on the default path falls back.
    ///
    /// Recognized overrides:
    /// - `CLAIM_HUB_LISTEN` — the listen [`SocketAddr`], e.g. `0.0.0.0:8080`.
    /// - `CLAIM_HUB_DATABASE` — the database file path.
    /// - `CLAIM_HUB_OPEN_READS` — the explicit open-reads opt-in (`true`/`false`), so an
    ///   empty-volume container can serve open reads on a trusted private network with no
    ///   config file; unset leaves the file's value (default authed-everything).
    pub fn load_with_env(source: ConfigSource<'_>) -> anyhow::Result<Self> {
        let mut config = Self::load_source(source)?;
        config.apply_env(&EnvVars::from_process())?;
        Ok(config)
    }

    /// Load the config named by `source`, applying the missing-default-path fallback.
    ///
    /// Split from [`Config::load_with_env`] so the fallback rule is tested without the
    /// process environment. The fallback fires **only** for a `NotFound` at the default
    /// path; any other IO error (a permission fault, a path that is a directory) and any
    /// parse error stays loud, and an explicit missing path stays loud.
    fn load_source(source: ConfigSource<'_>) -> anyhow::Result<Self> {
        match source {
            ConfigSource::Explicit(path) => Self::load(path),
            ConfigSource::Default(path) => match std::fs::read_to_string(path) {
                Ok(text) => Self::from_toml(&text)
                    .map_err(|source| anyhow::anyhow!("config `{}`: {source}", path.display())),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                Err(err) => Err(anyhow::anyhow!("config `{}`: {err}", path.display())),
            },
        }
    }

    /// Map the `[deriver]` section onto the deriver's
    /// [`DeriverConfig`](claim_hub_core::DeriverConfig).
    ///
    /// The two `<N>d` strings are parsed into [`claim_core::Days`] here so a malformed
    /// value fails loudly, naming the field — never a silent fallback that would age
    /// claims on a window nobody set. Config is an input to `derive()` and its hash keys
    /// the deriver's memo, so a change here invalidates cached answers like any other
    /// input change.
    ///
    /// # Errors
    ///
    /// Returns an error naming `[deriver].default_max_age` or `[deriver].max_age_override`
    /// when its value is not a valid `<N>d` day count.
    pub fn deriver_config(&self) -> anyhow::Result<claim_hub_core::DeriverConfig> {
        let default_max_age = parse_days_field(
            "[deriver].default_max_age",
            self.deriver.default_max_age.as_deref(),
        )?;
        let max_age_override = parse_days_field(
            "[deriver].max_age_override",
            self.deriver.max_age_override.as_deref(),
        )?;
        Ok(claim_hub_core::DeriverConfig {
            default_max_age,
            max_age_override,
        })
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
        if let Some(open_reads) = &env.open_reads {
            // The explicit open-reads opt-in as an env var, so an empty-volume container
            // (no config file) can serve open reads on a trusted private network without a
            // TOML file — but still only by *explicitly* setting this, never by default. A
            // value other than a clear boolean is a loud error, not a silent "false": a
            // typo must not quietly leave the hub authed-everything with no authenticator
            // (which then fails the read-auth boot check) OR quietly open it.
            self.read_auth.open_reads = parse_bool_env("CLAIM_HUB_OPEN_READS", open_reads)?;
        }
        Ok(())
    }
}

/// Parse a boolean environment override, accepting only unambiguous spellings.
///
/// `true`/`1` → `true`, `false`/`0` → `false` (case-insensitive). Anything else is a loud
/// error naming the variable, so a typo like `CLAIM_HUB_OPEN_READS=yes` fails the boot
/// rather than being read as `false` (silently authed) or `true` (silently open) — the
/// value is security-load-bearing, so it must be exact.
fn parse_bool_env(var: &str, value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(anyhow::anyhow!(
            "{var}=`{other}`: expected `true` or `false`"
        )),
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
    open_reads: Option<String>,
}

impl EnvVars {
    /// Read the recognized overrides from the process environment. An unset or
    /// non-UTF-8 variable is treated as absent (the file's value stands).
    fn from_process() -> Self {
        Self {
            listen: std::env::var("CLAIM_HUB_LISTEN").ok(),
            database: std::env::var("CLAIM_HUB_DATABASE").ok(),
            open_reads: std::env::var("CLAIM_HUB_OPEN_READS").ok(),
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
        // The explicit-path case: an operator named this file, so a missing one is a
        // loud error naming it — never a silent fallback to defaults.
        let err = Config::load("/no/such/hub/config.toml").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("/no/such/hub/config.toml"),
            "names the file: {message}"
        );
    }

    #[test]
    fn a_missing_default_config_falls_back_to_defaults() {
        // The empty-volume boot: no `--config` given and no `hub.toml` present, so the
        // config is the empty default and the env overrides alone drive the boot.
        let missing = Path::new("/no/such/hub/hub.toml");
        let config = Config::load_source(ConfigSource::Default(missing)).unwrap();
        assert_eq!(config.listen, default_listen());
        assert_eq!(config.database, default_database_path());
    }

    #[test]
    fn a_missing_explicit_config_stays_a_loud_error() {
        // A path the operator named with `--config` that does not exist is a typo, not
        // an empty-volume boot: it must fail loudly, naming the file, never fall back.
        let missing = Path::new("/no/such/hub/named.toml");
        let err = Config::load_source(ConfigSource::Explicit(missing)).unwrap_err();
        assert!(
            err.to_string().contains("named.toml"),
            "names the missing explicit file: {err}"
        );
    }

    #[test]
    fn a_malformed_default_config_stays_a_loud_error() {
        // The fallback is for a *missing* default file only. A default `hub.toml` that
        // exists but is malformed must never silently become defaults — a hub half-
        // honoring a typo'd config is the drift this product kills.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.toml");
        std::fs::write(&path, r#"listen = "not-an-address""#).unwrap();
        let err = Config::load_source(ConfigSource::Default(&path)).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("hub.toml"), "names the file: {message}");
        assert!(message.contains("listen"), "names the field: {message}");
    }

    #[test]
    fn config_default_equals_the_empty_toml_parse() {
        // `Config::default()` must be exactly what an empty file parses to, so the
        // fallback config and the serde defaults never drift apart.
        let from_empty = Config::from_toml("").unwrap();
        let from_default = Config::default();
        assert_eq!(from_default.listen, from_empty.listen);
        assert_eq!(from_default.database, from_empty.database);
        assert!(from_default.stores.is_empty());
        assert!(from_default.oidc.is_none());
        assert!(!from_default.read_auth.open_reads);
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
            ..EnvVars::default()
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
            ..EnvVars::default()
        };
        let err = config.apply_env(&env).unwrap_err();
        assert!(
            err.to_string().contains("CLAIM_HUB_LISTEN"),
            "names the variable: {err}"
        );
    }

    #[test]
    fn the_open_reads_env_override_sets_the_policy() {
        // The explicit env opt-in flips the policy to open, so an empty-volume container
        // can serve open reads with no config file — but only by setting it.
        let mut config = Config::from_toml("").unwrap();
        assert!(!config.read_auth.open_reads);
        let env = EnvVars {
            open_reads: Some("true".to_owned()),
            ..EnvVars::default()
        };
        config.apply_env(&env).unwrap();
        assert!(config.read_auth.open_reads);
    }

    #[test]
    fn a_false_open_reads_env_override_keeps_reads_authed() {
        let mut config = Config::from_toml("[read_auth]\nopen_reads = true\n").unwrap();
        let env = EnvVars {
            open_reads: Some("false".to_owned()),
            ..EnvVars::default()
        };
        config.apply_env(&env).unwrap();
        assert!(!config.read_auth.open_reads);
    }

    #[test]
    fn a_malformed_open_reads_env_override_names_the_variable() {
        // The value is security-load-bearing, so a typo fails loudly rather than being read
        // as `false` (silently authed) or `true` (silently open).
        let mut config = Config::from_toml("").unwrap();
        let env = EnvVars {
            open_reads: Some("yes".to_owned()),
            ..EnvVars::default()
        };
        let err = config.apply_env(&env).unwrap_err();
        assert!(
            err.to_string().contains("CLAIM_HUB_OPEN_READS"),
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

    #[test]
    fn a_full_read_auth_section_parses_the_issuer_and_tokens() {
        // The hub-13 read-auth config: an IdP issuer and one or more hashed scoped tokens.
        let config = Config::from_toml(
            r#"
            [read_auth]
            open_reads = false

            [read_auth.issuer]
            issuer = "https://idp.acme.example"
            audience = "https://hub.acme.example"
            jwks_url = "https://idp.acme.example/.well-known/jwks.json"

            [[read_auth.tokens]]
            name = "ci-dashboard"
            scopes = ["read"]
            hash = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            "#,
        )
        .unwrap();
        assert!(!config.read_auth.open_reads);
        let issuer = config.read_auth.issuer.expect("issuer present");
        assert_eq!(issuer.issuer, "https://idp.acme.example");
        assert_eq!(issuer.audience, "https://hub.acme.example");
        assert_eq!(config.read_auth.tokens.len(), 1);
        assert_eq!(config.read_auth.tokens[0].name, "ci-dashboard");
    }

    #[test]
    fn an_unknown_read_auth_key_is_rejected() {
        // `deny_unknown_fields` on the read-auth sections: a typo'd or stray key is a loud
        // parse error, not a silently ignored line that could weaken auth.
        let err = Config::from_toml("[read_auth]\nopen_read = true\n").unwrap_err();
        assert!(
            err.to_string().contains("open_read"),
            "names the key: {err}"
        );
    }

    #[test]
    fn an_absent_deriver_section_maps_to_no_windows() {
        // No `[deriver]` section: the deriver applies no default and no override — a
        // passing claim with no `hub.max-age` stays fresh forever, the CLI's stance.
        let config = Config::from_toml("").unwrap();
        let deriver = config.deriver_config().unwrap();
        assert_eq!(deriver.default_max_age, None);
        assert_eq!(deriver.max_age_override, None);
    }

    #[test]
    fn a_deriver_section_parses_day_counts() {
        let config = Config::from_toml(
            r#"
            [deriver]
            default_max_age = "30d"
            max_age_override = "7d"
            "#,
        )
        .unwrap();
        let deriver = config.deriver_config().unwrap();
        assert_eq!(deriver.default_max_age, Some("30d".parse().unwrap()));
        assert_eq!(deriver.max_age_override, Some("7d".parse().unwrap()));
    }

    #[test]
    fn a_malformed_deriver_window_names_the_field() {
        // A bad day count is loud, naming which knob to fix — never a silent fallback
        // that would age claims on a window nobody set.
        let config = Config::from_toml("[deriver]\ndefault_max_age = \"soon\"\n").unwrap();
        let err = config.deriver_config().unwrap_err();
        assert!(
            err.to_string().contains("[deriver].default_max_age"),
            "names the field: {err}"
        );
    }
}
