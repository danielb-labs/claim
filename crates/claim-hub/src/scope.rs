//! Read/act scopes: the "read broadly, act narrowly" authorization model.
//!
//! Authentication answers *who*; a scope answers *what they may do*. The hub's v1 read
//! surfaces all require the [`Scope::Read`] scope; a future act surface (ack, audit-request
//! — HUB-IMPLEMENTATION.md §4.4, deferred) will require [`Scope::Act`]. v1 ships the scope
//! *model* ahead of any act endpoint (the item's requirement 4): scopes are defined,
//! enforced on every read route, and a scope violation is a `403` — so when an act endpoint
//! lands it slots into the same [`RequiredScope`] check with no new machinery.
//!
//! The split is load-bearing: a token minted or issued for reading must not silently gain
//! the power to act. A route declares the scope it needs ([`RequiredScope`]); a principal
//! carries the scopes it was granted ([`GrantedScopes`]); the layer authorizes only when
//! the grant contains the requirement. Broadening (a `read` grant covering every read
//! route) is deliberate; escalation (a `read` grant reaching an `act` route) is impossible
//! because `read` is not `act`.

use serde::Deserialize;

/// A permission a principal may hold and a route may require.
///
/// Two variants in v1. `Read` covers every read surface (the API, the UI/twins, the MCP
/// tools); `Act` covers the write/act surfaces a later item adds. They are distinct values,
/// so a `read`-only principal can never reach an `act` route — the "read broadly, act
/// narrowly" rule made a type, not a convention.
///
/// The wire spelling (in a token's `scope` claim or a config `scopes` entry) is the
/// lower-case word, matching OAuth 2.1's space-delimited `scope` string convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Read the derived model: claims, sets, dossiers, the feed, the UI, and MCP tools.
    Read,
    /// Perform an act (ack, audit-request, …). No v1 route requires this yet; the model
    /// ships ahead of the endpoints so they slot in without new auth plumbing.
    Act,
}

impl Scope {
    /// The wire token for this scope — the word a config `scopes` entry and a JWT `scope`
    /// claim spell it as.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Act => "act",
        }
    }

    /// Parse one scope word, or `None` if it is not a recognized scope.
    ///
    /// Unknown scope words in a JWT's `scope` claim are ignored (not an error): an IdP may
    /// issue tokens carrying scopes for many resources, and a scope this hub does not model
    /// is simply not one of *its* grants. Returning `None` lets the caller drop it while
    /// keeping the ones it recognizes.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "read" => Some(Scope::Read),
            "act" => Some(Scope::Act),
            _ => None,
        }
    }
}

/// The scope a route requires, checked against a principal's [`GrantedScopes`].
///
/// A newtype over [`Scope`] so a route's requirement reads as a requirement, not a bare
/// permission, at its call site. Every v1 read route requires `RequiredScope(Scope::Read)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredScope(pub Scope);

impl RequiredScope {
    /// The read scope every v1 read surface requires.
    pub const READ: RequiredScope = RequiredScope(Scope::Read);

    /// Whether `granted` satisfies this requirement.
    #[must_use]
    pub fn is_satisfied_by(self, granted: &GrantedScopes) -> bool {
        granted.contains(self.0)
    }
}

/// The scopes a principal was granted — parsed from a hub-minted token's config or an IdP
/// JWT's `scope` claim.
///
/// Deliberately a small set with a `contains` check rather than a bitmask or a role: v1 has
/// two scopes and will grow slowly, and an explicit set keeps "does this principal hold
/// `act`?" a direct, auditable question. A principal with an empty grant satisfies no
/// route — the safe default, so a token whose scopes could not be parsed authorizes
/// nothing rather than everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantedScopes(Vec<Scope>);

impl GrantedScopes {
    /// A grant of exactly `scopes`.
    #[must_use]
    pub fn new(scopes: impl IntoIterator<Item = Scope>) -> Self {
        Self(scopes.into_iter().collect())
    }

    /// Parse an OAuth 2.1 space-delimited `scope` string into the scopes this hub models,
    /// dropping any word it does not recognize.
    ///
    /// An IdP token may carry scopes for several resources; only the words this hub knows
    /// (`read`, `act`) become grants, and the rest are ignored (see [`Scope::parse`]). An
    /// empty or all-unrecognized string yields an empty grant, which authorizes nothing.
    #[must_use]
    pub fn from_scope_claim(scope: &str) -> Self {
        Self(scope.split_whitespace().filter_map(Scope::parse).collect())
    }

    /// Whether this grant includes `scope`.
    #[must_use]
    pub fn contains(&self, scope: Scope) -> bool {
        self.0.contains(&scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_grant_satisfies_read_route_not_act_route() {
        let granted = GrantedScopes::new([Scope::Read]);
        assert!(RequiredScope(Scope::Read).is_satisfied_by(&granted));
        assert!(
            !RequiredScope(Scope::Act).is_satisfied_by(&granted),
            "a read grant must never reach an act route"
        );
    }

    #[test]
    fn act_grant_does_not_imply_read() {
        // Scopes are distinct, not hierarchical: an act-only grant does not satisfy read.
        // A real token would carry both; the point is that neither implies the other.
        let granted = GrantedScopes::new([Scope::Act]);
        assert!(!RequiredScope(Scope::Read).is_satisfied_by(&granted));
        assert!(RequiredScope(Scope::Act).is_satisfied_by(&granted));
    }

    #[test]
    fn empty_grant_satisfies_nothing() {
        let granted = GrantedScopes::default();
        assert!(!RequiredScope(Scope::Read).is_satisfied_by(&granted));
        assert!(!RequiredScope(Scope::Act).is_satisfied_by(&granted));
    }

    #[test]
    fn scope_claim_parses_known_words_and_drops_unknown() {
        let granted = GrantedScopes::from_scope_claim("read act openid profile");
        assert!(granted.contains(Scope::Read));
        assert!(granted.contains(Scope::Act));
        // `openid`/`profile` are not hub scopes; they are dropped, not an error.
        assert_eq!(granted, GrantedScopes::new([Scope::Read, Scope::Act]));
    }

    #[test]
    fn a_scope_claim_with_no_known_words_grants_nothing() {
        let granted = GrantedScopes::from_scope_claim("openid profile email");
        assert!(!RequiredScope::READ.is_satisfied_by(&granted));
    }

    #[test]
    fn scope_round_trips_its_wire_word() {
        assert_eq!(Scope::parse(Scope::Read.as_str()), Some(Scope::Read));
        assert_eq!(Scope::parse(Scope::Act.as_str()), Some(Scope::Act));
        assert_eq!(Scope::parse("nope"), None);
    }
}
