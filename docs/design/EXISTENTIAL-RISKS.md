# `claim` — existential risks and the approaches to each

Status: menu, not a plan. July 2026.

This document lays out the risks that could sink `claim` and, under each, the
possible approaches to it. **Every approach listed under "Other possible
approaches" is an option to weigh — none is currently being pursued.** The
baseline under each risk is what the plan (PRODUCT.md, HUB.md, CLI-HUB-BOUNDARY.md)
already does or intends; everything after it is drawn from four first-principles
approach lists and the grounded research (`ADOPTION-RESEARCH.md`), and is marked
not-currently-pursued with a one-line reason.

`claim` in one line: bind a plain-language fact to an executable check, commit it
to git; a stateless CLI reports `held`/`drifted`/`broken`; a per-org hub ingests
those verdicts and derives staleness, due-ness, and drift routing.

The risks, in order:

1. Duplicate / polluting claims.
2. Getting agents and people to consult the relevant claims before acting, and
   covering the crucial files.
3. Knowing when to capture a claim, and covering the crucial files without minting
   junk.
4. Check discrimination — the tautology problem: ensuring a check actually fails
   when the fact becomes false, not just passes forever.

Then a closing read on the business/category question, as framing.

**Recurring ideas across risks.** The same few moves surface under more than one
risk. Where they do, they are noted inline; collected here so the convergence is
visible:

- **Key on the check's polarity/behavior, not its prose.** A check's `negate`
  flag and exit semantics are machine-readable and unforgeable; a statement's
  words are not. This underpins the twin-safe dedup approaches (Risk 1) and the
  discrimination approaches (Risk 4).
- **Blast-radius / reachability from the check's read-set.** Expanding a claim's
  watch set to everything that can reach the files its check reads appears as a
  coverage engine for consultation (Risk 2), a targeting engine for capture
  (Risk 3), and a fault-injection basis for discrimination (Risk 4).
- **Mutation-test the check.** Perturbing the world and requiring the check to go
  red is the core of Risk 4, appears as behavioral-equivalence dedup in Risk 1,
  and as a birth-gate strengthening in Risk 3.
- **Fire at registry/CI time over the whole corpus, not only inside `claim add`.**
  A gate that only runs in `claim add` is bypassed by a hand-committed file; the
  stronger approaches under Risks 1 and 3 run over the merged corpus.

---

## Risk 1 — Duplicate / polluting claims

**The risk.** Two claims that assert the same or nearly the same fact both live in
the corpus. They split ownership, double the nag, and — unlike a stale wiki — their
checks do not force the redundancy into view: two near-duplicate checks both exit 0
forever, so nothing notices they are redundant. The corpus rots by accretion, not
by going false.

**Why it's hard (the honest catch).** Three constraints any approach must survive:

- **The negation twin.** The most similar existing claim by embedding is often the
  opposite fact ("X enabled" vs "X disabled"): antonyms occur in near-identical
  contexts, so they land close in embedding space. High cosine is a good candidate
  generator and a terrible merge decider; a careless reviewer merges two opposite
  facts.
- **The hand-committed-file bypass.** A claim written by editing a file and
  committing (never running `claim add`) skips any gate the CLI owns. A dedup gate
  that only fires inside `claim add` is bypassable by construction.
- **Checks-don't-dedup.** Two claims that both pass green are never forced into
  comparison by verification; redundancy is invisible to the runtime. Something
  external must force the comparison.
- The research adds: no surveyed memory system has made purely automatic
  consolidation trustworthy. Mem0's production audit found 97.8% junk, an 808-copy
  feedback loop, and a regression to MD5-exact dedup that stored contradictory
  facts. PR-gated writes are exactly the mitigation the literature recommends —
  keep the human in the merge decision.

**The current/baseline approach.** Suggest-at-write-time, human-decides-at-review.
The plan already commits to: the **check-digest** (SHA-256 over each check's
canonical definition — kind, run/instruction, negate, skip, normalized), which the
hub computes to bucket verdicts and which doubles as a byte-identical-check dedup
key; **suggest-at-write-time embedding candidates** decided by a human at PR review
(the hub can suggest edges via semantic similarity, but suggestions land as PRs,
never silent graph mutations — deferred); **canonicalization of equivalent
world-facts** (designate one canonical instance, verify once, fan the verdict out —
deferred, and explicitly not an ontology); **supersede-via-git** (`retire`/`amend`
remove or rewrite, git history is the record of invalidation); and `contradicts`
edges returning, if ever, as agent-suggested conflict detection. The honest catch
the plan carries: the candidate generator only fires if the author authors through
a path the hub sees, and the negation twin will be the top suggestion.

**Other possible approaches** (not currently pursued):

- **A. Content-addressed claim identity.** The claim's id is a hash of its
  normalized fact-shape (statement key, canonical run, `negate`, sorted `supports`),
  not a chosen slug; two claims that hash equal are the same file path, so an exact
  duplicate is a git-level collision visible in the diff with no gate to bypass.
  Domain: content-addressing / Merkle (git objects, IPFS, Nix). Handles the catch:
  dedup happens at git-write, not at check time, so it survives the hand-commit
  bypass; including `negate`/polarity in the hash makes the twin correctly two
  distinct objects. Tradeoff: breaks the human-readable kebab-case `id` that
  `supports:` and wiki-links point at, and hashing the statement risks
  ontology-creep; only reaches exact-shape dupes. Differs from baseline: moves
  dedup from the hub (after merge) into git (at merge, in the truth).

- **B. Structural fact-key + nominal statement.** Separate a claim's structural
  identity (what its check mechanically asserts — target paths, pattern, polarity)
  from its nominal identity (the prose); dedup on the structural key, keep the prose
  free, so `rg -q 'X' src/` and `grep -rq 'X' ./src` produce the same key. Domain:
  nominal vs structural typing; database candidate keys. Handles the catch: the key
  includes polarity, so twins get different keys even at cosine 0.98; a CI lint
  drifts when two live claims share a key, catching the hand-committed case.
  Tradeoff: parsing arbitrary shell into a canonical structural form is open-ended
  and fragile — realistically a per-check-kind normalizer for a whitelist of shapes,
  a research project; agent/human checks have no structural form to key on. Differs
  from baseline: the digest is a syntactic hash (`rg` ≠ `grep`); this is a semantic
  key that collapses equivalent-but-differently-written checks.

- **C. Two-vector similarity: fact-vector AND polarity-vector.** Represent each
  claim as two signals — what it is about, and which way it points — and call two
  claims duplicates only when topic matches AND polarity matches; same-topic +
  opposite-polarity is flagged as a *contradiction*, not a duplicate. Domain:
  "Beyond Cosine Similarity" (arXiv 2601.13251); stance/sentiment decomposition.
  Handles the catch: this is the approach designed for the twin — polarity is taken
  from the check itself (`negate`, exit semantics), machine-readable and unforgeable,
  so the twin becomes the highest-value output rather than a false merge; runs at
  index time over hand-committed claims too. Tradeoff: the whole
  embedding/suggestion layer is deferred to the hub, and polarity-from-check is clean
  only for `cmd`+`negate` — agent-check polarity is buried in prose. Differs from
  baseline: removes the twin from the human's plate by making polarity a separate,
  check-derived axis, instead of leaning on the reviewer to catch it.

- **D. Entity-resolution over the supports-graph.** Two claims are candidate
  duplicates when they support the same decision anchor and read the same files —
  resolve by edges and read-set, not statement similarity. Domain: knowledge-graph
  entity resolution / record linkage (block on shared attributes, then match).
  Handles the catch: same anchor + opposite polarity is "contradictory reasons for
  one decision," a louder finding than duplicate; the join is over `supports` edges
  the hub already indexes, no run needed, and edges are in frontmatter regardless of
  how the file arrived. Tradeoff: depends on authors writing `supports` edges
  (optional today) and on read-set tracing (deferred); a claim with no edges is
  invisible, so it is a complement to text similarity, not a replacement. Differs
  from baseline: dedups over graph position rather than prose or external-fact
  equivalence, reusing the cross-repo routing index rather than a new embedding index.

- **E. The check IS the dedup oracle: behavioral equivalence.** Two `cmd` checks are
  duplicates iff they agree on every input; mine their read-sets, construct probe
  states (including a staged red), and if two checks return identical verdicts across
  all probes they are behaviorally equivalent. Domain: differential / property-based
  equivalence checking. Handles the catch: this makes checks dedup by construction —
  equivalence is defined by check behavior, the one thing `claim` can execute; a twin
  returns opposite verdicts on the same staged world, so it can never be falsely
  merged; runs at CI/registry time. Tradeoff: N² over staged states, only for cheap
  deterministic `cmd` checks with stageable reds, and constructing valid probe states
  automatically is unsolved in general — realistically a hub batch job over a
  pre-filtered cluster. Differs from baseline: nothing in the plan uses check
  *execution* as the dedup signal; the digest compares definitions, embeddings
  compare prose. (Reuses the `--witness-cmd` staged-red machinery, tying dedup to
  discriminating power — see Risk 4.)

- **F. Supersession lattice via bi-temporal edges.** Do not dedup by deleting —
  record a `supersedes: <id>` edge so the newer claim invalidates the older, which
  becomes a tombstone that redirects; at most one claim per fact is live. Domain:
  bi-temporal knowledge graphs (Zep/Graphiti `valid_at`/`invalid_at`); Stack Overflow
  close-as-duplicate (redirect, never delete). Handles the catch: supersession is
  directional and human-authored, so a twin is never auto-superseded; the edge is
  frontmatter, indexed however the file arrived; a live claim whose fact-key matches
  a tombstone is a loud finding. Tradeoff: solves the "I found a duplicate, now what?"
  resolution step, not detection — it needs a detector (A–E) to fire first, and adds
  an edge type (ontology the org must learn). Differs from baseline: the plan's
  resolution is `retire` (delete the file, git is the record) with no forward pointer;
  this keeps the reversible, navigable redirect the research shows is durable.

- **G. Immune-system self/non-self: a claim must be provably distinct to be
  admitted.** Invert the burden — instead of scanning for dupes after the fact, a
  new claim must demonstrate it is not already covered: its check must distinguish it
  from the nearest existing claim (a world-state where the two checks disagree) before
  it is admitted. Domain: immune self/non-self discrimination; negative selection.
  Handles the catch: admission requires a distinguishing input; no distinguishing
  state means redundant, refused; a twin trivially has one, so it is correctly
  admitted and flagged; enforced at CI over the registry, not only at `claim add`.
  Tradeoff: finding a distinguishing probe automatically is the equivalence problem in
  reverse and equally hard, and it raises authoring cost the pilot's <5-min guard
  protects; a softer "warn on no-known-distinction" is viable sooner. Differs from
  baseline: every planned mechanism is detect-then-suggest (post-hoc, advisory); this
  is prevent-at-admission — the strongest structural stance, the highest friction.

- **H. Canonical fact-URI namespace.** A world-fact gets one canonical URI
  ("vendor:stripe/rate-limit"); every claim about it references the URI, so forty pins
  on the same vendor limit are forty subscribers to one address. Domain: canonical
  URIs / URNs (DOI, package coordinates, schema.org `@id`). Handles the catch: dedup
  is by declared shared address, not inferred; two claims on one URI with opposite
  assertions are a visible conflict at that address; the ref is frontmatter, indexed
  at sync. Tradeoff: requires a governed namespace and authors agreeing on canonical
  addresses — the ontology/governance cost the spec warns against; helps only world
  facts, not repo-facts (the v1 wedge). Differs from baseline: makes canonicalization
  author-declared and explicit rather than hub-inferred — nominal identity instead of
  structural inference.

- **I. Semantic/AST diff of statement + check.** Compare claims by tree-diff of their
  structured content (parsed statement key phrases + parsed check AST), so
  "near-duplicate" means "small structural edit distance" — interpretable and
  twin-safe. Domain: semantic/AST diff (tree-sitter, GumTree, Fiberplane's `drift`).
  Handles the catch: a negation is a large edit in the check AST (the polarity node
  flips) even when it is a tiny prose edit, so AST distance separates twins that
  cosine collapses; computed on definitions at index time. Tradeoff: same
  check-parsing fragility as B, and it only reaches near-dups whose checks are
  structurally close — two claims stating one fact with totally different check
  strategies have distant ASTs and slip through. Differs from baseline: exact,
  twin-aware, explainable, but narrower — a precision instrument where embeddings are
  recall; best as the confirm step behind embedding candidates.

- **J. CRDT-style convergent claim set.** Model the claim set as a CRDT keyed on
  fact-identity so two agents independently authoring "the same" claim in parallel
  branches merge into one on integration rather than producing two rows. Domain: CRDTs
  / convergent replicated data types. Handles the catch: convergence is at merge time
  on the fact-key (which includes polarity, so twins do not converge — they surface as
  a concurrent conflict); hand-committed and CLI-authored claims merge identically.
  Tradeoff: git's merge is line-based, not semantic — a real claim CRDT needs a custom
  merge driver and a stable fact-key (A/B), and concurrent duplication is a narrow
  slice (most dupes are authored months apart). Over-engineered for v1 scale. Differs
  from baseline: the plan treats concurrent same-slug creation as a normal git conflict
  for a human; this makes same-*fact* concurrent creation converge automatically.

*Convergence note.* The approaches sort hard by the three catches: anything keyed on
prose inherits the twin and needs a human backstop; anything firing only in `claim
add` is bypassed by a hand-commit; anything needing a runtime comparison to happen
inherits checks-don't-dedup. The approaches that route around all three key on the
check's machine-readable polarity/behavior (C, E) and fire over the whole corpus at
registry/CI time.

---

## Risk 2 — Consulting the relevant claims before acting, and covering the crucial files

**The risk.** An agent or human about to change code must reliably encounter the
load-bearing facts governing what they touch, before acting on a stale assumption.
The overriding objective is coverage of the *crucial* files — the load-bearing ones
an agent will actually act on — not any file. A mechanism that covers every trivial
file and misses the one load-bearing file has failed.

**Why it's hard (the honest catch).** Three constraints:

- **Presence is not obedience.** A hook can inject a fact into context; the model can
  still ignore it. Instruction-following degrades with load (68% accuracy at 500
  instructions; <30% perfect compliance in agentic runs). Prompt-layer rules are
  advisory by design.
- **"The model decides to look" measurably fails.** MCP tools go uncalled (agents
  used code mode only 6% of the time when both were available; some servers' tools
  never called at all); agent-requested rules, `llms.txt`, and passive skills all
  under-fire. Tool-selection reliability falls off a cliff past ~20 tools. The one
  deterministic layer is harness hooks that inject context directly.
- **The coverage gap.** The mechanism presumes a claim's `supports`/read-set names the
  file being edited. The invalidating change routinely lands in a file no claim
  mentions (the upload handler that reaches XmlParser eight months later, nowhere near
  the exception file).
- The research adds a cost the always-on approach pays: injecting more is not free.
  LLM-generated always-on instruction files *reduced* task success ~3% and raised cost
  20%+; compliance itself has a cost, and a big rules file is attention-diluted, not
  skimmed. `llms.txt` has ~9% adoption and no measurable benefit — publishing a file
  agents *could* read does nothing; something must put it in front of them.

**The current/baseline approach.** Hooks execute the CLI; hub MCP as a fallback; the
`supports` anchor as the mapping. Concretely the plan intends: **harness hooks** that
run the CLI (a SessionStart digest plus a PreToolUse hook on Edit/Write that
path-matches `supports`/read-set and injects only the matching facts) for deterministic
*presence*; the **CLI as the retrieval primitive** everything calls (`claim list
--json`, `claim check --json` — greppable, works in every harness, what the hooks
execute); a **small directive section in CLAUDE.md/AGENTS.md** advertising the CLI (not
inlining the corpus); and the **hub MCP** (`context`, `dossier`, `drifts`, `due`,
`search`) last, as a thin adapter for surfaces that cannot run a shell. The mapping is
the `supports` anchor plus post-v1 read-set tracing. Shared property: the fact is
*shown* to the agent as dated evidence to weigh, never as an order to obey; nothing
blocks on it. The honest catch the plan carries: hooks are per-harness glue `claim`
must build and maintain, presence is not obedience, and the mapping presumes the
read-set names the edited file.

**Other possible approaches** (not currently pursued):

*Group A — make a violated claim fail the change (obedience for free).*

- **A1. Claim-as-build-error.** Compile the relevant claims' checks into the change's
  own build/CI gate so a drifted load-bearing claim turns the change red, with the
  statement and owner in the failure text. Domain: compilers / type systems. Handles
  the catch: obedience is irrelevant — the agent cannot merge a red; covers exactly the
  files the check reads. Tradeoff: this is the plan's *promote-to-a-real-test* end
  state, and making blocking the default reintroduces the "author deletes the assertion
  because no reason is attached" failure at scale and collides with invariant #6 (nag,
  not block) — it belongs as a deliberate per-claim opt-in ratchet (`enforce: block`),
  defaulting off. Differs from baseline: the plan never blocks (comment + standing
  issue only); this makes a violated claim a hard gate. Same check, opposite lifecycle
  position.

- **A2. Poka-yoke interlock at the edit site.** A PreToolUse hook that does not merely
  inject the fact but *denies* the edit (hook `deny`) when the touched path is governed
  by an unacknowledged claim, returning the fact as the denial reason. Domain: poka-yoke
  / machine interlocks. Handles the catch: converts presence into "you may not proceed
  until you have seen this"; the interlock fires only for load-bearing claims, so
  friction concentrates on crucial files. Tradeoff: a hard block on the changer
  (invariant #6 tension), per-harness and Claude-Code-specific, and an
  acknowledgement gate the agent auto-clears is theatre. Differs from baseline: the
  plan's hook informs; this withholds the tool until the fact is seen.

- **A3. Reference monitor / policy engine on the change.** A mediation layer evaluates
  the diff against a policy compiled from the claims ("no change may make a load-bearing
  claim drift without an owner ack") and admits or rejects like an authorization
  decision. Domain: reference monitors (complete mediation) / OPA / Kyverno. Handles the
  catch: the monitor looks on every change, so coverage is a guarantee not a hope,
  especially paired with blast-radius inference (B1). Tradeoff: a full policy layer is
  enormous surface and a *new* required workflow — the adoption-killer the research flags
  — and it re-centralizes decisions the plan keeps distributed to owners. Differs from
  baseline: the plan has no evaluator that admits/rejects a change; verdicts are
  telemetry, never a gate.

- **A4. Effect/capability system.** Treat "editing a governed file" as a capability the
  agent does not hold by default; grant it for the session only after the governing
  claims are surfaced and (for high-stakes claims) acknowledged. Domain: capability
  security / effect systems. Handles the catch: the model cannot skip the fact because
  skipping it means never acquiring the capability to edit; capabilities mint per
  load-bearing region, so trivial files carry no friction. Tradeoff: fights the harness's
  own permission model, is per-harness, and only Claude Code's `deny` approximates it —
  it is A2 with more machinery. Differs from baseline: the plan grants the fact alongside
  full ambient edit authority; this withholds authority until the fact is honored.

*Group B — close the coverage gap (the crucial fact reaches the agent even in an
unwatched file).*

- **B1. Blast-radius reachability from the check's read-set.** Expand each claim's watch
  set from "files the check reads" to "everything that can reach those files" — callers,
  importers, transitive dependents — so a change three hops away still surfaces the fact.
  Domain: static taint analysis / reverse call-graph reachability. Handles the catch: the
  coverage gap *is* this — the change was reachable to the governed code but not in it;
  reverse-reachability is the precise formalization of "could this change make the fact
  false," and it feeds whichever delivery mechanism (hook/gate). It is the only approach
  that would have caught the canonical upload-handler failure. Tradeoff: a correct reverse
  call-graph is per-language, expensive, and never sound across dynamic dispatch /
  reflection / FFI — an unsound graph that silently misses the crucial caller is worse
  than an honest grep-the-whole-tree; ship it as an optimization over run-everything, not
  as the floor. Differs from baseline: the read-set is files the check *reads*; this is
  files that can *reach* what the check reads — a transitive closure the plan does not
  compute. (Same reachability engine recurs in Risks 3 and 4.)

- **B2. Learned file→fact association from drift history.** Learn which files, when
  changed, have historically co-occurred with a claim drifting, and surface that claim on
  future edits to those files even if no static edge connects them. Domain: change-coupling
  / association-rule mining ("developers who touched X also broke Y"); recommenders.
  Handles the catch: catches invalidating changes no static analysis links (config,
  fixtures, indirect couplings), complementing B1's blind spots. Tradeoff: needs a drift
  history that does not exist until the tool has run a long time (cold-start), it is
  probabilistic (false associations are exactly the >10% noise that kills the channel),
  and it re-introduces a statistical component into a deterministic product. Differs from
  baseline: the plan's edges are declared or read-derived (deterministic); this adds a
  learned, probabilistic edge kept out of v1.

- **B3. Semantic-index reachability (embed the diff, retrieve the claim).** Embed the
  changed hunks and retrieve the top-k claims whose statements are semantically nearest,
  so a fact reaches the agent when the change is conceptually about the same thing,
  regardless of path. Domain: RAG / semantic code search. Handles the catch:
  path-independence — a prose-described change retrieves the relevant claim by meaning,
  reaching the case where the invalidating file shares no token with the watch set.
  Tradeoff: the same negation-trap and precision problems as dedup (antonyms co-locate;
  high cosine is a good candidate generator, a bad decider) — as a coverage mechanism it
  floods with near-misses, and flooding is context rot, the very thing that dilutes the
  one crucial fact. Useful as a candidate generator behind a filter, never as the delivery
  path. Differs from baseline: the plan matches on path (`supports`/read-set); this matches
  on meaning, at a precision cost the plan avoids.

*Group C — make the fact structurally unavoidable rather than merely present.*

- **C1. Pre-change read-back checklist.** Before a governed change, the agent must
  *produce* — not merely receive — the governing facts and state how its change relates to
  each, a challenge-and-response the change cannot proceed without. Domain: aviation /
  surgical checklists (the item is read back, not just read). Handles the catch: making the
  agent emit the fact is a far stronger signal of processing than silent injection; the
  checklist is populated from the claims governing the touched region, so it lists only
  crucial facts. Tradeoff: enforcing a genuine read-back (vs rubber-stamping "does not
  affect") requires judging the agent's response, itself an unreliable LLM step, and it
  adds latency to every governed edit; an unjudged checklist decays to theatre. Differs
  from baseline: the plan pushes the fact and moves on; this makes the agent pull it back
  through its own output.

- **C2. Progressive disclosure keyed to the action.** Disclose nothing at session start
  and the full governing fact at the instant of the governed action, so it arrives with
  zero competition and maximal relevance, late in context where attention is best. Domain:
  progressive disclosure (UI) + just-in-time context engineering. Handles the catch:
  context rot and early-instruction bias are dilution effects of volume and position;
  delivering one fact at the edit with nothing else competing is where a single instruction
  is most followed. Tradeoff: this is the closest of all approaches to the plan (it *is* the
  PreToolUse hook, disciplined), so its distinctness is marginal, and dropping the
  SessionStart digest loses a useful surface — fold its "minimal, edit-scoped payload"
  discipline into the planned hook rather than building it separately. Differs from
  baseline: the plan injects a SessionStart digest *and* edit-time facts; this removes the
  digest and injects one fact per action.

- **C3. Feature-flag / ratchet gating of the change surface.** Gate whether an agent may
  touch a governed region behind a per-region flag (off / warn / block) and ratchet regions
  from warn→block only as their claims prove trustworthy, so enforcement grows with
  confidence and never demands a full corpus. Domain: feature flags + gradual-typing
  ratchets (Untracked→Lenient→Strict). Handles the catch: obedience is enforced only where a
  region has earned it (its claims discriminate, low false-drift), dodging the
  false-positive-fatigue death spiral of indiscriminate blocking; the ratchet aims at
  load-bearing regions first. Tradeoff: presupposes both the blocking mechanism (Group A)
  and a track-record signal (a mature verdict history), neither in v1 — it is the governance
  layer over A1/A2. Differs from baseline: the plan has one non-blocking mode for all claims;
  this is a per-region, confidence-graded enforcement ladder.

*Convergence note.* B1 (blast-radius) attacks the coverage gap the other two catches are
downstream of — a delivery mechanism can only deliver facts for files the mapping names.
A1 is the strongest answer to presence-is-not-obedience but is a lifecycle escalation, not
a consultation mechanism, and is the same promote-to-a-gate the plan already contemplates.
C1 is the best near-term, low-infrastructure lever on obedience, buildable as a skill/hook
over the CLI the plan already ships. None commits a verdict or stores status.

---

## Risk 3 — When to capture a claim, and covering the crucial files without minting junk

**The risk.** A decision worth recording must become a claim at near-zero authoring
cost, or the corpus never reaches the coverage its value depends on. Manual-capture
knowledge tools rot (ADRs, wikis, Guru). The objective is coverage of the *crucial*
files — the load-bearing decisions an agent will act on — without minting junk.

**Why it's hard (the honest catch).** Three constraints:

- **Preferences aren't checkable.** Most corrections ("don't use barrel imports here")
  have no falsifiable check; forcing one mints a tautology.
- **Incident/world facts often can't be re-staged.** The honest check would cost a
  project to build; the cheap check verifies a proxy that drifts from the fact.
- **Flooding is the same disease.** Hitting a coverage number by minting green-forever
  checks re-creates the pollution problem (Mem0's 97.8%-junk audit).
- The research adds the economics: ADRs fail at both ends — capture friction at decision
  time and no mechanism noticing staleness — and the root cause is structural (the
  cost/value imbalance of documenting rationale, Grudin's "capturer isn't the
  beneficiary"). Every shipped 2025-26 system converged on *agent-proposes*, differing
  only on QC. Revealed friction threshold for *authoring* is ~zero; people will *review*
  a good draft inside a workflow they were already in (a PR). Propose/confirm works only
  when proposals are scarce, high-precision, and machine-pre-validated (Dependabot's
  CI-green PR, Sentry's failing-then-passing test); otherwise it is alert fatigue.

**The current/baseline approach.** An exception-diff ratchet plus correction and incident
triggers. The plan intends: an **exception-diff ratchet** (a new pin / suppression / skip /
waiver in a diff demands a claim in the same PR — mechanical, false-positive-free about
*whether* a decision happened, landing in the existing workflow); **correction moments**
(a human corrects the machine — the highest-signal moment — the agent drafts statement +
check and runs `claim add`, gated on whether a discriminating check is derivable, else
routed to a plain rules file); and **incident/revert state-transitions** (capture when the
cost of not-knowing was just paid, and a red state exists to write the check against). All
are event triggers that fire when a decision happens. The honest catch the plan carries:
the check an author actually writes verifies the *artifact*, not the *reason*, and stays
green after the reason is moot — the tautology problem (Risk 4), which the birth gate does
not prevent.

**Other possible approaches** (not currently pursued). These sit on a different axis from
the planned triggers: they make the *absence* of a claim fail (A), identify *which* files
are crucial independent of any triggering event (B), attack the tautology at the source (C),
or make capture a byproduct of work already happening (D).

*Group A — make the absence of a claim fail.*

- **A1. Crucial-file coverage gate ("claimfmt for decisions").** A repo-local lint that
  fails CI when a file on a maintained crucial set has no claim covering it, the way
  `# typed: false` blocks untyped new modules. Domain: gradual typing's grow-only ratchet;
  Google Test Certified "no untested new code." Handles the catch: it never demands a
  *check*, only that a crucial file be *accounted for* — including an explicit `none`-check
  claim (dated, attributed, still degrading to a scheduled human look) or a waiver, so no
  tautology is forced; it rewards accounting, not volume, so junk is bounded. Tradeoff:
  needs the crucial-set definition (Group B) first, and a `none`-claim escape hatch risks
  becoming a rubber-stamp — wants pilot data on whether `none` claims convert to real
  checks. Differs from baseline: the plan fires on a *decision artifact appearing*; this
  fires on a *crucial file lacking coverage*, closing the unwatched-file gap from the
  coverage side.

- **A2. Orphaned-reason detector.** Walk `supports` edges the other way: find decision
  artifacts pointed at by nothing (a Trivy suppression's `reason:`, an eslint-disable
  description, a `# noqa` with text) and demand the reason be captured. Domain: double-entry
  accounting (every entry needs a counter-entry). Handles the catch: fires only where an
  artifact already carries half a decision, so the reason prose is already written
  (near-zero authoring), and the set is finite and pre-existing (no flooding); suppressions
  are load-bearing by construction. Tradeoff: format-specific parsers are maintenance
  surface (the fragmentation problem cuts both ways); it is the ratchet's read-only cousin
  and the ratchet ships first because it catches new ones at the cheapest moment. Differs
  from baseline: the plan gates *new* suppressions at PR time; this harvests the *existing*
  backlog — a one-time coverage jump the forward ratchet never reaches.

- **A3. Load-bearing-comment escalation.** Detect comments asserting a falsifiable,
  externally-breakable fact ("must stay in sync with", "assumes X is never null here") and
  nag that the fact is unclaimed — the comment is a claim that forgot to bind a check.
  Domain: auto-instrumentation (the signal is already emitted, just unwired). Handles the
  catch: classify comments and escalate only the falsifiable ones (pure preferences
  ignored), so no tautology is minted; the comment text seeds the statement. Tradeoff:
  classification precision is unproven and false positives here are pure noise against the
  ~10% fatigue ceiling; wants the crucial-set scoring first so it scans only crucial files.
  Differs from baseline: the plan triggers on a *change*; this mines stationary prose
  already in the repo for latent claims.

*Group B — identify which files are crucial, then drive coverage of those.* (None of these
mint claims; they produce a ranked crucial set that Group A gates against and the triggers
prioritize.)

- **B1. Blast-radius / criticality scoring from the dependency graph.** Rank files by how
  much rests on them — reverse-dependency fan-in, importer count, import-graph centrality —
  and declare the top stratum crucial. Domain: risk-based testing / criticality analysis;
  the hub's own "most-depended-on claims" list applied to code. Handles the catch: scoring
  mints nothing, so it can't produce a tautology — it only aims the other mechanisms; effort
  concentrates on the ~5% of files most changes route through, so a small corpus covers a
  large fraction of what agents act on. Tradeoff: cross-language import analysis is
  language-specific work; the single-team wedge (CLAUDE.md/AGENTS.md) has a smaller,
  hand-obvious crucial set, so scoring earns its keep only over real source trees. Differs
  from baseline: the plan has *no file-targeting* — it fires wherever an artifact appears;
  this decides *where coverage should exist* before any artifact appears. (Same engine as
  Risk 2's B1.)

- **B2. Change-frequency × impact (churn-hotspot).** Rank by `commit_frequency ×
  blast_radius` — files that change often *and* are depended on heavily are where a fact is
  most likely to silently rot. Domain: risk-based prioritization (probability × impact);
  Tornhill hotspots. Handles the catch: a ranking, mints nothing; targets the rot-likely
  files, where a checked fact pays off most. Tradeoff: needs real history to be meaningful,
  and adds a second signal to tune before B1's single signal is validated. Differs from
  baseline: orthogonal — the plan is event-driven and history-blind; this is history-driven
  and event-blind.

- **B3. Bus-factor / knowledge-concentration targeting.** Rank by how few people understand
  a file (single dominant author, low contributor count, long untouched by anyone active) —
  the facts most likely to be *lost* rather than *broken*. Domain: bus-factor / knowledge-risk
  analysis; the Grudin insight. Handles the catch: ranking only; surfaces the tacit facts in
  one person's head that no diff will ever trigger, the category the event-driven plan can
  never reach; low volume by nature, so it can't flood. Tradeoff: the capture step is a human
  interview — high-friction and Grudin-violating (the expert pays) — so it needs the
  agent-drafts-from-interview loop first, so the expert only reviews. Differs from baseline:
  the plan captures *observed* decisions; this captures *unobserved tacit* ones from
  people-signals git already records.

*Group C — attack the tautology at the source (make the check discriminate).* (These compose
with the plan's triggers rather than replacing them; the deeper catalog is Risk 4.)

- **C1. Mutation-gated birth.** At `claim add`, don't just require the check passes now;
  require it *fails* against a machine-generated mutation of the world that should break the
  fact. Domain: mutation testing + property-based testing. Handles the catch: the direct
  answer to the tautology catch — it turns `--witness-cmd` from optional into a *generated*
  signal (for `grep -q 'libfoo==4.2'`, mutate the manifest line in a scratch copy and confirm
  the check flips to drifted); a tautology survives the mutation and is refused; preferences
  that produce no discriminating check route to `none`/rules. Tradeoff: generating a
  *meaningful* mutation is easy for line-in-a-file greps and hard in general, and can't run
  against world/agent checks — a `cmd`-check tool, not universal, shipped opt-in first.
  Differs from baseline: the birth gate proves a check *can pass*; this proves it *can fail* —
  the missing half of the honesty invariant.

- **C2. Check-template library keyed by decision kind.** Ship discriminating check templates
  indexed by decision kind (CVE-not-applicable → reachability grep with `negate`; version-pin
  → manifest assertion + upstream-issue-open probe; flaky-skip → `skip.unless` + `until`;
  import-ban → reference-count grep with `negate`); the trigger fills in the blanks. Domain:
  contract-first "the test is the spec"; Sentry's error→regression-test auto-draft. Handles
  the catch: templates are pre-vetted for discriminating power once, so an author can't
  accidentally write the tautological version; decision kinds with no discriminating template
  (pure preference) route to rules/`none` instead of minting. Tradeoff: the template set must
  be earned per decision kind from real examples — shipping speculative templates re-introduces
  the guessing problem one layer up. Differs from baseline: the plan says *when* to capture;
  this says *what check to write* once capturing.

- **C3. Discriminating-power ledger.** The hub flags a check that has *never* reported drifted
  across its whole life despite the world changing around it — a tautology suspect, the way a
  test with a 100% pass rate across churn is suspected of asserting nothing. Domain:
  always-green-test smell + observability's "this alert never fired, is it wired up?". Handles
  the catch: catches the tautologies the birth gate and templates missed, using signal only the
  hub has (long-run verdict history vs registry churn on the read-set); never auto-deletes —
  routes to a human with evidence. Tradeoff: a hub feature needing the ledger, registry, and
  read-set tracing (deferred) plus a long history to be meaningful. Differs from baseline: the
  plan is capture-side and one-shot; this is continuous corpus-health, catching tautologies that
  slip past every birth-time gate. (Same idea as Risk 4's A10.)

*Group D — make capture a byproduct of work already happening.*

- **D1. Session-diff distillation.** At the end of an agent session that touched crucial files,
  the agent distills "what did I have to learn/assume to make this change correctly?" into
  candidate claims — from its own session, since it just paid the discovery cost. Domain: data
  provenance/lineage + Copilot Memory's "store a corrected version when reality contradicts."
  Handles the catch: the agent proposes, a human confirms at PR time (scarce, pre-validated —
  the only propose/confirm shape that survives); the agent runs the birth gate before proposing,
  so preference-only learnings drop. Tradeoff: depends on the harness-hook integration and the
  crucial-set scoping, and agent-drafted claims are trusted less by design — the confirm UX must
  be excellent before it scales. Differs from baseline: the plan triggers on a specific artifact
  class or a correction; this triggers on any crucial-file work session, capturing discovery that
  never surfaced as a suppression or a correction.

- **D2. Coverage as a first-class dashboard metric with an owner.** Make crucial-set coverage %
  a visible, owned number on the hub dashboard, the way test-coverage or vuln-burn-down is, so a
  team can run a bounded "cover our top-50 crucial files" campaign instead of hoping ambient
  triggers add up. Domain: Vanta/Drata control-coverage burn-down; test-coverage ratchets. Handles
  the catch: the *denominator is the crucial set* (Group B), not "all files," so 100% is small,
  reachable, honest, and un-inflatable by minting on trivial files; each covering claim must pass
  the birth gate to count. Tradeoff: a hub UI feature presuming the crucial-set scoring and a real
  corpus, and making coverage a *target* risks Goodhart's law unless the quality gates (C1/C3) are
  already strong. Differs from baseline: the plan is bottom-up and ambient; this is top-down and
  bounded — the funded-project shape Swimm's pivot showed is where doc-coupling actually gets done.

*Convergence note.* The research names the tautology (Group C / Risk 4), not too-few-claims, as
the biggest threat: scaling coverage the naive way *is* the pollution problem. The strongest moves
make coverage *safe to grow* (C1/C2), *aimed at the files that matter* (B1/A1), and *self-cleaning
over time* (C3). All are distinct from the planned triggers on the same axis: the plan decides
*when a decision is captured*; these decide *whether the check is worth anything*, *which files
must be covered at all*, and *how the corpus stays honest as it grows*.

---

## Risk 4 — Check discrimination (the tautology problem)

**The risk.** The checks people write are tautological. They verify the artifact ("the
manifest still says 4.2"), not the reason ("the upstream CJK bug is still unfixed"), and stay
green forever after the reason is moot. `claim add`'s birth gate proves a check *can pass*; it
does not prove the check *can fail when the fact becomes false*. The property `claim` needs is
**discrimination**: the check goes red precisely when, and only when, the fact stops being true.
The research flags this as the single biggest existential risk — solve it and `claim` is a
painkiller; fail it and it is a better-engineered ADR, which history says rots.

**Why it's hard (the honest catch).** The baseline signal, `--witness-cmd`, is an optional,
author-supplied, single, at-authoring-time perturbation with four structural limits any approach
must beat:

1. **The author writes the counterfactual.** The same person who wrote a tautological check
   writes the perturbation that "proves" it discriminates, and will write the one perturbation
   their check happens to catch (`sed -i s/4.2/4.3/`), not the one that corresponds to the fact
   actually becoming false (upstream shipping a fix). Witness proves *a* red exists, not that the
   red *tracks the fact*.
2. **One perturbation, one direction.** It tests a single point; it says nothing about the space
   of ways the fact could go false, nor about false positives (does it stay green when the fact is
   still true but the world moved?).
3. **At authoring time only.** A check that discriminated at birth can decay into a tautology as
   the code around it changes (the file it grepped is renamed; the assertion now matches nothing
   and passes vacuously).
4. **Advisory and unrecorded.** No downstream consumer knows whether a claim was witnessed, so the
   corpus can't be filtered or weighted by discriminating power.

The honest floor is a taxonomy every approach must respect. A recorded reason is one of:
**CHECKABLE** (a discriminating check exists — the only bucket the core mechanism is for);
**EXPIRABLE** (no discriminating check, but a natural clock — `skip.until`, which `claim` already
has); or **PROSE** (a preference with no falsifiable observation — not a claim; belongs in a rules
file). Discrimination machinery must be able to say "this reason cannot be made discriminating" and
refuse to pretend — a tool that forces every reason into CHECKABLE manufactures the very
tautologies it means to prevent.

**The current/baseline approach.** `--witness-cmd`: an optional confidence signal. `claim add`
creates a throwaway detached worktree at HEAD outside the repo, runs the author's `<cmd>` to mutate
that isolated tree, runs the check there, and requires it reports `Drifted`. It is observed once,
never recorded (a verdict is telemetry), and never a gate — a fact whose red can't be staged is
verified by its passing check alone. Its promotion to mandatory would fix only limit #4 and weakly
#1. Every approach below attacks at least one of #1–#3 in a way promotion cannot. All preserve the
invariants: a Broken check under a perturbation is Broken, never a pass (#1); the tool owns
negation, never `sh -c "! ..."` (#2); discrimination results are telemetry, reported via `--json`,
never committed (#4).

**Other possible approaches** (not currently pursued):

- **A1. Mutation testing of the check.** Auto-generate a batch of world-perturbations and require
  the check to catch a required fraction, reporting a survival set ("these 3 mutations left the
  check green"). Domain: mutation testing (PIT, Stryker, `cargo-mutants`). Discrimination = mutation
  score; a score of 0 is provably a tautology. Because the *tool* generates the mutants (from the
  check's own read-set), it defeats author collusion (#1) and single-point coverage (#2). Slot:
  CHECKABLE only; a near-zero score signals the reason is really EXPIRABLE or PROSE. Tradeoff:
  generating *fact-relevant* mutants (not just any diff that makes a grep miss) needs to understand
  what the fact means; a generic byte-mutator produces dismissible mutants and re-creates fatigue.
  Differs from witness: a tool-generated batch the author can't cherry-pick, yielding a score, not a
  single pass. (Same core as Risk 3's C1 and Risk 1's E.)

- **A2. Metamorphic discrimination.** Instead of proving one red exists, assert a *relation*: if the
  world changes toward "fact false," the verdict must move Held→Drifted; if it changes toward "more
  clearly true," it must stay Held. Domain: metamorphic testing (compilers, ML — no single-output
  oracle). A tautology has a constant response; a discriminating check has a monotone one. Slot:
  CHECKABLE — the invariance arm ("stays Held when still true") is what nothing in `claim` currently
  tests, catching the false-positive tautology (a check that reds on any change, Fiberplane's failure
  mode). Tradeoff: requires authors to articulate a relation — more than "paste the command"; the
  natural v2 of witness. Differs from witness: tests both directions and the invariance arm, not one
  direction once.

- **A3. Property-based / fuzzed world states.** Generate many world-states, label each with the
  fact's ground truth from an independent oracle, and require the check's verdict to agree — a check
  is discriminating iff no state disagrees. Domain: property-based testing (QuickCheck, `proptest`),
  fuzzing with shrinking. Discrimination = agreement rate with an independent oracle; shrinking yields
  the minimal world-change the check fails to notice. Slot: CHECKABLE with a cheap independent oracle
  (strongest for structural facts like "no import of X" — generate trees with/without the import,
  label by construction). Tradeoff: needs a per-fact-shape generator and oracle — real per-kind
  machinery, worth it only for a few common structural shapes. Differs from witness: machine-generated
  worlds with *independent* ground-truth labels, measuring a rate, not one author-authored pass.

- **A4. Fault injection at check-run time.** Deliberately break the check's environment (rename its
  target file, drop its binary, empty its input) and require the verdict is *not Held*. Domain: chaos
  engineering / fault injection (Chaos Monkey, Jepsen). Targets a specific, common tautology: a check
  that passes because it *matched nothing* (a grep with a typo, a `test -f` on a moved path). It is
  invariant #1 (broken-never-passes) promoted from a runtime guarantee into an authoring-time audit of
  the check itself. Slot: CHECKABLE, and the cheapest, most general, lowest-false-positive probe — it
  needs no fact-specific oracle, only the check's own read-set. Tradeoff: depends on read-set tracing
  (deferred) to know what to perturb. Differs from witness: perturbs toward "the check's inputs are
  gone," a different axis, catching the vacuous-match tautology the author who wrote a vacuous grep
  won't think to stage.

- **A5. Continuous re-audit of discriminating power (witness-on-a-clock).** Re-prove discrimination
  periodically, not just at birth, because a check that discriminated at authoring can rot as the code
  drifts. Domain: regression testing / test-suite health monitoring (mutation score over time). Attacks
  limit #3: a `grep -q 'foo' src/mod_a.rs` discriminates until `mod_a.rs` is renamed, after which it
  matches nothing and passes vacuously; re-running the probe on the hub's cadence catches this decay and
  routes it as its own drift ("this claim's check stopped being able to fail"). The CLI stays stateless
  (`claim probe <id>` reports now, stores nothing); the hub schedules and remembers. Tradeoff: requires
  the hub's scheduler and a probe (A1/A2/A4) to schedule — the composition layer over the others.
  Differs from witness: a monitored, decaying property on a clock, not a one-shot at birth.

- **A6. Adversarial discrimination.** An independent agent is tasked to find a world-state where the
  fact is false but the check still reports Held; if it can, the check is not discriminating, and the
  counterexample is the proof. Domain: red-teaming / adversarial ML (the "second agent instructed to
  disprove the first"). Handles facts too semantic for mechanical mutation ("the CJK bug is still
  unfixed") — the adversary reasons about what upstream shipping a fix would look like rather than
  mutating bytes, and is independent of the author, defeating collusion (#1) even for judgment-shaped
  facts. Slot: CHECKABLE, the semantic frontier; repeated failure to falsify signals PROSE. Tradeoff:
  requires trusted, metered agent execution, and an adversary is non-deterministic — it can miss a real
  hole or hallucinate one, needing the spot-audit apparatus (deferred). Differs from witness: an
  adversary independent of the author searches for the perturbation, and can reason about semantic facts
  no `sed` can perturb.

- **A7. Falsifiability gate: refuse the unfalsifiable, route it to expiry/prose.** Before accepting a
  check as discriminating, require the author to name the *falsifier* — the observable event that would
  make the fact false — and if none can be named, refuse to file it as a CHECKABLE claim. Domain:
  Popperian falsifiability. This is the *classifier* the taxonomy needs: a nameable falsifier ("upstream
  closes the bug") unlocks the witness/mutation path; a date-only falsifier steers to `skip.until`; no
  falsifier ("I prefer this style") refuses `add` and prints "this is a rule, not a claim." Slot: this
  *is* the taxonomy, enforced at the front door — the cheapest and most important single intervention,
  because most tautologies are PROSE/EXPIRABLE reasons forced into a check. Tradeoff: adds authoring
  friction (the thing that kills adoption above ~5 min), and a clumsy version nags authors into writing
  fake falsifiers, worse than nothing — needs calibration data first. Differs from witness: asks the
  prior question ("could a check ever falsify this?") and routes the two-thirds of reasons that can't
  to `skip`/prose; witness only refuses an unwitnessed red.

- **A8. Formal / bounded-model discrimination.** For facts expressible over a formal artifact (an
  import graph, a config schema, a type), *prove* the check equivalent to the fact via a bounded model
  checker rather than sampling perturbations. Domain: bounded model checking / SMT (CBMC, Alloy, TLA+).
  A proof of equivalence is the strongest possible discrimination guarantee — no surviving mutant exists
  by construction, within the bound. Slot: CHECKABLE, the formal sub-slice — narrowest, highest-assurance.
  Tradeoff: enormous machinery for a narrow slice; most `claim` facts (upstream behavior, vendor limits,
  CJK rendering) have no formal model, and it contradicts the "thin shell over grep" ethos — conceivable
  only as proven check *templates*, far future. Differs from witness: proves over a bounded space vs
  sampling one point — a different assurance class entirely.

- **A9. Discrimination via check-template provenance.** Ship a curated library of check templates each
  proven or empirically shown to discriminate for a fact-shape (`import-absent`, `version-pinned`,
  `upstream-issue-open`, `file-hash-stable`); a claim built from a template inherits proven discriminating
  power, a claim built from raw shell is marked lower-trust and routed to the heavier probes. Domain:
  vetted-primitive design (linters ship proven rules; crypto ships one audited AES). Moves trust from N
  per-check audits to a handful of template audits, and makes the discriminating check the path of least
  resistance. Slot: CHECKABLE — it *shrinks* the tautological region by making discrimination the easy
  default. Tradeoff: requires knowing the common fact-shapes, which only a real corpus reveals; a premature
  library codifies the wrong primitives, and it doesn't help novel facts. Differs from witness: prevents
  the tautology up front by instantiating a pre-vetted check, rather than auditing an arbitrary one
  post-hoc. (Same as Risk 3's C2.)

- **A10. Discrimination as a corpus statistic (measure, don't guarantee).** Don't gate any single claim;
  let the hub measure how often each check has *ever* changed verdict, and flag checks green for their
  whole life despite churn on their read-set as suspected tautologies. Domain: observability / SLO
  monitoring; always-green-test detection. Purely empirical, zero authoring cost: a check *provably* Held
  on every run across changes to the files it reads is a strong statistical tautology signal without
  perturbing anything — "revealed discrimination" from history rather than "designed discrimination" from a
  probe. Slot: distinguishes mis-filed CHECKABLE (tautology) from correctly-filed CHECKABLE that simply
  never got a chance to fire, by whether the read-set changed. Tradeoff: requires the hub, the verdict
  stream, and read-set tracing (all deferred) plus a long history — a late-stage backstop, cold-start blind.
  Differs from witness: an undesigned, over-time, observational inference, no perturbation, zero authoring
  cost. (Same as Risk 3's C3.)

*Convergence note.* No approach makes an unfalsifiable fact checkable — A1–A10 make a *checkable* fact's
check provably discriminating, or *detect* that a check isn't. A7's routing (and the courage to say "this
belongs in a rules file") is the only honest answer for PROSE, and `skip.until` for EXPIRABLE. The mechanical
probes (A1, A4) are cheapest and most deterministic; the semantic ones (A6) reach facts no `sed` can perturb
but cost determinism; A5/A10 add the over-time dimension the CLI can't hold.

---

## The business / category read (framing, not a risk to solve)

This is context for weighing the risks above, drawn from the research. It is not a fifth risk.

- **A funded adjacent category exists; `claim`'s exact niche is unclaimed.** "Agent memory /
  agent-context infrastructure" is unambiguously funded as of mid-2026 — Mem0 ($24M), Cognee, Supermemory,
  Letta/MemGPT, and the headline Engram ($98M at a $600M valuation, pitched on cutting agent token cost).
  Coding-agent context infra is where the money is (Cursor/Anysphere ~$4B ARR; Glean $150M at $7.2B). But
  every funded memory player reconciles *observed statements* (temporal invalidation, conflict resolution,
  write-time schemas); **none verifies a fact against reality with an executable check.** The nearest funded
  neighbors (Dosu, Unblocked) sell retrieval/doc-automation and bet retrieval makes explicit capture
  unnecessary. So `claim` would define a sub-category adjacent to a hot funded one, not enter a crowded field.
  Honest caveat: the dedicated competitor sweep didn't complete, so a stealth/OSS neighbor can't be fully
  ruled out; the nearest named one, Fiberplane's `drift`, is a code-changed-near-this-doc detector, not a
  fact-falsity checker.

- **The problem has no category name yet.** "Context engineering" names the *practice* of curating a
  context window; "context rot" means long-context degradation — neither names `claim`'s problem, *recorded
  knowledge silently going false over time*. That is both an opportunity (define it) and a go-to-market cost
  (teach the buyer the problem exists). The "CI for facts" / "Dependabot for facts" framing borrows a
  category the buyer already understands.

- **Protocol vs product.** Context conventions (AGENTS.md ~60k+ repos, MCP, `llms.txt`) are standardizing
  and are largely un-monetizable by a third party — "publish a file agents can read" captures no value (the
  `llms.txt` lesson). What is left to sell is the verification loop and the hub's derived intelligence — the
  schedule, drift routing, cross-repo index. This favors the split the plan already commits to: an open
  format + CLI (protocol-shaped, free, drives the coverage the value depends on) with a paid hub
  (product-shaped, captures the over-time, cross-repo intelligence a stateless file can't hold).

- **The wedge: painkiller, not vitamin.** The one funding-validated painkiller framing in 2026 is
  agent-cost/agent-reliability — a stale CLAUDE.md fact is inherited every session and wastes agent runs, felt
  by one person in week one. The compliance/audit wedge (CVE-waiver management) is plausible but unverified as
  a market and means a process-adoption slog before value. The vitamin trap to avoid is "better organizational
  knowledge / a second brain" — every manual-capture knowledge tool that sold this rotted.

- **The load-bearing empirical constraints for survival.** Precision, not recall, governs whether the nag
  channel lives: above ~10% not-actioned drift alerts, engineers mute it (Google Tricorder disables an
  analyzer past that; an "effective false positive" is defined by *human non-action*, not tool correctness).
  Integrate at the PR/CI run, not a dashboard: Infer's fix rate went 0%→>70% moving batch→diff at the *same*
  false-positive rate. Bootstrap by ratchet (new exceptions must carry a claim; grandfather the backlog;
  coverage only grows), gating on a *passing check*, not mere presence (the CODEOWNERS caveat: enforcing a
  review happens doesn't make the content correct).

- **The honest bottom line.** `claim` aims at a real, funding-validated problem from an angle nobody funded
  has taken (executable verification), with a wedge the market confirms and an architecture the historical
  record endorses. Its survival turns on the one thing the design does not yet guarantee: **that the checks
  people write discriminate** (Risk 4). The top adoption risks, in the research's order: (1) the
  check-tautology problem, (2) false-positive fatigue above ~10%, (3) retrieval-obviates-capture, (4)
  incumbent fast-follow (Copilot Memory's ship-and-verify-at-read design is one conceptual step from executable
  verification), (5) coverage cold-start and the "who writes the check" friction. `claim`'s defensibility is
  being executable, deterministic, git-committed, and PR-reviewed — narrower and harder to fake than
  prompt-based re-check, but a harder sell and a smaller initial market than "memory for all agents."
