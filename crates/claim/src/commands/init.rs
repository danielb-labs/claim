//! `claim init`: scaffold a `.claims/` store in the current repository.

use std::env;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::InitArgs;
use crate::output::{emit, warn, Format};
use claim_store::git::is_inside_work_tree;
use claim_store::Store;

/// The machine form of `claim init`, stable across runs.
///
/// `created` distinguishes a fresh scaffold from a re-run against an existing
/// store, so a script can tell whether it just set things up. Both are success.
#[derive(Debug, Serialize)]
struct InitReport {
    /// Always `"ok"`, so a consumer keys on a status field rather than exit code
    /// alone.
    status: &'static str,
    /// The repository root the store was created under.
    root: String,
    /// The `.claims/` directory path.
    claims_dir: String,
    /// Whether this run created the store (`true`) or found it already present
    /// (`false`).
    created: bool,
}

/// Scaffold `.claims/` in the target directory, idempotently.
///
/// The store root is the directory the store lives in — `--dir` when given, else
/// the current directory. This is where later commands anchor: [`claim_store::discover`]
/// walks up to find this same `.claims/`. Re-running is not an error; it reports
/// `created: false`.
///
/// # Errors
///
/// Fails if the current directory cannot be read, or the store directory cannot be
/// created (see [`Store::init`]).
pub fn run(args: &InitArgs, format: Format) -> Result<()> {
    let root = match &args.dir {
        Some(dir) => dir.clone(),
        None => env::current_dir().context("could not read the current directory")?,
    };

    let (store, created) = Store::init(&root)?;

    // A store outside a git repository is technically usable but degenerate: `claim
    // add` cannot attribute an authored claim without a commit and identity, so warn
    // rather than let the user discover it later. Not fatal — `git init` may simply
    // come next.
    if !is_inside_work_tree(store.root()) {
        warn(
            "this store is not inside a git repository; `claim add` needs one to attribute \
             authored claims. Run `git init` here before adding claims.",
        );
    }

    let report = InitReport {
        status: "ok",
        root: store.root().display().to_string(),
        claims_dir: store.claims_dir().display().to_string(),
        created,
    };

    emit(format, &report, || {
        if created {
            println!("Created claim store at {}", report.claims_dir);
        } else {
            println!("Claim store already present at {}", report.claims_dir);
        }
        println!("Add your first claim with `claim add`.");
    })
}
