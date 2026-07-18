//! Shared test scaffolding for the tool-handler unit tests.
//!
//! Builds a throwaway git repo with a `.claims/` store and a deterministic
//! identity, so the query and create logic can be driven against a real store and
//! a real git provenance source without a network or the developer's ambient git
//! config. Compiled only under `#[cfg(test)]`.

#![cfg(test)]

use std::path::Path;
use std::process::Command;

use claim_store::Store;
use tempfile::TempDir;

/// A temp git repo that is also a claim store, the working root for one test.
pub struct TestStore {
    dir: TempDir,
    pub store: Store,
}

impl TestStore {
    /// A fresh git repo with a committed file (so `HEAD` resolves to a real sha)
    /// and a scaffolded `.claims/` store, with a deterministic local identity.
    pub fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Test Agent"]);
        git(dir.path(), &["config", "user.email", "agent@example.com"]);
        std::fs::write(dir.path().join("requirements.txt"), "libfoo==4.2\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "init"]);
        let (store, _) = Store::init(dir.path()).expect("init store");
        TestStore { dir, store }
    }

    /// The repo root, also the store root.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Write a standalone claim file at `.claims/<id>.md`, creating parents. The
    /// `id` may be namespaced (`payments/pin` lands at `.claims/payments/pin.md`).
    pub fn write_claim(&self, id: &str, contents: &str) {
        let path = self.root().join(".claims").join(format!("{id}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// A minimal valid claim body with the given id, statement, and supports.
    pub fn claim_text(id: &str, statement: &str, supports: &[&str]) -> String {
        let supports_block = if supports.is_empty() {
            String::new()
        } else {
            let mut b = String::from("supports:\n");
            for s in supports {
                b.push_str(&format!("  - {s}\n"));
            }
            b
        };
        format!(
            "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n{supports_block}---\n{statement}\n"
        )
    }

    /// Whether the repo's working tree has uncommitted changes (used to prove
    /// `create` writes but does not commit).
    pub fn working_tree_has_changes(&self) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        !out.stdout.is_empty()
    }
}

/// Run a git command in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}
