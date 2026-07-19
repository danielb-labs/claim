//! The router's storage-side helpers: owners from the mirror, and the fired set.
//!
//! Two impure jobs the pure core ([`claim_hub_core::nag`]) cannot do: reading CODEOWNERS
//! out of the synced git mirror to resolve an owner *at fire time* (invariant #3 —
//! provenance derived from git, never a stored owner field), and reconstructing the
//! router's fired set from the ledger's `nag` events so "already nagged" is derived, not
//! stored (invariant #3).
//!
//! ## Owners from CODEOWNERS at fire time
//!
//! The registry sync already keeps a bare mirror per store; owner resolution reads
//! CODEOWNERS out of that local mirror with a single `git show <commit>:<path>`, so no
//! forge call happens when a nag fires. The matcher mirrors the CI glue's
//! (`ci/render.mjs`'s `ownersFor`) semantics exactly — **last matching pattern wins**, over
//! the subset a claims store needs (a catch-all `*`, directory prefixes with GitHub's
//! anchoring rules, and basename globs) — so a hub-side owner and a CI-glue-side owner for
//! the same claim never disagree. An unresolvable owner is **not** silently dropped: the
//! router turns an empty owner list into a dead-letter queue item (invariant #6).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use claim_hub_core::{fire_key_of, FireKey};

use crate::error::{Result, StoreError};
use crate::ledger::Ledger;
use crate::sync::mirror_path;

/// The CODEOWNERS file locations checked, in GitHub's own precedence order.
///
/// GitHub reads the **first** of these that exists; the hub does the same, so a store's
/// owners resolve from the same file GitHub (and the CI glue) would use. A store under
/// `.claims/` most often keeps its CODEOWNERS at `.github/CODEOWNERS`.
const CODEOWNERS_PATHS: &[&str] = &["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"];

/// Resolve the owners of a claim file at a commit, from CODEOWNERS in the store's mirror.
///
/// Reads the first existing CODEOWNERS (`CODEOWNERS`, `.github/CODEOWNERS`, then
/// `docs/CODEOWNERS`, GitHub's precedence order) from the bare mirror at
/// `commit` with `git show`, then applies the last-matching-pattern-wins matcher to
/// `claim_file` (the claim's path relative to the store root, e.g.
/// `.claims/payments/pin.md`). Returns the owners of the last matching pattern, or an empty
/// vector when no CODEOWNERS exists or no pattern matches — an **unowned** claim the router
/// routes to the dead-letter queue rather than dropping (invariant #6).
///
/// Owner resolution is a local git read — the mirror is already synced — so a fire never
/// waits on the forge (invariant #3: provenance from git, no stored owner field). A commit
/// that cannot be read (an absent mirror, a pruned sha) yields no owners: the router then
/// dead-letters, loud, rather than failing the whole tick over one claim.
///
/// # Errors
///
/// Does not error on a missing CODEOWNERS or an unreadable commit — those are the
/// legitimate "no owner" case, handled as a dead-letter. It returns a [`StoreError`] only
/// if the `git` binary cannot be spawned at all ([`StoreError::GitSpawn`]), which is a
/// deployment fault the caller reports.
pub fn resolve_owners(
    mirror_root: &Path,
    store_id: &str,
    commit: &str,
    claim_file: &str,
) -> Result<Vec<String>> {
    let mirror = mirror_path(mirror_root, store_id);
    let text = read_codeowners(&mirror, store_id, commit)?;
    Ok(text
        .map(|text| owners_for(claim_file, &text))
        .unwrap_or_default())
}

/// Read the first existing CODEOWNERS file from the mirror at `commit`, or `None`.
///
/// Tries each of [`CODEOWNERS_PATHS`] with `git -C <mirror> show <commit>:<path>`; the
/// first that succeeds wins. A `git show` that fails (the path does not exist at that
/// commit, or the commit is unresolvable) is not an error here — it is the "no CODEOWNERS"
/// case — so a store with no CODEOWNERS resolves to no owners and the router dead-letters.
/// Only a failure to *spawn* git is surfaced as an error.
fn read_codeowners(mirror: &Path, store_id: &str, commit: &str) -> Result<Option<String>> {
    for path in CODEOWNERS_PATHS {
        // `<commit>:<path>` is a single git object spec; the commit is a resolved sha from
        // the ledger and the path is a fixed constant, so neither is option-like. There is
        // no `--end-of-options` on `show`; the inputs are internally trusted.
        let spec = format!("{commit}:{path}");
        let output = Command::new("git")
            .arg("-C")
            .arg(mirror)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "false")
            .env("SSH_ASKPASS", "false")
            .args(["show", &spec])
            .output()
            .map_err(|source| StoreError::GitSpawn {
                store: store_id.to_owned(),
                args: format!("show {spec}"),
                source,
            })?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
    }
    Ok(None)
}

/// Match a repo-relative `path` against CODEOWNERS and return the owners of the **last**
/// matching pattern, or an empty vector if none matches.
///
/// The semantics mirror the CI glue's `ownersFor` (`ci/render.mjs`) exactly, so a hub-side
/// owner and a glue-side owner never disagree (GitHub's rule: later patterns win over
/// earlier ones). Comments (`#`) and blank lines are skipped. Each line is a pattern
/// followed by one or more owner tokens.
#[must_use]
pub fn owners_for(path: &str, codeowners: &str) -> Vec<String> {
    let mut owners: Vec<String> = Vec::new();
    for raw in codeowners.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(pattern) = parts.next() else {
            continue;
        };
        if matches_pattern(path, pattern) {
            owners = parts.map(str::to_owned).collect();
        }
    }
    owners
}

/// Whether a CODEOWNERS `pattern` matches a repo-relative `path`.
///
/// A subset of GitHub's globbing sufficient for a claims store, matching `ci/render.mjs`:
///
/// - `*` matches everything.
/// - A trailing-slash directory pattern matches a directory prefix. GitHub anchors it to
///   the repo root only when it has a leading slash or an interior slash (`/payments/`,
///   `.claims/payments/`); a bare `payments/` matches a `payments` directory at any depth.
/// - Otherwise the pattern is a glob where `*` matches any run of non-`/` characters,
///   anchored to root when it has a leading or interior slash, else matched against the
///   basename anywhere in the tree (`*.md` matches `docs/x.md`).
fn matches_pattern(path: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let has_interior_slash = pattern.trim_end_matches('/').contains('/');
    let anchored = pattern.starts_with('/');

    if let Some(dir) = pattern.strip_suffix('/') {
        let dir = dir.trim_start_matches('/');
        if anchored || has_interior_slash {
            return path == dir || path.starts_with(&format!("{dir}/"));
        }
        // Unanchored bare directory: a path segment equal to `dir` followed by more path.
        return path == dir
            || path.starts_with(&format!("{dir}/"))
            || path.contains(&format!("/{dir}/"));
    }

    let glob = pattern.strip_prefix('/').unwrap_or(pattern);
    glob_matches(path, glob, anchored || has_interior_slash)
}

/// Match a `path` against a CODEOWNERS `glob` where `*` matches any run of non-`/`
/// characters. When `anchored`, the glob must match the whole path; otherwise it matches
/// the path's final segment anywhere in the tree (`*.md` → `docs/x.md`).
///
/// A hand-rolled matcher rather than a regex dependency: the glob vocabulary is one
/// metacharacter (`*` = `[^/]*`), so a small segment-walk is clearer and dependency-free.
fn glob_matches(path: &str, glob: &str, anchored: bool) -> bool {
    if anchored {
        return glob_segment_matches(path, glob);
    }
    // Unanchored: the glob matches the basename, i.e. the last `/`-segment, or the whole
    // path when the glob itself has no `/`. Mirroring `(^|/)<glob>$`, we test the tail.
    if glob_segment_matches(path, glob) {
        return true;
    }
    path.rfind('/')
        .map(|i| glob_segment_matches(&path[i + 1..], glob))
        .unwrap_or(false)
}

/// Whether `glob` (with `*` = any run of non-`/`) matches the whole of `text`.
///
/// A standard two-pointer wildcard match restricted so `*` never crosses a `/` — the
/// CODEOWNERS convention that `*.md` matches a file in a directory, not a path spanning
/// directories.
fn glob_segment_matches(text: &str, glob: &str) -> bool {
    // Split on `*`; each literal chunk must appear in order, the first anchored at the
    // start and the last at the end, and no chunk may straddle a `/`.
    let t: Vec<char> = text.chars().collect();
    let g: Vec<char> = glob.chars().collect();
    wildcard(&t, &g)
}

/// Two-pointer `*`-glob match where `*` matches any run of non-`/` characters.
fn wildcard(text: &[char], glob: &[char]) -> bool {
    let (mut ti, mut gi) = (0usize, 0usize);
    let (mut star_g, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < text.len() {
        if gi < glob.len() && (glob[gi] == text[ti]) {
            ti += 1;
            gi += 1;
        } else if gi < glob.len() && glob[gi] == '*' {
            star_g = Some(gi);
            star_t = ti;
            gi += 1;
        } else if let Some(sg) = star_g {
            // Backtrack: let the last `*` consume one more char, but never a `/`.
            if text[star_t] == '/' {
                return false;
            }
            gi = sg + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while gi < glob.len() && glob[gi] == '*' {
        gi += 1;
    }
    gi == glob.len()
}

/// The set of fire keys the ledger's `nag` events have already recorded.
///
/// This is the router's memory, **derived from the ledger, not stored** (invariant #3): a
/// transition whose fire key is in this set has already fired; one absent is new. Rebuilt
/// by scanning the ledger for `nag` events and reading each one's fire key — so a restart
/// that re-scans the ledger reaches the identical set and never re-fires. A nag event
/// missing its fire key is ill-formed telemetry and is skipped (it cannot suppress a real
/// fire — a silent miss invariant #6 forbids).
///
/// # Errors
///
/// Propagates a store read fault from the ledger scan.
pub async fn fired_keys<L: Ledger>(ledger: &L) -> Result<HashSet<FireKey>> {
    let stored = ledger.scan_from(crate::Position(0)).await?;
    Ok(stored
        .iter()
        .filter_map(|s| fire_key_of(&s.event))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catch_all_matches_everything() {
        assert!(matches_pattern(".claims/x.md", "*"));
        assert!(matches_pattern("anything/at/all", "*"));
    }

    #[test]
    fn last_matching_pattern_wins() {
        let codeowners = "*                       @acme/eng\n\
                          .claims/payments/       @acme/payments\n";
        assert_eq!(
            owners_for(".claims/payments/pin.md", codeowners),
            vec!["@acme/payments"]
        );
        // A file the specific rule does not cover falls to the catch-all.
        assert_eq!(
            owners_for(".claims/other.md", codeowners),
            vec!["@acme/eng"]
        );
    }

    #[test]
    fn an_anchored_directory_matches_at_root_only() {
        let codeowners = "/payments/  @acme/payments\n";
        assert_eq!(
            owners_for("payments/pin.md", codeowners),
            vec!["@acme/payments"]
        );
        // Not at root: an anchored rule does not match a nested `payments/`.
        assert!(owners_for("apps/payments/pin.md", codeowners).is_empty());
    }

    #[test]
    fn a_bare_directory_matches_at_any_depth() {
        // The natural `payments/` rule must route a store under `.claims/` — the earlier
        // CI-glue bug was anchoring this to root, which misrouted every nested claim.
        let codeowners = "payments/  @acme/payments\n";
        assert_eq!(
            owners_for(".claims/payments/pin.md", codeowners),
            vec!["@acme/payments"]
        );
        assert_eq!(
            owners_for("payments/pin.md", codeowners),
            vec!["@acme/payments"]
        );
    }

    #[test]
    fn a_basename_glob_matches_anywhere() {
        let codeowners = "*.md  @acme/docs\n";
        assert_eq!(owners_for("docs/x.md", codeowners), vec!["@acme/docs"]);
        assert_eq!(owners_for("x.md", codeowners), vec!["@acme/docs"]);
        // A star never crosses a slash within a segment glob.
        assert!(owners_for("x.md.txt", codeowners).is_empty());
    }

    #[test]
    fn an_interior_slash_glob_is_anchored() {
        let codeowners = ".claims/*.md  @acme/claims\n";
        assert_eq!(
            owners_for(".claims/pin.md", codeowners),
            vec!["@acme/claims"]
        );
        // `*` does not cross a slash, so a nested file under `.claims/` is not matched.
        assert!(owners_for(".claims/sub/pin.md", codeowners).is_empty());
    }

    #[test]
    fn multiple_owners_on_one_line_are_all_returned() {
        let codeowners = "*  @acme/eng @octocat\n";
        assert_eq!(owners_for("x", codeowners), vec!["@acme/eng", "@octocat"]);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let codeowners = "# a comment\n\n*  @acme/eng\n";
        assert_eq!(owners_for("x", codeowners), vec!["@acme/eng"]);
    }

    #[test]
    fn no_codeowners_text_yields_no_owners() {
        assert!(owners_for("x", "").is_empty());
    }
}
