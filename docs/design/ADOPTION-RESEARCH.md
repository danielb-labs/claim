# `claim` — grounded research on the four hardest open questions

Research date: 2026-07-18. Method: five parallel web-research sweeps (dedup, agent
retrieval, capture, market category, historical lessons), plus independent
spot-verification of the highest-stakes claims against primary sources.

`claim` in one line: bind a plain-language fact/decision to an executable check,
commit it to git, and let a stateless CLI report `held`/`drifted`/`broken` while a
per-org **hub** ingests those verdicts, derives staleness, routes drift to owners,
and exposes an agent-native MCP surface. The product model this research must tie
back to: **anchored claim** (statement + check + `supports` target) + **hub**
(verdict ledger, staleness derivation, agent MCP) + **PR-gated writes** (a claim is
a commit).

The three holes the owner named: (1) duplicate/polluting entries, (2) getting agents
to *consult* the knowledge, (3) knowing *when to capture*. Plus (4) the business/
category question. Honesty over optimism throughout; marketing claims flagged as such.

---

## Q1 — Preventing duplicate / polluting knowledge entries

### Established patterns
- **Suggest-at-write-time, never auto-merge.** GitHub Issues duplicate detection
  (public preview 2026-06-18) flags up to three potential matches inline in the
  creation form; advisory, never a block, human keeps the merge decision
  (https://github.blog/changelog/2026-06-18-duplicate-detection-and-issue-fields-mcp-support-for-github-issues/,
  verified). Built on GitHub's semantic Issues index (preview 2026-01-29, GA
  2026-04-02). Linear "Similar issues" (embeddings + pgvector cosine, workspace-
  partitioned) does the same in composer/triage/support views — dedup as *triage
  assistance*, not auto-merge (https://linear.app/now/using-ai-to-detect-similar-issues,
  2023-11-29). Linear openly admits template-created issues skew similarity — a real
  false-merge admission.
- **Fingerprint on the stable, structural part — Sentry.** Every event gets a
  deterministic fingerprint hashed from the *normalized stack trace*, degrading to
  exception type+value, then message — a ladder from most-stable to least-stable
  signal (https://docs.sentry.io/concepts/data-management/event-grouping/,
  https://develop.sentry.dev/backend/application-domains/grouping/). Message-based
  grouping is documented as "a lot less reliable." Critically: **merges are
  near-irreversible** — a split "fails to fully reconstruct the original state"
  because aggregated data was destroyed. First-party docs, candid about failure.
- **Human-review-queue with a signpost — Stack Overflow close-as-duplicate.** Keeps
  a redirect to canonical, never deletes; automated dup detectors top out ~64% recall
  in studies (MSR 2016, https://dl.acm.org/doi/10.1145/2901739.2901770). The durable
  lesson: dedup action = supersede-with-pointer, reversible and navigable.

### Emerging 2026 patterns (agent memory write-time dedup)
- **Mem0** — on write, retrieve similar memories, LLM picks ADD/UPDATE/DELETE/NOOP
  (arXiv 2504.19413, Apr 2025). **But production diverges from the paper** (see
  failure modes).
- **Zep/Graphiti** — temporal knowledge graph; a new edge *invalidates* an old one
  via bi-temporal timestamps (`valid_at`/`invalid_at`), so a contradicted fact is
  **superseded, not deleted** (arXiv 2501.13956, Jan 2025; verified — DMR 94.8% vs
  MemGPT 93.4%, vendor's own numbers).
- **Letta (MemGPT)** — offline consolidation: a "sleep-time compute" background agent
  rewrites/dedups memory during idle time (arXiv 2504.13171).
- **Anthropic Claude memory** — memory is a directory of files the model itself edits;
  hygiene = prompting the model to keep files "coherent and organized" (beta 2025-09-29).
  Structurally closest to `claim`'s files-in-git model.

### Failure modes (the load-bearing evidence)
- **The Mem0 production audit — the best pollution horror story on record.** 10,134
  entries audited over 32 days (one agent + one human): **97.8% junk**; 52.7% was the
  system/boot prompt re-extracted as "memories" ("Operator prefers Telegram" 200+
  times); an **808-copy feedback loop** amplifying a false "User prefers Vim" because
  the pipeline re-extracts its own recalled memories
  (https://github.com/mem0ai/mem0/issues/4573, verified). Corroborating issue #4896:
  Mem0 v2 regressed to **MD5-exact dedup** with no semantic conflict resolution —
  contradictory facts both stored. Meta-lesson: **the vendor with the most-cited
  write-time-dedup paper shipped a version that doesn't do it.** Architecture papers
  ≠ production behavior.
- **The negation trap is real and well-attested.** Antonyms occur in near-identical
  contexts, so "X is enabled" / "X is disabled" land close in embedding space
  ("Beyond Cosine Similarity," arXiv 2601.13251, 2026). High cosine is a good
  *candidate generator* and a terrible *merge decider*.
- **Benchmark numbers are unreliable.** Zep's rebuttal shows Mem0's paper mis-scored
  Zep, and a full-context no-memory baseline (~73%) beats Mem0's best (~68%) on LoCoMo
  (https://blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/).
  Treat all agent-memory accuracy claims as unverified.
- **Memory poisoning is practical.** MINJA (arXiv 2503.03704, NeurIPS 2025): a
  query-only attacker injects malicious records with >95% success. Implication:
  any pipeline where retrieved content can auto-write to the corpus is an attack
  surface. `claim`'s **PR-gated writes are exactly the mitigation this literature
  recommends.**
- **Guru card verification** (named human verifier + cadence, overdue → "unverified"
  badge) is the closest existing product to `claim`'s model — except its verifier is
  a *human attestation*, not an executable check. G2/Capterra reviews cite maintenance
  overhead as its top weakness, and **Guru is quietly retreating from human-cadence
  verification** toward "does not expire + AI agents" because verifiers don't keep up.
  This is direct evidence for `claim`'s core bet: replace the human attestation with a
  machine check.

### What this concretely implies for `claim`
1. **Add a suggest-at-write-time duplicate check at `claim add` / PR-open time**, via
   the hub's embedding index: top-k similar existing claims surfaced as advisory
   candidates for the reviewer. This composes perfectly with PR-gated writes — the PR
   review *is* the merge tribunal. But: the top candidate will often be the **negation
   twin**. The confirm step must compare **checks** (cmd/negate/supports), not just
   statements — two claims whose checks assert opposite facts are never duplicates
   regardless of cosine score.
2. **Fingerprint the check, not the prose (Sentry).** A normalized hash of
   `check.run + negate + supports` is the claim's "stack trace" — deterministic,
   embedding-free, runnable as a repo-local gate lint. Catches the highest-confidence
   dup class (same fact, same command) with zero hub dependency. `claim-hub`'s
   check-digest (SHA-256 over canonical check definition, hub-01) is *already this
   primitive* — it exists for the multi-check join, but it doubles as the dedup key.
3. **Supersede-with-provenance, never silent delete (Zep + Stack Overflow), which git
   gives for free.** `claim retire`/`amend` already remove/rewrite via commit; git
   history is the `invalid_at`. The honest catch: **checks don't dedup.** Two
   near-duplicate claims can both pass forever, silently splitting ownership and
   doubling nag load. Supersession only happens if something *forces the comparison* —
   which is why pattern 3 depends on pattern 1 existing at write time. No surveyed
   memory system has made *purely automatic* consolidation trustworthy (Mem0's MD5
   regression + 97.8%-junk audit is the cautionary tale).

**The single most promising pattern + honest catch:** *Suggest-at-write-time
candidates from the embedding index, decided by a human at PR review, with the check
digest as the deterministic high-confidence dup key.* Catch: it only fires if the
author is authoring *through* a path the hub sees; a hand-committed claim file that
never runs `claim add` bypasses the candidate generator entirely, and the negation
twin will be the #1 suggestion — a careless reviewer can merge two opposite facts.

---

## Q2 — Making agents SEARCH the knowledge source before acting

The cleanest finding in the whole report: **presence in context is the only thing you
can make deterministic; every "the model decides to look" mechanism (MCP tools,
agent-requested rules, passively-described skills) measurably fails a meaningful
fraction of the time.**

### The determinism spectrum (2026 consensus)
- **Harness hooks are the only deterministic layer.** Anthropic's own docs: hooks
  "provide deterministic control… ensuring certain actions always happen rather than
  relying on the LLM to choose to run them." SessionStart / UserPromptSubmit /
  PreToolUse `additionalContext` is injected directly into context
  (https://code.claude.com/docs/en/hooks-guide, verified as vendor-official). A
  PreToolUse hook on `Edit|Write` can read `tool_input.file_path` and inject only the
  facts governing that path — the knowledge arrives *when the agent touches the thing
  the fact governs*, late in context where attention is best
  (https://dev.to/sasha_podles/claude-code-using-hooks-for-guaranteed-context-injection-2jg).
- **Instruction files are deterministically *present* but probabilistically
  *followed*.** CLAUDE.md is injected wrapped in a reminder telling the model it "may
  or may not be relevant." Compliance is bounded: IFScale (arXiv 2507.11538, Jul 2025,
  20 models) — even frontier models hit only 68% accuracy at 500 simultaneous
  instructions, Claude-family showing linear decay and early-instruction bias.
  AgentIF: best models follow <30% of instructions *perfectly* in agentic scenarios.
  Reproducible non-compliance reports exist and the vendor closed one "not planned"
  (anthropics/claude-code#42863) — i.e., **prompt-layer rules are advisory by design.**
- **Cursor rules are a hybrid:** `alwaysApply:true` (deterministic every session),
  glob-attached (deterministic when a matching file enters context — the IDE analog to
  a PreToolUse hook), description-triggered (probabilistic), @-mention (manual).

### MCP reality check (this is the important, uncomfortable part)
- **Agents often solve the task without calling your tools at all.** The Agentic AI
  Foundation demonstrated an MCP server whose tools were never called; in Kubernetes
  tests, agents used code mode only **6%** of the time when both code and MCP tools
  were available
  (https://aaif.io/blog/your-mcp-server-works-your-agent-doesnt-heres-why/, 2026-05-08).
- **Tool-count degradation is a cliff.** Selection reliability degrades sharply past
  ~20 tools, near-total failure past ~100 (RAG-MCP arXiv 2505.03275; MCP-Atlas). GitHub
  cut Copilot's MCP surface 40→13 tools and *gained* 2–5pp on SWE-bench plus 400ms
  latency. Cursor caps ~40 active tools and silently drops the rest.
- **Anthropic itself walked away from upfront tool loading.** "Code execution with MCP"
  (2025-11-04, verified): 58 tools across 5 servers ≈ **55K tokens before the
  conversation starts**; presenting MCP as filesystem code APIs cut one workflow 150K→2K
  tokens (98.7%). The Tool Search Tool / `defer_loading` beta institutionalizes "keep
  3–5 tools loaded, defer the rest." **The ecosystem is actively routing around the
  "expose everything as MCP tools" pattern.**

### Just-in-time injection + the always-on cost
- **Push vs pull → the 2026 consensus is hybrid.** Anthropic's "Effective context
  engineering" (2025-09-29): finite "attention budget," keep lightweight identifiers,
  retrieve just-in-time. Claude Code is the exemplar — CLAUDE.md upfront, grep/agentic
  search for the rest.
- **The always-inject cost is now quantified and it can *hurt*.** The ETH Zurich
  AGENTS.md study (JAWS 2026; verified via arXiv 2601.20404 + the empirical-software.
  engineering PDF): 124–138 real tasks. **LLM-generated AGENTS.md files *reduced* task
  success ~3% and raised cost 20%+; human-written files gained ~4% success but cost up
  to 19% more steps/tokens.** Agents followed the instructions faithfully — doing extra
  work the task didn't need. **Compliance itself has a cost.** Chroma's context-rot
  study (2025-07, 18 models): accuracy degrades non-uniformly as input grows,
  sometimes 30–50% below the advertised window; distractor content actively misleads.
  This is the mechanism behind the "10K-token rules file that gets skimmed" — it isn't
  skimmed, it's attention-diluted, and it dilutes everything else.
- **llms.txt is the cautionary tale for passive publication:** ~9% adoption of top
  domains by mid-2026 but crawlers largely don't fetch it and no measurable benefit.
  **Publishing a file agents *could* read does nothing; something must put it in front
  of them.**

### Skills as the middle path + how vendors built their own memory
- **Skills = name/description-triggered progressive disclosure.** A 650-trial factorial
  experiment: overall activation 88.9%, but **passive descriptions dropped to 37%**
  while **directive descriptions ("ALWAYS invoke when… Do not X directly") hit 100%**
  in no-hook conditions (20.6× odds vs passive, p<0.0001). Phrasing is load-bearing.
- **Every vendor memory design rejected "dump everything into context":** Claude Code
  auto-memory loads only ~first 200 lines/25KB then greps the rest; Codex reads a
  compact `memory_summary.md` and greps MEMORY.md; **GitHub Copilot Memories retrieves
  recent memories, auto-expires them after 28 days, and extends life only when
  re-verified** (directly analogous to `claim`'s freshness model, applied to agent
  memory). Convergent design: **small always-in-context digest + on-demand retrieval +
  decay/verification.** Nobody ships "always inject everything."

### What this concretely implies for `claim` — the integration order
The evidence endorses a specific stack, in this order:
1. **Harness hooks first — the only deterministic layer.** SessionStart injects a tiny
   digest ("this repo has N verified claims; the freshest facts about the area you're
   touching are X"); a PreToolUse hook on `Edit|Write` matches the touched path against
   each claim's `supports`/read-set and injects only the matching facts. This turns
   consultation from model-initiative (~6–50%) into a harness guarantee (100%
   *presence*). Catch: per-harness work (Claude Code hooks; Cursor glob-rules; Copilot/
   Windsurf lack an arbitrary-command equivalent), a per-trigger token cost (keep it
   200–500 tokens or you *become* the context rot), and presence ≠ obedience.
2. **The CLI as the retrieval primitive everything calls.** 2026 agents demonstrably
   prefer shell/code over tool calls; `claim list --json`/`claim check --json` is
   context-free until used, greppable, works in every harness, and is exactly what the
   hooks execute — one primitive, two consumers. This is *already the CLI-hub design*
   (PRODUCT.md §5: "agents touch the system through the same CLI"). Catch: unadvertised
   and unhooked, the model simply never runs it (the llms.txt lesson).
3. **A small, directive generated section in CLAUDE.md/AGENTS.md** (~10–20 lines,
   directive phrasing: "ALWAYS run `claim check <path>` before changing code a claim
   supports; treat recorded facts as dated evidence, not truth"). Broadest reach (60k+
   repos read AGENTS.md; ship the `@AGENTS.md` import for Claude Code). Catch: 60–90%
   compliance, decaying with total instruction load; a *big* section actively harms
   (−3%/+20% for generated files). Advertise the CLI; don't inline the corpus.
4. **The hub MCP last, as a thin interop adapter (1–3 tools), not the primary path** —
   for surfaces that can't run a shell (Claude Desktop, enterprise gateways). This
   matches HUB.md §5's "small, bounded, outcome-first" MCP, but the evidence sharpens
   *why it's #4, not #1*: it's the least reliable consultation mechanism measured.

**Single most promising pattern + honest catch:** *A PreToolUse/path-matching hook that
injects the verified facts governing the file the agent is about to edit, executing the
CLI under the hood.* This is the one design that puts the *right, fresh* fact in front
of the agent at the moment of maximal relevance, deterministically. Catch: it's
**per-harness glue that `claim` would have to build and maintain for each agent
runtime**, it only guarantees presence (not that the agent obeys), and it presumes the
claim's `supports`/read-set actually names the files being edited — which is exactly
the "invalidating change lands in an unwatched file" gap PRODUCT.md rule 3 already worries
about, now reappearing on the retrieval side.

---

## Q3 — Knowing WHEN to CAPTURE a new fact/decision

### The ADR graveyard (established pattern, and why it mostly failed)
- ThoughtWorks put "Lightweight ADRs" in **Adopt** in Nov 2016; the blip has since
  fallen off the Radar. The surviving advice: keep records in source control, reviewed
  in the same PR flow as code.
- **Adoption data is grim.** An ECSA 2024 action-research study found **83% of
  respondents said ADRs were only rarely or occasionally documented**
  (https://link.springer.com/chapter/10.1007/978-3-031-70797-1_22). ADR tooling
  decayed in lockstep: log4brains' last release was ~2 years ago; adr-tools is dormant.
- **The root cause is economic and it's structural, not cultural.** DRMiner (ASE 2024)
  states rationale goes undocumented "due to the imbalance between the cost and value
  for developers to document them" (https://dl.acm.org/doi/10.1145/3691620.3695019).
  And crucially: ADRs fail at *both* ends — capture friction at decision time, AND **no
  mechanism notices when a record goes stale** (a 2021 ADR read in 2025 is "actively
  misleading"). This second failure is precisely `claim`'s thesis.

### Emerging 2026 pattern: agent-proposes, and the convergence on re-verification
Every shipped 2025–2026 system landed on **agent-proposes**; they diverge only on QC:
- **Human-confirm-before-save:** Devin Knowledge (edit/regenerate/dismiss before save),
  Cursor Memories (sidecar model proposes, user approves — and Cursor *demotes*
  auto-memories to a private, uncommitted tier, steering durable knowledge to committed
  rules files). Windsurf Cascade does the same two-tier split.
- **Org-approval-delay:** CodeRabbit Learnings — trigger is *a human correcting the
  machine* in a review reply (the highest-signal moment); default applies immediately,
  optional 1–30 day admin window; QC is still "recommended quarterly manual review to
  purge contradictions" (https://docs.coderabbit.ai/knowledge-base/learnings).
- **No-confirm-but-verify-at-read + expire:** **GitHub Copilot Memory** (preview
  2026-01-15, on-by-default for Pro 2026-03-04) — the industry's newest system
  independently concluded that stored knowledge must be **re-verified against reality
  at read time**. Verified from the engineering post: "When an agent encounters a
  stored memory, it verifies the citations in real-time… If the code contradicts the
  memory… the agent is encouraged to store a corrected version"; memories auto-expire
  (~28 days) unless refreshed
  (https://github.blog/ai-and-ml/github-copilot/building-an-agentic-memory-system-for-github-copilot/).
  **This is the single most important piece of prior art for `claim`:** the biggest
  vendor in the space arrived at re-verification as the answer to memory rot — but their
  verification is *prompt-based* ("the agent squints at the cited code"), whereas
  `claim`'s is *executable and deterministic*. That gap is `claim`'s clearest
  differentiation, externally validated by a competitor's design.
- **The revealed friction threshold is ~zero.** Claude Code *retired* its one-keystroke
  `#` manual-capture shortcut in favor of automatic auto-memory; Grudin (1994) — the
  most predictive law here — says systems fail when the capturer isn't the beneficiary,
  and ADR authorship is a pure Grudin violation. Software Engineering at Google ch. 10:
  g3doc succeeded where the wiki failed because docs moved into the *same artifact, same
  change, same review* as code. The market's implied answer: humans tolerate roughly
  zero marginal *authoring* cost, but will *review* a good draft inside a workflow they
  were already in (a PR).

### Capture-as-side-effect and the cautionary tales
- **Sentry** turns a production error into an auto-drafted regression test (the failure
  *specifies* the check); **Rootly/incident.io** auto-draft retrospectives on incident
  resolution. Lesson: the check can be machine-drafted *when the triggering event fully
  specifies it*.
- **Swimm is the cautionary tale.** Raised $27.6M Series A (2021) for code-coupled,
  drift-detecting team docs; today its homepage sells enterprise COBOL/mainframe
  modernization — **the everyday-team doc-coupling product did not sustain the company**
  (observable on swimm.io, 2026). Even low-friction coupled-doc capture with drift
  detection failed to make everyday teams author docs; the money was where documentation
  is a funded *project*, not a habit.
- **DeepDocs/Mintlify/Fern** sync docs from diffs — viable only where the doc is
  *derivable from the diff* (API surface, config). A decision's "why" is precisely what
  is **not** in the diff, which is why these tools do references, not rationale.

### The mechanical trigger nobody has built
- **Suppression/pin/waiver artifacts already carry half a decision, fragmented across a
  dozen incompatible formats:** ESLint `-- description` (enforceable via
  `require-description`), osv-scanner `[[IgnoredVulns]]` with optional `reason`/
  `ignoreUntil`, Snyk `.snyk`, Trivy suppressions, VEX "not affected" justifications.
  Andrew Nesbitt (2026-03-19) documents this exact fragmentation and calls for a
  standard policy format (https://nesbitt.io/2026/03/19/the-fragmented-world-of-dependency-policy.html).
  **No tool walks a diff, spots a new pin/suppression/skip, and demands a durable,
  checked record.** This is an open niche directly adjacent to `claim`. (Danger JS is
  the closest shipped "you did X, now explain" mechanic, but rules are hand-written and
  demand prose, not checked records.)

### Agent-proposes / human-confirms: where it drowns
- Dependabot's reputation is more equivocal than assumed: 65% of dependency activity,
  security PRs mostly merged <48h (EMSE 2024, https://link.springer.com/article/10.1007/s10664-024-10523-y),
  but 11.3% of projects *deprecated* it and developers configure it toward silence
  (arXiv 2206.07230); it is the canonical **alert-fatigue** case study (arXiv 2502.06175).
- AI-review noise is quantified: a 28-PR audit rated 15% of CodeRabbit comments
  "Useless/Noise," 21% "Nitpicking." **Propose/confirm works only when proposals are
  scarce, high-precision, and pre-validated by machine before the human sees them
  (Dependabot's CI-green PR, Sentry's failing-then-passing test).**

### What this concretely implies for `claim` — the 3 most promising triggers
1. **Exception diffs (a new pin/suppression/skip/waiver in the diff *is* a decision).**
   Mechanically detectable with near-zero ambiguity; a `claim lint` CI rule ("this diff
   adds a pin with no claim covering it") converts an existing artifact into a birth
   trigger inside the PR the author is already in — the g3doc lesson. **Honest catch —
   the check-tautology problem (the biggest single risk in the whole product):** the
   check an author or agent will actually write ("assert the manifest still says 4.2")
   verifies the *pin*, not the *reason*, and stays green after upstream fixes the CJK
   bug. The honest check ("export a CJK PDF under 5.x and diff it") is expensive and
   nobody builds it. Expect a taxonomy: reasons that are *checkable*, reasons that are
   only *expirable* (`skip`+`until`, which `claim` already has), and reasons that are
   neither — and the third bucket is prose, not a claim. `claim`'s `add` Held-at-birth
   gate proves a check *can pass*, **not that it can fail** — vacuous checks pass forever
   and the gate can't detect it. This is why `--witness-cmd` (witness a red) matters far
   more than its "optional" status suggests: discriminating power is the real quality bar.
2. **Correction moments (human corrects machine; agent learns a constraint the hard
   way) — gated on check-derivability.** The trigger every vendor converged on. An agent
   drafts `statement + check` at the correction moment and runs `claim add`, whose
   Held-at-birth gate is a *strictly stronger* version of Copilot Memory's prompt-based
   JIT verification. **Honest catch:** most corrections are *preferences* with no
   falsifiable check ("don't use barrel imports here"); forcing a check mints vacuous
   ones. The router matters more than the trigger: correction → *is a discriminating
   check derivable?* → yes: `claim add`; no: plain rules/memory file.
3. **Incident/revert state-transitions (capture when the cost of not-knowing was just
   paid).** Fixes Grudin's asymmetry — the person who ate the 2 a.m. page *is* the future
   beneficiary — and a red state exists in reality, so the check can be born the honest
   way (written failing against the bad state, verified Held against the fix — Sentry's
   pattern). **Honest catch:** low frequency (won't populate a store alone), integration
   surface is *outside git* (incident tools), and many incident-facts can't be re-staged
   safely, so the check verifies a *proxy* (config guard present) that drifts from the
   fact it proxies.

**Single most promising trigger + honest catch:** *Exception diffs — grep the diff for
new pins/suppressions/skips/waivers and demand a claim in the same PR.* It is mechanical,
false-positive-free about *whether* a decision happened, lands in an existing workflow,
and maps onto artifacts (Trivy/osv/eslint-disable) that already exist. The catch is the
whole ballgame: **who writes a check that verifies the reason rather than the artifact,
and how do you stop the corpus filling with tautological green-forever checks** — a
problem no memory system has solved and which `claim`'s birth gate does not, by itself,
prevent.

---

## Q4 — The business/category question, enforcement, and adoption

*(Note: the dedicated market-research sweep hit a provider usage limit and completed
only its agent-memory-funding angle; I independently verified the rest of this section
directly against primary sources — every figure below is spot-checked.)*

### Is a category forming? A funded *adjacent* category exists; `claim`'s exact niche is unclaimed.
- **"Agent memory / agent-context infrastructure" is unambiguously a funded category as
  of mid-2026** — 7+ raises: Mem0 $24M (2025-10-28, Basis Set/YC/GitHub Fund), Cognee
  $7.5M seed (2026-02), Supermemory $2.6M (2025-10, angels incl. Jeff Dean),
  Letta/MemGPT $10M seed (2024-09), plus 2026 entrants Interloom ($16.5M), Jedify ($24M
  Series A). The headline: **Engram exited stealth with $98M at a $600M valuation
  (2026-06-23; General Catalyst, Kleiner, Sequoia; angels Karpathy, Abbeel, Wiz's
  Rappaport; customers Microsoft/Notion/Harvey), pitched explicitly on cutting agent
  token cost "up to 100×"** (verified, cnbc.com/2026/06/23 + prnewswire). Coding-agent
  context infra is where the real money is: **Cursor/Anysphere reached ~$4B ARR by June
  2026 and agreed a $60B all-stock acquisition by SpaceX** (verified). Glean (enterprise
  search + agents) raised $150M at **$7.2B** (2025-06, >$100M ARR, verified).
- **Engineering-knowledge players sell retrieval/doc-automation, not verification.**
  Dosu ($8M seed; now positions as "Knowledge Infrastructure for Agents and Humans" but
  is documentation-automation from code+conversations) and Unblocked ($30M total, Series
  A 2025-05; RAG-over-exhaust that *retrieves* past decisions, never mints checked
  records) are the nearest funded neighbors — both bet retrieval makes explicit capture
  unnecessary, neither verifies a fact against reality (verified).
- **But `claim`'s exact thing — recorded engineering facts bound to *executable
  re-verification* — is an unclaimed niche.** Every funded memory company reconciles
  *observed statements* (Zep/Graphiti temporal invalidation, Mem0 conflict resolution,
  xmemory write-time schemas); **none verifies a fact against reality with an executable
  check.** The closest existing verification is Guru's human re-attestation (retreating,
  per Q1) and GitHub Copilot Memory's prompt-based JIT citation re-check (per Q3) — both
  softer than an executable check. So `claim` would be **defining a sub-category adjacent
  to a hot funded one, not entering a crowded field.** Honest caveat: the dedicated
  competitor sweep didn't run, so a stealth/OSS neighbor can't be fully ruled out — but
  the nearest *named* neighbor, **Fiberplane's `drift`** (open-sourced March 2026, binds
  markdown to code via tree-sitter AST fingerprints and fails CI on change), is a
  code-changed-near-this-doc detector, **not** a fact-falsity checker, and PRODUCT.md
  already surveys it correctly.

### The category-name reality
- **"Context engineering" is an established 2025 term** (Tobi Lütke tweet 2025-06-18 →
  Karpathy endorsement → Anthropic formalized it 2025-09; verified) — but it names the
  *practice* of curating an LLM's context window, not a product category `claim` sits in.
- **"Context rot" means long-context degradation** (Chroma, 2025-07; verified directly)
  — NOT stale files. `claim`'s actual problem — *recorded knowledge silently going
  false over time* — **has no adopted category name yet.** That is both an opportunity
  (define it) and a go-to-market cost (you have to teach the buyer the problem exists).

### Protocol vs product
- **The conventions are standardizing and are largely un-monetizable by a third party.**
  AGENTS.md (~60k+ repos, now under the Linux Foundation's Agentic AI Foundation),
  MCP (adopted by OpenAI and Google, has a registry), and llms.txt (~9% adoption but no
  measurable benefit and crawlers ignore it) are all *conventions/protocols*. If context
  conventions standardize, "publish a file agents can read" captures no value (the
  llms.txt lesson). **What is left to sell is the verification loop and the hub's derived
  intelligence — the schedule, the drift routing, the cross-repo index — not the file
  format.** This favors `claim` being **an open format + CLI (protocol-shaped, free,
  drives adoption) with a paid hub (product-shaped, captures value)** — precisely the
  git/GitHub split PRODUCT.md already commits to. The evidence says that split is the
  right instinct: the format must be free and standard to get coverage; the money is in
  the derived, over-time, cross-repo intelligence a stateless file can't hold.

### The wedge question — painkillers vs vitamins
- **The one funding-validated painkiller framing in 2026 is agent-cost/agent-reliability:**
  Engram's $98M is a direct bet that *stale/bloated context makes agents expensive and
  wrong.* `claim`'s honest version: a stale CLAUDE.md fact is inherited every session and
  wastes agent runs — the PROPOSAL's own wedge (§8: "a caught lie is felt immediately as
  a prevented wasted session"). This is the strongest painkiller because the buyer
  already feels the pain and already has budget for agent tooling.
- **The compliance/audit wedge (CVE-waiver / security-exception management) is a
  plausible painkiller but unverified as a market** — Vanta/Drata prove continuous
  control-verification sells, and the fragmented-suppression-format problem (Nesbitt,
  Q3) is real, but selling to security means a process-adoption slog before value (which
  PROPOSAL §8 explicitly rejects as the *first* wedge, rightly).
- **The vitamin trap to avoid:** "better organizational knowledge / a second brain."
  Every manual-capture knowledge tool that sold this framing rotted (wikis, ADRs, Guru's
  maintenance overhead). Value that "grows with coverage and freshness" is a classic
  vitamin unless a single user gets value in week one — which is why PROPOSAL §8's
  agent-context-files wedge (one person, same week, felt as a prevented wasted session)
  is the correct painkiller framing and the compliance play is correctly deferred.

### What this concretely implies for `claim`
1. **Lead with the agent-cost/agent-reliability painkiller, not "knowledge governance."**
   The buyer with budget and felt pain in 2026 is the team running coding agents whose
   context files rot. This is already PROPOSAL §8; the market evidence (Engram, Cursor,
   Copilot Memory's re-verify design) strongly confirms it.
2. **Keep the git/GitHub split: free open format + CLI (protocol, drives coverage),
   paid hub (product, captures value in the derived over-time intelligence).** Conventions
   don't monetize; verification loops and cross-repo derivation do.
3. **Name the problem.** There is no category term for "recorded knowledge going silently
   false." `claim`'s "CI for facts" / "Dependabot for facts" framing is doing real work —
   it borrows a category the buyer already understands (CI, Dependabot) to name a problem
   they feel but can't yet name.

**Single clearest business signal + honest catch:** *A hot, well-funded adjacent
category (agent memory, $100M+ rounds) validates that "agents need trustworthy context"
is a real, funded problem — and every funded player stops at storing/retrieving observed
statements, leaving executable verification genuinely open.* Catch: that same funding
means the incumbents (Engram, Mem0, GitHub Copilot Memory) can add a verification step
faster than `claim` can build a hub — and Copilot Memory's ship-and-verify-at-read design
shows the biggest vendor is already one conceptual step away. `claim`'s defensibility is
*executable, deterministic, git-committed, PR-reviewed* verification — narrower and
harder to fake than prompt-based re-check, but also a harder sell (someone must write the
check) and a smaller initial market than "memory for all agents."

---

## The through-line: what made metadata/knowledge tools succeed vs fail

*(Historical-lessons research; every empirical anchor below independently verified.)*

The predictive law is **automatic vs manual capture**, and its corollary **who pays vs
who benefits** (Grudin 1994 — the most-cited framing in this space, a Grudin violation
is when the capturer isn't the beneficiary; ADR authorship is a pure violation).

**The graveyard (manual capture, author-pays-stranger-benefits):**
- **Code comments do NOT co-evolve with code.** Wen/Nagy ICPC 2019 (1.3B AST changes,
  1,500 systems, 3.3M commits; quotes re-extracted from the primary PDF): only **13–20%
  of code changes trigger a comment change**, and co-evolution happens in just **7% of
  cases for method comments, 13% for class comments.** The anchor stat of the whole
  report. (Correction: an earlier draft said "~90% co-evolve"; the *primary-source*
  numbers are the damning inverse above.)
- **Docs rot *silently*.** Tan/Treude EMSE 2023 (arXiv 2212.01479, re-extracted from PDF):
  **28.9% of the most popular GitHub projects currently contain an outdated reference;
  82.3% were outdated at some point**; documentation "gets outdated 'silently'… there
  are no crashes or error messages to indicate that documentation is no longer
  up-to-date." **This paper states `claim`'s product thesis almost verbatim.** Aghajani
  ICSE 2019: **up-to-dateness = 39% of all documentation content issues.**
- **The asymmetry, quantified — the single best pitch stat:** GitHub 2017 Open Source
  Survey (N≈5,500+): **93% observe incomplete/outdated docs as a problem, yet 60% of
  contributors rarely or never contribute to docs** (opensourcesurvey.org, verbatim).
  That gap is the product's whole reason to exist. Forward & Lethbridge 2002 adds the
  objection `claim` must beat: industry "may overemphasize document maintenance relative
  to a professional's *tolerance of outdated content*" — humans tolerate stale docs, so
  only a tool that makes staleness *loud and un-ignorable* helps.
- **ADRs are piloted then abandoned:** 83% rarely/occasionally documented (ECSA 2024);
  of 921 GitHub repos with ADRs, **~50% contain only 1–5 records** (MSR 2023) — adopted,
  written briefly, dropped. Curated lists rot too: `sindresorhus/awesome` audit found
  **14% of 47,941 links dead** (2020). (No hard "% of a corporate wiki is stale" figure
  exists in the literature — use the abandonment narrative, not a fabricated number.)
- **Data catalogs**: manual stewardship failed; **Gartner retired its Metadata Management
  Magic Quadrant for an *Active Metadata Market Guide* (2021)**, predicting the market
  would "cease to be a stand-alone market"; DataHub's creator: crawled metadata "gets
  staler and staler, leading to diminished trust," and a push interface "creates good
  *contracts* between producers of metadata." The market pivoted to auto-harvest from
  real query/dbt activity to *eliminate* manual curation. **The single cleanest analogy
  for `claim`** — and the push-contract framing mirrors `claim`'s verdict-as-attested-
  telemetry model exactly.

**The successes (automatic capture, or enforced at an existing gate):**
- **Types/tests/CI**: checked automatically, red is loud, capture forced at write time
  by the compiler.
- **g3doc at Google** (SWE at Google ch.10; verified): docs succeeded when moved into the
  *same artifact, same change, same review* as code — and a *higher* bar (review,
  owners) produced **better** docs, not fewer. Direct evidence for `claim`'s PR-gated,
  reviewed-like-code model.
- **CODEOWNERS/branch protection**: works because the *forge enforces it at PR time* —
  no new workflow. `claim`'s security-class "a human must review" rides exactly this.
  **Critical nuance (verified):** a CODEOWNERS study (844,492 PRs) found **>half of PRs
  don't satisfy adherence criteria and 79% of code owners aren't top-100 contributors** —
  enforcing that a review *happens* does not make the reviewed content *correct*. Direct
  warning for `claim`: mechanically requiring a claim to *exist* is not the same as the
  claim being *true*; only the executable check gives correctness. The ratchet (below)
  must gate on a *passing check*, not mere presence.
- **"The config IS the artifact" still drifts without continuous re-check:** IaC/Terraform
  enforces on `apply`, but nothing forces reality to stay matched *between* applies —
  ~1/3 of teams tie config drift to a production incident (Firefly 2026). Even the
  strongest "artifact = truth" case needs the re-verification loop `claim` provides.
- **Dependabot**: capture is automatic (lockfiles already exist) — but the alert-fatigue
  tax is the warning (Q3).

**The false-positive ceiling — the number that governs whether the nag survives:**
- **Google Tricorder (CACM 2018, quotes re-extracted from PDF): "up to 10% effective
  false positives"; developers must feel the check is right "at least 90% of the time";
  "If the ratio… goes above 10%, the Tricorder team *disables* the analyzer"** — and an
  "effective false positive" is defined by *human action* ("developers did not take
  positive action"), not by whether the tool was technically right. Tricorder holds
  itself below ~5%. This is the empirical validation of PROPOSAL §9's kill threshold, and
  PROPOSAL's 1-in-3 is *lenient* versus Google's 1-in-10 — `claim` should aim tighter than
  its own stated bar. **Corollary for invariant #1:** a `Broken` verdict the human
  dismisses counts as an effective false positive against this budget, so the *rate* of
  Broken (not just its honesty) must stay low or the channel dies.
- **The strongest single quote for "integrate at the PR, not a dashboard":** Facebook
  Infer (CACM 2019, re-extracted): "batch deployment saw a **0% fix rate**" / "near
  silence"; moved to diff/PR time, "the fix rate **rocketed to over 70%. The same program
  analysis, with the same false positive rate.**" Timing/workflow, not accuracy, was the
  only variable. Google FindBugs corroborates: a bug dashboard "outside the developers'
  usual workflow" got a **16% fix rate**. This is the load-bearing evidence for the
  CLI-in-CI / hub-ingests-verdicts architecture over any standalone UI or committed log.

**Cold-starting a coverage-dependent tool (the bootstrap playbook):**
- **Gradual typing's ratchet** (mypy/TypeScript; verified): lenient by default, a
  separate CI check requires *new* code to be typed, modules promote Untracked→Lenient→
  Strict via one-line config — "things only improve." Auto-generation seeds the initial
  corpus (MonkeyType/PyAnnotate infer annotations). The `claim` analogue: don't demand a
  full corpus; ratchet — every *new* pin/suppression/skip must carry a claim (Q3 trigger
  1), the existing backlog is grandfathered, coverage only grows. This is the documented
  way coverage-dependent tools bootstrap from one repo, one team.

**Enforcement without mandate (bottom-up → org standard):**
- Prettier/ESLint/pre-commit/conventional-commits spread when they were **zero-config,
  scaffolded into templates, and sat in the critical path of an existing workflow** — not
  when they required a *new* workflow. Falsifiable prediction: a `claim` that requires a
  new authoring ritual will stall; one that attaches to the PR (lint on exception diffs)
  and the agent session (hooks) will spread.

**Five empirical lessons as falsifiable design predictions:**
1. *A claim not re-checked by a machine will rot at the rate prose does — so the check is
   the product, and a claim with a no-op/tautological check is worthless.* Prediction:
   claimless-check claims drift to false at comment/doc rates (13–20% of code changes
   update the related comment; 82.3% of projects carry an outdated reference at some
   point); real-executing-check claims do not. Evidence: Wen ICPC 2019, Tan/Treude EMSE
   2023 (both re-extracted), Dropbox "verified documentation."
2. *Unrewarded maintenance for a deferred beneficiary won't happen unless the tool
   re-attaches cost to a named owner and makes neglect visible.* Prediction: routing drift
   to the git-derived author/reviewer beats an unowned queue. Evidence: Grudin 1988;
   GitHub 2017 (93% see the problem, 60% won't fix); Google's "Last reviewed by…" byline
   "led to increased adoption"; Stack Overflow's reputation vs the wiki.
3. *Above ~10% not-actioned noise the nag channel dies — precision, not recall, is the
   survival constraint.* Prediction: once >~10% of drift alerts are dismissed without
   action (measured by human response, not tool correctness), engineers mute the channel.
   Evidence: Tricorder's 10%-disable rule (re-extracted); npm audit "trained… to ignore";
   11.3% of projects deprecated Dependabot.
4. *A check on a new dashboard won't be looked at; it must ride the PR/CI run and be
   unbypassable there.* Prediction: `claim` as a required PR/CI status sees
   order-of-magnitude higher action than a standalone dashboard or a skippable local hook.
   Evidence: Infer 0%→>70% batch→diff at *identical FP rate* (re-extracted); FindBugs
   dashboard 16% fix; `git commit --no-verify` bypasses local hooks by design.
5. *Bootstrap by auto-seeding a permissive draft, defaulting to a no-op lowest rung, and
   ratcheting an allow-list that only grows — never demand a full corpus up front, and
   gate the ratchet on a passing check, not mere presence.* Prediction: hand-author-from-
   zero stalls in the long tail; auto-seed + file-by-file adoption + block-net-new-unclaimed
   crosses into standing use. Evidence: Figma's grow-only allow-list with topological
   seeding; Sorbet default-`# typed: false`; MonkeyType/PyAnnotate/ts-migrate auto-seed;
   Google Test Certified "no untested new code"; plus the CODEOWNERS caveat (presence ≠
   correctness) — gate on the check, not the file's existence.

---

## How big could this be, and what has to be true

**The strongest painkiller wedge.** Agent-context-file freshness for teams running coding
agents (PROPOSAL §8), sold on *agent cost and reliability*, not "knowledge governance."
The market validates the pain: Engram raised $98M on "stale/bloated context makes agents
expensive," Cursor is at ~$4B ARR, and GitHub Copilot Memory *ships read-time
re-verification* — three independent confirmations that "agents need trustworthy,
current context" is real and funded. `claim`'s edge is the one thing none of them have:
**executable, deterministic, git-committed, PR-reviewed verification** — a stale CLAUDE.md
sentence that goes red is a prevented wasted session, felt by one person in week one.

**The top adoption risks, in order:**
1. **The check-tautology problem (Q3).** If the checks people actually write verify the
   *artifact* not the *reason* ("manifest still says 4.2" instead of "the CJK bug is still
   unfixed"), the corpus fills with green-forever claims that prove nothing, and `claim`
   becomes a vitamin wearing a painkiller's clothes. The birth gate proves a check *can
   pass*, not that it can *fail* — discriminating power (the `--witness-cmd` red) is the
   real quality bar and is currently optional. **This is the single biggest threat.**
2. **False-positive fatigue.** Above ~10% false drifts, the nag channel dies (Tricorder).
   The whole hub/routing apparatus is worthless if the signal is muted.
3. **Retrieval-obviates-capture.** Unblocked/Dosu/Glean bet that good search over exhaust
   makes explicit capture unnecessary. `claim`'s only answer: retrieval finds what was
   *said*; only a check knows if it's still *true*. That answer holds only if the checks
   discriminate (see risk 1).
4. **Incumbent fast-follow.** Copilot Memory is one conceptual step from executable
   verification; a funded memory startup could add a check. `claim`'s moat is being
   git-native, PR-reviewed, and honest-by-construction (the golden invariants) — narrower
   and harder to fake, but also a smaller beachhead.
5. **Coverage cold-start + the "who writes the check" friction** — mitigated by the ratchet
   (new exceptions must carry a claim) and agent-drafted checks, but unproven at scale.

**Product or protocol? Both, in the split `claim` already chose.** The evidence is clear:
conventions (AGENTS.md, MCP, llms.txt) standardize and *don't* monetize for a third party;
value lives in the verification loop and the hub's over-time, cross-repo derivation that a
stateless file cannot hold. So: **an open format + CLI (protocol-shaped, free, drives the
coverage the tool's value depends on) with a paid hub (product-shaped, captures value).**
This is the git/GitHub model PRODUCT.md commits to, and the market data endorses it —
the harness/format layer is being commoditized (60k AGENTS.md repos, MCP registry), while
the durable, sellable intelligence is exactly the derived staleness/routing/index the hub
owns.

**The honest bottom line.** `claim` is aimed at a real, funding-validated problem
("agents need current, trustworthy context") from an angle nobody funded has taken
(executable verification of stated facts), with a go-to-market wedge the market confirms
(agent-cost/reliability) and an architecture the historical record endorses (attach to
PR + agent session, ratchet coverage, free format + paid derived intelligence). Its
survival turns on one thing the design does *not* yet guarantee: **that the checks people
write discriminate — fail when the fact is false — rather than tautologically pass
forever.** Solve that (make witnessed-red the norm, not an option; adversarially review
check discriminating power; template checks per trigger; be honest that some "reasons"
are only expirable and some are just prose), stay under a ~10% false-drift rate, and the
tool is a painkiller in a hot market. Fail it, and it is a better-engineered ADR — which
history says rots.

---

## Source appendix (independently verified during this research, not just via subagents)
- GitHub Issues duplicate detection (2026-06-18); semantic Issues index (2026-01-29 preview,
  2026-04-02 GA).
- Mem0 97.8%-junk audit (github.com/mem0ai/mem0/issues/4573); Mem0 $24M Series A
  (2025-10-28, TechCrunch).
- Zep/Graphiti (arXiv 2501.13956); Sentry grouping/merge-irreversibility (Sentry dev docs).
- Google Tricorder ~10% FP tolerance / <5% actual (Sadowski et al.; CACM).
- Anthropic "Code execution with MCP" (2025-11-04): 58 tools ≈ 55K tokens, 150K→2K (98.7%).
- ETH Zurich AGENTS.md efficiency study (JAWS 2026 / arXiv 2601.20404): LLM-generated files
  −3% success / +20% cost.
- GitHub Copilot Memory read-time citation re-verification + ~28-day expiry (github.blog
  engineering post + docs).
- AGENTS.md ~60k repos (Linux Foundation AAF); "context engineering" origin (Lütke
  2025-06-18 → Karpathy → Anthropic 2025-09); "context rot" = long-context degradation
  (Chroma 2025-07, verified directly).
- Engram $98M/$600M (2026-06-23, cnbc/prnewswire); Cursor ~$4B ARR + SpaceX $60B
  (2026-06); Glean $150M/$7.2B (2025-06).
- Fiberplane `drift` (open source, 2026-03; github.com/fiberplane/drift).
- Data-catalog manual-stewardship failure / active-metadata pivot (Gartner 2021 MQ
  retirement; DataHub push-contract); SWE-at-Google g3doc (ch.10); gradual-typing ratchet
  (mypy/Sorbet/Figma/MonkeyType); Grudin (1988/1994); Nesbitt on fragmented dependency
  policy (2026-03-19); Guru verification workflow + Trust Score erosion + vendor retreat
  to automated verification; Dosu ($8M) / Unblocked ($30M) as retrieval-not-verification
  neighbors.

**Four highest-stakes quotes re-extracted verbatim from primary PDFs** (not just via
subagent): Wen/Nagy ICPC 2019 (13–20% / 7% / 13% co-evolution); Tan/Treude EMSE 2023
(28.9% / 82.3% / "silently"); Infer CACM 2019 (0%→>70% at same FP rate); Tricorder CACM
2018 (10% disable rule, "effective false positive" = no human action). Claims left
**flagged unverified** and NOT relied on: State-of-JS percentages, gofmt "~90%" (use
~70%), `--no-verify` bypass rate, Sorbet 150k-file figures, "94% of LLM errors are type
errors," any numeric "distrust below X% coverage," and any hard "% of a wiki is stale"
(no such figure exists in the literature — the abandonment narrative is what's supported).
