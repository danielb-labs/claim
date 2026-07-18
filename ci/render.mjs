// Turn a `claim check --json` report into the markdown a human sees, and decide
// whether there is anything to say at all. This is the load-bearing logic of both
// CI lanes: the workflow YAML only runs `claim`, calls this, and hands the result
// to the GitHub API. Everything a reviewer might get wrong — grouping drift from
// broken checks, rendering supports, finding the file owner, deciding clean vs.
// dirty — lives here where it is unit-tested against real `claim check` JSON, not
// buried in inline `github-script` where it cannot be.
//
// Two consumers, one renderer: the on-change lane renders a PR comment, the clock
// lane renders an issue body. Both share `classify` and the grouped `findingsBody`;
// they differ only in framing text and the marker, so a fix to the grouping fixes
// both.
//
// No dependencies, by policy (CLAUDE.md: every dependency is attack surface). Node's
// standard library only. ESM so the same file is both a CLI (`node render.mjs`) and
// an importable module the tests exercise.

import { readFileSync } from "node:fs";
import { join } from "node:path";

// A hidden HTML marker stamped at the top of every rendered body. The workflow finds
// its single existing comment/issue by this exact string and edits it in place, so a
// hundred pushes leave one comment, not a hundred. The marker must never change once
// shipped or old comments become unfindable and the lane starts spamming; if it ever
// must change, the workflow has to search for both. Kept distinct per lane so a repo
// that runs both never confuses a PR comment for the standing issue.
export const COMMENT_MARKER = "<!-- claim-bot:on-change -->";
export const ISSUE_MARKER = "<!-- claim-bot:clock -->";

// A claim's rendered category, in escalation order. `broken` is loudest: the check
// could not run, so we know nothing — invariant #1, a broken check is never a pass
// and is surfaced most prominently. `drifted` means the fact is no longer true.
// `unresolved` means a `supports` target (a decision the claim justifies) vanished —
// the claim went loud rather than staying quietly green (docs/design/PRODUCT.md section 4).
const CATEGORY = {
  broken: { key: "broken", heading: "Broken checks", rank: 3 },
  drifted: { key: "drifted", heading: "Drifted claims", rank: 2 },
  unresolved: { key: "unresolved", heading: "Unresolved supports", rank: 1 },
};

/**
 * Classify one claim result from `claim check --json`'s `claims[]` into the single
 * most severe category it falls into, or `null` if it is entirely clean.
 *
 * Severity, highest first: a broken check (couldn't run — we know nothing), then a
 * drift (the fact is false), then an unresolved support (a decision it justified is
 * gone). A claim is reported under exactly one heading — its worst — so a claim that
 * is both broken and has an unresolved support appears once, under "Broken", not
 * twice. This mirrors the tool's own exit-code contract (2 > 1 > 0) so the comment
 * and the exit code never tell different stories.
 */
export function classifyClaim(claim) {
  const verdicts = (claim.checks || []).map((c) => c.verdict);
  if (verdicts.includes("broken")) return CATEGORY.broken;
  if (verdicts.includes("drifted") || verdicts.includes("unverifiable")) {
    return CATEGORY.drifted;
  }
  const anyUnresolved = (claim.supports || []).some((s) => s.resolved === false);
  if (anyUnresolved) return CATEGORY.unresolved;
  return null;
}

/**
 * Match a repo-relative path against CODEOWNERS and return the owners for the
 * last matching pattern, or `[]` if none matches.
 *
 * CODEOWNERS semantics that matter here (GitHub's rules): later patterns win over
 * earlier ones, so we scan top-to-bottom and keep the last match. A trailing `/`
 * matches a directory and everything under it; a leading `/` anchors to the repo
 * root; a bare `*` matches everything. We implement the common subset — directory
 * prefixes, `*` globs, and exact paths — which covers what a claims store needs.
 * Owners are the whitespace-separated tokens after the pattern (`@user`, `@org/team`,
 * or an email); comments (`#`) and blank lines are skipped.
 *
 * This is deliberately a small, testable matcher rather than a dependency: getting
 * "the last matching rule wins" right is the whole job, and it is one loop.
 */
export function ownersFor(path, codeownersText) {
  if (!codeownersText) return [];
  let owners = [];
  for (const raw of codeownersText.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const parts = line.split(/\s+/);
    const pattern = parts[0];
    const patternOwners = parts.slice(1).filter((t) => t.length > 0);
    if (matchesPattern(path, pattern)) owners = patternOwners;
  }
  return owners;
}

/**
 * Does a CODEOWNERS `pattern` match a repo-relative `path`?
 *
 * Supports the subset a claims store uses: `*` (everything), a leading-slash anchor,
 * a trailing-slash directory prefix, and `*` as a single path-segment wildcard. A
 * pattern with no slash matches by basename anywhere in the tree, as GitHub does.
 */
function matchesPattern(path, pattern) {
  if (pattern === "*") return true;

  // A pattern with no `/` (other than a possible trailing one) matches the basename
  // anywhere in the tree: `*.md` matches `docs/x.md`.
  const hasInteriorSlash = pattern.replace(/\/$/, "").includes("/");
  const anchored = pattern.startsWith("/");

  if (pattern.endsWith("/")) {
    // Directory prefix. GitHub anchors it to the repo root ONLY when it has a leading
    // slash or an interior slash (`/payments/`, `.claims/payments/`); a bare
    // `payments/` matches a `payments` directory at ANY depth. Anchoring a bare
    // directory pattern to root — the earlier bug — misroutes every claim in a store
    // that lives under `.claims/` when an org writes the natural `payments/` rule.
    const dir = pattern.replace(/^\//, "").replace(/\/$/, "");
    if (anchored || hasInteriorSlash) {
      return path === dir || path.startsWith(dir + "/");
    }
    // Unanchored: match `dir/…` at any depth, i.e. a path segment equal to `dir`
    // followed by more path.
    return new RegExp("(^|/)" + escapeRegExp(dir) + "/").test(path);
  }

  const glob = pattern.replace(/^\//, "");
  const re = globToRegExp(glob, anchored || hasInteriorSlash);
  return re.test(path);
}

/** Escape a literal string for embedding in a RegExp. */
function escapeRegExp(s) {
  return s.replace(/[.+*?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Compile a CODEOWNERS glob to a RegExp. `*` matches any run of non-`/` characters.
 * When `anchored`, the pattern must match from the repo root; otherwise it may match
 * any suffix path segment (basename-style).
 */
function globToRegExp(glob, anchored) {
  const escaped = glob.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, "[^/]*");
  return anchored ? new RegExp("^" + escaped + "$") : new RegExp("(^|/)" + escaped + "$");
}

/**
 * Extract the plain-language statement from a standalone claim file's markdown.
 *
 * `claim check --json` does not carry the statement (only `id`, `file`, verdicts, and
 * supports), and the on-change lane runs report-only so `claim drift` — which reads
 * persisted status — sees nothing to join against. The statement is the "why anyone
 * cares" the comment exists to show, so we read it from the file the checkout already
 * has: the markdown body after the closing `---` of a standalone claim's frontmatter.
 *
 * Deliberately narrow: it handles the standalone one-file-per-claim form only.
 * Embedded claims (a `<!-- claim` block inside CLAUDE.md) put the statement above the
 * opener and are not parsed here; those, and any read failure, return `null` and the
 * comment simply omits the statement rather than guessing. Correctness over coverage —
 * a wrong statement would be worse than a missing one. The authoritative parser is in
 * `claim-core`; this is a display convenience, not a second source of truth.
 */
export function statementFromFile(text) {
  if (typeof text !== "string") return null;
  // Frontmatter is fenced by a leading `---` line and a closing `---` line; the
  // statement is the body after the close. An embedded-claim host file does not open
  // with `---`, so this returns null for it.
  const lines = text.split("\n");
  if (lines[0].trim() !== "---") return null;
  let close = -1;
  for (let i = 1; i < lines.length; i++) {
    if (lines[i].trim() === "---") {
      close = i;
      break;
    }
  }
  if (close === -1) return null;
  const body = lines.slice(close + 1).join("\n").trim();
  return body === "" ? null : firstLine(body);
}

/**
 * The full classification of a `claim check --json` report: the reportable claims
 * (broken/drifted/unresolved) with their owners resolved, the load errors, the
 * per-claim faults, and a single `clean` flag the caller keys the whole decision on.
 *
 * `clean` is true only when there is nothing to route: no reportable claim, AND no
 * load error, AND no per-claim fault. Each of the latter two is never clean —
 *
 * - A load error (`errors[]`: a malformed or duplicate-id claim file) floors the
 *   tool's exit at 2, because a claim file that will not parse is a silent gap.
 * - A fault (`notes[]`: a claim whose own check ran, but whose verdict log could not
 *   be read) *also* floors the tool's exit at 2. The check may have held, but the tool
 *   cannot compute the claim's status from an unreadable log — so it does not know the
 *   fact holds. Ignoring `notes[]` here would render "the store is clean" and let the
 *   clock lane CLOSE its nag while the tool is saying "I can't tell." That is a false
 *   green, invariant #6, in the CI layer; the fault keeps the store dirty.
 */
export function classify(report, codeownersText, readStatement) {
  const items = [];
  for (const claim of report.claims || []) {
    const category = classifyClaim(claim);
    if (!category) continue;
    items.push({
      id: claim.id,
      file: claim.file,
      category: category.key,
      owners: ownersFor(claim.file, codeownersText),
      statement: readStatement ? readStatement(claim.file) : null,
      checks: claim.checks || [],
      supports: claim.supports || [],
    });
  }
  const errors = report.errors || [];
  const notes = report.notes || [];
  return {
    clean: items.length === 0 && errors.length === 0 && notes.length === 0,
    items,
    errors,
    notes,
    exit: report.exit,
    reportOnly: report.report_only === true,
  };
}

/** Group classified items by category, in escalation order (broken first). */
function groupByCategory(items) {
  const order = [CATEGORY.broken, CATEGORY.drifted, CATEGORY.unresolved];
  return order
    .map((cat) => ({ cat, items: items.filter((it) => it.category === cat.key) }))
    .filter((g) => g.items.length > 0);
}

/** Render one claim's owners as a mention string, or a routing note when unowned. */
function ownersLine(owners) {
  if (owners.length === 0) {
    // An unowned claim is a routing dead-letter, a first-class problem, not a
    // dropped notification (docs/design/PRODUCT.md section 5). Say so instead of omitting it.
    return "_owner: unknown (no CODEOWNERS match — routing dead-letter)_";
  }
  return "owner: " + owners.join(" ");
}

/** Render one classified claim: id, file, statement, why it fired, supports, owner. */
function claimBlock(item) {
  // `id` and `file` come from claim files, so defang them too before they land in a
  // code span, even though the schema constrains them.
  const lines = [];
  lines.push(`- **${code(item.id)}** — ${code(item.file)}`);

  // The statement is the fact in plain language — "what your change broke". Show it
  // when we could read it; omit the line entirely rather than print an empty quote.
  // Defanged: a statement is prose a person wrote and can contain an @handle.
  if (item.statement) {
    lines.push(`  - > ${defang(item.statement)}`);
  }

  // The broken check's evidence is the single most useful thing to show: it says
  // *why* it could not run (missing binary, missing file). Surface the first line.
  if (item.category === "broken") {
    const broken = item.checks.find((c) => c.verdict === "broken");
    if (broken) {
      lines.push(`  - check ${detailFor(broken)}`);
      const ev = firstLine(broken.evidence);
      if (ev) lines.push(`  - ${code(ev)}`);
    }
  }

  // Supports are the decisions this claim justifies — "why anyone cares". Show them
  // all; mark the ones that no longer resolve, since those are themselves a finding.
  if (item.supports.length > 0) {
    lines.push("  - supports:");
    for (const s of item.supports) {
      const mark = s.resolved === false ? " — **unresolved** (target is gone)" : "";
      lines.push(`    - ${code(s.target)}${mark}`);
    }
  } else {
    lines.push("  - supports: _(nothing declared)_");
  }

  lines.push("  - " + ownersLine(item.owners));
  return lines.join("\n");
}

/** The `detail` line for a check, defensively defaulting when absent. */
function detailFor(check) {
  return check && check.detail ? check.detail : "could not run";
}

/** The first non-blank line of a possibly-multiline evidence string, or `null`. */
function firstLine(text) {
  if (!text) return null;
  for (const line of String(text).split("\n")) {
    const t = line.trim();
    if (t) return t;
  }
  return null;
}

/**
 * Neutralize free-text that came from a claim file, a tool error, or evidence before
 * it goes into markdown, so it cannot address people or forge links in the comment or
 * issue. This text can carry fragments of the offending file — an error message
 * quoting a path, a statement someone wrote — so an `@mention` or a bare URL in it
 * would otherwise render live and ping unrelated users or autolink. We defang the
 * `@` that starts a mention (`@team` -> `@​team`) and the `://` that starts an
 * autolink, and collapse backticks so the text cannot break out of a code span. This
 * is display hygiene, not a security boundary: the content is still shown, just inert.
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

/**
 * Render the faults section: unloadable claim files (`errors[]`) and per-claim
 * verdict-log read faults (`notes[]`). Both mean the tool could not determine a
 * fact's status, so both are surfaced under one loud heading and both keep the store
 * dirty (see `classify`). Free-text (`e.message`, a note) is defanged so a path or
 * message it quotes cannot mention people or forge links.
 */
function faultsSection(errors, notes) {
  if (errors.length === 0 && notes.length === 0) return null;
  const lines = ["### Unresolved faults (the tool could not determine status)", ""];
  for (const e of errors) {
    lines.push(`- \`${defang(e.file)}\`: ${defang(e.message)}`);
  }
  for (const note of notes) {
    lines.push(`- ${defang(note)}`);
  }
  return lines.join("\n");
}

/**
 * Render the shared body of grouped findings (used by both the comment and the
 * issue). Returns the markdown between the header and the footer.
 */
function findingsBody(classified) {
  const sections = [];
  for (const group of groupByCategory(classified.items)) {
    sections.push(`### ${group.cat.heading}`);
    sections.push("");
    sections.push(group.items.map(claimBlock).join("\n"));
    sections.push("");
  }
  const faults = faultsSection(classified.errors, classified.notes);
  if (faults) {
    sections.push(faults);
    sections.push("");
  }
  return sections.join("\n").trimEnd();
}

/**
 * Render the on-change PR comment body.
 *
 * The comment is advisory and idempotent: it opens with `COMMENT_MARKER` so the
 * workflow edits its one existing comment rather than posting a new one each push,
 * and it never asks CI to fail — invariant #6, drift routes, it does not block the
 * person making the change. When the store is clean this returns a short all-clear so
 * a previously-red comment visibly goes green (the alternative, deleting the comment,
 * loses that signal).
 */
export function renderComment(report, codeownersText, readStatement) {
  return commentFrom(classify(report, codeownersText, readStatement));
}

/** Render the comment body from an already-classified report (classify once). */
function commentFrom(classified) {
  const out = [COMMENT_MARKER];
  if (classified.clean) {
    out.push("### claim: all checks held");
    out.push("");
    out.push("Every on-change check passed and every support resolved. Nothing to review.");
    return out.join("\n") + "\n";
  }
  out.push("### claim: this PR affects recorded facts");
  out.push("");
  out.push(
    "These claims changed state under your diff. This is **advisory** — it never blocks the merge; it routes what changed to whoever owns the decision it rested on.",
  );
  out.push("");
  out.push(findingsBody(classified));
  out.push("");
  out.push(footer(classified));
  return out.join("\n") + "\n";
}

/**
 * Render the clock-lane standing-issue body.
 *
 * Same grouping and owner resolution as the comment; different framing (a queue of
 * what is due-and-broken across the store, not a diff) and a different marker so the
 * find-or-create step never confuses it with a PR comment. The caller closes the
 * issue when `classified.clean`; this still renders a clean body for the transition.
 */
export function renderIssue(report, codeownersText, readStatement) {
  return issueFrom(classify(report, codeownersText, readStatement));
}

/** Render the issue body from an already-classified report (classify once). */
function issueFrom(classified) {
  const out = [ISSUE_MARKER];
  if (classified.clean) {
    out.push("### The claim store is clean");
    out.push("");
    out.push("No claim is drifted, broken, or missing a support. This issue will be closed.");
    return out.join("\n") + "\n";
  }
  out.push("### Claims due & drifted");
  out.push("");
  out.push(
    "The scheduled run found claims that need a human. Each is grouped by what went wrong and tagged with its CODEOWNERS owner.",
  );
  out.push("");
  out.push(findingsBody(classified));
  out.push("");
  out.push(footer(classified));
  return out.join("\n") + "\n";
}

/** The shared footer: the exit-code reading and the advisory reminder. */
function footer(classified) {
  const parts = [];
  parts.push("---");
  const broken = classified.items.some((it) => it.category === "broken");
  const cannotTell = broken || classified.errors.length > 0 || classified.notes.length > 0;
  if (cannotTell) {
    parts.push(
      "A **broken** check, an unloadable file, or an unreadable verdict log means the tool could not tell whether the fact holds — treat it as failing, not passing.",
    );
  }
  parts.push("Resolve by fixing the claim (`claim amend`) or closing it (`claim retire`).");
  return parts.join("\n");
}

// --- CLI entry point ------------------------------------------------------------
//
// `node render.mjs --mode comment|issue [--codeowners PATH] [--report FILE]`
// reads the `claim check --json` report from `--report` or stdin, resolves owners
// from `--codeowners` (default `.github/CODEOWNERS`), and prints the rendered body to
// stdout. It also sets its own exit code to mirror the finding — 0 clean, non-zero
// dirty — but note the *workflow* never fails the PR on this: the on-change job runs
// this in a step whose result is only used to build the comment. This exit code is
// for the clock lane and for local testing.

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
  const mode = args.mode || "comment";
  const codeownersPath = args.codeowners || ".github/CODEOWNERS";
  let codeownersText = "";
  try {
    codeownersText = readFileSync(codeownersPath, "utf8");
  } catch {
    // No CODEOWNERS is legal — owners simply resolve to "unknown" and the body says
    // so. A missing file must never crash the lane and swallow the whole report.
    codeownersText = "";
  }
  const raw = args.report ? readFileSync(args.report, "utf8") : readFileSync(0, "utf8");
  const report = JSON.parse(raw);

  // Resolve statements from the checkout the workflow already has. `--root` is the
  // store root the `file` paths in the report are relative to (the repo root by
  // default). A read failure is not fatal: the statement line is simply omitted.
  const root = args.root || ".";
  const readStatement = (file) => {
    try {
      return statementFromFile(readFileSync(join(root, file), "utf8"));
    } catch {
      return null;
    }
  };

  // Classify once, then both the body and the exit code derive from it.
  const classified = classify(report, codeownersText, readStatement);
  const body = mode === "issue" ? issueFrom(classified) : commentFrom(classified);
  process.stdout.write(body);
  // Exit 0 clean, 1 dirty. The on-change workflow does not gate on this (it only uses
  // the body); the clock workflow uses it to decide open-vs-close the standing issue.
  process.exitCode = classified.clean ? 0 : 1;
}

// Run as a CLI only when invoked directly, so importing this module for tests has no
// side effects.
if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
