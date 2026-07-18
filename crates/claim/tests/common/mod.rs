//! Shared setup for the CLI integration tests.
//!
//! Every test drives the built `claim` binary against a throwaway git repo, so the
//! tests are hermetic: a fixed git identity is set *in the repo*, never read from
//! ambient config, and no test touches the network or another test's directory.

// This module is compiled into each test binary (`init.rs`, `add.rs`) separately,
// and no single binary exercises every helper, so per-binary dead-code warnings are
// expected and not a signal of real dead code.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::cargo_bin;
use tempfile::TempDir;

/// A temp directory that is a git repo with a deterministic identity, the working
/// directory for one test's `claim` invocations.
pub struct TestRepo {
    dir: TempDir,
}

impl TestRepo {
    /// A fresh git repo with `user.name`/`user.email` set locally and one initial
    /// commit, so `HEAD` resolves to a real sha (the ordinary case). Use
    /// [`TestRepo::unborn`] for the no-commit edge.
    pub fn new() -> Self {
        let repo = Self::init_bare_repo();
        // A committed file gives HEAD a real sha and a tracked file the checks and
        // perturbations can act on.
        repo.write("requirements.txt", "libfoo==4.2\n");
        repo.git(&["add", "requirements.txt"]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    /// A fresh git repo with a deterministic identity but *no commit* — an unborn
    /// HEAD, for the git-edge test.
    pub fn unborn() -> Self {
        let repo = Self::init_bare_repo();
        repo.write("requirements.txt", "libfoo==4.2\n");
        repo
    }

    /// A temp directory that is *not* a git repository, for the "store outside a
    /// repo" warning. Named `TestRepo` for reuse of its helpers even though there is
    /// no repo here.
    pub fn no_git() -> Self {
        let dir = TempDir::new().expect("make temp dir");
        TestRepo { dir }
    }

    fn init_bare_repo() -> Self {
        let dir = TempDir::new().expect("make temp dir");
        let repo = TestRepo { dir };
        repo.git(&["init", "-q"]);
        // Identity is set locally so no ambient global config leaks in and the
        // actor recorded in the log is exactly this.
        repo.git(&["config", "user.name", "Test User"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo
    }

    /// The repo's root path, the working directory for `claim` and the store root.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write a file relative to the repo root, creating parents.
    pub fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    /// Read a file relative to the repo root.
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path().join(rel)).unwrap()
    }

    /// Whether a path relative to the repo root exists.
    pub fn exists(&self, rel: &str) -> bool {
        self.path().join(rel).exists()
    }

    /// Run a git command in the repo, asserting success.
    pub fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A `claim` command rooted at this repo, ready for arguments and assertions.
    pub fn claim(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::from_std(Command::new(cargo_bin("claim")));
        cmd.current_dir(self.path());
        cmd
    }

    /// Read every verdict-log entry file under a claim id, as parsed JSON, in
    /// filename order (which is chronological — the stamp leads the name).
    pub fn log_entries(&self, id: &str) -> Vec<serde_json::Value> {
        let dir = self.path().join(".claims/log").join(id);
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|_| panic!("log dir for {id} should exist"))
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        names.sort();
        names
            .iter()
            .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
            .collect()
    }
}
