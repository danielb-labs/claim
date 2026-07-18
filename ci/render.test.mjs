// Unit tests for the CI renderer, run by Node's built-in test runner
// (`node --test`). These are the real coverage for the CI lanes: GitHub Actions
// cannot run locally, so the guarantee that a drift produces the right comment — with
// the right owner, grouped correctly, and never phrased as a block — is carried here.
//
// The fixtures are captured verbatim from a real `claim check --json` run (see
// ci/fixtures/), so a change to the CLI's JSON shape breaks these tests rather than
// silently mis-rendering in production.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
  classify,
  classifyClaim,
  ownersFor,
  statementFromFile,
  renderComment,
  renderIssue,
  COMMENT_MARKER,
  ISSUE_MARKER,
} from "./render.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (name) => JSON.parse(readFileSync(join(here, "fixtures", name), "utf8"));
const CODEOWNERS = readFileSync(join(here, "fixtures", "CODEOWNERS"), "utf8");

// A deterministic statement resolver keyed by the report's claim files, so the
// body assertions do not touch the filesystem. Files not in the map resolve to null,
// exercising the graceful-omit path.
const STATEMENTS = {
  ".claims/payments/libfoo-pin.md": "We pin libfoo at 4.2; 5.x corrupts CJK PDF export.",
  ".claims/builds/ci-node.md": "CI runs on Node 20.",
  ".claims/infra/config-v1.md": "config.txt declares v=1.",
};
const readStatement = (file) => STATEMENTS[file] ?? null;

// --- ownersFor: the last-matching-rule-wins CODEOWNERS matcher --------------------

test("ownersFor: last matching pattern wins over an earlier catch-all", () => {
  assert.deepEqual(ownersFor(".claims/payments/libfoo-pin.md", CODEOWNERS), ["@acme/payments"]);
  assert.deepEqual(ownersFor(".claims/builds/ci-node.md", CODEOWNERS), ["@acme/platform"]);
});

test("ownersFor: falls back to the catch-all when no specific rule matches", () => {
  assert.deepEqual(ownersFor(".claims/infra/config-v1.md", CODEOWNERS), ["@acme/eng"]);
});

test("ownersFor: no CODEOWNERS text yields no owners", () => {
  assert.deepEqual(ownersFor(".claims/x.md", ""), []);
});

test("ownersFor: a directory prefix matches nested files but not a sibling", () => {
  const co = "/docs/ @team-docs\n";
  assert.deepEqual(ownersFor("docs/deep/x.md", co), ["@team-docs"]);
  assert.deepEqual(ownersFor("documents/x.md", co), []);
});

test("ownersFor: a bare glob matches by basename anywhere", () => {
  const co = "*.md @writers\n";
  assert.deepEqual(ownersFor("a/b/c.md", co), ["@writers"]);
  assert.deepEqual(ownersFor("a/b/c.rs", co), []);
});

test("ownersFor: multiple owners on one line are all returned", () => {
  const co = "* @a @b @c\n";
  assert.deepEqual(ownersFor("x", co), ["@a", "@b", "@c"]);
});

test("ownersFor: comments and blank lines are ignored", () => {
  const co = "# a comment\n\n* @fallback\n";
  assert.deepEqual(ownersFor("x", co), ["@fallback"]);
});

test("ownersFor: an un-anchored dir pattern matches at any depth (GitHub semantics)", () => {
  // The bug this guards: `payments/` was matched anchored-to-root, so a store under
  // `.claims/` misrouted every payments claim to the catch-all. GitHub matches a
  // bare `payments/` at any depth.
  const co = "* @eng\npayments/ @pay\n";
  assert.deepEqual(ownersFor(".claims/payments/libfoo.md", co), ["@pay"]);
  assert.deepEqual(ownersFor("payments/x.md", co), ["@pay"]);
  // A leading-slash dir pattern stays anchored to root.
  const anchored = "* @eng\n/payments/ @pay\n";
  assert.deepEqual(ownersFor(".claims/payments/libfoo.md", anchored), ["@eng"]);
  assert.deepEqual(ownersFor("payments/x.md", anchored), ["@pay"]);
});

// --- statementFromFile: pull the plain-language fact from a claim file ------------

test("statementFromFile: extracts the body after standalone frontmatter", () => {
  const file = ["---", "id: x", "checks:", "  - kind: cmd", "---", "The fact holds."].join("\n");
  assert.equal(statementFromFile(file), "The fact holds.");
});

test("statementFromFile: takes the first non-blank body line", () => {
  const file = ["---", "id: x", "---", "", "First line.", "Second line."].join("\n");
  assert.equal(statementFromFile(file), "First line.");
});

test("statementFromFile: a host file that does not open with frontmatter is null", () => {
  // An embedded claim lives inside another file; we do not parse those here.
  const file = ["# A context file", "<!-- claim", "id: x", "-->"].join("\n");
  assert.equal(statementFromFile(file), null);
});

test("statementFromFile: unterminated frontmatter is null, not a guess", () => {
  assert.equal(statementFromFile("---\nid: x\nno close"), null);
});

test("statementFromFile: an empty body is null", () => {
  assert.equal(statementFromFile("---\nid: x\n---\n\n"), null);
});

test("renderComment: omits the statement line when none can be resolved", () => {
  // With no resolver, the block still renders — just without the quote line.
  const body = renderComment(fixture("one-drift.json"), CODEOWNERS);
  assert.doesNotMatch(body, /^ {2}- > /m);
  assert.match(body, /payments\/libfoo-pin/);
});

// --- classifyClaim: one claim to its single worst category ------------------------

test("classifyClaim: a held check with a resolved support is clean (null)", () => {
  const claim = {
    checks: [{ verdict: "held" }],
    supports: [{ target: "t", resolved: true }],
  };
  assert.equal(classifyClaim(claim), null);
});

test("classifyClaim: a drift is 'drifted'", () => {
  assert.equal(classifyClaim({ checks: [{ verdict: "drifted" }] }).key, "drifted");
});

test("classifyClaim: an unverifiable verdict is 'drifted' (never a pass)", () => {
  assert.equal(classifyClaim({ checks: [{ verdict: "unverifiable" }] }).key, "drifted");
});

test("classifyClaim: a broken check outranks everything else", () => {
  const claim = {
    checks: [{ verdict: "broken" }, { verdict: "drifted" }],
    supports: [{ target: "t", resolved: false }],
  };
  assert.equal(classifyClaim(claim).key, "broken");
});

test("classifyClaim: a held check with a vanished support is 'unresolved'", () => {
  const claim = {
    checks: [{ verdict: "held" }],
    supports: [{ target: "gone", resolved: false }],
  };
  assert.equal(classifyClaim(claim).key, "unresolved");
});

// --- classify: the whole report to a clean/dirty decision -------------------------

test("classify: a clean report is clean with no items and no errors", () => {
  const c = classify(fixture("clean.json"), CODEOWNERS);
  assert.equal(c.clean, true);
  assert.equal(c.items.length, 0);
  assert.equal(c.errors.length, 0);
});

test("classify: a load error alone is never clean", () => {
  const c = classify(fixture("load-error.json"), CODEOWNERS);
  assert.equal(c.clean, false);
  assert.equal(c.items.length, 0);
  assert.equal(c.errors.length, 1);
});

test("classify: the mixed report yields one item per non-clean claim, owners resolved", () => {
  const c = classify(fixture("mixed.json"), CODEOWNERS);
  assert.equal(c.clean, false);
  assert.equal(c.items.length, 3);
  const byId = Object.fromEntries(c.items.map((it) => [it.id, it]));
  assert.equal(byId["infra/config-v1"].category, "broken");
  assert.equal(byId["payments/libfoo-pin"].category, "drifted");
  assert.equal(byId["builds/ci-node"].category, "unresolved");
  assert.deepEqual(byId["payments/libfoo-pin"].owners, ["@acme/payments"]);
  assert.deepEqual(byId["builds/ci-node"].owners, ["@acme/platform"]);
  // infra/ has no specific rule, so it falls to the catch-all.
  assert.deepEqual(byId["infra/config-v1"].owners, ["@acme/eng"]);
});

// --- renderComment: the exact PR-comment body -------------------------------------

test("renderComment: a clean store renders an all-clear that starts with the marker", () => {
  const body = renderComment(fixture("clean.json"), CODEOWNERS);
  assert.ok(body.startsWith(COMMENT_MARKER), "must start with the idempotency marker");
  assert.match(body, /all checks held/);
  assert.doesNotMatch(body, /Drifted claims/);
});

test("renderComment: a single drift renders its statement, support, and owner", () => {
  const body = renderComment(fixture("one-drift.json"), CODEOWNERS, readStatement);
  const expected = [
    COMMENT_MARKER,
    "### claim: this PR affects recorded facts",
    "",
    "These claims changed state under your diff. This is **advisory** — it never blocks the merge; it routes what changed to whoever owns the decision it rested on.",
    "",
    "### Drifted claims",
    "",
    "- **`payments/libfoo-pin`** — `.claims/payments/libfoo-pin.md`",
    "  - > We pin libfoo at 4.2; 5.x corrupts CJK PDF export.",
    "  - supports:",
    "    - `requirements.txt#libfoo`",
    "  - owner: @acme/payments",
    "",
    "---",
    "Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`).",
    "",
  ].join("\n");
  assert.equal(body, expected);
});

test("renderComment: the mixed report groups broken, then drifted, then unresolved", () => {
  const body = renderComment(fixture("mixed.json"), CODEOWNERS, readStatement);
  const expected = [
    COMMENT_MARKER,
    "### claim: this PR affects recorded facts",
    "",
    "These claims changed state under your diff. This is **advisory** — it never blocks the merge; it routes what changed to whoever owns the decision it rested on.",
    "",
    "### Broken checks",
    "",
    "- **`infra/config-v1`** — `.claims/infra/config-v1.md`",
    "  - > config.txt declares v=1.",
    "  - check exit 2",
    "  - `grep: config.txt: No such file or directory`",
    "  - supports: _(nothing declared)_",
    "  - owner: @acme/eng",
    "",
    "### Drifted claims",
    "",
    "- **`payments/libfoo-pin`** — `.claims/payments/libfoo-pin.md`",
    "  - > We pin libfoo at 4.2; 5.x corrupts CJK PDF export.",
    "  - supports:",
    "    - `requirements.txt#libfoo`",
    "  - owner: @acme/payments",
    "",
    "### Unresolved supports",
    "",
    "- **`builds/ci-node`** — `.claims/builds/ci-node.md`",
    "  - > CI runs on Node 20.",
    "  - supports:",
    "    - `docs/decisions/node20.md` — **unresolved** (target is gone)",
    "  - owner: @acme/platform",
    "",
    "---",
    "A **broken** check or an unloadable claim file means the tool could not tell whether the fact holds — treat it as failing, not passing.",
    "Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`).",
    "",
  ].join("\n");
  assert.equal(body, expected);
});

test("renderComment: a load error is surfaced under the faults heading", () => {
  const body = renderComment(fixture("load-error.json"), CODEOWNERS);
  assert.match(body, /### Unresolved faults \(the tool could not determine status\)/);
  assert.match(body, /broken-malformed\.md.*missing required field 'checks'/);
  // A load error triggers the "could not tell" footer note.
  assert.match(body, /could not tell whether the fact holds/);
});

test("render: free-text is defanged so it cannot mention people or forge links", () => {
  // S3: a tool error / statement carrying an @handle or a URL must render inert, since
  // this content can quote fragments of the offending file.
  const hostile = {
    exit: 2,
    claims: [
      {
        id: "x/y",
        file: ".claims/x/y.md",
        checks: [{ verdict: "drifted", detail: "exit 1" }],
        skipped: [],
        supports: [{ target: "@evil/team see http://x.test", resolved: false }],
      },
    ],
    errors: [{ file: ".claims/bad.md", message: "blame @security or see https://phish.test now" }],
  };
  const body = renderComment(hostile, "");
  // No live mention: every '@' immediately followed by a word char has been broken.
  assert.doesNotMatch(body, /@(?=[A-Za-z0-9])/);
  // No live autolink: no bare '://' survives.
  assert.doesNotMatch(body, /:\/\//);
  // The content is still shown, just neutralized.
  assert.match(body, /evil/);
  assert.match(body, /security/);
});

// --- the advisory-never-blocks invariant, asserted directly -----------------------

test("renderComment: never uses blocking language, always names itself advisory", () => {
  const body = renderComment(fixture("mixed.json"), CODEOWNERS);
  assert.match(body, /\*\*advisory\*\*/);
  assert.match(body, /never blocks the merge/);
  assert.doesNotMatch(body, /\bblocking\b/i);
  assert.doesNotMatch(body, /merge is blocked/i);
  assert.doesNotMatch(body, /required check/i);
});

// --- an unowned claim is a visible dead-letter, not a silent drop -----------------

test("renderComment: an unowned drifted claim renders a dead-letter note", () => {
  const report = fixture("one-drift.json");
  const body = renderComment(report, ""); // no CODEOWNERS at all
  assert.match(body, /owner: unknown \(no CODEOWNERS match — routing dead-letter\)/);
});

// --- renderIssue: the clock-lane standing-issue body ------------------------------

test("renderIssue: a clean store renders a close-me body with the issue marker", () => {
  const body = renderIssue(fixture("clean.json"), CODEOWNERS);
  assert.ok(body.startsWith(ISSUE_MARKER));
  assert.match(body, /store is clean/);
  assert.match(body, /will be closed/);
});

test("renderIssue: the mixed report renders the queue with grouping and owners", () => {
  const body = renderIssue(fixture("mixed.json"), CODEOWNERS);
  assert.ok(body.startsWith(ISSUE_MARKER));
  assert.match(body, /### Claims due & drifted/);
  assert.match(body, /### Broken checks/);
  assert.match(body, /### Drifted claims/);
  assert.match(body, /### Unresolved supports/);
  assert.match(body, /@acme\/payments/);
  assert.match(body, /@acme\/platform/);
  // The two markers are distinct so the two lanes never edit each other's surface.
  assert.notEqual(COMMENT_MARKER, ISSUE_MARKER);
  assert.ok(!body.includes(COMMENT_MARKER));
});

test("renderIssue: a single drift renders the exact expected body", () => {
  const body = renderIssue(fixture("one-drift.json"), CODEOWNERS, readStatement);
  const expected = [
    ISSUE_MARKER,
    "### Claims due & drifted",
    "",
    "The scheduled run found claims that need a human. Each is grouped by what went wrong and tagged with its CODEOWNERS owner.",
    "",
    "### Drifted claims",
    "",
    "- **`payments/libfoo-pin`** — `.claims/payments/libfoo-pin.md`",
    "  - > We pin libfoo at 4.2; 5.x corrupts CJK PDF export.",
    "  - supports:",
    "    - `requirements.txt#libfoo`",
    "  - owner: @acme/payments",
    "",
    "---",
    "Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`).",
    "",
  ].join("\n");
  assert.equal(body, expected);
});

test("renderIssue: a load error is surfaced under the faults heading", () => {
  const body = renderIssue(fixture("load-error.json"), CODEOWNERS);
  assert.match(body, /### Unresolved faults \(the tool could not determine status\)/);
  assert.match(body, /broken-malformed\.md/);
});

test("renderIssue: never uses blocking language", () => {
  const body = renderIssue(fixture("mixed.json"), CODEOWNERS);
  assert.doesNotMatch(body, /\bblocking\b/i);
  assert.doesNotMatch(body, /merge is blocked/i);
  assert.doesNotMatch(body, /required check/i);
});
