//! Path-prefix matching for "claims about these paths" queries.
//!
//! Both front doors answer the same question — which claims are *about* a given
//! repo path — and must answer it identically, so the rule lives here once rather
//! than copied into `claim list` and the MCP `query` tool (the duplication this
//! crate exists to end). v1 does not trace a check's read-set, so the paths a
//! claim is about are approximated by its file location plus its `supports`
//! targets; this is the same best-effort match both consumers use.

use claim_core::SupportTarget;

/// Whether a claim is "about" `prefix`: its file path or any `supports` target's
/// path lies under `prefix`.
///
/// `claim_path` is the claim file's path relative to the store root (e.g.
/// `.claims/src/a.md`); the `.claims/` store prefix is stripped before matching,
/// because a user (or agent) thinks in repo paths (`src/…`), not the store's
/// internal layout. A `supports` decision ref names a repo-relative path in its
/// part before `#` (e.g. `requirements.txt#libfoo`), matched as-is; a bare claim
/// id has no path meaning and simply will not match a real prefix.
#[must_use]
pub fn claim_matches_path(claim_path: &str, supports: &[SupportTarget], prefix: &str) -> bool {
    let stripped = claim_path.strip_prefix(".claims/").unwrap_or(claim_path);
    if under_prefix(stripped, prefix) {
        return true;
    }
    supports.iter().any(|s| {
        let path_part = s.as_str().split('#').next().unwrap_or(s.as_str());
        under_prefix(path_part, prefix)
    })
}

/// Whether `path` is under the directory/prefix `prefix`, matched by path
/// segments.
///
/// Segment-wise, not raw substring, so `src` matches `src/a.md` but not
/// `srcfoo/a.md`: a prefix names a directory boundary, and a substring match would
/// wrongly pull in a sibling whose name merely starts with the same letters. A
/// prefix equal to the path also matches (a claim named directly). An empty prefix
/// matches everything. A leading `./` on either side is normalized away.
#[must_use]
pub fn under_prefix(path: &str, prefix: &str) -> bool {
    let path = path.trim_start_matches("./");
    let prefix = prefix.trim_start_matches("./").trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim_core::parse_claim_file;

    fn supports(targets: &[&str]) -> Vec<SupportTarget> {
        let list = targets
            .iter()
            .map(|t| format!("  - {t}\n"))
            .collect::<String>();
        let text = format!(
            "---\nid: a\nchecks:\n  - kind: cmd\n    run: \"true\"\nsupports:\n{list}---\nS.\n"
        );
        parse_claim_file("a.md", &text).unwrap().supports
    }

    #[test]
    fn under_prefix_matches_on_segment_boundaries() {
        assert!(under_prefix(".claims/src/a.md", ".claims/src"));
        assert!(under_prefix("src/a.md", "src"));
        assert!(under_prefix("src/a.md", "src/"));
        // A prefix that equals the path matches (a claim named directly).
        assert!(under_prefix("src/a.md", "src/a.md"));
        // An empty prefix matches everything.
        assert!(under_prefix("anything", ""));
        // A leading ./ on either side is normalized away.
        assert!(under_prefix("./src/a.md", "src"));
    }

    #[test]
    fn under_prefix_rejects_a_sibling_with_a_shared_name_start() {
        // The bug a raw substring match would introduce: `src` must not match
        // `srcfoo/`.
        assert!(!under_prefix("srcfoo/a.md", "src"));
        assert!(!under_prefix("other/a.md", "src"));
    }

    #[test]
    fn claim_matches_path_strips_the_claims_prefix() {
        // A claim at `.claims/src/a.md` matches the repo path `src`, because the
        // consumer thinks in repo paths, not the store's internal layout.
        assert!(claim_matches_path(".claims/src/a.md", &[], "src"));
        assert!(claim_matches_path(".claims/src/a.md", &[], "src/a.md"));
        assert!(!claim_matches_path(".claims/src/a.md", &[], "other"));
    }

    #[test]
    fn claim_matches_path_matches_a_supports_target_path() {
        // A supports decision ref names a repo path in its part before `#`; a claim
        // under payments/ still matches a query for the file it supports.
        let s = supports(&["requirements.txt#libfoo"]);
        assert!(claim_matches_path(
            ".claims/payments/pin.md",
            &s,
            "requirements.txt"
        ));
        assert!(!claim_matches_path(".claims/payments/pin.md", &s, "src"));
    }
}
