//! A CLI-level error carrying a stable, machine-readable `kind`.
//!
//! `--json` error output must be actionable by an agent without regexing English
//! prose (item 7's consumers key on the shape, not the wording). So the commands
//! raise an [`AppError`] whose [`ErrorKind`] is a coarse, stable discriminator —
//! `dirty-tree`, `not-witnessed`, `duplicate-id`, and so on — alongside the
//! human message.
//!
//! Not every failure is an `AppError`: an I/O fault, a git spawn failure, or a
//! `claim-core` parse error surfaces through `anyhow` with no `kind`, and
//! [`kind_of`] reports [`ErrorKind::Other`] for those. The typed kinds are reserved
//! for the *contract* failures a caller is expected to branch on.

use std::fmt;

/// A coarse, stable classification of a command failure, serialized in the `--json`
/// error object's `kind` field.
///
/// Kept deliberately small: each variant is a failure mode an agent might handle
/// differently (retry after committing for `DirtyTree`, pick a new id for
/// `DuplicateId`, fix the check for `NotWitnessed`). The kebab-case rename is the
/// wire form; adding a variant is backward-compatible because consumers match known
/// kinds and fall back on the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// No `.claims/` store was found from the current directory upward.
    NoStore,
    /// A claim with the requested id already exists in the store.
    DuplicateId,
    /// The tracked working tree has uncommitted changes, so the default
    /// witnessed-red restore would destroy them.
    DirtyTree,
    /// The green run against the current tree reported `Drifted` — the fact is
    /// already false.
    DriftedGreen,
    /// The green run reported `Broken` — the check cannot run.
    BrokenGreen,
    /// The witnessed-red step did not observe a `Drifted`, so the check is not
    /// trusted.
    NotWitnessed,
    /// The tree could not be restored to a state where the fact holds after the
    /// perturbation.
    NotRestored,
    /// A required input was absent with no terminal to prompt for it, or an
    /// interactive witness was needed in a non-interactive run.
    MissingInput,
    /// A supplied value (id, trigger, max-age) failed validation.
    InvalidInput,
    /// Any failure without a more specific contract kind (I/O, git, parse).
    Other,
}

impl ErrorKind {
    /// The wire string for this kind, matching the serde rename.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NoStore => "no-store",
            ErrorKind::DuplicateId => "duplicate-id",
            ErrorKind::DirtyTree => "dirty-tree",
            ErrorKind::DriftedGreen => "drifted-green",
            ErrorKind::BrokenGreen => "broken-green",
            ErrorKind::NotWitnessed => "not-witnessed",
            ErrorKind::NotRestored => "not-restored",
            ErrorKind::MissingInput => "missing-input",
            ErrorKind::InvalidInput => "invalid-input",
            ErrorKind::Other => "other",
        }
    }
}

/// A command failure with a machine-readable [`ErrorKind`] and a human message.
///
/// Constructed by the commands for contract failures and propagated through
/// `anyhow` (it implements [`std::error::Error`]). [`kind_of`] recovers the kind
/// from an `anyhow::Error` at the top level so the JSON error object can carry it;
/// a wrapper `context` above an `AppError` does not hide the kind, because the
/// lookup walks the whole cause chain.
#[derive(Debug)]
pub struct AppError {
    kind: ErrorKind,
    message: String,
}

impl AppError {
    /// A new error of `kind` with `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        AppError {
            kind,
            message: message.into(),
        }
    }

    /// This error's kind.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

/// The [`ErrorKind`] of an `anyhow::Error`, found by walking its cause chain for an
/// [`AppError`].
///
/// Walking the chain (not just the top) means a broad `.context("…")` wrapper above
/// a typed failure does not erase its kind. A chain with no `AppError` — a plain
/// I/O or git error — reports [`ErrorKind::Other`].
#[must_use]
pub fn kind_of(err: &anyhow::Error) -> ErrorKind {
    for cause in err.chain() {
        if let Some(app) = cause.downcast_ref::<AppError>() {
            return app.kind();
        }
    }
    ErrorKind::Other
}

/// Shorthand to build an `anyhow::Error` wrapping a typed [`AppError`].
///
/// Lets a command write `return Err(app(ErrorKind::DirtyTree, "…"))` and keeps the
/// kind recoverable by [`kind_of`].
pub fn app(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AppError::new(kind, message))
}
