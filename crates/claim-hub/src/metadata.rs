//! RFC 9728 protected-resource metadata: how a client discovers where to get a token.
//!
//! A read surface that answers `401` is useless to a client that does not know *how* to
//! authenticate. RFC 9728 (OAuth 2.0 Protected Resource Metadata) closes that gap, and the
//! MCP authorization spec builds on it: a protected resource publishes a metadata document
//! at `/.well-known/oauth-protected-resource`, and a `401` points to it via a
//! `WWW-Authenticate: Bearer resource_metadata="<url>"` challenge. A client reads the
//! document to learn the resource identifier and the authorization server(s) — the
//! customer's IdP — it should obtain a token from.
//!
//! The document is **unauthenticated by design**: it is discovery data, carrying no
//! secret, and a client that cannot yet authenticate must be able to read it. Its exposure
//! is deliberate, not an oversight (a security requirement of the item).
//!
//! When the hub has no IdP configured (the scoped-token floor only), the document still
//! serves — it just lists no `authorization_servers`, since there is no IdP to point at;
//! the hub-minted-token path is out of band (an operator hands a client a token). The
//! `WWW-Authenticate` pointer is still emitted, so a client always learns the resource and
//! the `Bearer` scheme.

use serde::Serialize;

use crate::scope::Scope;

/// The well-known path the protected-resource metadata is served at (RFC 9728 §3).
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";

/// The RFC 9728 protected-resource metadata document.
///
/// Serialized to JSON at [`METADATA_PATH`]. `resource` is the hub's own identifier (its
/// configured read audience — the value a token's `aud` must equal, so a client requests a
/// token for exactly this resource). `authorization_servers` lists the IdP issuer(s) a
/// client obtains a token from; empty when the hub runs on the scoped-token floor with no
/// IdP. `scopes_supported` advertises the scopes a token may carry, so a client requests
/// the right one (`read` for the read surfaces).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResourceMetadata {
    /// The protected resource's identifier — the hub's configured read audience.
    pub resource: String,
    /// The authorization server issuer URLs a client may obtain a token from. Empty when no
    /// IdP is configured (the scoped-token floor); a hub-minted token is provisioned out of
    /// band.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authorization_servers: Vec<String>,
    /// The bearer methods this resource accepts — always `["header"]`: the credential
    /// arrives in the `Authorization` header, never a query string (which would leak the
    /// token into logs and referrers).
    pub bearer_methods_supported: Vec<String>,
    /// The scopes a token for this resource may carry, so a client requests the right one.
    pub scopes_supported: Vec<String>,
}

impl ResourceMetadata {
    /// The metadata for a hub whose resource id is `resource`, pointing at `issuer` when an
    /// IdP is configured.
    ///
    /// `resource` is the configured read audience; `issuer` is the read IdP's issuer URL,
    /// or `None` on the scoped-token floor (then `authorization_servers` is empty). The
    /// advertised scopes are the hub's read scope — the only scope a v1 route requires.
    #[must_use]
    pub fn new(resource: impl Into<String>, issuer: Option<String>) -> Self {
        Self {
            resource: resource.into(),
            authorization_servers: issuer.into_iter().collect(),
            bearer_methods_supported: vec!["header".to_owned()],
            scopes_supported: vec![Scope::Read.as_str().to_owned()],
        }
    }

    /// The `WWW-Authenticate` challenge value a `401` carries, pointing a client at the
    /// metadata document at `metadata_url` (RFC 9728 §5.1, the MCP auth flow).
    ///
    /// The `Bearer` scheme with a `resource_metadata` parameter is exactly what an MCP
    /// client (and any RFC 9728-aware client) reads to discover how to authenticate. The
    /// value is a header string, never a secret.
    #[must_use]
    pub fn www_authenticate(metadata_url: &str) -> String {
        format!("Bearer resource_metadata=\"{metadata_url}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_with_an_idp_lists_the_authorization_server() {
        let meta = ResourceMetadata::new(
            "https://hub.acme.example",
            Some("https://idp.acme.example".to_owned()),
        );
        assert_eq!(meta.resource, "https://hub.acme.example");
        assert_eq!(meta.authorization_servers, vec!["https://idp.acme.example"]);
        assert_eq!(meta.bearer_methods_supported, vec!["header"]);
        assert_eq!(meta.scopes_supported, vec!["read"]);
    }

    #[test]
    fn metadata_without_an_idp_omits_authorization_servers() {
        // The scoped-token floor: no IdP to point at, so the field is omitted (not a
        // fabricated empty issuer). A client still learns the resource and the Bearer scheme.
        let meta = ResourceMetadata::new("https://hub.acme.example", None);
        assert!(meta.authorization_servers.is_empty());
        let json = serde_json::to_value(&meta).unwrap();
        assert!(
            json.get("authorization_servers").is_none(),
            "an empty authorization_servers is omitted, not serialized: {json}"
        );
        assert_eq!(json["resource"], "https://hub.acme.example");
    }

    #[test]
    fn www_authenticate_points_at_the_metadata_document() {
        let value = ResourceMetadata::www_authenticate(
            "https://hub.acme.example/.well-known/oauth-protected-resource",
        );
        assert_eq!(
            value,
            "Bearer resource_metadata=\"https://hub.acme.example/.well-known/oauth-protected-resource\""
        );
    }
}
