# The things that could sink `claim`, and the options for each

Status: this is a menu, not a plan. July 2026.

Nothing here is decided. This document lists the problems that could kill `claim`.
Under each problem it lists the options we could use to deal with it. For each
problem there's a "current plan" — what we already intend to do. Everything after
that, under "Other options," is just an option to think about. None of it is being
built right now. Each one comes with a one-line reason it's not being built yet.

First, what `claim` is, in plain words. You take a fact written in plain English —
say, "we pin library X to version 4.2 because a newer version has a bug." You attach
a small command that can check whether that fact is still true. You commit both to
git, the same way you commit code. Then a command-line tool runs the check and tells
you one of three things: the fact still holds, the fact has drifted (it's no longer
true), or the check broke (it couldn't run). A separate service we call the "hub"
collects those results over time. The hub is the part that knows the history: which
facts are going stale, which are overdue for a re-check, and who to tell when a fact
breaks. The tool itself remembers nothing between runs.

Two words you'll see a lot:

- A **check** is the small command attached to a fact. Running it tells us if the
  fact is still true.
- A **claim** is the fact plus its check, committed to git.

The four problems, in order:

1. Duplicate and junk claims piling up.
2. Getting agents and people to actually look at the relevant claims before they
   change code — and making sure the important files are covered.
3. Knowing when a decision is worth recording as a claim — and covering the important
   files without filling the list with junk.
4. Making sure a check actually fails when the fact becomes false, instead of passing
   forever no matter what.

At the end there's a section on the business question. That's background, not a
problem to solve.

## A few ideas that show up in more than one place

The same handful of ideas keep coming back under different problems. Here they are up
front so you can see the pattern.

- **Judge a check by what it does, not by how the fact is worded.** A check has a
  clear yes/no behavior, and a flag that says whether it's checking for a thing being
  present or absent. A computer can read that behavior exactly. It can't read the
  meaning of an English sentence the same way, and worse, a sentence can be worded to
  say anything. So the reliable signal is the check's behavior. This idea shows up in
  finding duplicates (problem 1) and in making checks trustworthy (problem 4).

- **Watch every file that could affect a check, not just the files the check reads.**
  A check reads some set of files. But a change far away can still make the fact
  false. So one idea is to widen the net: watch not just the files a check reads, but
  everything that connects to those files — everything that calls them, imports them,
  or depends on them. This same idea helps with getting the fact in front of the agent
  (problem 2), deciding which files matter (problem 3), and testing whether a check
  really works (problem 4).

- **Break the world on purpose and check that the check notices.** Deliberately change
  something that should make the fact false, then confirm the check turns red. If it
  stays green, the check is worthless. This is the heart of problem 4. It also helps
  spot duplicate checks (problem 1) and strengthen the moment a claim is created
  (problem 3).

- **The hub is the real authority, not the tool's local gate.** The hub sees every
  claim in git, no matter how it got there. Someone can hand-write a claim file and
  commit it, skipping the tool entirely. So the serious work of finding duplicates and
  verifying claims belongs at the hub, looking at all the claims together — not at a
  local check the tool runs when you add a claim, which someone can bypass. The
  stronger options under problems 1 and 3 run at the hub for this reason.

---

## Problem 1 — Duplicate and junk claims

**The problem.** Two claims end up saying the same thing. Now one fact has two owners
and sends two reminders. And nothing catches it — both checks keep passing, so the
tool never notices they're duplicates. Unlike a stale wiki page, which at least looks
wrong when you read it, two duplicate checks both stay green forever. The list of
claims just quietly fills with repeats. It doesn't rot by going false. It rots by
piling up.

**Why this is hard.** Two traps any solution has to survive.

First, the opposite-fact trap. To find duplicates, the computer looks for claims
worded similarly. It measures how close two pieces of text are in meaning (a standard
technique: turn each sentence into a list of numbers and see how close the lists are).
But "X is on" and "X is off" are worded almost the same. They're about the same thing;
they just disagree. So the closest match to a new claim is often its exact opposite.
That's fine for saying "hey, take a look at this one." It's dangerous if you then merge
them — you'd be combining two claims that contradict each other.

Second, checks don't compare themselves. Two claims that both pass green are never
forced to face each other. The tool runs each check, sees green, and moves on. Nothing
makes it notice they're redundant. Something outside the normal run has to force the
comparison.

One more point, about where this work should happen, not a trap. Finding duplicates
belongs at the hub, looking at all the claims together. The hub sees every claim
committed to git, however it was written. Hand-writing a claim file and committing it
doesn't dodge the analysis — it just means the analysis has to live at the hub, where
it sees everything, and not in a local gate the tool runs when you add a claim.

What the research adds: no memory system anyone studied has made fully automatic
merging trustworthy. One AI-memory product checked its own stored memory and found
almost all of it was junk, including a loop where the system kept re-saving its own
recalled memories over and over. The safe pattern is to keep a human in the merge
decision, reviewing it inside a pull request. (Details in `ADOPTION-RESEARCH.md`.)

**The current plan.** Suggest possible duplicates when a claim is written; let a human
decide during review. Concretely:

- **A fingerprint of the check.** Take each check's exact definition and hash it into a
  short fixed string. Two checks with the same definition get the same fingerprint, so
  byte-for-byte identical checks are easy to spot.
- **Suggest similar claims at write time.** The hub can point out claims that look
  similar in meaning. But these only ever show up as suggestions in a pull request,
  never as automatic changes to the list. (Not built yet.)
- **Pick one official version of a shared world-fact.** If many claims are really about
  the same outside fact, name one as the official one, check it once, and share the
  result. (Not built yet, and deliberately not an attempt to build a formal vocabulary
  of facts.)
- **Replace via git.** To remove or rewrite a claim, use the existing `retire` and
  `amend` commands. Git history is the record of what got replaced and when.
- **Conflict edges**, if they ever come back, as an agent suggesting "these two claims
  seem to disagree."

The honest catch the plan carries: the suggestion only appears if the author writes
the claim through a path the hub sees, and the opposite-fact trap means the top
suggestion is often the exact contradiction of the new claim.

**Other options** (not being built):

- **Name each claim after what it checks, so exact copies collide.** Today a person
  picks a name for a claim. Instead, build the name automatically from the fact and its
  check. Then two claims that check the exact same thing get the exact same name, and
  land on the exact same file. Git shows that as a conflict the moment you commit, so
  you can't miss it, and there's no gate to bypass. Because the name includes whether
  the check is looking for presence or absence, a fact and its opposite get different
  names — they correctly stay separate. Example: two people both add "library X is
  pinned to 4.2"; the auto-generated name is identical, so the second one collides with
  the first in git. Downsides: the names become unreadable, other claims that point at
  this one by name break, and it only catches exact copies, not near-copies.

- **Give each claim a machine-built identity from its check, and keep the wording
  free.** Separate two things: what the check mechanically does (which files it looks
  at, what pattern, present or absent) and the English wording. Match duplicates on the
  mechanical part, and let the wording be whatever. Example: `rg -q 'X' src/` and
  `grep -rq 'X' ./src` do the same thing, so they'd get the same mechanical identity
  even though they're typed differently. Because the identity includes present-vs-absent,
  a fact and its opposite stay separate even when worded almost identically. A CI check
  can flag when two live claims share an identity, which catches the hand-committed
  case too. Downside: turning an arbitrary shell command into a clean mechanical form
  is genuinely hard and fragile — realistically you'd only handle a short list of known
  command shapes, and checks that are questions for an agent or a human have no
  mechanical form at all.

- **Compare two signals: what a claim is about, and which way it points.** Represent
  each claim with two separate measurements — its topic, and its direction (is the
  thing on or off, present or absent). Call two claims duplicates only when the topic
  matches AND the direction matches. Same topic but opposite direction is flagged as a
  contradiction, not a duplicate. Example: "feature X enabled" and "feature X disabled"
  share a topic but point opposite ways, so this correctly calls them a contradiction
  instead of merging them. The direction comes from the check itself, which the
  computer can read exactly, so the opposite-fact trap turns into the most useful
  output instead of a dangerous merge. Downside: the whole similarity layer is
  deferred to the hub anyway, and reading the direction off a check is clean only for
  simple commands — for checks that are questions to an agent, the direction is buried
  in the wording.

- **Match duplicates by which decision they support and which files they read.** Two
  claims are candidate duplicates when they back the same decision and read the same
  files. Judge by those connections, not by wording. Example: two claims both support
  the decision "keep the old XML parser" and both read the same parser file — likely
  the same claim twice. If they support the same decision but point opposite ways,
  that's "two contradictory reasons for one decision," which is a louder and more
  useful finding than a plain duplicate. The links are already recorded in the claim
  files, so this works no matter how a file got committed. Downside: it depends on
  authors actually writing those "supports" links (optional today) and on tracking
  which files a check reads (not built yet); a claim with no links is invisible to
  this, so it's a supplement to wording-based matching, not a replacement.

- **Use the checks themselves to find duplicates: two checks that always agree are the
  same check.** Two command-checks are duplicates if they give the same answer in
  every situation. Figure out which files each reads, build a bunch of test worlds
  (including one deliberately broken so the check should fail), and run both checks
  against all of them. If they always give the same verdict, they're the same check in
  practice. Example: two checks meant to confirm "no debug flag in the config" — run
  both against a config with the flag and without it; if they agree every time, keep
  one. A fact and its opposite give opposite verdicts on the same broken world, so they
  can never be falsely merged. Downside: comparing every check against every other is a
  lot of work, it only works for cheap, predictable command-checks where you can stage
  a failure, and building valid test worlds automatically is an unsolved problem in
  general — realistically a hub batch job over an already-narrowed group of suspects.

- **Don't delete duplicates — point the old one at the new one.** Instead of removing a
  duplicate, record that the newer claim replaces the older one. The old one becomes a
  signpost that redirects to the new one. At most one claim per fact is live at a time.
  A person writes this link on purpose, so a fact and its opposite never get linked by
  accident. Example: like closing a question as a duplicate on Stack Overflow — the old
  page stays and points you to the canonical one, and nothing is lost. If a live claim
  matches something already marked as replaced, that's a loud finding. Downside: this
  solves "I found a duplicate, now what do I do about it" — it does not find duplicates
  in the first place, so it still needs one of the detectors above to fire first. It
  also adds a new link type the team has to learn.

- **A claim has to prove it's different before it's allowed in.** Flip the burden.
  Instead of scanning for duplicates afterward, make every new claim prove it isn't
  already covered before it's admitted. It has to show a situation where its check and
  the nearest existing check would disagree. Example: adding "no calls to the old API"
  when a similar claim exists — you must produce a world where one check passes and the
  other fails; if you can't, the claim is redundant and refused. A fact's opposite
  trivially has such a distinguishing situation, so it's correctly let in and flagged.
  This runs in CI over all the claims, not only when you add one. Downsides: finding
  that distinguishing situation automatically is just as hard as proving two checks are
  identical, and it makes writing a claim slower — which fights the goal of keeping
  authoring under five minutes. A gentler version ("warn when we can't tell them apart")
  is doable sooner.

- **Give each outside fact one official address, and have claims subscribe to it.** A
  world-fact gets one canonical address, like `vendor:stripe/rate-limit`. Every claim
  about it points at that address. Then forty claims about the same vendor limit are
  forty subscribers to one address, obviously grouped. Example: instead of forty
  separate claims about Stripe's rate limit, forty claims all reference the one Stripe
  rate-limit address, so the duplication is visible at a glance. Two claims at the same
  address that assert opposite things are an obvious conflict. Downside: this needs
  someone to run and govern the list of official addresses, and authors to agree on
  them — the kind of shared-vocabulary overhead the project wants to avoid. It also only
  helps for outside-world facts, not facts about this repo's own code, which is the
  first thing `claim` targets.

- **Compare the structure of two claims, not just their wording.** Break each claim into
  its structured parts — the key phrases of the statement, and the parsed structure of
  the check — and compare those structures. "Near-duplicate" then means "a small edit
  away." Example: flipping a check from "present" to "absent" is a big change in the
  check's structure even though it's a tiny wording change, so this cleanly separates a
  fact from its opposite. Downside: it has the same fragile check-parsing problem as
  the mechanical-identity option above, and it only catches near-duplicates whose checks
  are built similarly — two claims stating the same fact with totally different check
  strategies look far apart and slip through. Best used as the careful confirming step
  after a wording-based search suggests candidates.

- **Merge duplicates automatically at git-merge time.** Model the whole set of claims so
  that if two agents independently write "the same" claim on separate branches, the two
  merge into one when the branches come together, instead of leaving two copies.
  Example: two agents on two branches each add "library X is pinned to 4.2"; on merge
  they collapse into a single claim rather than two. The merge keys on a fact identity
  that includes direction, so a fact and its opposite do not merge — they show up as a
  genuine conflict for a human. This handles the case where two people create the same
  claim at the same time. Downsides: git's merge works line by line, not by meaning, so
  this needs a custom merge tool and a stable fact identity (from one of the naming
  options above). And same-time duplication is a narrow slice of the problem — most
  duplicates are written months apart, not in parallel. Probably too much machinery for
  where the product is now.

*Sorting it out.* The options split by the two traps. Anything that keys on wording
inherits the opposite-fact trap and needs a human as a backstop. Anything that needs
the checks to actually be run and compared inherits "checks don't compare themselves."
And all of this belongs at the hub, which sees every committed claim, not in a local
gate you can bypass. The options that dodge all of this are the ones that key on the
check's own behavior and run over all the claims at the hub.

---

## Problem 2 — Getting the relevant claims in front of whoever is about to change code

**The problem.** An agent or a person is about to change some code. Before they act on a
stale assumption, they need to reliably see the important facts that govern what they're
touching. The thing that matters is covering the important files — the ones an agent
will actually act on — not every file. A system that covers a hundred trivial files but
misses the one load-bearing file has failed.

**Why this is hard.** Three traps.

First, showing a fact doesn't mean it gets obeyed. You can force a fact into the agent's
context, but the model can still ignore it. As you pile on more instructions, models
follow them less well. In tests, even the best models followed only about two-thirds of
instructions when given five hundred at once, and rarely followed all of them perfectly
in real multi-step runs. Rules in the prompt are advisory by design — the model may or
may not act on them.

Second, "let the model decide to look it up" reliably fails. When agents have to choose
to call a tool to fetch information, they often just don't. In one test agents used the
available tool only about 6% of the time. Optional rules, published reference files, and
passive skills all fire far less than you'd hope. And once an agent has more than about
twenty tools to pick from, its ability to pick the right one falls off a cliff. The one
layer that always works is a harness hook — code the agent runtime runs automatically,
which injects the fact directly whether the model asked for it or not.

Third, the coverage gap. All of this assumes a claim knows which file is being edited —
that its links or its read-set name that file. But the change that makes a fact false
often lands in a file no claim mentions. Example: a claim guards an XML parser, but the
breaking change is in an upload handler that only reaches that parser eight months and
several hops later, in a file nowhere near it. No claim is watching that file, so
nothing fires.

The research adds a cost the "always show everything" approach pays. Injecting more is
not free. Machine-written always-on instruction files actually made tasks succeed about
3% less often and cost 20% more. A big rules file isn't skimmed and acted on — it dilutes
the model's attention and buries the one fact that mattered. And simply publishing a file
that agents could read does nothing; something has to actively put it in front of them.

**The current plan.** Use harness hooks to run the tool, fall back to the hub's tool
interface, and use the "supports" links as the map from files to facts. Concretely:

- **Harness hooks** that run the tool automatically. One runs at the start of a session
  and shows a short digest. Another runs right before an edit, matches the file being
  touched against each claim's links and read-set, and injects only the matching facts.
  This guarantees the fact is present, without relying on the model to fetch it.
- **The command-line tool as the one way to fetch claims.** Everything calls the same
  tool (`claim list --json`, `claim check --json`). It's greppable, works in every
  harness, and is exactly what the hooks run under the hood.
- **A short pointer in `CLAUDE.md` / `AGENTS.md`** that tells the agent the tool exists —
  without dumping all the claims into that file.
- **The hub's tool interface last**, as a thin adapter for places that can't run a shell.

The shared principle: the fact is shown to the agent as dated evidence to weigh, never as
an order it must obey, and nothing is blocked. The honest catch: hooks are per-harness
glue `claim` has to build and maintain, showing a fact doesn't force obedience, and it all
assumes the read-set names the file being edited.

**Other options** (not being built). They fall into three groups.

*Group A — make breaking a claim actually fail the change (obedience for free).*

- **Turn the relevant checks into part of the build, so a broken fact turns the change
  red.** Compile the governing claims' checks into the change's own build or CI. If a
  load-bearing claim has drifted, the change fails to build, with the fact and its owner
  printed in the failure. Example: you edit the config that pins library X; the check
  fails the build and says "library X pin is no longer valid — owner: Dana." Now obedience
  is beside the point — the agent can't merge a red build, and it covers exactly the
  files the check reads. Downside: this is the end state where a claim becomes a real
  test, and making blocking the default brings back the failure where authors just delete
  the assertion to make the red go away. It clashes with the principle of nagging rather
  than blocking. It belongs as a deliberate opt-in per claim, off by default.

- **Refuse the edit until the person has seen the fact.** A pre-edit hook that doesn't
  just show the fact but refuses the edit when the file is governed by a claim the person
  hasn't acknowledged, giving the fact as the reason for refusal. Example: you try to edit
  the parser; the hook blocks the edit and says "you must acknowledge: this parser must
  stay in sync with the schema." This turns "here's a fact" into "you may not proceed until
  you've seen this," and it only fires for load-bearing claims, so the friction lands on
  the files that matter. Downside: it's a hard block on the person making the change (clashes
  with the nag-not-block principle), it's specific to one harness, and if the agent can just
  auto-click "acknowledged," it's theater.

- **A gatekeeper that checks every change against the claims and admits or rejects it.** A
  layer that sits between the change and merging. It evaluates the change against a policy
  built from the claims — "no change may break a load-bearing claim without the owner
  acknowledging it" — and admits or rejects, like an access-control decision. Example: the
  gatekeeper sees a diff that would drift a load-bearing claim, finds no owner acknowledgment,
  and rejects it. Because it looks at every change, coverage is guaranteed rather than hoped
  for — especially combined with the "watch everything that reaches these files" option below.
  Downside: a full policy layer is a huge amount of surface and a brand-new required workflow,
  which is exactly the kind of thing that kills adoption. It also re-centralizes decisions the
  plan deliberately keeps with individual owners.

- **Treat editing a governed file as a permission the agent doesn't have by default.** The
  agent isn't allowed to edit a governed file until the governing claims have been shown to
  it (and, for high-stakes claims, acknowledged); then it gets the permission for that session.
  Example: the agent can't touch the auth module until it's been shown the claim "sessions must
  expire in 24h," after which it's granted edit access. The model can't skip the fact, because
  skipping it means never getting permission to edit, and permission is granted only for the
  load-bearing regions, so trivial files carry no friction. Downside: it fights the harness's own
  permission system, it's per-harness, and in practice only one harness's "deny" feature comes
  close — it's really the "refuse the edit" option above with more machinery.

*Group B — close the coverage gap, so the fact reaches the agent even when it's editing an
unwatched file.*

- **Watch everything that can reach the check's files, not just the files themselves.** Widen
  each claim's watch set from "the files the check reads" to "everything that can reach those
  files" — callers, importers, things that depend on them, several hops out. Then a change three
  hops away still surfaces the fact. Example: the claim guards the XML parser; this widens its
  watch set to include the distant upload handler that eventually reaches the parser, so editing
  the handler surfaces the parser's claim. This is the one option that would have caught the
  eight-months-later upload-handler case. Downside: building a correct "what reaches what" map is
  per-language, expensive, and never fully reliable when code uses dynamic dispatch, reflection, or
  foreign calls. A map that silently misses the one crucial caller is worse than honestly watching
  the whole tree. Ship it as a speed-up over "just check everything," not as the floor.

- **Learn which files, when changed, tend to break which claims — from history.** Watch the
  history: which files, when edited, have in the past tended to coincide with a claim drifting.
  Then surface that claim on future edits to those files, even when nothing structurally connects
  them. Example: history shows that edits to a certain fixtures file have repeatedly preceded a
  claim about parsing going red, so future edits to that fixtures file surface that claim. This
  catches connections no structural analysis sees — config, fixtures, indirect couplings. Downside:
  it needs a long history that doesn't exist until the tool has run a while (nothing to learn from
  on day one), it's probabilistic, and false connections are exactly the kind of noise that makes
  people mute the channel. It also adds a guessing component to a product that's otherwise
  deterministic.

- **Match the change to claims by meaning, not by file path.** Take the changed lines, measure
  their meaning, and pull up the claims whose statements are closest in meaning — so a fact reaches
  the agent when the change is conceptually about the same thing, regardless of which file it's in.
  Example: a change reworks how dates are parsed; even in a file no claim names, this pulls up the
  claim "all timestamps are stored in UTC" because it's about the same topic. Downside: it hits the
  same opposite-fact trap and the same precision problems as duplicate-finding — opposites look
  similar, and matching by meaning floods the agent with near-misses. Flooding is exactly the
  attention-dilution that buries the one crucial fact. Useful only as a candidate generator behind a
  filter, never as the delivery path.

*Group C — make the fact impossible to skip, not just present.*

- **Make the agent read the facts back before a governed change.** Before a governed change, the
  agent must produce — not just receive — the governing facts and say how its change relates to each.
  It can't proceed without answering. Example: before editing the payments module, the agent must
  write out "the claims here are: refunds must be idempotent, and amounts are in cents; my change
  affects neither" before it's allowed to continue. Making the agent say the fact back is a much
  stronger sign it actually processed it than silently injecting it, and the checklist only lists the
  facts governing the touched area, so it's just the crucial ones. Downside: judging whether the
  agent really engaged (versus rubber-stamping "does not affect") needs another unreliable model step,
  and it adds delay to every governed edit. An unjudged checklist rots into theater.

- **Show nothing at the start and the full fact only at the exact moment of the action.** Disclose
  nothing at session start. Deliver the full governing fact at the instant of the governed action, so
  it arrives with nothing else competing and maximum relevance, late in the context where the model
  pays the most attention. Example: no claims at session start; the moment the agent edits the rate
  limiter, it gets exactly one fact — "this limit is shared with the vendor's; don't raise it past
  1000/s." Downside: this is basically the pre-edit hook the plan already has, done with discipline, so
  it's barely distinct — and dropping the session-start digest loses a useful surface. Better to fold
  its "one minimal fact at the edit" discipline into the planned hook than to build it separately.

- **Gate edits to a region behind a per-region switch, and tighten it only as the claims earn trust.**
  For each region of code, a switch: off, warn, or block. Move a region from warn to block only once
  its claims have proven trustworthy (they discriminate well, they rarely false-alarm). Enforcement
  grows with confidence, and you never need to cover the whole codebase at once. Example: the auth
  module's claims have a clean track record, so its switch goes to "block"; a noisy new module stays at
  "warn" until its claims settle. Enforcement lands only where a region has earned it, dodging the
  false-alarm fatigue that indiscriminate blocking causes, and the tightening aims at the load-bearing
  regions first. Downside: it presumes both a blocking mechanism (Group A) and a track record (a mature
  history of results), neither of which exists early on — it's the governance layer on top of the
  blocking options.

*Sorting it out.* Watching everything that reaches the files attacks the coverage gap that the other two
traps sit downstream of — you can only deliver a fact for a file the map names. Turning a violated claim
into a build error is the strongest answer to "present isn't obeyed," but it's really a promote-this-to-a-
gate move, not a way to consult a claim, and the plan already contemplates it. Making the agent read the
facts back is the best near-term, low-infrastructure lever on obedience, buildable as a small skill or hook
over the tool the plan already ships. None of these store a result or a status.

---

## Problem 3 — Knowing when to record a claim, and covering the important files without minting junk

**The problem.** A decision worth recording has to become a claim at almost zero effort, or the list of
claims never grows big enough to be worth anything. Knowledge tools that rely on people to write things down
by hand — architecture decision records, wikis, internal knowledge bases — all rot. The goal is to cover the
important files — the load-bearing decisions an agent will act on — without minting junk.

**Why this is hard.** Three traps.

First, most corrections aren't checkable. Most of the time when someone corrects the machine ("don't use
barrel imports here"), there's no way to write a command that could ever prove it true or false. Force a
check onto it and you've minted a check that passes forever and means nothing.

Second, some facts can't be re-staged to test. For incident facts and outside-world facts, the honest check
would cost a whole project to build. So people write a cheap check that tests a stand-in for the real fact,
and the stand-in drifts away from the fact it's supposed to represent.

Third, chasing a coverage number brings back the junk problem. If you hit a coverage target by minting
checks that are green forever, you've re-created the pollution from problem 1 (recall the AI-memory product
that found almost all its stored memory was junk).

The research adds the economics. Architecture decision records fail at both ends: they're a pain to write
at decision time, and nothing ever notices when they go stale. The root cause is structural — the person who
pays to document the reasoning isn't the person who benefits later, so it doesn't get done. Every recent
system converged on the same answer: the agent proposes, a human confirms. The tolerable effort for
authoring is about zero; people will review a good draft, but only inside a workflow they were already in,
like a pull request. And propose-and-confirm only works when the proposals are rare, high-quality, and
already machine-checked — otherwise it becomes alert fatigue people learn to ignore.

**The current plan.** A ratchet on exception-diffs, plus triggers on corrections and incidents. Concretely:

- **The exception-diff ratchet.** When a diff adds a new dependency pin, a suppression, a skip, or a
  waiver, require a claim in the same pull request. This is mechanical and never wrong about *whether* a
  decision happened, and it lands in the workflow the author is already in.
- **Correction moments.** When a human corrects the machine — the highest-signal moment — the agent drafts a
  statement and a check and runs `claim add`. This only proceeds if a real discriminating check can be
  derived; if not, it's routed to a plain rules file instead.
- **Incidents and reverts.** Capture a claim right after the cost of not-knowing was just paid, when a
  broken state actually exists to write the check against.

All three fire when a decision happens. The honest catch: the check the author actually writes tends to
verify the artifact ("the manifest still says 4.2"), not the reason ("the upstream bug is still unfixed"),
and it stays green after the reason is gone. That's the tautology problem (problem 4), which the birth gate
doesn't prevent.

**Other options** (not being built). These sit on a different axis from the planned triggers. Group A makes
the *absence* of a claim fail. Group B figures out *which* files are important, independent of any triggering
event. Group C attacks the always-green-check problem at its source. Group D makes capture a byproduct of
work that was happening anyway.

*Group A — make the absence of a claim fail.*

- **Fail CI when an important file has no claim covering it.** A repo-local lint that fails CI when a file on
  a maintained list of important files has no claim covering it — the way some type systems refuse new code
  that isn't typed. Example: a file is on the "important" list but no claim references it, so CI fails until
  someone adds one (or explicitly waives it). It never demands a *check* — it only demands the file be
  accounted for, including an explicit "no check here" claim (still dated, attributed, and still degrading to
  a scheduled human look) or a waiver. So no tautology is forced, and it rewards accounting rather than
  volume, which keeps junk bounded. Downside: it needs the list of important files (Group B) first, and the
  "no check here" escape hatch risks becoming a rubber stamp — it wants pilot data on whether those convert
  into real checks. Different from the plan: the plan fires when a decision *appears*; this fires when an
  important file *lacks* coverage.

- **Find reasons written down with nothing pointing at them, and demand they be captured.** Walk the
  "supports" links backwards: find decision artifacts that nothing points at — a security-scan suppression
  with a `reason:`, an eslint-disable with a description, a `# noqa` with text — and demand the reason be
  captured as a claim. Example: a Trivy suppression says "ignoring CVE-1234, not reachable in our usage" and
  no claim references it, so the tool demands one. It only fires where an artifact already carries half a
  decision, so the reason prose is already written (near-zero effort), the set is finite and already exists
  (no flooding), and suppressions are load-bearing by nature. Downside: parsers for each of these formats are
  maintenance work, and it's the read-only cousin of the exception-diff ratchet — the ratchet ships first
  because it catches new ones at the cheapest moment. Different from the plan: the plan gates *new*
  suppressions; this harvests the *existing backlog*.

- **Find comments that assert a breakable fact and nag that they aren't claims.** Detect comments that
  assert a fact that could break from the outside — "must stay in sync with the schema," "assumes X is never
  null here" — and nag that the fact isn't a claim. The comment is a claim that forgot to attach a check.
  Example: a comment says "// keep this list in sync with config.yaml" and the tool flags it as an unclaimed
  fact worth binding a check to. Only the breakable comments get escalated; pure preferences ("prefer short
  functions") are ignored, so no tautology is minted, and the comment text seeds the statement. Downside:
  telling breakable facts from preferences reliably is unproven, and false positives here are pure noise — it
  wants the important-file scoring first so it only scans the important files. Different from the plan: the
  plan triggers on a *change*; this mines prose already sitting in the repo.

*Group B — figure out which files are important, then drive coverage of those.* (None of these mint claims.
They produce a ranked list of important files that Group A gates against and the triggers prioritize.)

- **Rank files by how much depends on them.** Rank files by how much rests on them — how many things import
  them, how central they are in the dependency graph — and call the top slice important. Example: a core
  auth module that three hundred files import ranks at the top; a one-off script ranks at the bottom. Ranking
  mints nothing, so it can't create a tautology — it only aims the other mechanisms. Effort concentrates on
  the roughly 5% of files most changes route through, so a small set of claims covers a large share of what
  agents act on. Downside: analyzing imports across languages is language-specific work, and a single team's
  small codebase has a short, obvious list of important files anyway — this earns its keep only over real,
  large source trees. Different from the plan: the plan has no file targeting; this decides where coverage
  *should* exist before any artifact appears.

- **Rank files by how often they change AND how much depends on them.** Rank by change frequency times how
  much depends on the file — files that change a lot *and* are depended on heavily are where a fact is most
  likely to quietly rot. Example: a config module that's edited every week and imported everywhere ranks
  highest, because that's where a stale fact will bite. It's just a ranking, mints nothing, and targets the
  rot-prone files where a checked fact pays off most. Downside: it needs real history to mean anything, and
  it adds a second signal to tune before the simpler "how much depends on it" ranking is even validated.
  Different from the plan: the plan is event-driven and ignores history; this is history-driven and ignores
  events.

- **Rank files by how few people understand them.** Rank by how few people understand a file — one dominant
  author, few contributors, untouched for a long time by anyone still active. These hold the facts most
  likely to be *lost* (the one person leaves) rather than *broken*. Example: a payments file only one
  engineer has ever meaningfully touched, and they're about to change teams — capture what they know before
  it's gone. It surfaces the tacit facts in one person's head that no diff will ever trigger, a category the
  event-driven plan can never reach, and it's naturally low-volume so it can't flood. Downside: capturing
  here means interviewing a human — high effort, and it's the expert who pays — so it needs the
  agent-drafts-from-the-interview loop first, so the expert only has to review. Different from the plan: the
  plan captures *observed* decisions; this captures *unobserved* knowledge from who-touched-what, which git
  already records.

*Group C — attack the always-green-check problem at its source (make the check actually discriminate).*
(These add to the plan's triggers rather than replacing them; the deeper list is problem 4.)

- **At birth, require the check to fail against a broken version of the world.** When you run `claim add`,
  don't just require the check passes now. Also require it *fails* against a machine-generated broken version
  of the world that should break the fact. Example: for a check that greps for `libfoo==4.2` in the manifest,
  the tool copies the repo, changes that line to `libfoo==4.3`, and confirms the check now reports drifted.
  A green-forever check survives the break and gets refused; a preference with no discriminating check gets
  routed to a rules file. Downside: generating a *meaningful* break is easy for "a line in a file" greps and
  hard in general, and it can't run against outside-world or agent checks — a tool for command-checks, opt-in
  first. Different from the plan: the birth gate proves a check *can pass*; this proves it *can fail* — the
  missing half.

- **Ship a library of proven check templates, one per kind of decision.** Ship discriminating check templates
  indexed by decision kind — CVE-not-applicable, version-pin, flaky-test-skip, banned-import — and the
  trigger fills in the blanks. Example: for "we pin library X," the template is "assert the manifest line AND
  probe that the upstream issue is still open," so the author can't accidentally write the green-forever
  version. Decision kinds with no good template (pure preference) route to a rules file instead of minting a
  junk check. Downside: the template set has to be earned from real examples per decision kind — shipping
  guessed templates just moves the guessing problem up a level. Different from the plan: the plan says *when*
  to capture; this says *what check to write* once you're capturing.

- **Have the hub flag checks that have never once gone red despite churn around them.** The hub flags a check
  that has *never* reported drifted across its whole life, even though the files it reads keep changing around
  it — a likely tautology, the way a test that passes 100% of the time across constant change is suspected of
  testing nothing. Example: a check on a file that's been edited fifty times has never gone red once; the hub
  flags it for a human to look at. It catches the tautologies the birth gate and templates missed, using
  history only the hub has. It never auto-deletes — it routes to a human with the evidence. Downside: it needs
  the hub, the history of results, and file-read tracking (not built yet), plus a long history to mean
  anything. Different from the plan: the plan checks quality once, at capture; this is continuous, catching
  tautologies that slip past every birth-time gate.

*Group D — make capture a byproduct of work already happening.*

- **At the end of a session, have the agent distill what it had to learn into candidate claims.** When an
  agent finishes a session that touched important files, have it distill "what did I have to learn or assume
  to get this right?" into candidate claims — from its own session, since it just paid the cost of figuring
  it out. Example: an agent that just fixed a timezone bug proposes "all stored timestamps are UTC; the API
  layer converts on the way out." A human confirms at pull-request time (rare and pre-validated — the only
  propose-and-confirm shape that survives), and the agent runs the birth gate before proposing, so
  preference-only learnings drop out. Downside: it depends on the harness-hook integration and the
  important-file scoping, and agent-drafted claims are trusted less by design — the confirm experience has to
  be excellent before it scales. Different from the plan: the plan triggers on a specific artifact or a
  correction; this triggers on any important-file work session, catching discoveries that never showed up as
  a suppression or a correction.

- **Make coverage a visible, owned number on a dashboard.** Make "percent of important files covered" a
  visible, owned number on the hub dashboard, the way test coverage or vulnerability burn-down is, so a team
  can run a bounded campaign — "cover our top 50 important files" — instead of hoping the ambient triggers
  add up. Example: the dashboard shows "38 of your top 50 important files are covered," and a team drives it
  to 50 as a project. The denominator is the *important set* (Group B), not "all files," so 100% is small,
  reachable, honest, and can't be inflated by minting junk on trivial files. Each covering claim must pass
  the birth gate to count. Downside: it's a hub UI feature that presumes the important-file scoring and a
  real codebase, and making coverage a *target* invites gaming unless the quality gates above are already
  strong. Different from the plan: the plan is bottom-up and ambient; this is top-down and bounded — the
  funded-project shape where doc-coupling actually gets done.

*Sorting it out.* The research says the biggest threat is the always-green-check problem (Group C / problem
4), not having too few claims. Scaling coverage the naive way *is* the pollution problem. The strongest moves
make coverage safe to grow (require the check to be able to fail; ship proven templates), aim it at the files
that matter (rank by dependence; gate on coverage), and keep it honest over time (flag the never-red checks).
All are distinct from the planned triggers: the plan decides *when* a decision is captured; these decide
*whether the check is worth anything*, *which files must be covered at all*, and *how the list stays honest as
it grows*.

---

## Problem 4 — Making sure a check actually fails when the fact goes false

**The problem.** The checks people write tend to be hollow. They verify the artifact ("the manifest still
says 4.2"), not the reason ("the upstream bug that made us pin it is still unfixed"), and they stay green
forever after the reason is gone. When you add a claim, the tool proves the check *can pass* right now. It
does not prove the check *can fail* when the fact becomes false. The property `claim` actually needs is this:
the check goes red exactly when the fact stops being true, and stays green as long as it's true. The research
calls this the single biggest threat to the whole product. Get it right and `claim` is a painkiller. Get it
wrong and it's a fancier version of the architecture decision records that history shows rot.

**Why this is hard.** The one signal the plan has for this — an optional "witness" command the author can
supply to prove their check can go red — has four built-in weaknesses any solution has to beat.

1. The author writes the fake-break themselves. The same person who wrote a hollow check writes the "proof"
   that it works, and they'll write the one break their check happens to catch (change 4.2 to 4.3 in the
   file), not the one that matches the fact actually going false (upstream shipping the fix). It proves *a*
   red exists, not that the red tracks the fact.

2. One break, one direction. It tests a single point. It says nothing about all the other ways the fact could
   go false, and nothing about false alarms (does the check stay green when the fact is still true but the
   code moved around it?).

3. It's only tested once, at authoring time. A check that worked at birth can rot into a hollow one as the
   code around it changes. Example: the check greps a file that later gets renamed; now it matches nothing and
   passes vacuously, forever.

4. It's advisory and unrecorded. Nothing downstream knows whether a claim was witnessed, so you can't filter
   or rank the list by how well the checks discriminate.

There's an honest floor here. Every recorded reason is one of three kinds:

- **Checkable** — a check exists that can tell truth from falsehood. This is the only kind the core mechanism
  is for.
- **Expirable** — no discriminating check, but there's a natural clock. Use the existing "skip until this
  date" feature.
- **Just prose** — a preference with nothing observable to check. This isn't a claim at all. It belongs in a
  plain rules file.

Any machinery here has to be able to say "this reason can't be made into a real check" and refuse to pretend
otherwise. A tool that forces every reason into "checkable" manufactures exactly the hollow checks it's
supposed to prevent.

**The current plan.** The witness command, as an optional confidence signal. When you add a claim, the tool
makes a throwaway copy of the repo outside your working tree, runs the author's command to break something in
that copy, runs the check there, and requires it reports drifted. It's observed once and never recorded (a
result is telemetry, not something committed), and it's never a hard gate — a fact whose red can't be staged
is verified by its passing check alone. Making the witness mandatory would fix only weakness 4 and weakly
weakness 1. Every option below attacks at least one of weaknesses 1 through 3 in a way that just making the
witness mandatory can't. All of them keep the invariants: a check that breaks under a perturbation counts as
broken, never as a pass; the tool owns "is this the absence of a thing," never a shell's interpretation of
`!`; and discrimination results are reported, never committed.

**Other options** (not being built):

- **Break the world many ways automatically and require the check to catch most of them.** Instead of one
  author-written break, have the tool generate a batch of breaks (from the files the check reads) and require
  the check to catch a required fraction. Report which ones slipped through. Example: for a check on a version
  pin, the tool auto-generates twenty variations of the manifest; if the check catches only two, it's mostly
  hollow, and you see the eighteen it missed. Because the tool generates the breaks, the author can't
  cherry-pick the one their check catches, and it tests many points instead of one. A score near zero means the
  reason is really expirable or just prose. Downside: generating breaks that actually correspond to the fact
  (rather than any random change) requires understanding what the fact means; a dumb byte-flipper produces
  dismissible breaks and re-creates fatigue. This is the same core idea as "use the check to fail against a
  break at birth" (problem 3) and "two checks that always agree are duplicates" (problem 1).

- **Test a relationship, not a single point: if the world moves toward false, the verdict must move toward
  red.** Instead of proving one red exists, assert a relationship. If the world changes toward "fact false,"
  the verdict must move from held to drifted. If it changes toward "even more clearly true," it must stay held.
  Example: for "no debug logging in production config," adding more non-debug settings must keep it green, and
  adding a debug setting must turn it red. A hollow check gives the same answer no matter what; a real check
  responds in the right direction. This is the only thing that tests the "stays green when still true" side,
  which catches the check that cries wolf on any change at all. Downside: it asks the author to describe a
  relationship, which is more than "paste the command" — this is the natural next version of the witness.

- **Generate many worlds, label each with an independent source of truth, and require the check to agree.**
  Generate many versions of the world, label each one with the fact's real answer from an independent source,
  and require the check's verdict to match every time. The check discriminates only if it never disagrees.
  Example: for "no import of the old library," generate lots of code trees, some with the import and some
  without — you know the answer by how you built each one — and require the check to get every one right;
  shrinking finds the smallest change the check fails to notice. Downside: it needs a per-fact-shape generator
  and an independent answer key, which is real machinery, worth building only for a few common structural
  shapes.

- **Deliberately break the check's own environment and require it not to pass.** Deliberately sabotage the
  check's surroundings — rename the file it reads, remove the binary it calls, empty its input — and require
  the verdict is anything but "held." Example: a check that greps a file for a pattern — rename that file; if
  the check still says "held," it was passing because it matched nothing, which is a hollow check. This targets
  a specific, common failure: a check that passes because it found nothing (a grep with a typo, a file-exists
  test on a path that moved). It's the "broken never passes" invariant turned from a runtime guarantee into an
  audit of the check itself. It's the cheapest, most general, lowest-false-alarm probe — it needs no
  fact-specific answer key, just the check's own inputs. Downside: it needs file-read tracking (not built yet)
  to know what to sabotage.

- **Re-prove discrimination on a schedule, not just at birth.** Re-prove that a check can still fail
  periodically, not just once at birth, because a check that worked at authoring time can rot as the code
  drifts. Example: a check that greps `mod_a.rs` works until someone renames that file to `mod_b.rs`, after
  which it matches nothing and passes vacuously; re-running the probe on the hub's schedule catches that decay
  and routes it as its own kind of drift — "this claim's check can no longer fail." The tool stays stateless
  (`claim probe <id>` reports now and stores nothing); the hub does the scheduling and remembering. Downside:
  it needs the hub's scheduler and one of the probes above to schedule — it's the layer that runs the others
  over time.

- **Have an independent agent hunt for a world where the fact is false but the check still passes.** Task a
  separate agent with finding a situation where the fact is false but the check still says "held." If it
  finds one, the check doesn't discriminate, and that situation is the proof. Example: for "the upstream CJK
  bug is still unfixed," the agent reasons about what an upstream fix would look like and finds that the check
  would still pass after such a fix — so the check is hollow. It handles facts too meaning-heavy for
  mechanical breaking, and it's independent of the author, so it defeats the author-writes-the-fake-break
  problem even for judgment-shaped facts. Repeated failure to find a hole is a signal the fact is really just
  prose. Downside: it needs trusted, metered agent runs, and an agent is non-deterministic — it can miss a
  real hole or invent a fake one, so it needs a spot-audit apparatus (not built yet).

- **Before accepting a check, make the author name what would make the fact false — and refuse if they
  can't.** Before accepting a check as real, require the author to name the *falsifier* — the observable
  event that would make the fact false. If they can't name one, refuse to file it as a real claim. Example:
  "upstream closes the bug" is a nameable falsifier, so it goes down the check path; "I prefer this style" has
  no falsifier, so `add` refuses and prints "this is a rule, not a claim." A date-only falsifier ("this is
  fine until the next release") steers to "skip until" instead. This is the classifier the three-kind taxonomy
  needs, enforced right at the front door — the cheapest and most important single move, because most hollow
  checks are preferences or expirables forced into a check. Downside: it adds friction at authoring time (the
  thing that kills adoption past about five minutes), and a clumsy version nags authors into writing fake
  falsifiers, which is worse than nothing — it needs calibration data first.

- **For facts over a formal structure, prove the check equals the fact.** For facts you can express over a
  formal structure — an import graph, a config schema, a type — *prove* the check is equivalent to the fact
  with a model checker, instead of sampling breaks. Example: for "nothing in this package imports the
  networking module," prove over the import graph that the check catches every possible violation, so no
  counterexample can exist within the bound. A proof is the strongest possible guarantee. Downside: it's
  enormous machinery for a narrow slice — most `claim` facts (upstream behavior, vendor limits, how text
  renders) have no formal model, and it clashes with the "thin shell over grep" spirit. Conceivable only as a
  few proven templates, far in the future.

- **Ship checks built from a vetted library, and trust those more than raw shell.** Ship a curated library of
  check templates, each one proven or shown by experience to discriminate for a given fact-shape — import
  absent, version pinned, upstream issue open, file hash stable. A claim built from a template inherits proven
  discriminating power. A claim built from raw shell is marked lower-trust and sent to the heavier probes.
  Example: a claim that uses the "import-absent" template is trusted; a claim someone wrote as a hand-rolled
  grep is flagged for extra scrutiny. This moves trust from auditing thousands of individual checks to
  auditing a handful of templates, and it makes the good check the easy path. Downside: it requires knowing
  the common fact-shapes, which only a real body of claims reveals — a premature library encodes the wrong
  primitives, and it doesn't help genuinely novel facts. This is the same idea as "ship a template library per
  decision kind" in problem 3.

- **Measure, don't guarantee: flag checks that have been green their whole life despite churn.** Don't gate
  any single claim. Instead, let the hub measure how often each check has *ever* changed its verdict, and flag
  the checks that have been green their entire life despite the files they read changing repeatedly. Example:
  a check whose files have been edited a hundred times but that has never once gone red is flagged as a likely
  tautology. It's purely observational and costs the author nothing — a check that's provably held on every
  run across real change is a strong statistical tell without breaking anything on purpose. It tells apart a
  real check that just never had a reason to fire (its files never changed) from a hollow one (its files
  changed and it still never fired). Downside: it needs the hub, the history of verdicts, and file-read
  tracking (all not built yet), plus a long history — a late-stage backstop, blind at the start. Same idea as
  the "flag never-red checks" option in problem 3.

*Sorting it out.* None of these makes an unprovable fact provable — they make a *checkable* fact's check
provably able to fail, or they *detect* that a check can't. Naming the falsifier up front (and being willing
to say "this belongs in a rules file") is the only honest answer for pure prose, and "skip until a date" is
the answer for expirables. The mechanical probes (break the world; sabotage the check's inputs) are cheapest
and most predictable. The reasoning agent reaches facts no mechanical break can touch, but costs
predictability. Re-proving on a schedule and measuring over history add the time dimension the stateless tool
can't hold by itself.

---

## The business question (background, not a problem to solve)

This is context for weighing the problems above. It's not a fifth problem. All of it is from the research.

- **A well-funded neighboring category exists, and `claim`'s exact spot is empty.** "Agent memory" — tools
  that give AI agents a memory — is clearly a funded category as of mid-2026. Several companies have raised
  money, including one headline raise of $98M. Coding-agent tooling in particular is where the big money is.
  But every funded memory company does the same thing: it stores and reconciles things that were *said* — it
  resolves conflicts, tracks what's newer, and so on. None of them verifies a fact against reality with a
  command you can run. The nearest funded neighbors sell search and doc-automation, betting that good search
  makes explicit capture unnecessary. So `claim` would be defining a small new category next to a hot funded
  one, not fighting into a crowded field. Honest caveat: the competitor sweep didn't finish, so a quiet or
  open-source neighbor can't be fully ruled out. The nearest one anyone named detects "code changed near this
  doc," which is not the same as checking whether a fact is still true.

- **The problem doesn't have a name yet.** "Context engineering" names the *practice* of curating what you
  feed a model. "Context rot" means a model getting worse over a very long input. Neither names `claim`'s
  problem — *recorded knowledge quietly going false over time*. That's both an opportunity (name it) and a
  cost (you have to teach buyers the problem exists). The "CI for facts" or "Dependabot for facts" framing
  helps, because it borrows a category buyers already understand.

- **Protocol versus product.** The file conventions agents read (AGENTS.md, MCP, llms.txt) are standardizing,
  and a third party can't really make money off them — "publish a file agents can read" captures no value.
  What's left to sell is the verification loop and the hub's derived intelligence: the schedule, the drift
  routing, the cross-repo index. That's exactly the split the plan already commits to — a free, open format
  and CLI (which drives the coverage the value depends on) plus a paid hub (which captures the over-time,
  cross-repo intelligence a stateless file can't hold).

- **The wedge: painkiller, not vitamin.** The one funding-validated painkiller framing in 2026 is
  agent-cost and agent-reliability. A stale fact in a CLAUDE.md file gets inherited every session and wastes
  agent runs — one person feels that in week one. The compliance/audit angle (managing CVE waivers) is
  plausible but unproven as a market, and selling to security means a slow process-adoption slog before
  anyone sees value. The trap to avoid is selling "better organizational knowledge" or "a second brain" —
  every hand-written knowledge tool that sold that framing rotted.

- **The hard empirical limits on survival.** Precision, not recall, decides whether the nag channel lives.
  Once more than about 10% of drift alerts go un-acted-on, engineers mute the channel. (An "effective false
  positive" is defined by a human not acting, not by whether the tool was technically right.) Integrate at
  the pull-request or CI run, not a separate dashboard: one analysis tool's fix rate jumped from basically 0%
  to over 70% purely by moving from a batch report to the diff, at the same false-positive rate. Bootstrap by
  ratchet: new exceptions must carry a claim, the existing backlog is grandfathered, and coverage only grows.
  And gate on a *passing check*, not mere presence — forcing a review to happen doesn't make the reviewed
  content correct.

- **The honest bottom line.** `claim` aims at a real, funding-validated problem from an angle nobody funded
  has taken (executable verification), with a wedge the market confirms and an architecture the historical
  record endorses. Its survival turns on the one thing the design doesn't yet guarantee: that the checks
  people write actually fail when the fact goes false (problem 4). The top adoption risks, in the research's
  order: (1) the hollow-check problem, (2) false-alarm fatigue above ~10%, (3) the bet that good search makes
  explicit capture unnecessary, (4) a big incumbent fast-following (one major vendor's ship-and-verify-at-read
  design is one step from executable verification), and (5) the cold-start coverage problem plus the "who
  writes the check" friction. `claim`'s edge is being executable, deterministic, git-committed, and
  PR-reviewed — harder to fake than a prompt-based re-check, but a harder sell and a smaller initial market
  than "memory for all agents."
