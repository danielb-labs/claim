//! The web UI, the markdown twins, and `llms.txt` — server-rendered over the read model.
//!
//! Every page is **one view-model struct rendered by two askama templates**: an HTML lens
//! (`*.html`) and a markdown-twin lens (`*.md`). Twin-parity is therefore structural, not
//! a discipline someone must remember — the HTML page and its `.md` twin are two lenses
//! over one struct, so they cannot describe different facts (HUB-IMPLEMENTATION.md §1.10).
//! A renamed view-model field is a compile error in *both* templates, not a blank cell in
//! one.
//!
//! The UI is a **read** (invariant #3): it derives through the JSON API's own
//! `read_state` — the exact derivation that surface serves — and stores nothing, appends
//! nothing. Every page shows its **as-of** (the ledger head, registry version, and clock the
//! answer derived from), so the UI can never present a green older than its evidence. Reads
//! are deterministic: the same (ledger head, registry version, clock) render byte-identical
//! pages.
//!
//! The dossier and the queue are **dated evidence to weigh, never instructions to obey**.
//! An agent reads the markdown twins; a hub surface an agent obeys blindly would be an
//! injection channel (PRODUCT.md §6). So the verdict history and producer provenance render
//! as observations carrying their origin, never as commands to the reader.
//!
//! ## Surfaces and the twin-path convention
//!
//! A page lives at its own path; its machine-readable twin lives at **that path plus a
//! `.md` suffix** — one predictable rule an agent can apply without a lookup table:
//!
//! | page | HTML | markdown twin |
//! |---|---|---|
//! | review queue | `/ui/queue` | `/ui/queue.md` |
//! | claim dossier | `/ui/claims/{id}` | `/ui/claims/{id}.md` |
//! | hub status | `/ui/status` | `/ui/status.md` |
//!
//! `/llms.txt` indexes every hub surface — the JSON API, the UI pages, and the twins — so
//! an agent discovers where to look with one fetch.

use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use claim_core::ClaimId;
use claim_hub_store::Registry;

use crate::api::{read_state, ReadState};
use crate::app::AppState;
use crate::http::problem;

mod view;

use view::{DossierView, QueueView, StatusView};

/// The UI router: the three pages, each at its own path with a `.md` twin, plus `/llms.txt`.
///
/// Mounted at the root by [`crate::build_app`]. The dossier uses a **catch-all** segment
/// (`{*rest}`) because a claim id is namespaced with `/` (e.g. `payments/libfoo-pin`); the
/// handler splits a trailing `.md` off the capture to pick the twin lens, exactly as the
/// JSON API splits `/dossier`. A single-segment route would 404 on any namespaced id.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/llms.txt", get(llms_txt))
        .route("/ui/queue", get(queue_html))
        .route("/ui/queue.md", get(queue_md))
        .route("/ui/status", get(status_html))
        .route("/ui/status.md", get(status_md))
        // The catch-all serves both `/ui/claims/{id}` (HTML) and `/ui/claims/{id}.md`
        // (twin); the handler branches on a trailing `.md`.
        .route("/ui/claims/{*rest}", get(claim_dossier))
}

/// The content type for a markdown twin: `text/markdown`, so a browser and an agent both
/// read it as text, not download it. `charset=utf-8` because the statements and evidence
/// are arbitrary Unicode.
const MARKDOWN: &str = "text/markdown; charset=utf-8";

/// The content type for `llms.txt`: plain UTF-8 text, the convention agents expect.
const PLAIN: &str = "text/plain; charset=utf-8";

/// Render a rendered-template string as a response with `content_type`, or a `500` naming
/// the render fault.
///
/// A template that fails to render is a hub bug, not a client error, so it is logged and
/// answered `500` — never a blank page that looks like an empty result (invariant #6: a
/// wrong answer stays loud).
fn rendered(body: Result<String, askama::Error>, content_type: &'static str) -> Response {
    match body {
        Ok(body) => ([(CONTENT_TYPE, content_type)], body).into_response(),
        Err(error) => {
            tracing::error!(%error, "a UI template failed to render");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the hub could not render this page right now",
            )
        }
    }
}

/// Render an HTML page from a rendered template string, or a `500` naming the render fault.
fn rendered_html(body: Result<String, askama::Error>) -> Response {
    match body {
        Ok(body) => Html(body).into_response(),
        Err(error) => {
            tracing::error!(%error, "a UI template failed to render");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the hub could not render this page right now",
            )
        }
    }
}

/// `GET /ui/queue`: the review queue as HTML — the human's primary "what needs a look".
async fn queue_html(State(state): State<AppState>) -> Response {
    match queue_view(&state).await {
        Ok(view) => rendered_html(view.render_html()),
        Err(error) => error.into_response(),
    }
}

/// `GET /ui/queue.md`: the review queue as its markdown twin — the same view model, the
/// markdown lens.
async fn queue_md(State(state): State<AppState>) -> Response {
    match queue_view(&state).await {
        Ok(view) => rendered(view.render_md(), MARKDOWN),
        Err(error) => error.into_response(),
    }
}

/// Build the queue view model from the one derivation both lenses render.
///
/// The queue's membership is the deriver's own due set ([`claim_hub_core::ReadModel::due`])
/// — every drifted, stale, or due-for-recheck claim — not a `standing == due` filter:
/// due-ness is a union of "needs attention now" states the model computes, which an
/// equality filter could not reproduce.
async fn queue_view(state: &AppState) -> Result<QueueView, crate::api::ReadError> {
    let read = read_state(state).await?;
    Ok(QueueView::from_read(&read))
}

/// `GET /ui/status`: the hub's health and position as HTML.
async fn status_html(State(state): State<AppState>) -> Response {
    match status_view(&state).await {
        Ok(view) => rendered_html(view.render_html()),
        Err(error) => error.into_response(),
    }
}

/// `GET /ui/status.md`: the hub's health and position as its markdown twin.
async fn status_md(State(state): State<AppState>) -> Response {
    match status_view(&state).await {
        Ok(view) => rendered(view.render_md(), MARKDOWN),
        Err(error) => error.into_response(),
    }
}

/// Build the status view model: the ledger head, registry version, rejection count, and the
/// as-of the position derives from.
///
/// The position is *derived* at read time from the store and the same read model the pages
/// render (invariant #3), so the status page can never disagree with the queue it links to:
/// both carry the one as-of. The rejection count is the store's own counter — a quiet source
/// of staleness a monitor must see (invariant #6).
async fn status_view(state: &AppState) -> Result<StatusView, crate::api::ReadError> {
    use claim_hub_store::Rejections;
    let read = read_state(state).await?;
    let rejection_count = state.store.rejection_count().await.map_err(|error| {
        crate::api::ReadError::from_store("cannot read the rejection count right now", error)
    })?;
    Ok(StatusView::new(&read, rejection_count))
}

/// `GET /ui/claims/{id}` and `GET /ui/claims/{id}.md`: a claim's dossier, HTML or twin.
///
/// The catch-all captures the whole namespaced tail. A trailing `.md` selects the markdown
/// twin **only when the id before it names a claim the model holds** — so a claim whose own
/// id legitimately ends in `.md` still renders its HTML dossier at `/ui/claims/{id}`, never
/// silently shadowed by the twin route (a silent wrong answer invariant #6 forbids).
async fn claim_dossier(State(state): State<AppState>, Path(rest): Path<String>) -> Response {
    let read = match read_state(&state).await {
        Ok(read) => read,
        Err(error) => return error.into_response(),
    };

    // A trailing `.md` is the twin request only if the id before it is a live claim.
    if let Some(id) = rest.strip_suffix(".md") {
        if read.model.claims.keys().any(|(_, cid)| cid == id) {
            return dossier(&state, &read, id, Lens::Markdown).await;
        }
    }
    dossier(&state, &read, &rest, Lens::Html).await
}

/// Which lens a dossier request renders: the HTML page or its markdown twin.
enum Lens {
    Html,
    Markdown,
}

/// Render one claim's dossier through `lens`, from the read model plus one registry read.
///
/// A claim the registry does not hold at its tip is a `404` — the dossier's git-referenced
/// statement and checks need a live registry entry, so an absent one is an honest 404, never
/// a fabricated standing (invariant #6). The `standing`, `history`, and `as_of` all come
/// from the one derived model, so the trust judgment is stamped with exactly the inputs it
/// derived from.
///
/// The model lookup happens **before** the [`ClaimId`] parse: an id the model does not hold
/// is a `404` regardless of whether the string parses, so a missing claim (including a twin
/// request for one whose stripped id would not parse) gets the honest not-found answer, not a
/// spurious `400`. A live claim's id always parses, since it came from the parser that built
/// the registry.
async fn dossier(state: &AppState, read: &ReadState, id: &str, lens: Lens) -> Response {
    let Some(((store, _), standing)) = read.model.claims.iter().find(|((_, cid), _)| cid == id)
    else {
        return problem(
            StatusCode::NOT_FOUND,
            &format!(
                "no claim `{id}` in the registry — it may not be synced yet, or it was retired \
                 with no verdict history"
            ),
        );
    };

    // A live claim's id parses, since the registry was built by the same parser; a parse fault
    // here would be an internal inconsistency, answered loudly rather than silently.
    let claim_id = match id.parse::<ClaimId>() {
        Ok(id) => id,
        Err(error) => {
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("claim `{id}` is in the model but its id does not parse: {error}"),
            )
        }
    };

    let registered = match state.store.claim(store, &claim_id).await {
        Ok(Some(registered)) => registered,
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                &format!(
                    "claim `{id}` in store `{store}` is retired (absent from the registry tip); \
                     its history is on the ledger but it has no live statement to render"
                ),
            )
        }
        Err(error) => {
            return crate::api::ReadError::from_store(
                "cannot read the claim from the registry right now",
                error,
            )
            .into_response()
        }
    };

    let view = DossierView::build(
        id,
        store,
        standing,
        &registered,
        &read.events,
        read.model.as_of,
    );
    match lens {
        Lens::Html => rendered_html(view.render_html()),
        Lens::Markdown => rendered(view.render_md(), MARKDOWN),
    }
}

/// `GET /llms.txt`: the machine index of every hub surface, per the `llms.txt` convention.
///
/// An agent fetches this one file to learn where the hub's reads live — the JSON API, the UI
/// pages, and the markdown twins — so discovery is a single request, not a crawl. It names
/// each surface and its twin-path rule; it is static text (no store read), so it always
/// answers even when the store is unreachable.
async fn llms_txt() -> Response {
    ([(CONTENT_TYPE, PLAIN)], LLMS_TXT).into_response()
}

/// The `/llms.txt` body: a stable, hand-maintained index of every hub surface.
///
/// Kept in one `const` (not a template) because it takes no derived data — it is the map of
/// the surface, not a rendering of the read model. The UI-surface test asserts it names each
/// page and endpoint, so a new surface that forgets to register here fails the gate.
const LLMS_TXT: &str = include_str!("../templates/llms.txt");

#[cfg(test)]
mod tests;
