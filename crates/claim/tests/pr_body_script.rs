//! Integration tests for `scripts/check-pr-body.sh`, the check behind the pr-template CI
//! lane. GitHub only pre-fills the PR template and never enforces it; this script is what
//! fails a PR whose body drops a template section, so its exit-code contract is pinned
//! here: 0 when every template section is present, 1 when one is missing (naming it), 2
//! when a source it reads (the template) is absent — broken, never a false pass.
//!
//! Each case builds a throwaway repo skeleton — the script plus a *controlled*
//! `.github/PULL_REQUEST_TEMPLATE.md` — so the required-section list the script derives is
//! the test's, not the repo's, and stays stable when the real template changes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The workspace root, derived from this crate's manifest dir (`crates/claim`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above crates/claim")
        .to_path_buf()
}

/// A controlled template with three live `## ` sections and noise the parser must ignore:
/// a top `#` title and a `###` subheading are not sections, and a `## ` line *inside* an
/// HTML comment block is commented-out guidance, not a required section. The script must
/// derive exactly `["What & why", "How", "Notes"]` — never `Commented`.
fn template() -> &'static str {
    "<!-- guidance block\n\
     ## Commented (inside a comment; not a required section)\n\
     end guidance -->\n\
     # Title (not a section)\n\
     ## What & why\n\
     ### A subheading, not a section\n\
     ## How\n\
     ## Notes\n"
}

/// A throwaway repo skeleton the script can run against: the script itself at its real
/// relative path (the script derives `repo_root` from its own location) and the given
/// template. Returns the skeleton root.
fn skeleton(with_template: bool) -> tempfile::TempDir {
    let ws = workspace_root();
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::create_dir_all(root.join(".github")).unwrap();

    fs::copy(
        ws.join("scripts/check-pr-body.sh"),
        root.join("scripts/check-pr-body.sh"),
    )
    .unwrap();
    if with_template {
        fs::write(root.join(".github/PULL_REQUEST_TEMPLATE.md"), template()).unwrap();
    }

    dir
}

/// Run the skeleton's copy of the script with `body` written to a file passed as `$1`,
/// and return its exit code.
fn run_with_body(root: &Path, body: &str) -> i32 {
    let body_path = root.join("body.md");
    fs::write(&body_path, body).unwrap();
    Command::new("bash")
        .arg(root.join("scripts/check-pr-body.sh"))
        .arg(&body_path)
        .status()
        .expect("run check-pr-body.sh")
        .code()
        .expect("script exited with a code")
}

/// A body carrying every section of `template()`, plus prose, as a real PR body would.
fn full_body() -> String {
    "## What & why\n\nDid a thing.\n\n\
     ## How\n\nRan the gate.\n\n\
     ## Notes\n\nNone.\n"
        .to_string()
}

#[test]
fn a_body_with_every_section_passes_with_exit_0() {
    let dir = skeleton(true);
    assert_eq!(
        run_with_body(dir.path(), &full_body()),
        0,
        "a body carrying every template section must pass"
    );
}

#[test]
fn a_body_with_crlf_line_endings_passes() {
    // GitHub delivers `pull_request.body` with CRLF line endings for bodies authored or
    // edited in the web UI — the common correctly-filled case. Every fixture above uses
    // LF only, so without this the suite would be green against a script that fails on
    // real GitHub input: each heading arrives as `## What & why\r` and a whole-line match
    // never matches. The script strips CRs, so a correctly-filled CRLF body must pass.
    let dir = skeleton(true);
    let crlf_body = full_body().replace('\n', "\r\n");
    assert_eq!(
        run_with_body(dir.path(), &crlf_body),
        0,
        "a correctly-filled body with CRLF line endings must pass"
    );
}

#[test]
fn a_commented_out_section_is_not_required() {
    // The template's `## Commented` heading sits inside an HTML comment block, so it is
    // not a live section; a body carrying only the three real sections must pass. Guards
    // against requiring guidance the rendered template never shows.
    let dir = skeleton(true);
    assert_eq!(
        run_with_body(dir.path(), &full_body()),
        0,
        "a `## ` heading inside a comment block must not be required"
    );
}

#[test]
fn a_body_missing_one_section_fails_with_exit_1_naming_it() {
    // Drop the `## Notes` section: the body is readable, just incomplete, so it drifts
    // (exit 1) rather than being broken, and the script must name the missing section.
    let body = full_body().replace("## Notes\n\nNone.\n", "");
    let dir = skeleton(true);

    let out = Command::new("bash")
        .arg(dir.path().join("scripts/check-pr-body.sh"))
        .arg({
            let p = dir.path().join("body.md");
            fs::write(&p, &body).unwrap();
            p
        })
        .output()
        .expect("run check-pr-body.sh");

    assert_eq!(
        out.status.code(),
        Some(1),
        "a body missing a section must fail with exit 1"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("## Notes"),
        "the missing section must be named in stderr, got: {stderr}"
    );
}

#[test]
fn a_freeform_body_with_no_sections_fails_with_exit_1() {
    // A PR opened with `--body "quick fix"` carries no `## ` headings at all: every
    // required section is missing, which is drift (exit 1), a loud fail — never a pass.
    let dir = skeleton(true);
    assert_eq!(
        run_with_body(dir.path(), "quick fix, trust me\n"),
        1,
        "a freeform body with no sections must fail"
    );
}

#[test]
fn an_empty_body_fails_with_exit_1() {
    // The genuinely-empty case the workflow must survive: an empty file has no headings,
    // so every section is missing — a fail (exit 1), not a crash and not a pass.
    let dir = skeleton(true);
    assert_eq!(
        run_with_body(dir.path(), ""),
        1,
        "an empty body must fail, not pass"
    );
}

#[test]
fn an_empty_body_on_stdin_fails_with_exit_1() {
    // With no path argument the script reads the body from stdin; an empty stdin is still
    // a fail (exit 1), exercising the stdin branch and the empty-body case together.
    let dir = skeleton(true);
    let mut child = Command::new("bash")
        .arg(dir.path().join("scripts/check-pr-body.sh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn check-pr-body.sh");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"")
        .expect("write empty stdin");
    let code = child.wait().expect("wait for script").code().unwrap();
    assert_eq!(code, 1, "an empty body on stdin must fail");
}

#[test]
fn an_absent_template_is_broken_with_exit_2() {
    // No .github/PULL_REQUEST_TEMPLATE.md: the script cannot derive the required sections,
    // so it is broken (exit 2), never a false pass — a check that could not run tells us
    // nothing (golden invariant #1). Distinct from an incomplete body (drift, exit 1).
    let dir = skeleton(false);
    assert_eq!(
        run_with_body(dir.path(), &full_body()),
        2,
        "a missing template must be broken (exit 2), not a pass"
    );
}

#[test]
fn an_absent_body_file_is_broken_with_exit_2() {
    // A path argument that does not exist: we were told to read a body and could not, so
    // it is broken (exit 2), never collapsing into a pass.
    let dir = skeleton(true);
    let code = Command::new("bash")
        .arg(dir.path().join("scripts/check-pr-body.sh"))
        .arg(dir.path().join("does-not-exist.md"))
        .status()
        .expect("run check-pr-body.sh")
        .code()
        .expect("script exited with a code");
    assert_eq!(
        code, 2,
        "a missing body file must be broken (exit 2), not a pass"
    );
}
