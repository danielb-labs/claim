// Render the hub's `GET /api/nags` view into the markdown a human sees, and deliver it to
// the forge as the standing issue and PR comments. This is the DELIVERY half of the nag
// loop (HUB-IMPLEMENTATION.md §3, §4.5 decision 4): the hub *renders* the nag content —
// it derives standing over the ledger, groups a breaking commit's claims into one item,
// resolves each owner from CODEOWNERS in its git mirror, and serves the result as JSON.
// The glue here *delivers* that content to the forge; it does not re-derive it. The hub is
// the single source of truth, and the forge write credential lives in this CI lane, never
// in the hub (the hub holds no forge write token in v1).
//
// The contrast with `render.mjs` is deliberate. `render.mjs` renders a `claim check --json`
// report *locally* for the on-change PR advisory — the CLI's own output, no hub involved.
// This file renders the *hub's already-derived nag view* — owners and grouping already
// resolved by the hub, not recomputed here. A reader of the standing issue and a reader of
// the hub's `/api/nags` therefore see one story, because the issue body is a faithful
// function of that response (invariant #4: the glue delivers, it does not invent a verdict).
//
// The delivered content is a pure function of the parsed `/api/nags` body. The *pull* — the
// authenticated, fail-loud fetch of that body — lives in `ci/hub-nags.sh`, factored out so
// the render logic here is unit-tested against mocked hub responses with no network
// (ci/nag-deliver.test.mjs), exactly as `render.mjs` is tested against `claim check` JSON.
//
// No dependencies, by policy (CLAUDE.md: every dependency is attack surface). Node's
// standard library only. ESM so the same file is both a CLI (`node nag-deliver.mjs`) and an
// importable module the tests exercise.

import { readFileSync } from "node:fs";

// Hidden HTML markers stamped at the top of every delivered body. The delivery step finds
// its single existing comment/issue by this exact string and edits it in place, so a
// hundred scheduled runs leave one issue, not a hundred. These markers are DISTINCT from
// `render.mjs`'s `claim-bot:on-change` / `claim-bot:clock` markers, because this is a
// different surface (the hub-sourced nag, not the locally-derived on-change advisory): a
// repo that ran both must never let one lane edit the other's comment. The markers must
// never change once shipped or old comments become unfindable and the lane starts spamming.
export const NAG_ISSUE_MARKER = "<!-- claim-bot:hub-nag -->";
export const NAG_COMMENT_MARKER = "<!-- claim-bot:hub-nag-pr -->";

// The transitions the hub routes, in escalation order for display. `drifted` is loudest
// (the fact is false now), then `stale` (aged past its window with no new verdict — we no
// longer know it holds), then `lapsed-skip` (a deferred check came due again). Any
// transition the hub adds later that this map does not know renders under its own raw name
// rather than being dropped (invariant #6: an unknown transition is surfaced, never hidden).
const TRANSITIONS = {
  drifted: { key: "drifted", heading: "Drifted", rank: 3 },
  stale: { key: "stale", heading: "Stale (aged out with no new verdict)", rank: 2 },
  "lapsed-skip": { key: "lapsed-skip", heading: "Lapsed skips (a deferred check is due)", rank: 1 },
};

/**
 * Parse and validate the hub's `GET /api/nags` response into the shape the renderer needs.
 *
 * The hub serves `{ nags, dead_letters, fired_this_pass }`, where `nags` and `dead_letters`
 * are arrays of nag items (a transition, its store and commit, its claims with statement and
 * supports, its resolved owners, its fire key). This asserts that shape and rejects a body
 * that is not it — a malformed hub response is a loud failure, never a silently-empty
 * "nothing to nag" that would blank the standing issue over a broken pull (invariant #6).
 *
 * `nags` and `dead_letters` missing default to empty (a hub with nothing to report may omit
 * them), but a present value that is not an array is an error: the difference between "the
 * hub said there is nothing" and "the hub sent garbage" must never collapse into a false
 * all-clear.
 */
export function parseNagView(body) {
  if (body === null || typeof body !== "object" || Array.isArray(body)) {
    throw new Error("hub /api/nags response is not a JSON object");
  }
  const nags = coerceItems(body.nags, "nags");
  const deadLetters = coerceItems(body.dead_letters, "dead_letters");
  return { nags, deadLetters };
}

/** Coerce a `nags`/`dead_letters` field to an array of items, rejecting a non-array present value. */
function coerceItems(value, field) {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) {
    throw new Error(`hub /api/nags field '${field}' is present but not an array`);
  }
  return value;
}

/**
 * Is the parsed hub view clean — nothing to nag about at all?
 *
 * Clean means both `nags` and `dead_letters` are empty. A dead-letter (a transition with no
 * resolvable owner) is NOT clean: it is a first-class problem (a nag about the inability to
 * nag), so a store with only dead-letters still keeps the standing issue open.
 */
export function isClean(view) {
  return view.nags.length === 0 && view.deadLetters.length === 0;
}

/** The display heading for a transition, falling back to its raw name for an unknown kind. */
function transitionHeading(transition) {
  const known = TRANSITIONS[transition];
  return known ? known.heading : `Transition: ${defang(String(transition))}`;
}

/** The display rank for a transition; an unknown kind sorts last (rank 0) but is never dropped. */
function transitionRank(transition) {
  const known = TRANSITIONS[transition];
  return known ? known.rank : 0;
}

/**
 * Render one nag item (owned or dead-lettered) from the hub's view.
 *
 * Everything shown here comes verbatim from the hub's response — the claim ids, statements,
 * supports, and resolved owners — so the delivered body is a faithful function of what the
 * hub derived, never a local recomputation. Free-text (statements, supports, owners) is
 * defanged so a claim file's prose cannot mention people or forge-link in the delivered
 * comment; the content is still shown, just inert.
 */
function nagItemBlock(item) {
  const lines = [];
  const claims = Array.isArray(item.claims) ? item.claims : [];
  const commit = item.commit ? ` (commit ${code(item.commit)})` : "";
  const heading = claims.length > 1 ? `${claims.length} claims${commit}` : `${claims.length} claim${commit}`;
  lines.push(`- **${heading}**`);

  for (const claim of claims) {
    lines.push(`  - **${code(claim.id)}**`);
    if (claim.statement) {
      lines.push(`    - > ${defang(claim.statement)}`);
    }
    const supports = Array.isArray(claim.supports) ? claim.supports : [];
    if (supports.length > 0) {
      lines.push("    - supports:");
      for (const s of supports) {
        lines.push(`      - ${code(s)}`);
      }
    }
  }

  lines.push(`  - ${ownersLine(item.owners)}`);
  return lines.join("\n");
}

/**
 * Render one item's owner line. Empty owners is a dead-letter — the hub could resolve no
 * CODEOWNERS owner — which is a first-class routing problem, said out loud rather than
 * omitted (invariant #6).
 *
 * Owners render **live** (not defanged): the owner is the routing target, and the whole
 * point of the nag is to notify them — the same choice `render.mjs` makes. Only free-text a
 * claim file authored (statements, supports) is defanged, because that content can smuggle an
 * unrelated `@handle`; a CODEOWNERS owner the hub resolved is a deliberate, trusted routing
 * target, not smuggled prose.
 */
function ownersLine(owners) {
  const list = Array.isArray(owners) ? owners.filter((o) => typeof o === "string" && o.length > 0) : [];
  if (list.length === 0) {
    return "_owner: unknown (the hub resolved no CODEOWNERS owner — routing dead-letter)_";
  }
  return "owner: " + list.join(" ");
}

/** Group nag items by transition, in escalation order (drifted first, unknown kinds last). */
function groupByTransition(items) {
  const byTransition = new Map();
  for (const item of items) {
    const key = item.transition;
    if (!byTransition.has(key)) byTransition.set(key, []);
    byTransition.get(key).push(item);
  }
  return [...byTransition.entries()]
    .sort((a, b) => transitionRank(b[0]) - transitionRank(a[0]) || String(a[0]).localeCompare(String(b[0])))
    .map(([transition, groupItems]) => ({ transition, items: groupItems }));
}

/**
 * Render the shared findings body from the hub's parsed view: the owned nags grouped by
 * transition, then the dead-letter queue under its own heading. Used by both the issue and
 * the comment so a fix to the grouping fixes both, and both are a faithful function of the
 * one hub response.
 */
function findingsBody(view) {
  const sections = [];
  for (const group of groupByTransition(view.nags)) {
    sections.push(`### ${transitionHeading(group.transition)}`);
    sections.push("");
    sections.push(group.items.map(nagItemBlock).join("\n"));
    sections.push("");
  }
  if (view.deadLetters.length > 0) {
    sections.push("### Dead-letter queue (no owner to route to)");
    sections.push("");
    sections.push(view.deadLetters.map(nagItemBlock).join("\n"));
    sections.push("");
  }
  return sections.join("\n").trimEnd();
}

/**
 * Render the standing-issue body from the hub's `/api/nags` response.
 *
 * The caller (the scheduled lane) opens/updates the one standing issue when there is a queue
 * and closes it when the store is clean; this still renders a clean body for the transition
 * so a previously-red issue visibly goes green. The body opens with `NAG_ISSUE_MARKER` so the
 * find-or-update step edits its one existing issue rather than opening a new one each run.
 */
export function renderNagIssue(body) {
  return issueFrom(parseNagView(body));
}

/** Render the issue body from an already-parsed view. */
function issueFrom(view) {
  const out = [NAG_ISSUE_MARKER];
  if (isClean(view)) {
    out.push("### The hub reports nothing due or drifted");
    out.push("");
    out.push("No claim is drifted, stale, or has a lapsed skip. This issue will be closed.");
    return out.join("\n") + "\n";
  }
  out.push("### Claims the hub is nagging about");
  out.push("");
  out.push(
    "The hub's scheduled derivation found facts that need a human. Each item is grouped by the hub (one breaking commit's claims are one item) and tagged with the owner the hub resolved from CODEOWNERS.",
  );
  out.push("");
  out.push(findingsBody(view));
  out.push("");
  out.push(footer());
  return out.join("\n") + "\n";
}

/**
 * Render the PR-comment body from the hub's `/api/nags` response.
 *
 * Same content as the issue, different framing and marker, so a repo delivering both never
 * lets the comment lane edit the standing issue. The comment is advisory: it routes what the
 * hub is nagging about to the PR, and never asks CI to fail (invariant #6 — drift routes, it
 * does not block the change).
 */
export function renderNagComment(body) {
  return commentFrom(parseNagView(body));
}

/** Render the comment body from an already-parsed view. */
function commentFrom(view) {
  const out = [NAG_COMMENT_MARKER];
  if (isClean(view)) {
    out.push("### The hub reports nothing due or drifted");
    out.push("");
    out.push("The hub is nagging about nothing right now.");
    return out.join("\n") + "\n";
  }
  out.push("### The hub is nagging about recorded facts");
  out.push("");
  out.push(
    "These are the hub's current nags, delivered here for visibility. This is **advisory** — it never blocks the merge; the hub routes each to whoever owns the decision it rested on.",
  );
  out.push("");
  out.push(findingsBody(view));
  out.push("");
  out.push(footer());
  return out.join("\n") + "\n";
}

/** The shared footer: how to resolve, and the pointer that the hub is the source. */
function footer() {
  return [
    "---",
    "This content is the hub's rendered nag view (`GET /api/nags`), delivered verbatim. Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`); the hub re-derives on its next tick.",
  ].join("\n");
}

/**
 * Neutralize free-text from a hub response before it goes into markdown, so a claim file's
 * statement, a support ref, or an owner string cannot address people or forge-link in the
 * delivered comment or issue. This mirrors `render.mjs`'s defang: break the `@` that starts a
 * mention, the `://` that starts an autolink, and collapse backticks so the text cannot break
 * out of a code span. Display hygiene, not a security boundary — the content is still shown,
 * just inert.
 */
function defang(text) {
  return String(text)
    .replace(/`/g, "'")
    .replace(/@(?=[A-Za-z0-9])/g, "@​")
    .replace(/:\/\//g, ":/​/");
}

/** Wrap defanged free-text in a code span, collapsing newlines so it stays one line. */
function code(text) {
  return "`" + defang(text).replace(/\s*\n\s*/g, " ").trim() + "`";
}

// --- CLI entry point ------------------------------------------------------------
//
// `node nag-deliver.mjs --mode issue|comment [--nags FILE]` reads the hub's `/api/nags`
// JSON from `--nags` or stdin and prints the rendered body to stdout. It sets its exit code
// to mirror the finding — 0 clean, 1 dirty — which the scheduled lane uses to decide
// open-vs-close the standing issue. A parse failure exits 2 (distinct from a clean/dirty
// finding), so the lane can tell "the hub sent garbage" from "nothing to nag."

/** Parse `--flag value` pairs into an object; unknown flags are ignored. */
function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      args[a.slice(2)] = argv[i + 1];
      i++;
    }
  }
  return args;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const mode = args.mode || "issue";
  let raw;
  try {
    raw = args.nags ? readFileSync(args.nags, "utf8") : readFileSync(0, "utf8");
  } catch (e) {
    process.stderr.write(`nag-deliver: could not read the hub response: ${e.message}\n`);
    process.exitCode = 2;
    return;
  }
  let body;
  try {
    body = JSON.parse(raw);
  } catch (e) {
    process.stderr.write(`nag-deliver: the hub response is not valid JSON: ${e.message}\n`);
    process.exitCode = 2;
    return;
  }
  let view;
  try {
    view = parseNagView(body);
  } catch (e) {
    process.stderr.write(`nag-deliver: ${e.message}\n`);
    process.exitCode = 2;
    return;
  }
  const out = mode === "comment" ? commentFrom(view) : issueFrom(view);
  process.stdout.write(out);
  process.exitCode = isClean(view) ? 0 : 1;
}

// Run as a CLI only when invoked directly, so importing this module for tests has no side
// effects.
if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
