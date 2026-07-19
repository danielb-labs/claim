//! Hub-minted scoped read tokens: the IdP-less authentication floor.
//!
//! A self-hoster with no external identity provider still must not serve open reads by
//! accident (HUB-IMPLEMENTATION.md §4.5, decision 5 — secure by default). This module is
//! that floor: the hub mints a high-entropy random bearer token, prints it **once**, and
//! stores only its **hash** in config. A client presents the raw token in
//! `Authorization: Bearer <token>`; the hub hashes what it receives and compares against
//! the configured hashes in constant time. A leaked config (or a leaked backup) yields
//! only hashes, from which the raw tokens cannot be recovered — the property the item
//! calls "hashed at rest."
//!
//! Why a plain SHA-256, not a password KDF (bcrypt/argon2): a minted token is 256 bits of
//! CSPRNG output, not a low-entropy human password. There is no dictionary to attack and
//! no feasible brute-force over a 2^256 space, so the slow, salted KDFs that exist to
//! stretch weak passwords buy nothing here and would add a heavy dependency. A single
//! unsalted cryptographic hash is exactly right for a high-entropy secret, and it is the
//! same RustCrypto SHA-256 the check-digest already uses (`claim-hub-core`).
//!
//! Why constant-time comparison: comparing the presented token's hash against a stored
//! hash byte-by-byte with `==` would short-circuit on the first differing byte, leaking —
//! through response timing — how many leading bytes matched, which an attacker grinds into
//! the secret. [`ConstantTimeEq`] compares the whole fixed-width hash regardless of where
//! it differs, so the compare time reveals nothing about the secret.
//!
//! What is deliberately absent: any log or error that echoes a raw token. A token appears
//! in exactly two places — the operator's terminal at mint time, and an inbound
//! `Authorization` header — and is never written to a log line, a config file, or an error
//! body (a security requirement of the item).

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::scope::{GrantedScopes, Scope};

/// The prefix every minted token carries, so a leaked secret is recognizable as a hub
/// credential (for revocation and secret-scanning) and a pasted config value is obviously
/// a token, not a hash.
const TOKEN_PREFIX: &str = "claimhub_";

/// The number of random bytes in a minted token before hex encoding. 32 bytes = 256 bits
/// of entropy — past any brute-force, so the token needs no stretching.
const TOKEN_ENTROPY_BYTES: usize = 32;

/// The `sha256:` marker a stored token hash carries, so the config value's algorithm is
/// explicit and a future hash migration is a new marker, not a silent reinterpretation of
/// the same bytes.
const HASH_ALGORITHM_PREFIX: &str = "sha256:";

/// A configured scoped read token: the scopes it grants and the hash of its secret.
///
/// This is what lives in the hub's config (`[[read_auth.tokens]]`), deserialized from
/// TOML. It holds **only the hash** of the token, never the token itself — the raw token
/// exists only in the operator's terminal at mint time and in an inbound request header.
/// A configured token with no scopes grants nothing (every route requires at least one
/// scope), so an empty `scopes` is a misconfiguration the config validation refuses
/// loudly rather than a token that silently authenticates but authorizes nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedToken {
    /// A label naming the token's purpose (e.g. `"ci-dashboard"`), for the operator's own
    /// reference. Not a secret and not load-bearing for auth — it is never compared and
    /// never trusted; it exists so a config file of several tokens is legible and a token
    /// can be revoked by deleting the right entry.
    #[serde(default)]
    pub name: String,
    /// The scopes this token grants. A request to a route is authorized only if the
    /// token's scopes include the route's required scope.
    pub scopes: Vec<Scope>,
    /// The `sha256:<hex>` hash of the token's secret. The raw token is never stored; this
    /// is compared, in constant time, against the hash of a presented token.
    pub hash: String,
}

impl ScopedToken {
    /// Whether `presented` is this token, by constant-time hash comparison.
    ///
    /// Hashes the presented raw token and compares the digest against this token's stored
    /// hash with [`ConstantTimeEq`], so the compare time reveals nothing about the secret
    /// (no early-out on the first differing byte). A stored hash that is not a well-formed
    /// `sha256:<64 hex>` never matches — a malformed config entry authenticates nothing,
    /// it does not accidentally match a short presented token.
    #[must_use]
    pub fn matches(&self, presented: &str) -> bool {
        let Some(stored) = self.stored_digest() else {
            return false;
        };
        let presented_digest = sha256(presented.as_bytes());
        presented_digest.ct_eq(&stored).into()
    }

    /// The scopes this token grants.
    #[must_use]
    pub fn granted_scopes(&self) -> GrantedScopes {
        GrantedScopes::new(self.scopes.iter().copied())
    }

    /// The 32 raw bytes of this token's stored `sha256:<hex>` hash, or `None` if the
    /// config value is not a well-formed SHA-256 hex hash.
    ///
    /// Decoding failure is not an error the caller handles — a malformed stored hash
    /// simply never matches any token — so it returns `None`, keeping [`matches`] a total
    /// function that cannot be tricked into a match by a bad config value.
    fn stored_digest(&self) -> Option<[u8; 32]> {
        let hex = self.hash.strip_prefix(HASH_ALGORITHM_PREFIX)?;
        decode_hex_32(hex)
    }

    /// Whether this configured token is well-formed: it has at least one scope and a
    /// parseable `sha256:<64 hex>` hash.
    ///
    /// The config validation calls this so a token that could never authenticate (an empty
    /// scope set) or never match (a malformed hash) is a loud boot error, not a silent
    /// dead entry — invariant #6: a misconfiguration is surfaced, never swallowed.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.scopes.is_empty() && self.stored_digest().is_some()
    }
}

/// A freshly minted token: the raw secret to hand out **once**, and the hash to store.
///
/// Returned by [`mint`]. `Debug` is implemented to redact both fields, so a `{:?}` on a
/// `Minted` cannot put a credential in a log; a caller reads [`raw`](Minted::raw)
/// explicitly to print the secret to the operator's terminal, and
/// [`config_hash`](Minted::config_hash) to print the value for the config file.
pub struct Minted {
    raw: String,
    config_hash: String,
}

impl Minted {
    /// The raw token secret — printed once to the operator, then never stored by the hub.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The `sha256:<hex>` value to paste into a `[[read_auth.tokens]]` entry's `hash`.
    #[must_use]
    pub fn config_hash(&self) -> &str {
        &self.config_hash
    }
}

/// Mint a new scoped token: 256 bits of CSPRNG entropy, hex-encoded with the hub prefix,
/// paired with its `sha256:` hash for config.
///
/// The randomness comes from the OS CSPRNG via `getrandom` — the minimal, audited
/// primitive over the platform entropy source, not a userspace PRNG that could be seeded
/// weakly. A failure to read entropy is the one error path: the hub refuses to mint a
/// guessable token rather than fall back to a weaker source (invariant #6 — fail loud, not
/// toward a weak secret).
///
/// # Errors
///
/// Returns an error string if the OS entropy source cannot be read — minting fails loudly
/// rather than producing a low-entropy token.
pub fn mint() -> Result<Minted, String> {
    let mut bytes = [0u8; TOKEN_ENTROPY_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| format!("could not read {TOKEN_ENTROPY_BYTES} bytes of OS entropy: {e}"))?;
    let raw = format!("{TOKEN_PREFIX}{}", encode_hex(&bytes));
    let config_hash = hash_for_config(&raw);
    Ok(Minted { raw, config_hash })
}

/// The `sha256:<hex>` config value for a raw token — the stored form of its secret.
///
/// Exposed so the `mint-token` subcommand and its tests can derive the stored hash of any
/// token deterministically, and so a test can prove the stored form is a hash of, and not
/// equal to, the raw token.
#[must_use]
pub fn hash_for_config(raw: &str) -> String {
    format!(
        "{HASH_ALGORITHM_PREFIX}{}",
        encode_hex(&sha256(raw.as_bytes()))
    )
}

/// SHA-256 of `input`, the 32-byte digest.
fn sha256(input: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().into()
}

/// Lower-case hex encoding of `bytes`, without a crate dependency.
///
/// Hand-written and covered by tests rather than pulling `hex`: it is a few lines, the
/// encoding is trivial and stable, and every avoided dependency is avoided attack surface
/// (CLAUDE.md's approved-deps discipline).
fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode exactly 64 lower- or upper-case hex characters into 32 bytes, or `None` if the
/// input is the wrong length or holds a non-hex character.
///
/// Strict on length (a stored hash is always 32 bytes) and total (any malformed input is
/// `None`, never a partial or panicking decode), so a bad config hash yields no digest and
/// therefore matches no token.
fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

/// The numeric value of one hex digit, or `None` if `c` is not a hex character.
fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for Minted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The raw secret and its hash are both withheld: a `{:?}` on a `Minted` must not
        // put a credential in a log.
        f.debug_struct("Minted")
            .field("raw", &"<redacted>")
            .field("config_hash", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_carries_the_prefix_and_full_entropy() {
        let minted = mint().unwrap();
        assert!(
            minted.raw().starts_with(TOKEN_PREFIX),
            "minted token is recognizable: {}",
            minted.raw()
        );
        // prefix + 32 bytes as 64 hex chars.
        assert_eq!(minted.raw().len(), TOKEN_PREFIX.len() + 64);
    }

    #[test]
    fn the_stored_form_is_a_hash_never_the_raw_token() {
        // The "hashed at rest" property: the config value is a sha256 of the token, and it
        // is not the token itself — a leaked config yields only the hash.
        let minted = mint().unwrap();
        assert!(minted.config_hash().starts_with("sha256:"));
        assert_ne!(minted.config_hash(), minted.raw());
        assert!(
            !minted.config_hash().contains(minted.raw()),
            "the raw token must not appear inside its stored hash"
        );
        // And the stored hash is exactly the hash of the raw token, recomputable.
        assert_eq!(minted.config_hash(), hash_for_config(minted.raw()));
    }

    #[test]
    fn two_mints_are_different() {
        // Distinct CSPRNG draws: two tokens minted back to back must not collide.
        assert_ne!(mint().unwrap().raw(), mint().unwrap().raw());
    }

    #[test]
    fn a_token_matches_its_own_hash_and_not_another() {
        let minted = mint().unwrap();
        let token = ScopedToken {
            name: "t".into(),
            scopes: vec![Scope::Read],
            hash: minted.config_hash().to_owned(),
        };
        assert!(token.matches(minted.raw()), "a token matches its own hash");
        assert!(
            !token.matches("claimhub_deadbeef"),
            "a different token does not match"
        );
        // A truncated prefix of the right token must not match either.
        let truncated = &minted.raw()[..minted.raw().len() - 1];
        assert!(
            !token.matches(truncated),
            "a truncated token does not match"
        );
    }

    #[test]
    fn a_malformed_stored_hash_matches_nothing() {
        // A config hash that is not a well-formed sha256:<64 hex> never matches — a bad
        // entry authenticates nothing rather than accidentally matching a short token.
        for bad in ["", "sha256:", "sha256:zz", "notsha256:0000", "sha256:00"] {
            let token = ScopedToken {
                name: "bad".into(),
                scopes: vec![Scope::Read],
                hash: bad.to_owned(),
            };
            assert!(
                !token.matches("claimhub_anything"),
                "bad hash {bad:?} matched"
            );
            assert!(!token.is_well_formed(), "bad hash {bad:?} is well-formed");
        }
    }

    #[test]
    fn a_token_with_no_scopes_is_not_well_formed() {
        // A scopeless token could authenticate but authorize nothing; the config
        // validation refuses it loudly, so it is flagged not-well-formed here.
        let minted = mint().unwrap();
        let token = ScopedToken {
            name: "scopeless".into(),
            scopes: vec![],
            hash: minted.config_hash().to_owned(),
        };
        assert!(!token.is_well_formed());
    }

    #[test]
    fn hex_round_trips() {
        let bytes: [u8; 32] = [
            0x00, 0x01, 0x02, 0x0f, 0x10, 0xff, 0xa5, 0x5a, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
            0xdd, 0xee, 0xff, 0x00,
        ];
        let hex = encode_hex(&bytes);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_hex_32(&hex), Some(bytes));
        // Upper-case hex decodes identically.
        assert_eq!(decode_hex_32(&hex.to_uppercase()), Some(bytes));
    }

    #[test]
    fn decode_hex_rejects_wrong_length_and_non_hex() {
        assert_eq!(decode_hex_32("00"), None);
        assert_eq!(decode_hex_32(&"0".repeat(63)), None);
        assert_eq!(decode_hex_32(&"0".repeat(65)), None);
        assert_eq!(decode_hex_32(&format!("g{}", "0".repeat(63))), None);
    }
}
