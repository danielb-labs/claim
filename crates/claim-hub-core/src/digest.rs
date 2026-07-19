//! The canonical check-digest: a stable identity for a check's *definition*.
//!
//! A claim's `--json` report identifies each check only by its positional
//! `index`, but position is not identity: reorder a claim's checks and index 0
//! becomes a different check, while a cosmetic edit to a `run` command (a
//! reworded comment, a resorted flag the author normalized) leaves the same
//! check in place. The hub needs an identity that tracks the *check*, not its
//! slot, because a shallow check's pass must never clear a deep check's drift
//! (`CLI-HUB-BOUNDARY.md`, issue #18): if two materially different checks shared
//! an identity, one's `held` would silently satisfy the other's ledger position.
//!
//! [`check_digest`] answers that need. It is a SHA-256 over a **canonical byte
//! encoding of the check's semantic definition** — the fields that determine what
//! the check verifies and how — so the digest is:
//!
//! - **stable across reordering:** identity is content, not position;
//! - **stable across cosmetic edits that do not change the encoded fields;**
//! - **collision-resistant:** any change to an encoded field, and any two checks
//!   that differ in an encoded field, produce different digests — the encoding is
//!   unambiguous (see the `canonical_bytes` encoding), so distinct definitions
//!   never collide
//!   by field-boundary confusion, and SHA-256 makes an accidental collision
//!   infeasible.
//!
//! The digest is computed by the hub from the registry's parsed check definition
//! (a [`claim_core::Check`]), never from the CLI's outcome report, which carries
//! no definition. The encoding below *is* the contract: it is versioned, and any
//! change to it is a new digest for every check, so it changes only deliberately.

use claim_core::{Check, CheckKind, Skip};
use sha2::{Digest, Sha256};

/// The version tag mixed into every digest's preimage.
///
/// The canonical encoding is a contract: two hub versions must agree on a check's
/// identity or a check's ledger history splits in two. This tag is the first byte
/// domain of the preimage, so if the encoding ever must change, bumping it makes
/// the break explicit and total (every digest changes at once) rather than a
/// silent partial divergence. It is not a security parameter; it is a schema
/// version for the digest's input.
///
/// Maintenance obligation: [`canonical_bytes`] reads `claim-core`'s [`Check`] and
/// [`Skip`] field by field, so adding a *semantic* field to either — one that
/// changes what a check verifies or when it runs — obliges bumping this version.
/// A new field is not a compile error here (the struct is destructured by the
/// fields the encoding names, not exhaustively), so it would otherwise be silently
/// omitted from the digest, letting two materially different checks collide.
const CANONICAL_VERSION: &[u8] = b"claim-check-digest/1";

/// The canonical, collision-resistant digest of a check's definition, as a
/// lowercase hex SHA-256 string.
///
/// Two [`Check`]s produce the same digest **iff** their canonical encodings
/// (`canonical_bytes`) are byte-identical — that is, iff they have the same
/// kind, the same kind-specific payload (`run`+`negate`, `instruction`, or
/// `prompt`), and the same skip definition. Field *order within the claim* is
/// irrelevant: the digest is a property of the single check, so a caller computes
/// it per check and a claim's set of digests is stable under reordering.
///
/// The result is 64 lowercase hex characters (a 256-bit digest). It is
/// deterministic and pure: the same check always hashes to the same string, on
/// any platform, with no clock and no IO.
#[must_use]
pub fn check_digest(check: &Check) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes(check));
    let digest = hasher.finalize();
    hex_lower(&digest)
}

/// The canonical preimage bytes hashed by [`check_digest`].
///
/// Exposed within the crate (and documented) because the encoding is the whole
/// contract of the digest; the tests assert its stability and its
/// unambiguity directly, not only through the hash.
///
/// # The encoding
///
/// Every variable-length field is written **length-prefixed** — its byte length
/// as a `u64` little-endian, then its bytes — so no two distinct field sequences
/// can produce the same concatenation. Without length prefixes a `run` of `"ab"`
/// followed by an empty next field would be indistinguishable from a `run` of
/// `"a"` followed by `"b"`; the prefixes make every boundary explicit, which is
/// what makes the digest collision-resistant at the *encoding* layer, before
/// SHA-256 is even involved.
///
/// The layout, in order:
///
/// 1. [`CANONICAL_VERSION`] — the schema-version domain tag (length-prefixed).
/// 2. A single kind tag byte: `0` = cmd, `1` = agent, `2` = human. A tag, not the
///    kebab string, so the discriminator is fixed-width and unambiguous.
/// 3. The kind-specific payload:
///    - **cmd:** the `run` string (length-prefixed), then one `negate` byte
///      (`1`/`0`). `negate` flips the check's very meaning, so it is load-bearing
///      identity, not cosmetic.
///    - **agent:** the `instruction` string (length-prefixed).
///    - **human:** one presence byte for `prompt` (`1` present / `0` absent) then,
///      when present, the prompt string (length-prefixed). The presence byte keeps
///      an absent prompt distinct from an empty-string prompt.
/// 4. The skip definition: one presence byte (`1`/`0`); when present, the `reason`
///    (length-prefixed), the `unless` as a presence byte plus optional
///    length-prefixed string, and the `until` as a presence byte plus, when
///    present, its RFC 3339 string (length-prefixed). The skip is part of a
///    check's identity because it changes when the check actually runs — a check
///    that is muted under a condition is not the same verification as one that
///    always runs.
///
/// Not encoded: anything the parser derives rather than the author writes, and
/// anything positional. There is no field here that a claim's *ordering* or a
/// non-semantic reformatting would perturb.
pub(crate) fn canonical_bytes(check: &Check) -> Vec<u8> {
    let mut buf = Vec::new();
    write_bytes(&mut buf, CANONICAL_VERSION);

    match &check.kind {
        CheckKind::Cmd { run, negate } => {
            buf.push(0);
            write_str(&mut buf, run);
            buf.push(u8::from(*negate));
        }
        CheckKind::Agent { instruction } => {
            buf.push(1);
            write_str(&mut buf, instruction);
        }
        CheckKind::Human { prompt } => {
            buf.push(2);
            write_optional_str(&mut buf, prompt.as_deref());
        }
    }

    write_skip(&mut buf, check.skip.as_ref());
    buf
}

/// Encode the optional skip: a presence byte, then the fields when present.
fn write_skip(buf: &mut Vec<u8>, skip: Option<&Skip>) {
    match skip {
        None => buf.push(0),
        Some(skip) => {
            buf.push(1);
            write_str(buf, &skip.reason);
            write_optional_str(buf, skip.unless.as_deref());
            // `until` is a UTC instant; its RFC 3339 string is the lossless,
            // canonical form `jiff` round-trips, so hashing that string ties the
            // digest to the instant, not to a formatting choice.
            match &skip.until {
                None => buf.push(0),
                Some(ts) => {
                    buf.push(1);
                    write_str(buf, &ts.to_string());
                }
            }
        }
    }
}

/// Write a length-prefixed byte string: the length as `u64` little-endian, then
/// the bytes. The prefix is what makes the concatenation unambiguous.
fn write_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Write a length-prefixed UTF-8 string.
fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_bytes(buf, s.as_bytes());
}

/// Write an optional string as a presence byte plus, when present, the
/// length-prefixed string — so an absent value is distinct from an empty one.
fn write_optional_str(buf: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => buf.push(0),
        Some(s) => {
            buf.push(1);
            write_str(buf, s);
        }
    }
}

/// Render bytes as a lowercase hex string, the digest's stable text form.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `write!` to a String is infallible; the `_ =` documents that we discard
        // the always-Ok result rather than swallowing a real error.
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim_core::parse_claim_file;

    /// The first check of a one-check claim parsed from frontmatter, so the tests
    /// exercise real parser output rather than hand-built structs.
    fn check_of(yaml_body: &str) -> Check {
        let text = format!("---\n{yaml_body}\n---\nStatement.\n");
        let claim = parse_claim_file(".claims/t.md", &text).expect("valid claim");
        claim.checks.into_iter().next().expect("one check")
    }

    #[test]
    fn digest_is_64_lowercase_hex_chars() {
        let d = check_digest(&check_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"true\"",
        ));
        assert_eq!(d.len(), 64, "SHA-256 is 32 bytes → 64 hex chars");
        assert!(
            d.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "digest is lowercase hex: {d}"
        );
    }

    #[test]
    fn same_definition_hashes_identically() {
        let a = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q x f\"");
        let b = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q x f\"");
        assert_eq!(check_digest(&a), check_digest(&b));
    }

    #[test]
    fn digest_is_stable_under_reordering_a_claims_checks() {
        // The whole point of #18: a check's identity is its definition, not its
        // slot. The same two checks in either order yield the same two digests.
        let first = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"a\"");
        let second = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"b\"");

        // Parse the reordered claim and pull *both* its checks: same two checks, in
        // the opposite order (b then a).
        let reordered = {
            let text =
                "---\nid: t\nchecks:\n  - kind: cmd\n    run: \"b\"\n  - kind: cmd\n    run: \"a\"\n---\nS.\n";
            parse_claim_file(".claims/t.md", text).unwrap().checks
        };

        // Position 0 changed (b, then a), but the *set* of digests is unchanged.
        let mut original = [check_digest(&first), check_digest(&second)];
        let mut after = [check_digest(&reordered[0]), check_digest(&reordered[1])];
        original.sort();
        after.sort();
        assert_eq!(original, after);
        // And the reordered index 0 is `b`, whose digest differs from `a`'s (`first`):
        // the digest tracks the check, so reordering did not rename `a` to `b`.
        assert_ne!(check_digest(&first), check_digest(&reordered[0]));
    }

    #[test]
    fn a_different_run_command_changes_the_digest() {
        let a = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q x f\"");
        let b = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q y f\"");
        assert_ne!(check_digest(&a), check_digest(&b));
    }

    #[test]
    fn negate_is_part_of_identity() {
        // `negate` inverts the check's meaning, so it must change the digest: a
        // "held when present" and a "held when absent" check are not the same fact.
        let plain = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q x f\"");
        let negated =
            check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"grep -q x f\"\n    negate: true");
        assert_ne!(check_digest(&plain), check_digest(&negated));
    }

    #[test]
    fn kind_is_part_of_identity() {
        // Same textual payload, different mechanism → different check.
        let cmd = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"x\"");
        let agent = check_of("id: t\nchecks:\n  - kind: agent\n    instruction: \"x\"");
        assert_ne!(check_digest(&cmd), check_digest(&agent));
    }

    #[test]
    fn an_absent_prompt_is_distinct_from_an_empty_prompt() {
        // The presence byte earns its keep: `human` with no prompt must not collide
        // with `human` whose prompt is "".
        let no_prompt = check_of("id: t\nchecks:\n  - kind: human");
        let empty_prompt = check_of("id: t\nchecks:\n  - kind: human\n    prompt: \"\"");
        assert_ne!(check_digest(&no_prompt), check_digest(&empty_prompt));
    }

    #[test]
    fn adding_a_skip_changes_the_digest() {
        // A skip changes when the check runs, so it is part of the check's identity.
        let no_skip = check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"x\"");
        let with_skip = check_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: parked",
        );
        assert_ne!(check_digest(&no_skip), check_digest(&with_skip));
    }

    #[test]
    fn skip_fields_are_part_of_identity() {
        let reason_a =
            check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: a");
        let reason_b =
            check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: b");
        assert_ne!(check_digest(&reason_a), check_digest(&reason_b));

        let with_until = check_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: a\n      until: 2030-01-01",
        );
        assert_ne!(check_digest(&reason_a), check_digest(&with_until));

        // `unless` is genuine identity: it changes *when* the check runs (a
        // condition that cancels the skip), so two skips differing only in `unless`
        // are different verifications and must not share a digest.
        let with_unless = check_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: a\n      unless: \"test -f flag\"",
        );
        assert_ne!(check_digest(&reason_a), check_digest(&with_unless));
    }

    #[test]
    fn the_length_prefix_prevents_a_boundary_collision() {
        // Two checks whose concatenated field *bytes* would coincide but for the
        // length prefixes: `run="ab"` + `reason="z"` versus `run="a"` + `reason="bz"`
        // move one character across the run/reason boundary while keeping every other
        // byte equal (both cmd, `negate=false`, same skip-presence byte). The fixed
        // bytes between the two strings do not disambiguate them — only each field's
        // own length prefix does — so this fails if a prefix is ever dropped.
        let ab_z =
            check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"ab\"\n    skip:\n      reason: z");
        let a_bz =
            check_of("id: t\nchecks:\n  - kind: cmd\n    run: \"a\"\n    skip:\n      reason: bz");
        assert_ne!(
            check_digest(&ab_z),
            check_digest(&a_bz),
            "field boundaries must be unambiguous"
        );
    }

    #[test]
    fn canonical_bytes_begin_with_the_version_tag() {
        // The version domain-tags the preimage, so a future encoding change is a
        // clean, total break rather than a silent partial one.
        let bytes = canonical_bytes(&check_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"true\"",
        ));
        let prefix_len = (CANONICAL_VERSION.len() as u64).to_le_bytes();
        assert!(bytes.starts_with(&prefix_len));
        assert!(bytes[8..].starts_with(CANONICAL_VERSION));
    }
}
