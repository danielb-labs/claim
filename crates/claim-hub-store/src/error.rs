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
}

/// The store's result type.
pub type Result<T> = std::result::Result<T, StoreError>;
