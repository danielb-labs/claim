//! Integration tests for `scripts/docs-cover-cli.sh`, the check the docs-coverage
//! claim runs. This script is the mechanism the whole backstop rests on, so its
//! exit-code contract is pinned here: 0 when every verb and MCP tool is documented,
//! 1 when one is missing (drift), 2 when a source it reads is absent (broken, never
//! a false pass).
//!
//! Each case builds a throwaway repo skeleton — the script, a real `claim` binary,
//! the real MCP server source, and a *controlled* `docs/index.html` — and runs the
//! script against it, so the docs the script judges are the test's, not the repo's.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::cargo_bin;

/// The workspace root, derived from this crate's manifest dir (`crates/claim`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above crates/claim")
        .to_path_buf()
}

/// A throwaway repo skeleton the script can run against: the script itself, the real
/// `claim` binary at `target/debug/claim`, the real MCP server source, and the given
/// `index.html`. Returns the skeleton root; the script derives its own `repo_root`
/// from the script's location within it, so everything must sit at the real relative
/// paths.
fn skeleton(index_html: &str, with_binary: bool) -> tempfile::TempDir {
    let ws = workspace_root();
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("crates/claim-mcp/src")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();

    fs::copy(
        ws.join("scripts/docs-cover-cli.sh"),
        root.join("scripts/docs-cover-cli.sh"),
    )
    .unwrap();
    fs::copy(
        ws.join("crates/claim-mcp/src/server.rs"),
        root.join("crates/claim-mcp/src/server.rs"),
    )
    .unwrap();
    fs::write(root.join("docs/index.html"), index_html).unwrap();

    if with_binary {
        fs::copy(cargo_bin("claim"), root.join("target/debug/claim")).unwrap();
    }

    dir
}

/// Run the skeleton's copy of the script and return its exit code.
fn run_script(root: &Path) -> i32 {
    Command::new("bash")
        .arg(root.join("scripts/docs-cover-cli.sh"))
        .status()
        .expect("run docs-cover-cli.sh")
        .code()
        .expect("script exited with a code")
}

/// An `index.html` documenting every current verb and MCP tool — the covered case.
/// Verbs appear as `claim <verb>`; MCP tools appear as their `<strong>tool</strong>`
/// entry inside a `<section id="mcp">` block, the exact shape the real site uses.
fn covered_index() -> String {
    let verbs = [
        "init", "add", "check", "list", "log", "drift", "amend", "retire", "stats", "graph", "docs",
    ];
    let mut html = String::from("<html><body>\n");
    for verb in verbs {
        html.push_str(&format!(
            "<p>Run <code>claim {verb}</code> to do the thing.</p>\n"
        ));
    }
    html.push_str("<section id=\"mcp\">\n");
    for tool in ["query", "report", "create"] {
        html.push_str(&format!("<li><strong>{tool}</strong> — a tool.</li>\n"));
    }
    html.push_str("</section>\n</body></html>\n");
    html
}

#[test]
fn all_documented_holds_with_exit_0() {
    let dir = skeleton(&covered_index(), true);
    assert_eq!(
        run_script(dir.path()),
        0,
        "a site documenting every verb and tool must hold"
    );
}

#[test]
fn a_missing_verb_drifts_with_exit_1() {
    // Drop the `claim retire` mention: an undocumented verb is drift, not a broken
    // check — the site is readable, it is just incomplete.
    let index = covered_index().replace(
        "<p>Run <code>claim retire</code> to do the thing.</p>\n",
        "",
    );
    let dir = skeleton(&index, true);
    assert_eq!(
        run_script(dir.path()),
        1,
        "an undocumented verb must drift (exit 1)"
    );
}

#[test]
fn a_missing_tool_drifts_with_exit_1() {
    // Drop the `create` tool's `<strong>` entry but leave the word in prose, to prove
    // the check keys on the reference entry, not an incidental mention — the exact
    // vacuous-pass that let item-14's `create` ship undocumented.
    let index = covered_index().replace(
        "<li><strong>create</strong> — a tool.</li>\n",
        "<li>the server can create claims, described elsewhere.</li>\n",
    );
    let dir = skeleton(&index, true);
    assert_eq!(
        run_script(dir.path()),
        1,
        "a tool with no <strong> entry must drift (exit 1) despite prose"
    );
}

#[test]
fn an_absent_binary_is_broken_with_exit_2() {
    // No `target/debug/claim`: the script cannot read the real verb surface, so it is
    // Broken (exit 2), never a false pass — a check that could not run tells us
    // nothing (golden invariant #1).
    let dir = skeleton(&covered_index(), false);
    assert_eq!(
        run_script(dir.path()),
        2,
        "a missing binary must be broken (exit 2), not a pass"
    );
}

#[test]
fn an_absent_doc_is_broken_with_exit_2() {
    // Remove docs/index.html entirely: a source the check reads is gone, which is
    // Broken (exit 2), distinct from an incomplete-but-present doc (drift, exit 1).
    let dir = skeleton(&covered_index(), true);
    fs::remove_file(dir.path().join("docs/index.html")).unwrap();
    assert_eq!(
        run_script(dir.path()),
        2,
        "a missing docs/index.html must be broken (exit 2)"
    );
}
