//! Integration tests for `claim docs`, driving the built binary.
//!
//! The load-bearing properties: the bundled site is *self-contained* (the path
//! `claim docs` prints is a real file whose sibling `assets/` images exist, so an
//! installed binary with no repository behind it still resolves every diagram); the
//! verb is *headless by default* — it prints the path and never opens a browser
//! unless `--open` is passed; and `--open` still *degrades* on a box with no opener
//! to printing the path and exiting 0 rather than failing. These make the docs
//! reachable for the user this verb exists for — someone who `cargo install`ed the
//! tool and has no `docs/` on disk.

use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::cargo_bin;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `claim` command whose docs cache is redirected to `cache` via
/// `CLAIM_DOCS_CACHE_DIR`, so a test never reads or writes the real user cache and
/// cannot race another test (or run) over shared files. The docs verb needs no store,
/// so no temp repo is set up; individual tests override `PATH` when they need to
/// simulate a box with no opener.
fn claim_with_cache(cache: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::from_std(Command::new(cargo_bin("claim")));
    cmd.env("CLAIM_DOCS_CACHE_DIR", cache);
    cmd
}

/// A `claim` command pointed at a fresh, isolated docs cache. The returned [`TempDir`]
/// owns that cache and must outlive the command's use, so the caller binds it.
fn claim() -> (assert_cmd::Command, TempDir) {
    let cache = TempDir::new().expect("create an isolated docs cache dir");
    let cmd = claim_with_cache(cache.path());
    (cmd, cache)
}

/// The single line of stdout the default prints, trimmed — the resolved page path.
fn path_line(output: &[u8]) -> String {
    String::from_utf8(output.to_vec())
        .expect("utf-8 stdout")
        .trim()
        .to_owned()
}

#[test]
fn default_prints_a_real_file_and_only_the_path() {
    let (mut claim, _cache) = claim();
    let out = claim
        .args(["docs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let path = path_line(&out);
    let page = Path::new(&path);
    assert!(page.is_file(), "docs must print a real file: {path}");
    assert!(
        page.ends_with("index.html"),
        "the default page is the overview: {path}"
    );
    // stdout is *only* the path, so `open "$(claim docs)"` composes.
    assert_eq!(
        path.lines().count(),
        1,
        "docs prints exactly one line: {path:?}"
    );
}

#[test]
fn the_bundled_site_is_self_contained() {
    // The property that makes an installed binary usable: the overview the verb
    // writes references images by relative path, and those images must exist next to
    // it. If they did not, an installed user (no repo, no docs/ on disk) would open a
    // page with broken diagrams.
    let (mut claim, _cache) = claim();
    let out = claim
        .args(["docs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let index = path_line(&out);
    let dir = Path::new(&index).parent().expect("page has a parent dir");

    let html = std::fs::read_to_string(&index).expect("read the written index.html");
    // Every `src="assets/..."` the HTML declares must resolve to a written file.
    for asset in ["architecture.png", "graph-propagation.png", "lifecycle.png"] {
        assert!(
            html.contains(&format!("assets/{asset}")),
            "index.html should reference assets/{asset}"
        );
        let on_disk = dir.join("assets").join(asset);
        let meta = std::fs::metadata(&on_disk)
            .unwrap_or_else(|_| panic!("assets/{asset} must be written next to index.html"));
        assert!(meta.len() > 0, "assets/{asset} must not be empty");
    }

    // The topic pages the HTML links to are bundled too, so those links resolve.
    for page in ["ci.md", "agent-checks.md", "dogfooding.md"] {
        assert!(
            dir.join(page).is_file(),
            "linked topic page {page} must be written"
        );
    }
}

#[test]
fn a_topic_selects_that_page() {
    let (mut claim, _cache) = claim();
    let out = claim
        .args(["docs", "ci"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(
        path_line(&out).ends_with("ci.md"),
        "`docs ci` selects the ci page"
    );
}

#[test]
fn an_unknown_topic_is_a_usage_error_naming_the_valid_ones() {
    let (mut claim, _cache) = claim();
    claim
        .args(["docs", "not-a-topic"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not-a-topic"))
        .stderr(predicate::str::contains("ci"))
        .stderr(predicate::str::contains("agent-checks"));
}

#[test]
fn json_shape_is_stable() {
    let (mut claim, _cache) = claim();
    let out = claim
        .args(["--json", "docs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON on stdout");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["opened"], false, "the default never opens");
    assert!(
        v["path"]
            .as_str()
            .is_some_and(|p| p.ends_with("index.html")),
        "path points at the overview: {v}"
    );
}

#[test]
fn default_human_output_snapshot() {
    // The insta obligation (CLAUDE.md): the human output is a deliberate, reviewable
    // surface. The default path is the stable one to snapshot — it never opens a
    // browser and prints only the resolved path — but the cache-dir prefix is machine-
    // and version-specific, so redact it to a fixed `<cache>/` token. What remains is
    // the exact shape a change must not silently alter: a single bare path line.
    let (mut claim, _cache) = claim();
    let out = claim
        .args(["docs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let redacted = redact_cache_path(&stdout);
    insta::assert_snapshot!(redacted);
}

/// Replace the per-run cache-directory prefix on the printed path with a fixed
/// `<cache>/` token, so the snapshot captures the shape without the machine- and
/// version-specific path. The default prints one bare line: the path ending in
/// `index.html`.
fn redact_cache_path(stdout: &str) -> String {
    stdout
        .lines()
        .map(|line| {
            if line.trim_end().ends_with("index.html") {
                "<cache>/index.html".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn open_without_an_opener_degrades_to_printing_the_path() {
    // Simulate a box with no browser opener: an empty PATH means `open`/`xdg-open`
    // cannot be found. With `--open` asked for but unavailable, the verb must still
    // exit 0, print the path on stdout, and warn on stderr that it could not open —
    // never fail, because a doc a user can open by hand is not an error. The stderr
    // assertion pins the note so deleting it is caught. (`--open` is the only path
    // that ever launches an opener, so it is the only one safe to exercise with an
    // empty PATH; the default never opens and so is exercised freely above.)
    let dir = tempfile::TempDir::new().unwrap();
    let (mut claim, _cache) = claim();
    claim
        .args(["docs", "--open"])
        .env("PATH", dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("index.html"))
        .stderr(predicate::str::contains("no browser opener was found"));
}

#[test]
fn concurrent_docs_runs_never_leave_a_torn_asset() {
    // The regression this pins: several `claim docs` processes sharing one cache
    // directory. Before the fix, they materialized the bundle with truncate-in-place
    // `fs::write`, so one process could observe an asset (e.g. architecture.png) at
    // length 0 while another was mid-write — the exact nondeterministic panic in
    // `the_bundled_site_is_self_contained` under full-workspace thread contention.
    // With atomic-rename materialization, an asset only ever appears at its full,
    // correct bytes. Eight processes race over one cache; afterward every materialized
    // asset must be non-empty and byte-identical to the source the binary embedded.
    let cache = TempDir::new().unwrap();

    let bin = cargo_bin("claim");
    let mut children = Vec::new();
    for _ in 0..8 {
        let child = Command::new(&bin)
            .arg("docs")
            .env("CLAIM_DOCS_CACHE_DIR", cache.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a concurrent `claim docs`");
        children.push(child);
    }

    let mut site_dir = None;
    for child in children {
        let out = child.wait_with_output().expect("await `claim docs`");
        assert!(
            out.status.success(),
            "a concurrent `claim docs` failed: {:?}",
            out.status
        );
        let printed = String::from_utf8(out.stdout)
            .expect("utf-8 path")
            .trim()
            .to_owned();
        let dir = Path::new(&printed)
            .parent()
            .expect("printed page has a parent dir")
            .to_path_buf();
        // Every run resolves the same shared cache, so all must agree on the site dir.
        match &site_dir {
            None => site_dir = Some(dir),
            Some(seen) => assert_eq!(seen, &dir, "runs disagreed on the cache site dir"),
        }
    }
    let site_dir = site_dir.expect("at least one run printed a path");

    // The site-relative assets and the source each was embedded from, under the
    // repository `docs/` tree two levels up from this crate's manifest.
    let docs_src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs");
    let assets = [
        "index.html",
        "README.md",
        "ci.md",
        "agent-checks.md",
        "dogfooding.md",
        "assets/architecture.png",
        "assets/graph-propagation.png",
        "assets/lifecycle.png",
    ];
    for rel in assets {
        let on_disk = std::fs::read(site_dir.join(rel))
            .unwrap_or_else(|e| panic!("{rel} was not materialized: {e}"));
        assert!(!on_disk.is_empty(), "{rel} is empty after concurrent runs");
        let source = std::fs::read(docs_src.join(rel))
            .unwrap_or_else(|e| panic!("source docs/{rel} unreadable: {e}"));
        assert_eq!(
            on_disk, source,
            "{rel} does not match its embedded source after concurrent runs"
        );
    }
}

#[test]
fn cache_dir_env_var_redirects_the_site_and_is_documented() {
    // CLAIM_DOCS_CACHE_DIR is user-facing behavior: it relocates a real user's cache
    // ahead of the platform default. The verb must honor it — the printed page must
    // land under the directory given — and `--help` must name it, matching the
    // CLAIM_AGENT_CMD precedent that documents its env var in both help and docs. A
    // rename that dropped either the honoring or the help text would slip past the
    // docs-cover backstop, which checks verbs, not env vars.
    let cache = TempDir::new().unwrap();
    let out = claim_with_cache(cache.path())
        .args(["docs"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let page = path_line(&out);
    assert!(
        Path::new(&page).starts_with(cache.path()),
        "the printed page {page} must be under CLAIM_DOCS_CACHE_DIR {}",
        cache.path().display()
    );

    let (mut claim, _cache) = claim();
    claim
        .args(["docs", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CLAIM_DOCS_CACHE_DIR"));
}
