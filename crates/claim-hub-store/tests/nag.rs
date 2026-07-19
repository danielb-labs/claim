//! Integration test for owner resolution from a real git mirror (hub-11).
//!
//! `owners_for` (the CODEOWNERS matcher) is unit-tested in `src/nag.rs`; this pins the
//! **end-to-end** path `resolve_owners` walks: a real bare git mirror (no network — a local
//! fixture used as the remote), a `git show <commit>:CODEOWNERS` read of it, and the match
//! against a claim's canonical path. The point is that owner resolution reads the mirror the
//! registry sync already maintains, so a fire resolves an owner with no forge call.

use std::path::Path;
use std::process::Command;

use claim_hub_store::{resolve_owners, sync_store, ConnectedStore, SqliteStore};
use tempfile::TempDir;

const STORE: &str = "github.com/acme/payments";

/// A fresh local git repo carrying the given files, committed on `main` — used as the sync
/// remote. Returns the temp dir (kept alive) so the fixture outlives the sync.
fn git_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "commit.gpgsign", "false"]);
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "seed"]);
    dir
}

/// Sync the fixture and return the mirror root and the synced tip commit.
async fn sync(store: &SqliteStore, fixture: &TempDir, mirror_root: &Path) -> String {
    let connected = ConnectedStore::new(STORE, fixture.path().to_string_lossy().into_owned());
    let outcome = sync_store(store, &connected, mirror_root)
        .await
        .expect("sync the fixture");
    outcome.commit
}

#[tokio::test]
async fn resolve_owners_reads_codeowners_from_the_synced_mirror() {
    let claim =
        "---\nid: payments/pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nThe pin holds.\n";
    let fixture = git_fixture(&[
        (".claims/payments/pin.md", claim),
        (
            ".github/CODEOWNERS",
            "*                       @acme/eng\n.claims/payments/       @acme/payments\n",
        ),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = TempDir::new().unwrap();
    let tip = sync(&store, &fixture, mirror_root.path()).await;

    // The specific rule wins over the catch-all for a claim under `.claims/payments/`.
    let owners = resolve_owners(mirror_root.path(), STORE, &tip, ".claims/payments/pin.md")
        .expect("resolve owners");
    assert_eq!(owners, vec!["@acme/payments"]);

    // A claim outside the specific rule falls to the catch-all.
    let other = resolve_owners(mirror_root.path(), STORE, &tip, ".claims/other.md")
        .expect("resolve owners");
    assert_eq!(other, vec!["@acme/eng"]);
}

#[tokio::test]
async fn no_codeowners_in_the_mirror_yields_no_owners() {
    // A store with no CODEOWNERS file: owner resolution returns empty (the router
    // dead-letters), never an error — the absence is the legitimate "no owner" case.
    let claim = "---\nid: payments/pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nHolds.\n";
    let fixture = git_fixture(&[(".claims/payments/pin.md", claim)]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = TempDir::new().unwrap();
    let tip = sync(&store, &fixture, mirror_root.path()).await;

    let owners = resolve_owners(mirror_root.path(), STORE, &tip, ".claims/payments/pin.md")
        .expect("resolve owners does not error on a missing CODEOWNERS");
    assert!(owners.is_empty(), "no CODEOWNERS → no owners (dead-letter)");
}
