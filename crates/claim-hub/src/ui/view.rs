//! The view models — one struct per page, each the single source both its HTML lens and
//! its markdown twin render from.
//!
//! A view model owns **display-ready data**: strings, small enums, and pre-formatted
//! optionals, computed once from the read model in each view's `build`/`from_read`/`new`
//! constructor. The templates only read fields and call zero-argument accessors, never
//! re-derive — so the HTML and the markdown twin are provably two lenses over identical data
//! (twin-parity by construction), and a renamed field breaks both templates at compile time
//! rather than silently blanking one.
//!
//! Nothing here reaches the store or the clock: a view model is built from an already-derived
//! `ReadState`, so the module is a pure projection and the pages inherit the read's
//! determinism.

use askama::Template;
use claim_core::Verdict;
use claim_hub_core::{AsOf, ClaimStanding, Standing};

use crate::api::ReadState;
use claim_hub_store::RegisteredClaim;

/// The inputs a derivation was computed from, formatted for display.
///
/// Every page carries its as-of (HUB.md §4): the ledger head, the registry version, and the
/// clock the answer derived from, so a rendered page can never be mistaken for fresher than
/// its evidence. Held as strings so a template interpolates them with no formatting logic of
/// its own.
pub(super) struct AsOfView {
    /// The ledger head seq the answer derived from — `0` on an empty ledger (the truthful
    /// birth position, not a fabricated value).
    pub ledger_head: String,
    /// The registry version the answer derived from.
    pub registry_version: String,
    /// The clock instant the answer derived at (RFC 3339).
    pub clock: String,
}

impl AsOfView {
    fn from_as_of(as_of: AsOf) -> Self {
        Self {
            ledger_head: as_of.ledger_head.to_string(),
            registry_version: as_of.registry_version.to_string(),
            clock: as_of.clock.to_string(),
        }
    }
}

/// One claim as a row in the review queue: its identity, its standing, and the freshness
/// instants that put it in the queue.
///
/// A row is dated evidence, never an instruction: it reports what the hub derived and when,
/// for a human to weigh. The `standing` is the kebab-case wire word (`drifted`, `stale`,
/// `verified`), so a template can both show it and key a CSS class or a text marker on it
/// with no mapping table.
pub(super) struct QueueRow {
    /// The claim's id (the namespaced handle).
    pub id: String,
    /// The store the claim lives in.
    pub store: String,
    /// The derived standing, as its wire word.
    pub standing: String,
    /// A short human phrase for the standing, for the HTML badge and the twin's prose.
    pub standing_label: String,
    /// When the claim last passed every check (RFC 3339), or `None` if never fully verified.
    pub verified_as_of: Option<String>,
    /// When the claim becomes (or became) stale by the clock, or `None` if no window applies.
    pub stale_at: Option<String>,
    /// When the claim is next due for a recheck, or `None` if it declares no cadence.
    pub due_at: Option<String>,
    /// The path to this claim's dossier page (HTML), for the queue to link each row.
    pub dossier_path: String,
}

impl QueueRow {
    fn from_standing(standing: &ClaimStanding) -> Self {
        Self {
            id: standing.id.clone(),
            store: standing.store.clone(),
            standing: standing_word(standing.standing),
            standing_label: standing_label(standing.standing).to_owned(),
            verified_as_of: standing.verified_as_of.map(|t| t.to_string()),
            stale_at: standing.stale_at.map(|t| t.to_string()),
            due_at: standing.due_at.map(|t| t.to_string()),
            dossier_path: format!("/ui/claims/{}", standing.id),
        }
    }
}

/// The review queue page: the due/drifted/stale set — the human's primary "what needs a
/// look" (HUB.md §5).
///
/// One struct, two templates ([`ui/queue.html`](../../templates/queue.html) and
/// [`ui/queue.md`](../../templates/queue.md)). The rows are the deriver's own due set, in the
/// model's (store, id) order, so the page is deterministic.
#[derive(Template)]
#[template(path = "queue.html")]
pub(super) struct QueueView {
    /// The queued claims, in (store, id) order — every drifted, stale, or due-for-recheck
    /// claim.
    pub rows: Vec<QueueRow>,
    /// The inputs this queue derived from.
    pub as_of: AsOfView,
}

/// The markdown twin of the queue, rendering the **same** [`QueueView`] fields — the parity
/// guarantee is that this and [`QueueView`] share one data struct, differing only in
/// template.
#[derive(Template)]
#[template(path = "queue.md", escape = "none")]
pub(super) struct QueueTwin<'a> {
    pub rows: &'a [QueueRow],
    pub as_of: &'a AsOfView,
}

impl QueueView {
    /// The path to this page's markdown twin — the page's own path plus `.md`, the one
    /// convention every twin follows. Called from the base template's footer.
    fn twin_path(&self) -> &'static str {
        "/ui/queue.md"
    }

    /// Build the queue from the derived read model: the due set, rendered as rows.
    pub(super) fn from_read(read: &ReadState) -> Self {
        let rows = read
            .model
            .due
            .iter()
            .filter_map(|key| read.model.claims.get(key).map(QueueRow::from_standing))
            .collect();
        Self {
            rows,
            as_of: AsOfView::from_as_of(read.model.as_of),
        }
    }

    /// Render the HTML page.
    pub(super) fn render_html(&self) -> Result<String, askama::Error> {
        self.render()
    }

    /// Render the markdown twin — the same fields, the markdown lens. The two renderers read
    /// one `QueueView`, so they cannot diverge on content.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        QueueTwin {
            rows: &self.rows,
            as_of: &self.as_of,
        }
        .render()
    }
}

/// One verdict in a claim's history, rendered for the dossier as a dated observation.
///
/// Dated evidence to *weigh*, never to obey (PRODUCT.md §6): the verdict, when the producer
/// reported it, the check it was about, the commit, the verified producer identity, and any
/// evidence. The producer is rendered as an origin line, never as an instruction, so a hub
/// surface an agent reads is not an injection channel.
pub(super) struct HistoryRow {
    /// The ledger seq — this observation's position in the append-only log.
    pub seq: String,
    /// The verdict reported, as its wire word (`held`/`drifted`/`broken`/`unverifiable`).
    pub verdict: String,
    /// The check's declared index the verdict was about.
    pub check_index: String,
    /// The commit sha the check was reported against.
    pub commit: String,
    /// When the producer reported it (RFC 3339).
    pub reported_at: String,
    /// The evidence the check recorded, if any (already capped at ingest).
    pub evidence: Option<String>,
    /// The verified producer identity, as a compact `key=value` origin line — the derived
    /// provenance (HUB.md §4), shown so the trust judgment is re-derivable, never as a
    /// command.
    pub producer: String,
}

impl HistoryRow {
    /// Render one ledger event as a history row, or `None` if the event is not a verdict.
    ///
    /// Only a [`EventKind::Verdict`](claim_hub_core::EventKind) event carries a verdict; a
    /// later kind (a nag, an ack) is not a verdict and must never render as one — that would
    /// be telemetry masquerading as a verdict (invariant #4). The enum is `#[non_exhaustive]`,
    /// so a new kind lands in the `_` arm and is excluded rather than mislabeled.
    fn from_event(seq: u64, event: &claim_hub_core::Event) -> Option<Self> {
        match event.kind {
            claim_hub_core::EventKind::Verdict => {}
            _ => return None,
        }
        Some(Self {
            seq: seq.to_string(),
            verdict: verdict_word(event.verdict),
            check_index: event.check.index.to_string(),
            commit: event.commit.clone(),
            reported_at: event.reported_at.to_string(),
            evidence: event.evidence.clone(),
            producer: format_producer(&event.producer.0),
        })
    }
}

/// One check of a claim by git reference: its declared index and content digest.
pub(super) struct CheckRow {
    /// The check's zero-based declared position.
    pub index: String,
    /// The check's canonical content digest — the ledger's join key.
    pub digest: String,
}

/// The claim dossier page: everything the org believes about one claim and how good that
/// belief is right now (HUB.md §5) — statement, checks by git reference, the derived
/// standing with its as-of, the verdict history, and derived provenance.
///
/// One struct, two templates. Author and PR-approval provenance come from git and the forge
/// (invariant #3); v1 renders what the registry already holds — the commit the claim was read
/// at and each verdict's verified producer — and fabricates no author or approval it has not
/// resolved.
#[derive(Template)]
#[template(path = "dossier.html")]
pub(super) struct DossierView {
    /// The claim's id.
    pub id: String,
    /// The store it lives in.
    pub store: String,
    /// The human-and-agent-readable statement — the real source of truth a check only
    /// approximates.
    pub statement: String,
    /// The derived standing, as its wire word.
    pub standing: String,
    /// A short human phrase for the standing.
    pub standing_label: String,
    /// When the claim last passed every check (RFC 3339), or `None` if never fully verified.
    pub verified_as_of: Option<String>,
    /// When the claim becomes (or became) stale by the clock, or `None` if no window applies.
    pub stale_at: Option<String>,
    /// When the claim is next due for a recheck, or `None` if it declares no cadence.
    pub due_at: Option<String>,
    /// The commit sha the claim (and its checks) were read at — the git reference the
    /// statement and checks resolve against.
    pub commit: String,
    /// The claim's checks by git reference: declared index and content digest.
    pub checks: Vec<CheckRow>,
    /// The targets this claim supports — the decisions or claims it justifies.
    pub supports: Vec<String>,
    /// The verdict history from the ledger, in ascending seq order — the dated evidence the
    /// standing derives from.
    pub history: Vec<HistoryRow>,
    /// The inputs the standing derived from.
    pub as_of: AsOfView,
    /// This claim's twin path (`/ui/claims/{id}.md`), computed once so the base template's
    /// footer links it without formatting logic.
    pub twin_path: String,
}

/// The markdown twin of the dossier, rendering the **same** [`DossierView`] fields.
#[derive(Template)]
#[template(path = "dossier.md", escape = "none")]
pub(super) struct DossierTwin<'a> {
    pub id: &'a str,
    pub store: &'a str,
    pub statement: &'a str,
    pub standing: &'a str,
    pub standing_label: &'a str,
    pub verified_as_of: &'a Option<String>,
    pub stale_at: &'a Option<String>,
    pub due_at: &'a Option<String>,
    pub commit: &'a str,
    pub checks: &'a [CheckRow],
    pub supports: &'a [String],
    pub history: &'a [HistoryRow],
    pub as_of: &'a AsOfView,
}

impl DossierView {
    /// Build the dossier view model from the derived standing, the registry entry, and the
    /// ledger events for this (store, claim).
    ///
    /// The `standing` and `as_of` come from the one derived model; the descriptive fields
    /// (`statement`, `checks`, `commit`, `supports`) from the registry entry read once more.
    /// That registry read is current-or-newer than the model's as-of — a safe direction: the
    /// body can describe the claim as newer, never as more verified than the standing.
    pub(super) fn build(
        id: &str,
        store: &str,
        standing: &ClaimStanding,
        registered: &RegisteredClaim,
        events: &[(u64, claim_hub_core::Event)],
        as_of: AsOf,
    ) -> Self {
        let history = events
            .iter()
            .filter(|(_, event)| event.store == store && event.claim == id)
            .filter_map(|(seq, event)| HistoryRow::from_event(*seq, event))
            .collect();
        let checks = registered
            .check_digests
            .iter()
            .enumerate()
            .map(|(index, digest)| CheckRow {
                index: index.to_string(),
                digest: digest.clone(),
            })
            .collect();
        Self {
            id: id.to_owned(),
            store: store.to_owned(),
            statement: registered.statement.clone(),
            standing: standing_word(standing.standing),
            standing_label: standing_label(standing.standing).to_owned(),
            verified_as_of: standing.verified_as_of.map(|t| t.to_string()),
            stale_at: standing.stale_at.map(|t| t.to_string()),
            due_at: standing.due_at.map(|t| t.to_string()),
            commit: registered.commit.clone(),
            checks,
            supports: registered.supports.clone(),
            history,
            as_of: AsOfView::from_as_of(as_of),
            twin_path: format!("/ui/claims/{id}.md"),
        }
    }

    /// This dossier's twin path (`/ui/claims/{id}.md`), for the base template's footer.
    fn twin_path(&self) -> &str {
        &self.twin_path
    }

    /// Render the HTML page.
    pub(super) fn render_html(&self) -> Result<String, askama::Error> {
        self.render()
    }

    /// Render the markdown twin from the same fields.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        DossierTwin {
            id: &self.id,
            store: &self.store,
            statement: &self.statement,
            standing: &self.standing,
            standing_label: &self.standing_label,
            verified_as_of: &self.verified_as_of,
            stale_at: &self.stale_at,
            due_at: &self.due_at,
            commit: &self.commit,
            checks: &self.checks,
            supports: &self.supports,
            history: &self.history,
            as_of: &self.as_of,
        }
        .render()
    }
}

/// The hub status page: the machine-readable health-and-position, HUB.md §5's `/status`
/// rendered as a human page and a twin.
///
/// The position is *derived* at read time (invariant #3) — the ledger head and registry
/// version come from the same read model the queue and dossier render, so status can never
/// disagree with them. The rejection count is a quiet source of staleness a monitor must see
/// (invariant #6).
#[derive(Template)]
#[template(path = "status.html")]
pub(super) struct StatusView {
    /// The ledger head — the position of the most recent event, `"—"` on an empty ledger.
    pub ledger_head: String,
    /// The registry version — the number of syncs applied.
    pub registry_version: String,
    /// How many ingests the hub has rejected — a rising count means telemetry is being turned
    /// away while the claims it would refresh go stale.
    pub rejection_count: String,
    /// The count of claims currently in the review queue, so the status page links to the
    /// work with a number, not a blind "go look".
    pub queued: String,
    /// The inputs this position derived from.
    pub as_of: AsOfView,
}

/// The markdown twin of the status page, rendering the **same** [`StatusView`] fields.
#[derive(Template)]
#[template(path = "status.md", escape = "none")]
pub(super) struct StatusTwin<'a> {
    pub ledger_head: &'a str,
    pub registry_version: &'a str,
    pub rejection_count: &'a str,
    pub queued: &'a str,
    pub as_of: &'a AsOfView,
}

impl StatusView {
    /// The path to this page's markdown twin. Called from the base template's footer.
    fn twin_path(&self) -> &'static str {
        "/ui/status.md"
    }

    /// Build the status view from the derived read model and the store's rejection count.
    pub(super) fn new(read: &ReadState, rejection_count: i64) -> Self {
        let as_of = read.model.as_of;
        Self {
            ledger_head: as_of.ledger_head.to_string(),
            registry_version: as_of.registry_version.to_string(),
            rejection_count: rejection_count.to_string(),
            queued: read.model.due.len().to_string(),
            as_of: AsOfView::from_as_of(as_of),
        }
    }

    /// Render the HTML page.
    pub(super) fn render_html(&self) -> Result<String, askama::Error> {
        self.render()
    }

    /// Render the markdown twin from the same fields.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        StatusTwin {
            ledger_head: &self.ledger_head,
            registry_version: &self.registry_version,
            rejection_count: &self.rejection_count,
            queued: &self.queued,
            as_of: &self.as_of,
        }
        .render()
    }
}

/// The kebab-case wire word for a standing (`verified`, `stale`, `drifted`, `suspect`,
/// `retired`) — the same string the JSON API serializes, so the UI and the API name a
/// standing identically.
fn standing_word(standing: Standing) -> String {
    // Serialize through the enum's own serde names, so the UI's word is the API's word by
    // construction — no hand-kept mapping to drift from the enum.
    serde_json::to_value(standing)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A short human phrase for a standing, for a badge or prose. Exhaustive over the standing
/// enum so a new variant forces a phrase here rather than defaulting to a blank.
fn standing_label(standing: Standing) -> &'static str {
    match standing {
        Standing::Verified => "verified",
        Standing::Stale => "stale — needs re-verification",
        Standing::Drifted => "drifted — the fact is false right now",
        Standing::Suspect => "suspect — a supported decision may no longer hold",
        Standing::Retired => "retired — deleted from git",
        // `Standing` is `#[non_exhaustive]`; a future variant must add its phrase here.
        _ => "unknown standing",
    }
}

/// The kebab-case wire word for a verdict (`held`/`drifted`/`unverifiable`/`broken`) — the
/// same string the JSON API serializes.
fn verdict_word(verdict: Verdict) -> String {
    serde_json::to_value(verdict)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Format a verified producer block as a compact, deterministic `key=value` origin line.
///
/// The producer is the verified identity behind a verdict (HUB.md §4). It is rendered as a
/// flat origin line, in sorted key order for determinism, so a reader can weigh where an
/// observation came from — never as an instruction to obey. Values are rendered as their
/// JSON scalar text; a nested value is shown as its compact JSON so nothing is silently
/// dropped.
fn format_producer(producer: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts: Vec<String> = producer
        .iter()
        .map(|(key, value)| {
            let rendered = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{key}={rendered}")
        })
        .collect();
    parts.sort();
    if parts.is_empty() {
        "(no producer identity)".to_owned()
    } else {
        parts.join(" ")
    }
}
