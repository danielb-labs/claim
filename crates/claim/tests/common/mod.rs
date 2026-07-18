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

    /// A git repo with a commit but *no* `user.name`/`user.email`, so resolving a
    /// verdict's actor fails. For proving `claim check --report-only` needs no
    /// identity while a persisting run does.
    ///
    /// `commit.gpgsign` is disabled and identity is passed only to the one commit
    /// via `-c`, so the working repo genuinely lacks a configured identity
    /// afterward.
    pub fn no_identity() -> Self {
        let dir = TempDir::new().expect("make temp dir");
        let repo = TestRepo { dir };
        repo.git(&["init", "-q"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.write("requirements.txt", "libfoo==4.2\n");
        repo.git(&["add", "requirements.txt"]);
        // Provide identity to this one commit only, so HEAD resolves but the repo
        // config carries no user.name/user.email for later verdicts.
        repo.git(&[
            "-c",
            "user.name=Seed",
            "-c",
            "user.email=seed@example.com",
            "commit",
            "-q",
            "-m",
            "initial",
        ]);
        repo
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
    ///
    /// Git's global and system config are pointed at nonexistent files so the
    /// machine's ambient `user.name`/`user.email` can never leak into a test — the
    /// only identity `claim` sees is what a `TestRepo` set locally. This is what
    /// makes the "no identity configured" test honest on a developer's machine,
    /// where a global identity is usually present.
    pub fn claim(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::from_std(Command::new(cargo_bin("claim")));
        cmd.current_dir(self.path());
        cmd.env("GIT_CONFIG_GLOBAL", self.path().join("nonexistent-global"));
        cmd.env("GIT_CONFIG_SYSTEM", self.path().join("nonexistent-system"));
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

    /// The number of verdict-log entry files under a claim id, or 0 if the log
    /// directory does not exist. For asserting a run wrote (or did not write) a
    /// verdict.
    pub fn log_count(&self, id: &str) -> usize {
        let dir = self.path().join(".claims/log").join(id);
        std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(std::result::Result::ok)
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// Write a standalone claim file directly under `.claims/`, bypassing
    /// `claim add`.
    ///
    /// Lets a test build a store with a precise shape — several claims, specific
    /// triggers, chosen `supports` — and seed its verdict log directly, without
    /// running `add`. The `.md` extension and the `.claims/` prefix are added, so
    /// `write_claim("payments/pin", ...)` lands at `.claims/payments/pin.md`.
    pub fn write_claim(&self, id: &str, frontmatter_and_body: &str) {
        self.write(&format!(".claims/{id}.md"), frontmatter_and_body);
    }

    /// Append a verdict entry to a claim's log with a chosen timestamp, so a test
    /// controls the due/stale arithmetic deterministically.
    ///
    /// Writes a JSON entry in the exact on-disk shape `read_entries` expects. The
    /// filename embeds the timestamp (colons swapped for hyphens) so a plain
    /// listing stays chronological, matching the tool's own naming. `verdict` is
    /// one of `held`, `drifted`, `unverifiable`, `broken`.
    pub fn write_verdict(&self, id: &str, at: &str, verdict: &str) {
        let dir = self.path().join(".claims/log").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = serde_json::json!({
            "at": at,
            "commit": "0".repeat(40),
            "actor": "Test User <test@example.com>",
            "event": { "type": "verification", "verdict": verdict, "evidence": null },
        });
        // A filename that sorts chronologically and is filesystem-safe: the stamp
        // with `:` replaced, plus a short disambiguator so two entries at one
        // instant do not collide.
        let stamp = at.replace(':', "-");
        let n = self.log_count(id);
        let name = format!("{stamp}-{n:04}.json");
        std::fs::write(dir.join(name), serde_json::to_vec_pretty(&entry).unwrap()).unwrap();
    }

    /// Append a retirement adjudication to a claim's log at a chosen timestamp, so
    /// a test can build a `Status::Retired` claim without the (unbuilt) `retire`
    /// verb.
    pub fn write_retirement(&self, id: &str, at: &str, note: &str) {
        let dir = self.path().join(".claims/log").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let entry = serde_json::json!({
            "at": at,
            "commit": "0".repeat(40),
            "actor": "Test User <test@example.com>",
            "event": { "type": "adjudication", "action": { "action": "retire", "note": note } },
        });
        let stamp = at.replace(':', "-");
        let n = self.log_count(id);
        std::fs::write(
            dir.join(format!("{stamp}-{n:04}.json")),
            serde_json::to_vec_pretty(&entry).unwrap(),
        )
        .unwrap();
    }

    /// A `claim` command with `now` pinned via the `CLAIM_NOW` env seam, so the
    /// due/stale arithmetic is measured against a fixed instant rather than the
    /// racing wall clock. `at` is an RFC 3339 timestamp.
    pub fn claim_at(&self, at: &str) -> assert_cmd::Command {
        let mut cmd = self.claim();
        cmd.env("CLAIM_NOW", at);
        cmd
    }
}
