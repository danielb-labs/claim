//! The workspace contract test: the real `claim` binary against a real store,
//! parsed through the hub's wire types.
//!
//! This is the test that keeps the two ends of the wire honest *from day one*
//! (`HUB-IMPLEMENTATION.md` §1.7). The hub's [`wire`](claim_hub_core::wire) types
//! are declared independently of the CLI's `--json` serialize structs, on purpose
//! — so nothing but an executable test can prove they still describe what the CLI
//! actually emits. Here we build a small temp store with a held claim, run the
//! freshly built `claim check --json`, and deserialize its real output into
//! [`CheckReport`]. If the CLI's report shape ever drifts from the hub's reader,
//! this fails in the gate, in this workspace, without waiting for a production
//! rejection.
//!
//! Determinism: no network, no wall-clock assertion, no ordering dependence. The
//! store is a throwaway temp dir with a fixed git identity set locally, and the
//! only facts asserted are structural (the report parsed; the claim's verdict is
//! `held`).

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use claim_core::Verdict;
use claim_hub_core::wire::CheckReport;
use tempfile::TempDir;

/// The absolute path to the freshly built `claim` binary under test.
///
/// This is a *cross-crate* integration test, so `CARGO_BIN_EXE_claim` — which
/// cargo sets only for the `claim` crate's own tests — is unavailable, and the
/// binary is not automatically built as a dependency of this crate's tests. So we
/// build it explicitly (`cargo build -p claim`, once per test binary via the
/// `OnceLock`) and locate it in the target directory alongside this test
/// executable. Building it here is what makes the contract test honest: it always
/// runs against the current CLI source, never a stale artifact, and works under
/// both `cargo test --workspace` and `cargo test -p claim-hub-core`.
fn claim_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(&cargo)
            .args(["build", "-p", "claim", "--bin", "claim"])
            .status()
            .expect("build the claim binary");
        assert!(status.success(), "cargo build -p claim failed");

        // This test executable lives at target/<profile>/deps/<name>; the binary
        // is two levels up, at target/<profile>/claim.
        let mut dir = std::env::current_exe().expect("test exe path");
        dir.pop(); // deps/
        dir.pop(); // <profile>/
        let bin = dir.join(if cfg!(windows) { "claim.exe" } else { "claim" });
        assert!(bin.is_file(), "claim binary not found at {}", bin.display());
        bin
    })
}

/// Run `git` in `dir`, asserting success, with ambient config walled off so the
/// developer's global identity cannot leak into the test.
fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", dir.join("nonexistent-global"))
        .env("GIT_CONFIG_SYSTEM", dir.join("nonexistent-system"))
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A `claim` command rooted at `dir`, with ambient git config walled off, so the
/// only identity the CLI sees is the one this test set locally.
fn claim(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(claim_bin());
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", dir.join("nonexistent-global"))
        .env("GIT_CONFIG_SYSTEM", dir.join("nonexistent-system"));
    cmd
}

/// Build a throwaway store that is a git repo with a deterministic identity, an
/// initialized `.claims/` store, and one held claim (a cmd check over a committed
/// file that satisfies it).
fn held_store() -> TempDir {
    let dir = TempDir::new().expect("temp dir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Contract Test"]);
    git(root, &["config", "user.email", "contract@example.com"]);

    // A committed file the check verifies, so the claim genuinely holds against
    // reality rather than trivially passing.
    std::fs::write(root.join("requirements.txt"), "libfoo==4.2\n").unwrap();
    git(root, &["add", "requirements.txt"]);
    git(root, &["commit", "-q", "-m", "initial"]);

    let init = claim(root).arg("init").output().expect("run claim init");
    assert!(
        init.status.success(),
        "claim init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // A held cmd claim: the grep succeeds against the pinned requirement, so the
    // report is a non-trivial `held`, not an empty run.
    std::fs::create_dir_all(root.join(".claims")).unwrap();
    std::fs::write(
        root.join(".claims/pin.md"),
        "---\nid: pin\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\n---\nWe pin libfoo at 4.2.\n",
    )
    .unwrap();

    dir
}

#[test]
fn real_claim_check_json_parses_into_the_hub_wire_types() {
    let store = held_store();

    let output = claim(store.path())
        .args(["check", "--json"])
        .output()
        .expect("run claim check --json");
    // A held claim exits 0; capture stdout regardless so a non-zero exit surfaces
    // the report that explains it.
    let stdout = String::from_utf8(output.stdout).expect("utf-8 report");
    assert!(
        output.status.success(),
        "claim check exited non-zero; stdout was:\n{stdout}"
    );

    let report = CheckReport::from_json(stdout.as_bytes()).unwrap_or_else(|e| {
        panic!("the hub's wire types must parse the CLI's real --json: {e}\n{stdout}")
    });

    // The report is the one held claim, and its check reads through as `held`
    // (the shared Verdict enum), proving the enum and the object shapes agree end
    // to end.
    assert_eq!(report.status, "ok");
    assert_eq!(report.exit, 0);
    assert_eq!(report.checked, 1);
    assert_eq!(report.ran, 1);
    let claim_result = report
        .claims
        .iter()
        .find(|c| c.id == "pin")
        .expect("the pin claim is in the report");
    assert_eq!(claim_result.checks.len(), 1);
    assert_eq!(claim_result.checks[0].verdict, Verdict::Held);
    // The process end carried its discriminator through the permissive wire type.
    assert_eq!(claim_result.checks[0].end.kind, "exited");
}
