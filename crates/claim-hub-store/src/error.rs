//! Errors raised by the hub's storage layer.
//!
//! These are library errors ([`thiserror`]), not binary errors: the hub binary
//! maps a failure to its own surface (an HTTP status, a log line). Raising a typed
//! enum lets a caller recover *why* a store operation failed — a schema or
//! connection fault versus a serialization fault — without matching on prose.

/// A failure to open the store or to run a ledger or registry operation.
///
/// Every variant is an infrastructure fault the caller reports, not a domain
/// outcome: an idempotent redelivery is a *success* the [`crate::Ledger`] returns,
/// not an error here (HUB.md §2), and the append-only triggers firing is a `Sql`
/// error only if something reached around the trait to attempt an update or delete.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The database could not be opened, migrated, or queried. Wraps sqlx's own
    /// error, which names the failing operation; the store never swallows it.
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),

    /// The embedded migrations could not be applied to the database. Distinct from
    /// a query fault so a first-boot schema failure is unambiguous.
    #[error("failed to apply migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// A verdict event was appended with no usable producer run — the `run` value in
    /// its producer block is absent or empty. A run-less verdict is unattributable
    /// (HUB.md §4's identity is (repository, run)), and admitting it would collapse
    /// every run-less observation for one (store, claim, check) into a single dedup
    /// bucket regardless of verdict. It is rejected loudly (invariant #6), never
    /// silently absorbed.
    #[error(
        "the producer block has no usable `run` (absent or empty); a verdict event \
         must carry the producer's run id to be attributable and deduplicated"
    )]
    MissingProducerRun,

    /// A value read from or written to a JSON column could not be (de)serialized —
    /// a producer block or a supports list. A corruption or version-skew signal,
    /// never a normal outcome.
    #[error("failed to (de)serialize a JSON column ({context}): {source}")]
    Json {
        /// Which column or value was being (de)serialized, so the message is
        /// actionable.
        context: String,
        /// The underlying serde_json error.
        source: serde_json::Error,
    },

    /// A stored value that the schema guarantees but a read must still parse back
    /// into a domain type failed to parse — a verdict string, a kind, a timestamp.
    /// Reaching this means the database holds a value no writer of this crate could
    /// have produced (corruption or a foreign writer), so it is loud, not coerced.
    #[error("a stored value could not be parsed back ({context}): {value:?}")]
    Corrupt {
        /// What was being parsed (the column and expected type), named for the fix.
        context: String,
        /// The offending stored value.
        value: String,
    },

    /// The `git` binary could not be spawned to mirror a store — it is not installed
    /// or not on `PATH`. Registry sync shells to the system `git` (the one runtime
    /// dependency beside the hub binary, HUB-IMPLEMENTATION.md §1.6), so a missing
    /// binary is a real deployment fault named for the operator, not a state to
    /// continue past silently. Distinct from a git command that ran and exited
    /// non-zero ([`StoreError::Git`]).
    #[error("failed to run `git {args}` while syncing store {store}; is git installed and on PATH? ({source})")]
    GitSpawn {
        /// The connected store the sync was mirroring, so the operator knows which.
        store: String,
        /// The git subcommand and arguments that could not be spawned.
        args: String,
        /// The underlying spawn error.
        source: std::io::Error,
    },

    /// A git command that must succeed to mirror or read a store ran and exited
    /// non-zero — a clone against an unreachable remote, a fetch that failed, a
    /// tip that could not be resolved. Carries git's own stderr so the operator sees
    /// exactly what git reported. A sync that cannot mirror is a loud failure the
    /// interval driver reports and retries next tick, never a silently empty
    /// snapshot that would retire every claim (invariant #6).
    #[error("`git {args}` failed while syncing store {store}: {stderr}")]
    Git {
        /// The connected store the sync was mirroring.
        store: String,
        /// The git subcommand and arguments that failed.
        args: String,
        /// Git's stderr, trimmed.
        stderr: String,
    },

    /// A filesystem fault while preparing a store's mirror or its tip checkout — a
    /// mirror directory or worktree could not be created, listed, or removed.
    /// Distinct from a git command failure so the message can name the path, and
    /// distinct from a malformed claim file, which is a recorded
    /// [`SyncFinding`](crate::SyncFinding), never an error that fails the sync.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, naming the path, so the message is actionable.
        context: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A store's checked-out `.claims/` corpus itself could not be read — the
    /// directory could not be listed. This is the whole-corpus failure
    /// [`claim_store::Store::load_all`] raises, distinct from a single malformed
    /// file (which becomes a recorded [`SyncFinding`](crate::SyncFinding)). Wraps
    /// the store crate's error message, since a sync failing to read a tip at all is
    /// an environment fault, not a per-claim nag.
    #[error("failed to read the .claims/ corpus while syncing store {store}: {reason}")]
    Corpus {
        /// The connected store whose tip corpus could not be read.
        store: String,
        /// The underlying store-load error, as text.
        reason: String,
    },
}

/// The store's result type.
pub type Result<T> = std::result::Result<T, StoreError>;
