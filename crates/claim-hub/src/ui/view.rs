//! The view models — one `*View` per page (the HTML lens) plus its `*Twin` (the markdown
//! lens), the twin derived from the view so both render one page's facts.
//!
//! A view model owns **display-ready data**: strings, small enums, and pre-formatted
//! optionals, computed once from the read model in each view's `build`/`from_read`/`new`
//! constructor. The templates only read fields and call zero-argument accessors, never
//! re-derive.
//!
//! Twin-parity is enforced, not merely hoped for. Askama needs a concrete struct per
//! template, so each page is a `*View` (the HTML lens) and a `*Twin` (the markdown lens)
//! that **borrows the page's own fields** through a `From<&*View>` conversion — the twin
//! cannot invent a value the page does not hold, and a new owned field on the view is a
//! natural pressure point on that conversion. A parity test then asserts every rendered
//! fact appears in both lenses, so a field wired into one template but not the other fails
//! the gate rather than silently blanking a cell.
//!
//! The two lenses escape differently, and must. The HTML templates auto-escape every
//! interpolation (askama's `.html` escaper), so the view exposes attacker-influenceable
//! fields raw and lets the template neutralize them. The markdown twins carry
//! `escape = "none"` — a markdown table cell is structural text askama must not touch — so
//! the twin holds the same facts pre-neutralized by [`markdown_cell_safe`]: an
//! attacker-controlled `evidence`, `producer`, `commit`, or `statement` cannot break the
//! table, break out of its row, or emit active markup that would go live when the `.md` is
//! rendered to HTML downstream (PRODUCT.md §6: a surface an agent reads must not be an
//! injection channel). The HTML lens is never double-escaped, and the markdown lens is
//! never left injectable — the two render the same underlying facts by construction.
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
    /// The path to this claim's dossier markdown twin (`dossier_path` + `.md`), precomputed so
    /// the queue twin links a row without appending a suffix in the template — symmetric with
    /// the dossier page's own `twin_path`.
    pub dossier_twin_path: String,
}

impl QueueRow {
    fn from_standing(standing: &ClaimStanding) -> Self {
        let dossier_path = format!("/ui/claims/{}", standing.id);
        let dossier_twin_path = format!("{dossier_path}.md");
        Self {
            id: standing.id.clone(),
            store: standing.store.clone(),
            standing: standing_word(standing.standing),
            standing_label: standing_label(standing.standing).to_owned(),
            verified_as_of: standing.verified_as_of.map(|t| t.to_string()),
            stale_at: standing.stale_at.map(|t| t.to_string()),
            due_at: standing.due_at.map(|t| t.to_string()),
            dossier_path,
            dossier_twin_path,
        }
    }
}

/// One queue row rendered safe for a markdown table cell — the markdown lens of a [`QueueRow`],
/// derived field-by-field through [`From`] so it holds the **same facts** as the HTML row.
///
/// The identity fields (`id`, `store`) pass through [`markdown_cell_safe`]: a live claim id and
/// store are parser-validated and cannot today carry a table-breaking character, but the twin
/// does not assume its source, so it defangs them anyway (defense-in-depth). The standing label,
/// the freshness instants, and the twin path are derived-safe (a fixed enum phrase, RFC 3339
/// timestamps, and a hub-built URL), so they render raw. The wire `standing` word the HTML row
/// carries is a CSS-badge key, not a fact, so the twin — which has no badge — omits it.
pub(super) struct QueueTwinRow {
    pub id: String,
    pub store: String,
    pub standing_label: String,
    pub verified_as_of: Option<String>,
    pub stale_at: Option<String>,
    pub due_at: Option<String>,
    pub dossier_twin_path: String,
}

impl From<&QueueRow> for QueueTwinRow {
    fn from(row: &QueueRow) -> Self {
        Self {
            id: markdown_cell_safe(&row.id),
            store: markdown_cell_safe(&row.store),
            standing_label: row.standing_label.clone(),
            verified_as_of: row.verified_as_of.clone(),
            stale_at: row.stale_at.clone(),
            due_at: row.due_at.clone(),
            dossier_twin_path: row.dossier_twin_path.clone(),
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

/// The markdown twin of the queue, rendering the **same** facts as [`QueueView`] through the
/// markdown lens.
///
/// Built from the view by [`From`], so the twin cannot name a claim the page does not; its rows
/// are [`QueueTwinRow`]s, each cell-safe for a markdown table (`escape = "none"`). A parity test
/// asserts every queued claim in the HTML also appears here.
#[derive(Template)]
#[template(path = "queue.md", escape = "none")]
pub(super) struct QueueTwin<'a> {
    pub rows: Vec<QueueTwinRow>,
    pub as_of: &'a AsOfView,
}

impl<'a> From<&'a QueueView> for QueueTwin<'a> {
    fn from(view: &'a QueueView) -> Self {
        Self {
            rows: view.rows.iter().map(QueueTwinRow::from).collect(),
            as_of: &view.as_of,
        }
    }
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

    /// Render the markdown twin — the same facts, the markdown lens. The twin is built from
    /// this view by [`From`], so it cannot diverge on content; its cells are neutralized for
    /// markdown, so it cannot be an injection channel.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        QueueTwin::from(self).render()
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
            // A nag (or a later kind) is not a verdict; excluded, not rendered as one.
            _ => return None,
        }
        // A verdict event carries both its verdict and its check; a verdict-kind event
        // missing either is malformed telemetry, excluded rather than rendered with a
        // fabricated verdict or check (invariant #4/#6).
        let (verdict, check) = (event.verdict?, event.check.as_ref()?);
        Some(Self {
            seq: seq.to_string(),
            verdict: verdict_word(verdict),
            check_index: check.index.to_string(),
            commit: event.commit.clone(),
            reported_at: event.reported_at.to_string(),
            evidence: event.evidence.clone(),
            producer: format_producer(&event.producer.0),
        })
    }
}

/// One history row rendered safe for a markdown table cell — the markdown lens of a
/// [`HistoryRow`], derived through [`From`] so it holds the **same** dated observation.
///
/// The attacker-influenceable fields are neutralized: `commit` (the producer-supplied OIDC
/// `sha`), `evidence` (arbitrary check stdout from an ingested report, only size-capped), and
/// `producer` (the verified OIDC token claims, recorded verbatim) all pass through
/// [`markdown_cell_safe`], so none can break the table, escape its row, or emit active markup.
/// The `seq`, `verdict` word, `check_index`, and `reported_at` are derived-safe (integers, a
/// fixed enum word, and an RFC 3339 instant) and render raw.
pub(super) struct HistoryTwinRow {
    pub seq: String,
    pub verdict: String,
    pub check_index: String,
    pub commit: String,
    pub reported_at: String,
    pub evidence: Option<String>,
    pub producer: String,
}

impl From<&HistoryRow> for HistoryTwinRow {
    fn from(row: &HistoryRow) -> Self {
        Self {
            seq: row.seq.clone(),
            verdict: row.verdict.clone(),
            check_index: row.check_index.clone(),
            commit: markdown_cell_safe(&row.commit),
            reported_at: row.reported_at.clone(),
            evidence: row.evidence.as_deref().map(markdown_cell_safe),
            producer: markdown_cell_safe(&row.producer),
        }
    }
}

/// One check of a claim by git reference: its declared index and content digest.
pub(super) struct CheckRow {
    /// The check's zero-based declared position.
    pub index: String,
    /// The check's canonical content digest — the ledger's join key.
    pub digest: String,
}

/// One check row rendered safe for a markdown table cell — the markdown lens of a [`CheckRow`].
///
/// A digest is a hex content hash the hub computes, so it cannot carry a table-breaking
/// character; it is defanged anyway so the twin makes no assumption about the value's source,
/// keeping every twin cell uniformly neutralized.
pub(super) struct CheckTwinRow {
    pub index: String,
    pub digest: String,
}

impl From<&CheckRow> for CheckTwinRow {
    fn from(row: &CheckRow) -> Self {
        Self {
            index: row.index.clone(),
            digest: markdown_cell_safe(&row.digest),
        }
    }
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

/// The markdown twin of the dossier, rendering the **same** facts as [`DossierView`] through
/// the markdown lens.
///
/// Built from the view by [`From`], so the twin cannot invent a fact the page does not hold.
/// The `.md` templates carry `escape = "none"`, so every attacker-influenceable value is
/// pre-neutralized for a markdown cell by [`markdown_cell_safe`]: the `statement` (rendered in
/// a blockquote), the `commit` (the producer's OIDC `sha`), and every history row's `commit`,
/// `evidence`, and `producer`. The `id`, `store`, and `supports` targets are parser-validated
/// but neutralized too, so no twin field is left assuming its source is safe. The `standing`
/// word, the standing label, the freshness instants, and the twin path are derived-safe and
/// borrow the view unchanged.
#[derive(Template)]
#[template(path = "dossier.md", escape = "none")]
pub(super) struct DossierTwin<'a> {
    pub id: String,
    pub store: String,
    pub statement: String,
    pub standing: &'a str,
    pub standing_label: &'a str,
    pub verified_as_of: &'a Option<String>,
    pub stale_at: &'a Option<String>,
    pub due_at: &'a Option<String>,
    pub commit: String,
    pub checks: Vec<CheckTwinRow>,
    pub supports: Vec<String>,
    pub history: Vec<HistoryTwinRow>,
    pub as_of: &'a AsOfView,
}

impl<'a> From<&'a DossierView> for DossierTwin<'a> {
    fn from(view: &'a DossierView) -> Self {
        Self {
            id: markdown_cell_safe(&view.id),
            store: markdown_cell_safe(&view.store),
            statement: markdown_cell_safe(&view.statement),
            standing: &view.standing,
            standing_label: &view.standing_label,
            verified_as_of: &view.verified_as_of,
            stale_at: &view.stale_at,
            due_at: &view.due_at,
            commit: markdown_cell_safe(&view.commit),
            checks: view.checks.iter().map(CheckTwinRow::from).collect(),
            supports: view
                .supports
                .iter()
                .map(|t| markdown_cell_safe(t))
                .collect(),
            history: view.history.iter().map(HistoryTwinRow::from).collect(),
            as_of: &view.as_of,
        }
    }
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

    /// Render the markdown twin — the same facts as the page, neutralized for markdown cells.
    /// The twin is built from this view by [`From`], so it cannot diverge on content.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        DossierTwin::from(self).render()
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

/// The markdown twin of the status page, rendering the **same** facts as [`StatusView`].
///
/// Built from the view by [`From`], so the twin cannot diverge from the page. Every field is a
/// hub-computed count or ledger position (integers and the as-of), never attacker input, so the
/// twin borrows them unchanged — there is no untrusted value on this page to neutralize.
#[derive(Template)]
#[template(path = "status.md", escape = "none")]
pub(super) struct StatusTwin<'a> {
    pub ledger_head: &'a str,
    pub registry_version: &'a str,
    pub rejection_count: &'a str,
    pub queued: &'a str,
    pub as_of: &'a AsOfView,
}

impl<'a> From<&'a StatusView> for StatusTwin<'a> {
    fn from(view: &'a StatusView) -> Self {
        Self {
            ledger_head: &view.ledger_head,
            registry_version: &view.registry_version,
            rejection_count: &view.rejection_count,
            queued: &view.queued,
            as_of: &view.as_of,
        }
    }
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

    /// Render the markdown twin — the same facts as the page. Built from this view by [`From`],
    /// so it cannot diverge on content.
    pub(super) fn render_md(&self) -> Result<String, askama::Error> {
        StatusTwin::from(self).render()
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

/// Neutralize an attacker-influenceable value so it is safe inside a `.md` twin, which renders
/// with `escape = "none"` and interpolates the value into a table cell, a blockquote, a
/// heading, or an inline code span — always mid-line, never at a line's start.
///
/// A value here can come from an ingested report (a check's `evidence`), an OIDC token (a
/// `producer` value, the `commit`), or git (the `statement`). The twin cannot assume its
/// source is trusted, so every such value is defanged before it is rendered. Without this, a
/// hostile producer could break the table with `|`, break out of a row or block with a newline
/// and emit real markdown (`### heading`, `> blockquote`, or `SYSTEM: …` prose), or smuggle
/// active markup (`[x](javascript:…)`, `<img src=x onerror=…>`) that goes live when the `.md`
/// is later rendered to HTML — turning a surface an agent reads into an injection channel
/// (PRODUCT.md §6, invariant #4: a verdict is dated evidence to weigh, never an instruction).
///
/// The result stays on one physical line and stays legible (metacharacters are escaped or
/// entity-encoded, never deleted wholesale):
/// - CR and LF collapse to a single space, so the value can never reach a line start — which is
///   both what keeps a table row intact and what makes the block-leading markers (`#`, `>`,
///   `-`, `*`, `+`) inert: a value spliced mid-line cannot begin a heading, blockquote, or list.
/// - `|` becomes `\|`, so it cannot open a new table column.
/// - `<` and `>` become HTML entities, so no raw tag (`<img onerror>`, `<script>`) survives even
///   after the `.md` is rendered to HTML.
/// - The inline-active metacharacters `` ` `` (code span), `[` `]` `(` `)` (link syntax), and
///   `\` (the escape char itself) are backslash-escaped, so no code span or link — and no
///   `[x](javascript:…)` — can form from attacker input.
///
/// The block-leading markers are deliberately **not** escaped: collapsing newlines already
/// denies the attacker a line start, so escaping a mid-word `-` or `#` would mangle benign text
/// (`run-1`, a commit message) for no security gain.
fn markdown_cell_safe(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            // Collapse a newline (or lone CR) to a space: this is what keeps a value on one
            // physical line, so it can neither end a table row nor reach a line start to begin a
            // block construct.
            '\r' | '\n' => out.push(' '),
            // Entity-encode angle brackets so a raw HTML tag cannot survive a later render of
            // the `.md` to HTML; markdown passes `<img …>`/`<script>` through verbatim.
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Backslash-escape the inline-active metacharacters: the table separator, code
            // spans, link syntax, and the escape char. A backslash before an ASCII punctuation
            // char is a markdown escape, rendering the literal char.
            '|' | '`' | '[' | ']' | '(' | ')' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::markdown_cell_safe;

    #[test]
    fn a_newline_collapses_to_a_space_so_the_value_cannot_reach_a_line_start() {
        // The row/block break is the prompt-injection vector: a newline would let the tail
        // render as markdown outside the cell. Both LF and a lone CR collapse.
        assert_eq!(markdown_cell_safe("a\nb"), "a b");
        assert_eq!(markdown_cell_safe("a\r\nb"), "a  b");
        assert_eq!(markdown_cell_safe("a\rb"), "a b");
        assert!(!markdown_cell_safe("### owned\nmalice").contains('\n'));
    }

    #[test]
    fn a_pipe_is_escaped_so_it_cannot_open_a_column() {
        assert_eq!(markdown_cell_safe("a|b"), "a\\|b");
    }

    #[test]
    fn angle_brackets_become_entities_so_no_raw_tag_survives() {
        // The `.md` may be rendered to HTML downstream; an entity cannot resurrect into a tag.
        assert_eq!(
            markdown_cell_safe("<img src=x onerror=alert(1)>"),
            "&lt;img src=x onerror=alert\\(1\\)&gt;"
        );
        assert!(!markdown_cell_safe("<script>").contains('<'));
    }

    #[test]
    fn link_and_code_metacharacters_are_escaped_so_no_active_link_forms() {
        // The classic active-link payload cannot form a real link once the brackets and parens
        // are escaped, so no `javascript:` href reaches a renderer.
        let safe = markdown_cell_safe("[x](javascript:alert(1))");
        assert_eq!(safe, "\\[x\\]\\(javascript:alert\\(1\\)\\)");
        // A backtick cannot close an inline code span it is interpolated into.
        assert_eq!(markdown_cell_safe("a`b"), "a\\`b");
        // A backslash is escaped, so the attacker cannot pre-consume our escaping backslash.
        assert_eq!(markdown_cell_safe("a\\|b"), "a\\\\\\|b");
    }

    #[test]
    fn benign_text_stays_legible() {
        // Readability is a requirement: an ordinary value passes through unchanged, and a
        // mid-word hyphen or hash — inert mid-line — is not mangled.
        assert_eq!(markdown_cell_safe("libfoo==4.2"), "libfoo==4.2");
        assert_eq!(markdown_cell_safe("run-1"), "run-1");
        assert_eq!(
            markdown_cell_safe("repository=acme/payments run=run-1"),
            "repository=acme/payments run=run-1"
        );
        assert_eq!(markdown_cell_safe("issue #42"), "issue #42");
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(markdown_cell_safe(""), "");
    }
}
