//! Shared HTTP response shapes, so every surface answers a failure the same way.
//!
//! The hub's error body is one wire shape — `{ "error": <reason> }` — and both the
//! ingest gate and the read API return it. Defining it once here is the point: two copies
//! of one wire contract drift silently (a renamed field, a changed shape), which is the
//! exact rot this product exists to kill. A client (human or agent) reads the reason the
//! same way regardless of which route refused it.

use axum::http::header::{HeaderName, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Build a `{ "error": <reason> }` JSON problem response with `status`.
///
/// The one non-2xx shape for every hub surface. `reason` is the loud, actionable message
/// naming what is wrong and, where possible, how to fix it (CLAUDE.md's error-message
/// rule); it is machine-readable under the `error` key so an agent can branch on it.
#[must_use]
pub fn problem(status: StatusCode, reason: &str) -> Response {
    (
        status,
        Json(ProblemBody {
            error: reason.to_owned(),
        }),
    )
        .into_response()
}

/// Build a problem response carrying extra headers — the read-auth `401`'s RFC 9728
/// `WWW-Authenticate` pointer being the one caller.
///
/// The same `{ "error": … }` body as [`problem`], plus each `(name, value)` header. A
/// header value that is not a valid HTTP header string is dropped rather than panicking:
/// the pointer is a best-effort discovery aid, and a malformed one must not turn a `401`
/// into a `500` (the client still gets the loud `401`).
#[must_use]
pub fn problem_with_headers(
    status: StatusCode,
    reason: &str,
    headers: &[(HeaderName, String)],
) -> Response {
    let mut response = problem(status, reason);
    for (name, value) in headers {
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name.clone(), value);
        }
    }
    response
}

/// The body of any hub error: a single machine-readable reason.
#[derive(Debug, Serialize)]
struct ProblemBody {
    /// The client-facing reason the request could not be answered.
    error: String,
}
