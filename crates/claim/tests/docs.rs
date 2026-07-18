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

/// A `claim` command that inherits the ambient environment (the docs verb needs no
/// store, so no temp repo is set up). Individual tests override `PATH` when they
/// need to simulate a box with no opener.
fn claim() -> assert_cmd::Command {
    assert_cmd::Command::from_std(Command::new(cargo_bin("claim")))
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
    let out = claim()
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
    let out = claim()
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
    let out = claim()
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
    claim()
        .args(["docs", "not-a-topic"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not-a-topic"))
        .stderr(predicate::str::contains("ci"))
        .stderr(predicate::str::contains("agent-checks"));
}

#[test]
fn json_shape_is_stable() {
    let out = claim()
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
    let out = claim()
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
    claim()
        .args(["docs", "--open"])
        .env("PATH", dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("index.html"))
        .stderr(predicate::str::contains("no browser opener was found"));
}
