# claim — a tool that checks whether the facts you wrote down are still true

Working name: `claim`. Draft v0.4, July 2026. This replaces SPEC.md (v0.3).
It folds in a round of review and a survey of what exists as of mid-2026
(section 3 and the appendix). One line for the whole thing: **Dependabot, but
for facts.**

---

## 1. The problem

Every codebase has places where someone wrote down a reason for a decision:

- A security scanner exception: "CVE-2024-31337 doesn't apply. User input
  never reaches XmlParser."
- A dependency pin: "Stay on libfoo 4.2. Version 5.x corrupts PDF export for
  CJK fonts."
- A skipped test: "Flaky on CI. Tracked in INFRA-2041."
- An agent context file (CLAUDE.md / AGENTS.md): "Run tests with `make test`.
  Never import lodash directly, use our wrapper."

Each of these statements was true when it was written. Someone checked.
Nothing checks afterward. The person who later makes the statement false is
usually a different person, working in a different part of the code, who has
no idea the statement exists. So the statement just sits there, wrong, and
people and tools keep acting on it.

A concrete story. The CVE exception above gets filed. Eight months later,
someone adds an upload handler that calls a helper, and the helper calls
XmlParser. No test fails, because nothing was watching. The vulnerability is
now live, behind a document that says it isn't.

This got worse in the last two years for a specific reason: coding agents.
A human team keeps a lot of these facts in its heads, and the person who
breaks a fact often remembers who relies on it. Agents remember nothing
between sessions. Every session either re-checks its facts from scratch
(slow and expensive) or trusts what's written in the repo. In the working
sessions that led to this proposal, roughly a third of written-down agent
diagnoses were wrong by the time the next session read them. Agents also
read context files like CLAUDE.md at the start of every single session, so
one stale sentence gets inherited hundreds of times.

## 2. Why a test doesn't fix this

The obvious answer is "write a test." Say someone had written one:
`assert count_references("XmlParser", "src/") == 0`. Now the upload-handler
PR goes red. Watch what happens next.

The PR author looks at the failure. They see a test that forbids calling a
function, with no reason given. Nothing about their change looks wrong. So
they conclude the test is stale, update the count or delete it, a reviewer
shrugs, and it merges. The check fired, and the outcome is the same.

The problem was never detection. The problem is that the person who trips
the check doesn't know why it exists, and the person who wrote the reason
never hears that it fired. A red build asks "is your change wrong?" When the
honest answer is "no, the world changed," the wrong person edits the
assertion and the reason is lost at exactly the moment it mattered.

So a real fix has to do three things:

1. Re-check the fact automatically.
2. Carry the reason, so whoever sees the alert understands what depends on
   the fact.
3. Get the alert to the person who owns the decision, with pressure that
   grows the longer they ignore it.

Everything below is built around those three requirements. Most existing
tools do one of them. None do all three.

## 3. What exists today, and why it doesn't solve this

Surveyed July 2026. Links in the appendix.

**Tests and architecture tests (ArchUnit, import-linter, dependency-cruiser).**
These can assert facts like "nothing in src/ imports XmlParser." But a
failing test blocks the person who made the change, carries no reason, and
gets edited by that person when the change looks legitimate. Section 2 is
the whole story. Right check, wrong lifecycle.

**Expiry dates on suppressions (Trivy's `exp:` field, vulnerability
management platforms like Rapid7 and Microsoft Defender, ESLint's
expiring-todo-comments).** These force a re-review on a date. This is the
strongest existing answer and it costs almost nothing. But the date is a
guess. If the fact breaks in month two and the expiry is month twelve, you
were wrong for ten months. If nothing changed, the re-review is busywork,
and people learn to rubber-stamp renewals. The expiry also doesn't record
how to re-check the fact, so every re-review starts from zero. We keep
expiry in the design as a backstop (section 6, rule 4), not as the main
mechanism.

**Dependabot and Renovate.** The closest thing in shape: watch something you
depend on, notify the owning repo when it changes, don't block anyone. They
also carry the clearest warning. Most organizations have thousands of open
Dependabot alerts that nobody reads. Notifications with no escalation decay
into background noise. The places where Dependabot works are the places that
wired it to a gate ("can't deploy with a critical vuln older than 30 days").
Any tool with this shape needs escalation built in, not bolted on. And
Dependabot only covers one kind of fact: package versions.

**Doc-sync tools (Swimm, DeepDocs, Ferndesk, Mintlify's agent).** These
detect that docs and code have drifted apart, then update the words to match
the code, often by opening an automatic PR. For a tutorial or API reference
that's the right behavior. For a decision justification it is exactly wrong.
If a code change breaks the reason behind a security exception, rewriting
the sentence to match the new code makes the document fresher and buries the
problem. The decision needed re-review, not new prose.

**Fiberplane's drift (open source, March 2026).** The closest tool that
exists. It anchors markdown docs to source files or specific
functions/classes, fingerprints the code, and fails CI when the anchored
code changes, so the doc must be updated before merge. Three gaps. First, it
detects "code near this doc changed," not "this stated fact is now false."
Most edits to a file don't invalidate the facts written about it, so
anchors fire on noise; and the change that actually breaks a fact can happen
in a file the doc never mentioned (the upload handler was nowhere near the
exception file). Second, it blocks the person making the change, which is
the failure mode in section 2. Third, it has no owners, no history, and no
way to state a fact about the world outside the repo ("the vendor's rate
limit is 100 rps").

**agents-lint (open source, 2026).** Checks AGENTS.md and CLAUDE.md against
the repo: do the mentioned paths exist, are the npm scripts real, are the
referenced packages installed. Useful, and proof that people feel this pain.
But it can only check facts it can figure out by itself, which means
universal ones. It cannot check "we pin libfoo 4.2 because 5.x corrupts CJK
PDF export," because only the author knows how that would be checked. No
custom checks, no owners, no history.

**Sourcegraph code monitors.** A saved search that alerts a Slack channel
when new matches appear in the code. This is the detection half of
repo-facts, sold as an enterprise feature. The alert is not attached to any
decision, carries no reason, has no owner and no record. It tells a channel
"this pattern appeared," and the channel has to remember why anyone cared.

**Monitoring and alerting (Datadog, PagerDuty, and the rest).** Production
already has this whole loop: a check, an alert, a runbook explaining why it
matters, an owner on call, and escalation when nobody responds. Nobody has
built that loop for the facts underneath engineering decisions. We copy its
hard-won lessons directly: group alerts by cause, damp flapping checks, and
escalate on a clock. That field also proves what kills these systems: false
alarms. Section 9 treats false-alarm rate as the number that decides whether
this tool should exist.

**ADRs.** Capture decisions and reasoning well. Nothing ever checks them.
They rot like all prose.

**Data quality tools (Great Expectations, Soda) and compliance automation
(Vanta, Drata).** Both continuously verify facts, which shows the pattern
works. But data tools verify schemas and pipelines they own, and compliance
tools verify a fixed catalog of controls for auditors, on their platform.
Neither lets you attach your own fact, with your own check, to your own
decision, in your own repo.

**Academic work.** "Assumption-based runtime verification" and
assume-guarantee monitoring study exactly this: keep checking your stated
assumptions after deployment. It never became a developer tool.

The summary: every piece exists somewhere. Executable checks exist. Expiry
exists. Notification-to-owner exists. Decision records exist. Escalation
exists. No tool combines them: **your stated fact, with your check attached,
bound to the decision it justifies, re-checked when it could have changed,
reported to the decision's owner, with growing pressure and a permanent
record.** That combination is this proposal.

## 4. The proposal

A small command-line tool. No server, no daemon, no database. Git is the
storage and the history.

A **claim** is three things written next to a decision:

1. A fact, in plain language.
2. A check: how to re-verify the fact.
3. A schedule: when re-verification is due.

Example, in a scanner-exceptions file:

```yaml
- cve: CVE-2024-31337
  reason: "Not applicable. User input never reaches XmlParser."
  claim:
    check: { cmd: "rg -q 'XmlParser' src/", negate: true }
    recheck: on-change
    max-age: 180d
```

Example, in a CLAUDE.md:

```markdown
Never import lodash directly. Use src/lib/util instead.
<!-- claim
check: { cmd: "rg -q \"from 'lodash'\" src/", negate: true }
recheck: on-change
max-age: 90d
-->
```

Running a check produces one of four results:

- **holds** — the fact is still true.
- **drifted** — the fact is no longer true.
- **can't-tell** — the check ran but couldn't determine the answer (an agent
  check that found conflicting evidence, a network check that timed out).
- **check-broken** — the check itself failed to run. This is loud, never
  silent, and never counts as "holds."

A note on that `negate: true`, because it matters more than it looks. The
tool maps exit codes itself: 0 means the command found what it looked for,
1 means it didn't, anything else means the check is broken. If you wrote the
negation in shell instead (`! rg -q ...`), then deleting the `src/`
directory, or running on a machine without rg installed, would make the
command "fail," the `!` would flip it to success, and a broken check would
report the fact as true indefinitely. A green light that can't turn red is
worse than no light. The tool owns negation so that broken always looks
broken.

## 5. What happens when a fact drifts

Drift is not failure. Nobody's build goes red by default. Instead:

1. **The PR that caused it gets a comment.** Not a block. The comment shows
   the fact, the decision it supports, and who owns that decision: "This
   change makes 'user input never reaches XmlParser' false. That fact is the
   justification for the CVE-2024-31337 exception, owned by the security
   team." This fixes the section 2 story. The author of the change now has
   the reason in front of them at the moment they have the most context.
   Often that alone is enough: they reconsider the change, or they loop in
   the owner themselves.

2. **The decision's owner gets a review item.** The owner is looked up at
   that moment, from CODEOWNERS or the team registry, based on the file the
   decision lives in. Owners are never written into the claim itself,
   because recorded names go stale faster than any other fact in a company.
   If no owner can be resolved, that is surfaced as its own problem, since
   an unowned security exception is worse than a drifted one.

3. **Pressure grows if nobody acts.** First a comment, then a recurring
   reminder, then a line in the owning team's own merge or deploy gate, then
   a block. How fast this ladder climbs is set per claim class: a security
   exception escalates over days, a dependency pin over months. The one rule
   is that drift can never be ignored forever. A signal that can be ignored
   forever will be, which is the Dependabot lesson.

4. **The owner decides.** Three outcomes:
   - **retire** — the world changed legitimately, the decision was
     re-reviewed, the claim is closed.
   - **amend** — the fact changed shape; update the statement and the check,
     keeping the history.
   - **promote** — the fact turned out to be a rule the team wants enforced.
     Emit a real test or CI gate, with the reason attached to it this time,
     and mark the claim promoted. This is the deliberate path from "observed
     fact" to "enforced rule," and the answer to "shouldn't these just be
     tests?" Some should, after they've proven they matter, and with their
     reason carried along.

Every check result and every decision is appended to a log with a
timestamp, the commit, and who or what performed it. Over time that log is
the thing a new team member or a new agent session can actually trust: not
"someone once said X," but "X, last verified two weeks ago, verified 41
times since 2026, amended once."

## 6. The rules that keep it honest

These came out of the review of v0.3. Each one closes a specific hole.

1. **Claims only attach to writing that already exists.** The trigger for
   writing a claim is recording a decision whose justification mentions a
   fact someone else could break. If nobody would have written the sentence
   anyway, there is no claim. This bounds the corpus to tens or hundreds per
   repo, not thousands, and the marginal cost is pasting the command you
   just ran to convince yourself.

2. **Every check must be seen failing once.** When a claim is created, the
   tool requires a demonstration that the check can detect the fact being
   false (run it against a state where it should fail, record that it did).
   A check that has never failed is decoration. This is the same logic as
   never trusting a test you haven't seen red.

3. **Watched paths come from what the check actually reads.** For
   `recheck: on-change`, the tool traces which files the check reads and
   derives the watch list from that, instead of asking the author to guess.
   A check that reads the whole tree simply runs on every merge, which is
   cheap for a grep. This closes the gap where the fact-breaking change
   lands in a file the author never thought to watch. Freshness is tracked
   by hashing what the check reads, not by commit ancestry, so rebases and
   squashes don't confuse it.

4. **Every claim has a max age.** Passing checks renew it. If the check
   breaks, or keeps returning can't-tell, or the claim has no check at all,
   the max age eventually forces a human review item. This is a dead-man's
   switch. Whatever goes wrong anywhere in the system, the end state is a
   person being nagged, never a stale green light. The design rule in one
   sentence: the failure mode is a nag, never a lie.

5. **Definitions and history live in different places.** The claim sits in
   the decision file, reviewed like code. Check results append to a
   machine-written log (git notes or a log directory), which is derived
   data: rebuildable, ignorable in diffs, and never a merge conflict with a
   human edit. CI running on a fork PR has no write access anyway, so PR
   runs only report; trusted runs (main, the scheduled job) persist results.

6. **Drift arrives grouped by cause.** One refactor that breaks twelve
   claims is one review item listing twelve facts, not twelve items. A
   flapping check (network timeouts alternating with passes) gets damped
   before a human ever sees it. Repeated can't-tell has its own clock: a
   claim that has been unverifiable for 90 days is itself a problem and
   escalates on rule 4.

7. **Agent-written claims are marked and trusted less.** The number that
   motivated this proposal, a third of agent-written diagnoses being wrong,
   applies to claim authors too. So: every claim records whether a human or
   an agent established it. Security-class claims require a human. A sample
   of "holds" verdicts from agent checkers gets re-checked by a second agent
   instructed to disprove the first. And when claims are shown to agents as
   context, they are shown as dated evidence ("verified 2026-07-01: holds"),
   never as instructions, because a claims file that agents obey blindly is
   an injection channel with a trust stamp on it.

## 7. Kinds of checks, and when they run

- **cmd** — a command, cheap and deterministic. Preferred whenever a cheap
  proxy for the fact exists. Note what the check verifies: the fact, not the
  decision. "Is upstream issue #123 still open" is one API call. "Is the 5.x
  upgrade safe now" is a project. Claims only need the first. When the fact
  dies, the expensive re-evaluation happens once, on purpose, by the right
  person.
- **agent** — a plain-language instruction executed by an agent ("check
  libfoo's changelog since 5.0 for a fix to CJK PDF corruption; try the
  repro if unclear"), returning a verdict plus an evidence note that goes in
  the log. For facts with no cheap command. These run on the clock, where
  the cost amortizes: expensive per run, cheap per month.
- **human** — a scheduled review item for a named role. For judgment calls.
  Honest, used sparingly.
- **none** — allowed. The claim is then just a dated, attributed statement,
  but rule 4 still applies, so it degrades into a scheduled human look
  rather than into silence. Still strictly better than a comment.
- **Tiered** — one claim, two checks: a cheap command weekly, a deep agent
  check quarterly.

When they run follows from what can change the fact. Repo facts ("nothing
imports X") can only change when a commit lands, so they run on merges,
filtered by watched paths. World facts ("the vendor's rate limit is 100
rps") don't care about your merges, so running them per-merge is pointless;
they run on a schedule, batched. The tool ships one command,
`claim check --due`, which is cheap to call and runs only what's due. Where
you call it from (CI step, cron, an agent's session start) is your business.
The tool has no scheduler, the same way git has no opinion about when you
fetch.

## 8. What to build first

Not security exceptions. Selling to a security team means adopting a process
before anyone feels value. Start where one person gets value the same week:
**agent context files.**

CLAUDE.md and AGENTS.md are decision records by the definition in this
document: durable statements that steer future actions, whose facts other
people break without knowing. They are read at the start of every agent
session, so a stale sentence costs money every day, and a caught lie is felt
immediately as a prevented wasted session. agents-lint has proven the demand
and only covers the auto-detectable cases. The pitch for version 0.1 is one
sentence: attach a check to any sentence in your CLAUDE.md, and find out
when it stops being true.

- **v0.1** — one binary. Parses claims from CLAUDE.md/AGENTS.md comment
  blocks and from a YAML key in exception-style files. cmd checks with the
  exit-code contract from section 4. Witnessed-red on creation. Content-hash
  freshness. `check`, `list`, `log`. Output readable by humans and `--json`
  for agents. PR-comment output for CI.
- **v0.2** — drift routing and the escalation ladder, owner lookup at fire
  time, `amend`/`retire`/`promote`, max-age enforcement, grouped reports,
  agent checks.
- **Later, only if the wedge works** — the organization layer: a read-only
  index built by scanning repos (repos stay the only source of truth),
  cross-repo routing for the case where team A's decision rests on a fact
  team B just broke, and shared world-facts checked once and fanned out.
  None of this is needed to validate the idea.

## 9. How we'd know the idea is wrong

Decided in advance, because a tool like this can limp along on plausibility
forever.

The test is prospective, not historical. Replaying old incidents doesn't
count, because knowing the incident tells you which check to write. Instead:
take one active repo, write claims for its real exception/pin/skip corpus
and its agent context files, seal them, and run for one quarter. Measure
three numbers:

1. **Real drifts caught** — cases where a fact broke and the right person
   found out from the tool.
2. **False alarms** — drift reports where the fact was actually fine. This
   is the number that kills alerting systems. If more than roughly one in
   three fired reports is a false alarm, the cheap-proxy-check idea doesn't
   hold and the tool should not exist in this form.
3. **Cost to author** — minutes per claim. If it's much over five, rule 1's
   "just paste the command you ran" story is wrong.

And if a quarter passes on an actively developed repo with nothing real
caught, then premise rot is rarer than this document argues, and that is
also an answer.

---

## Appendix: survey of existing tools (July 2026)

| Tool | What it does | What's missing for this problem |
|---|---|---|
| [fiberplane/drift](https://github.com/fiberplane/drift) | Anchors markdown to files/symbols, fails CI when anchored code changes | Detects nearby edits, not fact falsity; blocks the changer; no owners, history, or world-facts |
| [agents-lint](https://github.com/giacomo/agents-lint) | Lints AGENTS.md/CLAUDE.md: paths exist, scripts exist, deps present | Only auto-detectable facts; no user-defined checks, owners, or history |
| [DeepDocs](https://deepdocs.dev), Swimm, [Ferndesk](https://ferndesk.com), Mintlify | Detect doc/code divergence, auto-update docs | Rewrites words to match code; a decision needs re-review, not new prose |
| Dependabot / Renovate | Watches dependencies, notifies owning repo | Deps only; no escalation, and its ignored-alert graveyards are the cautionary tale |
| [Trivy `exp:`](https://trivy.dev), Rapid7/Defender exceptions, expiring TODOs | Expiry dates force re-review | Date is a guess; no record of how to re-check; kept here only as backstop |
| [Sourcegraph code monitors](https://sourcegraph.com/docs/code-monitoring) | Saved search alerts a channel on new matches | Alert tied to nothing; no reason, owner, or record |
| ArchUnit / import-linter | Asserts structural facts in CI | Blocks the changer without the reason; section 2 failure |
| Datadog / PagerDuty | Check, alert, runbook, owner, escalation for production | Right loop, wrong layer; nobody applies it to decision premises |
| Vanta / Drata | Continuously verifies compliance controls | Fixed catalog, their platform, not your facts in your repo |
| Great Expectations / Soda | Executable checks on data | Pipeline-owned, schema-scoped, not tied to decisions |
| ADR tooling | Records decisions and context | Nothing is checked, ever |
| Academic assume-guarantee monitoring | Re-checks stated assumptions at runtime | Never left the literature |

Searched: GitHub (new repos 2025–2026 matching assumption/claim/drift/context-rot),
web search for startups and launches, HN. Note that "context rot" in 2026
mostly names a different problem (model degradation over long chat contexts,
after Chroma's 2025 report), not stale context files. No tool found that
combines author-stated facts, attached checks, decision binding, owner
routing, escalation, and history. The neighborhood is active. The
combination is open.
