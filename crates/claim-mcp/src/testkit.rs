//! Shared test scaffolding for the tool-handler unit tests.
//!
//! Builds a throwaway git repo with a `.claims/` store and a deterministic
//! identity, so the query and report logic can be driven against a real store and
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
            "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\n{supports_block}---\n{statement}\n"
        )
    }

    /// Append a verdict entry directly to a claim's log with a chosen timestamp,
    /// so a test controls the status arithmetic deterministically. `verdict` is one
    /// of `held`, `drifted`, `unverifiable`, `broken`.
    pub fn write_verdict(&self, id: &str, at: &str, verdict: &str, evidence: Option<&str>) {
        let dir = self.root().join(".claims/log").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let evidence_json = match evidence {
            Some(e) => serde_json::Value::String(e.to_owned()),
            None => serde_json::Value::Null,
        };
        let entry = serde_json::json!({
            "at": at,
            "commit": "0".repeat(40),
            "actor": "Seed <seed@example.com>",
            "event": { "type": "verification", "verdict": verdict, "evidence": evidence_json },
        });
        // A filename embedding the timestamp keeps a plain listing chronological,
        // matching the tool's own naming; a unique suffix avoids collisions.
        let stamp = at.replace(':', "-");
        let name = format!(
            "{stamp}-{:016x}.json",
            fnv(&serde_json::to_vec(&entry).unwrap())
        );
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(&entry).unwrap()).unwrap();
    }

    /// The number of verdict-log entry files under a claim id, for asserting a
    /// write did (or did not) happen.
    pub fn log_count(&self, id: &str) -> usize {
        let dir = self.root().join(".claims/log").join(id);
        std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// The parsed verdict-log entries under a claim id, in filename order.
    pub fn log_entries(&self, id: &str) -> Vec<serde_json::Value> {
        let dir = self.root().join(".claims/log").join(id);
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
            .iter()
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect()
    }

    /// Whether the repo's working tree has uncommitted changes (used to prove
    /// `report` writes but does not commit).
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

/// FNV-1a over bytes, for a deterministic unique filename suffix in tests.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
