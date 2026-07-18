//! The crate's error type.
//!
//! `claim-core` returns typed errors so callers can distinguish a malformed
//! claim from a filesystem failure and respond differently — a parse error names
//! a file to fix, an I/O error names a condition to retry. The binaries map
//! these onto exit codes and human messages. Feature work extends this enum
//! rather than reaching for stringly-typed errors.

/// Errors produced by `claim-core`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A claim file could not be parsed. Carries the offending path and a reason
    /// a human can act on.
    #[error("{path}: {reason}")]
    Parse {
        /// The file that failed to parse.
        path: String,
        /// Why it failed, phrased so the author can fix it.
        reason: String,
    },

    /// An underlying I/O failure, with the path it concerned.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory involved.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// The crate's result alias.
pub type Result<T> = std::result::Result<T, Error>;
