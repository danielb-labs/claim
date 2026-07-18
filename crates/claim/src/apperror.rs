//! A CLI-level error carrying a stable, machine-readable `kind`.
//!
//! `--json` error output must be actionable by an agent without regexing English
//! prose (item 7's consumers key on the shape, not the wording). So the commands
//! raise an [`AppError`] whose [`ErrorKind`] is a coarse, stable discriminator —
//! `not-witnessed`, `duplicate-id`, `drifted-green`, and so on — alongside the
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
/// differently (pick a new id for `DuplicateId`, fix the check for `NotWitnessed`,
/// supply a real change for `NoChange`). The kebab-case rename is the wire form;
/// adding a variant is backward-compatible because consumers match known kinds and
/// fall back on the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// No `.claims/` store was found from the current directory upward.
    NoStore,
    /// A claim with the requested id already exists in the store.
    DuplicateId,
    /// The establishing run against the current tree reported `Drifted` — the fact
    /// is already false.
    DriftedGreen,
    /// The establishing run reported `Broken` — the check cannot run.
    BrokenGreen,
    /// The optional `--witness-cmd` step did not observe a `Drifted`, so the check
    /// could not be shown to discriminate.
    NotWitnessed,
    /// A required input was absent with no terminal to prompt for it, or
    /// `--witness-cmd` was requested on an unborn HEAD (no commit to check out).
    MissingInput,
    /// A supplied value (id, trigger, max-age) failed validation, or an id that must
    /// name an existing claim (`retire`, `amend`) does not.
    InvalidInput,
    /// A `claim amend` supplied no field, or fields identical to the claim's current
    /// values: there is nothing to change, so nothing is written.
    NoChange,
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
            ErrorKind::DriftedGreen => "drifted-green",
            ErrorKind::BrokenGreen => "broken-green",
            ErrorKind::NotWitnessed => "not-witnessed",
            ErrorKind::MissingInput => "missing-input",
            ErrorKind::InvalidInput => "invalid-input",
            ErrorKind::NoChange => "no-change",
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

/// The [`ErrorKind`] of an `anyhow::Error`, found by walking its cause chain for a
/// typed failure.
///
/// Walking the chain (not just the top) means a broad `.context("…")` wrapper above
/// a typed failure does not erase its kind. Two typed families are recognized: the
/// CLI's own [`AppError`] for contract failures the commands raise, and
/// [`claim_store::StoreError`] from the shared store crate, whose `NoStore` variant
/// maps to [`ErrorKind::NoStore`] — the single "run `claim init`" signal every verb
/// reports identically, now that store discovery lives in `claim-store` rather than
/// in a CLI module that could construct an `AppError` directly. A chain with neither
/// — a plain I/O or git error — reports [`ErrorKind::Other`].
#[must_use]
pub fn kind_of(err: &anyhow::Error) -> ErrorKind {
    for cause in err.chain() {
        if let Some(app) = cause.downcast_ref::<AppError>() {
            return app.kind();
        }
        if let Some(claim_store::StoreError::NoStore { .. }) =
            cause.downcast_ref::<claim_store::StoreError>()
        {
            return ErrorKind::NoStore;
        }
    }
    ErrorKind::Other
}

/// Shorthand to build an `anyhow::Error` wrapping a typed [`AppError`].
///
/// Lets a command write `return Err(app(ErrorKind::DuplicateId, "…"))` and keeps
/// the kind recoverable by [`kind_of`].
pub fn app(kind: ErrorKind, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AppError::new(kind, message))
}
