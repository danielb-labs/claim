// Unit tests for the nag-delivery renderer, run by Node's built-in test runner
// (`node --test`). GitHub Actions cannot run locally, so the guarantee that the hub's
// `/api/nags` view delivers as the right issue/comment markdown — grouped, owner-tagged,
// dead-letters visible, and a faithful function of the hub response — is carried here.
//
// The fixtures (ci/fixtures/nags-*.json) are the hub's `GET /api/nags` shape
// (`{ nags, dead_letters, fired_this_pass }`), so a change to that contract breaks these
// tests rather than silently mis-delivering in production. No network: the hub response is
// a fixture, exactly as render.mjs's tests mock `claim check --json`.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  parseNagView,
  isClean,
  renderNagIssue,
  renderNagComment,
  NAG_ISSUE_MARKER,
  NAG_COMMENT_MARKER,
} from "./nag-deliver.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (name) => JSON.parse(readFileSync(join(here, "fixtures", name), "utf8"));

// --- parseNagView: validate the hub's response shape ------------------------------

test("parseNagView: a well-formed view yields nags and dead_letters", () => {
  const view = parseNagView(fixture("nags-mixed.json"));
  assert.equal(view.nags.length, 3);
  assert.equal(view.deadLetters.length, 1);
});

test("parseNagView: missing nags/dead_letters default to empty", () => {
  const view = parseNagView({ fired_this_pass: 0 });
  assert.deepEqual(view.nags, []);
  assert.deepEqual(view.deadLetters, []);
});

test("parseNagView: a non-object body is a loud error, never an empty view", () => {
  // The critical false-green guard: garbage from the hub must not read as "nothing to nag,"
  // which would blank the standing issue. It throws instead.
  assert.throws(() => parseNagView(null), /not a JSON object/);
  assert.throws(() => parseNagView([]), /not a JSON object/);
  assert.throws(() => parseNagView("nope"), /not a JSON object/);
});

test("parseNagView: a present-but-non-array nags field is an error", () => {
  // "The hub sent garbage" must never collapse into "the hub said nothing."
  assert.throws(() => parseNagView({ nags: "oops" }), /'nags' is present but not an array/);
  assert.throws(() => parseNagView({ dead_letters: {} }), /'dead_letters' is present but not an array/);
});

// --- isClean: the open-vs-close decision ------------------------------------------

test("isClean: an empty view is clean", () => {
  assert.equal(isClean(parseNagView(fixture("nags-clean.json"))), true);
});

test("isClean: a view with only dead-letters is NOT clean", () => {
  // A dead-letter is a first-class problem (a nag about the inability to nag), so it keeps
  // the standing issue open — never a false all-clear.
  assert.equal(isClean(parseNagView(fixture("nags-dead-letter-only.json"))), false);
});

test("isClean: a view with any owned nag is not clean", () => {
  assert.equal(isClean(parseNagView(fixture("nags-mixed.json"))), false);
});

// --- renderNagIssue: the standing-issue body --------------------------------------

test("renderNagIssue: a clean view renders a close-me body with the issue marker", () => {
  const body = renderNagIssue(fixture("nags-clean.json"));
  assert.ok(body.startsWith(NAG_ISSUE_MARKER), "must start with the idempotency marker");
  assert.match(body, /nothing due or drifted/);
  assert.match(body, /will be closed/);
  assert.doesNotMatch(body, /Drifted/);
});

test("renderNagIssue: the mixed view renders the exact expected body", () => {
  const body = renderNagIssue(fixture("nags-mixed.json"));
  const expected = [
    NAG_ISSUE_MARKER,
    "### Claims the hub is nagging about",
    "",
    "The hub's scheduled derivation found facts that need a human. Each item is grouped by the hub (one breaking commit's claims are one item) and tagged with the owner the hub resolved from CODEOWNERS.",
    "",
    "### Drifted",
    "",
    "- **2 claims (commit `9f2c1ab`)**",
    "  - **`payments/checkout-flow`**",
    "    - > Checkout retries idempotently on a 5xx.",
    "    - supports:",
    "      - `docs/decisions/idempotent-checkout.md`",
    "      - `src/checkout/retry.rs`",
    "  - **`payments/libfoo-pin`**",
    "    - > We pin libfoo at 4.2; 5.x corrupts CJK PDF export.",
    "    - supports:",
    "      - `requirements.txt#libfoo`",
    "  - owner: @acme/payments",
    "",
    "### Stale (aged out with no new verdict)",
    "",
    "- **1 claim (commit `1c0ffee`)**",
    "  - **`builds/ci-node`**",
    "    - > CI runs on Node 20.",
    "  - owner: @acme/platform",
    "",
    "### Lapsed skips (a deferred check is due)",
    "",
    "- **1 claim (commit `d15ea5e`)**",
    "  - **`infra/nightly-audit`**",
    "    - > The nightly security audit finds no criticals.",
    "    - supports:",
    "      - `docs/decisions/audit-cadence.md`",
    "  - owner: @acme/platform @acme/security",
    "",
    "### Dead-letter queue (no owner to route to)",
    "",
    "- **1 claim (commit `b4dc0de`)**",
    "  - **`orphan/no-owner`**",
    "    - > This fact has no CODEOWNERS entry, so the hub could not route it.",
    "  - _owner: unknown (the hub resolved no CODEOWNERS owner — routing dead-letter)_",
    "",
    "---",
    "This content is the hub's rendered nag view (`GET /api/nags`), delivered verbatim. Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`); the hub re-derives on its next tick.",
    "",
  ].join("\n");
  assert.equal(body, expected);
});

test("renderNagIssue: groups drifted first, then stale, then lapsed-skip", () => {
  const body = renderNagIssue(fixture("nags-mixed.json"));
  const drifted = body.indexOf("### Drifted");
  const stale = body.indexOf("### Stale");
  const lapsed = body.indexOf("### Lapsed skips");
  assert.ok(drifted >= 0 && stale >= 0 && lapsed >= 0);
  assert.ok(drifted < stale, "drifted before stale");
  assert.ok(stale < lapsed, "stale before lapsed-skip");
});

// --- renderNagComment: the PR-comment body ----------------------------------------

test("renderNagComment: uses its own marker, never the issue marker", () => {
  const body = renderNagComment(fixture("nags-mixed.json"));
  assert.ok(body.startsWith(NAG_COMMENT_MARKER));
  assert.ok(!body.includes(NAG_ISSUE_MARKER), "the comment must not carry the issue marker");
  assert.notEqual(NAG_ISSUE_MARKER, NAG_COMMENT_MARKER);
});

test("renderNagComment: is advisory and never uses blocking language", () => {
  const body = renderNagComment(fixture("nags-mixed.json"));
  assert.match(body, /\*\*advisory\*\*/);
  assert.match(body, /never blocks the merge/);
  assert.doesNotMatch(body, /\bblocking\b/i);
  assert.doesNotMatch(body, /required check/i);
});

test("renderNagComment: a clean view renders an all-clear", () => {
  const body = renderNagComment(fixture("nags-clean.json"));
  assert.ok(body.startsWith(NAG_COMMENT_MARKER));
  assert.match(body, /nagging about nothing/);
});

// --- content-matches-hub: the delivered body is a function of the hub response ----

test("the delivered body carries every claim id, statement, and owner the hub returned", () => {
  // The invariant #4 guarantee: the glue delivers what the hub rendered, verbatim — it does
  // not drop or invent. Every claim id, statement, support, and owner from the response
  // appears in the delivered body.
  const raw = fixture("nags-mixed.json");
  const body = renderNagIssue(raw);
  for (const group of [...raw.nags, ...raw.dead_letters]) {
    for (const claim of group.claims) {
      assert.ok(body.includes(claim.id), `claim id ${claim.id} must appear`);
      if (claim.statement) {
        assert.ok(body.includes(claim.statement), `statement for ${claim.id} must appear`);
      }
      for (const s of claim.supports) {
        assert.ok(body.includes(s), `support ${s} must appear`);
      }
    }
    for (const owner of group.owners) {
      assert.ok(body.includes(owner), `owner ${owner} must appear`);
    }
  }
});

test("a grouped commit's N claims render as ONE item, not N", () => {
  // The hub already grouped one breaking commit's claims into one item; the glue must render
  // that as one item with N claims, honoring the hub's grouping rather than re-splitting.
  const body = renderNagIssue(fixture("nags-mixed.json"));
  assert.match(body, /- \*\*2 claims \(commit `9f2c1ab`\)\*\*/);
  // Exactly one "2 claims" header for that commit.
  const matches = body.match(/2 claims \(commit `9f2c1ab`\)/g) || [];
  assert.equal(matches.length, 1);
});

// --- defang: hostile free-text lands inert ----------------------------------------

test("claim-authored free-text from the hub is defanged; the resolved owner stays live", () => {
  // A statement or support can smuggle an unrelated @handle, so it is defanged. The
  // CODEOWNERS owner the hub resolved is the deliberate routing target and renders live — the
  // whole point of the nag is to notify them (the same split render.mjs makes).
  const hostile = {
    nags: [
      {
        transition: "drifted",
        store: "s",
        commit: "c",
        claims: [
          {
            id: "x/y",
            commit: "c",
            statement: "blame @evil or see https://phish.test now",
            supports: ["@team/gone see http://x.test"],
          },
        ],
        owners: ["@acme/payments"],
        fire_key: "k",
        fired_this_pass: false,
      },
    ],
    dead_letters: [],
    fired_this_pass: 0,
  };
  const body = renderNagIssue(hostile);
  // The smuggled mention in the statement is broken; the legitimate owner mention survives.
  assert.doesNotMatch(body, /@evil/);
  assert.doesNotMatch(body, /@team\/gone/);
  assert.match(body, /owner: @acme\/payments/);
  // No live autolink from the smuggled free-text: no bare '://' survives.
  assert.doesNotMatch(body, /:\/\//);
  // The smuggled content is still shown, just neutralized.
  assert.match(body, /evil/);
  assert.match(body, /phish/);
});

test("a smuggled HTML-comment marker in free-text is defanged, not rendered literally", () => {
  // A hostile statement could carry the idempotency marker (or any `<!-- -->`) verbatim. The
  // find-or-update is label+bot-scoped so a smuggled marker cannot redirect it, but the text
  // must still render inert rather than as a live HTML comment.
  const hostile = {
    nags: [
      {
        transition: "drifted",
        store: "s",
        commit: "c",
        claims: [
          {
            id: "x/y",
            commit: "c",
            statement: "sneaky <!-- claim-bot:hub-nag --> marker",
            supports: [],
          },
        ],
        owners: ["@acme/payments"],
        fire_key: "k",
        fired_this_pass: false,
      },
    ],
    dead_letters: [],
    fired_this_pass: 0,
  };
  const body = renderNagIssue(hostile);
  // Only the ONE marker the renderer itself stamps survives; the smuggled `<!--` is broken.
  const markerHits = body.split(NAG_ISSUE_MARKER).length - 1;
  assert.equal(markerHits, 1, "exactly one real marker, the one the renderer stamps");
  // The smuggled text is still shown, just with its comment-open neutralized.
  assert.match(body, /sneaky/);
  assert.match(body, /marker/);
});

// --- pluralization: "0 claim" would be wrong --------------------------------------

test("nagItemBlock pluralizes on claim count: 0 and 2+ say 'claims', 1 says 'claim'", () => {
  const withClaims = (n) => ({
    nags: [
      {
        transition: "drifted",
        store: "s",
        commit: "abc",
        claims: Array.from({ length: n }, (_, i) => ({
          id: `x/y${i}`,
          commit: "abc",
          statement: "s",
          supports: [],
        })),
        owners: ["@acme/eng"],
        fire_key: "k",
        fired_this_pass: false,
      },
    ],
    dead_letters: [],
    fired_this_pass: 0,
  });
  // An empty group (0 claims) must not render "0 claim" — the plural is keyed on === 1.
  assert.match(renderNagIssue(withClaims(0)), /- \*\*0 claims \(commit `abc`\)\*\*/);
  assert.match(renderNagIssue(withClaims(1)), /- \*\*1 claim \(commit `abc`\)\*\*/);
  assert.match(renderNagIssue(withClaims(2)), /- \*\*2 claims \(commit `abc`\)\*\*/);
});

// --- an unknown transition is surfaced, never dropped -----------------------------

test("an unknown transition renders under its raw name rather than vanishing", () => {
  // The hub's Transition enum is #[non_exhaustive]; a future kind this renderer does not know
  // must still be delivered (invariant #6), under its own name.
  const view = {
    nags: [
      {
        transition: "spot-audit",
        store: "s",
        commit: "c",
        claims: [{ id: "a/b", commit: "c", statement: "audit me", supports: [] }],
        owners: ["@acme/eng"],
        fire_key: "k",
        fired_this_pass: false,
      },
    ],
    dead_letters: [],
    fired_this_pass: 0,
  };
  const body = renderNagIssue(view);
  assert.match(body, /### Transition: spot-audit/);
  assert.match(body, /a\/b/);
});
