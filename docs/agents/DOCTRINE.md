# Project doctrine

Agent reference, read when the work touches it. Moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line summary and a pointer here.

## darkmux's grand vision (agent-facing)

The user-facing **"What darkmux is for"** section in `README.md` is the canonical version of the project's north-star. Below is how the same five claims translate into operational doctrine for an AI agent (Claude Code, OpenClaw, Cursor, etc.) working on darkmux or driving it on behalf of an operator.

1. **Optimization, not replacement.** When the operator asks you to pick a model from `lms ls` or propose a profile, prefer *complement* over *duplicate*. A team where every model is a 35B reasoner is not a team — it's a stack of identical instruments. The same logic applies *within* each role family (see **Project posture → Role families** below): a profile with three different 35B specialists and no 4B utility agent is missing its compactor; conversely, a profile of nothing but utility agents has no specialist to do the actual judgment-dependent work. Read the existing profile registry first; propose additions that fill gaps in the right family (utility: compactor / radio router; specialist: coder / reviewer / analyst) rather than swapping like for like.

2. **Harness, then model.** When the operator reports slow or wrong outputs, **check the harness before the model**. Compaction config, context-window mismatches, loaded-state drift, profile-vs-loaded model — all of these can produce large wall-clock regressions that look like model problems but are actually harness problems. Default action: run `darkmux doctor`, read the eureka findings, surface those *before* suggesting the operator change models.

3. **The lab + the loop.** darkmux is not just an inspection tool — it's the loop. When you have a tuning hypothesis (e.g., *"primary at 64K instead of 100K might fit this 32GB tier"*), the correct action sequence is: **baseline → single-variable change → re-measure → compare → record in notebook**. Each step has a darkmux primitive. Do NOT skip the baseline. Do NOT change two variables at once. The discipline is the point — without it, the comparison is uninterpretable.

4. **Team integrity is your responsibility.** When proposing config changes, frame them in terms of *how this affects the team's shape*, not just an isolated metric. *"Drop the compactor to free RAM"* reduces working memory; consider whether the remaining team can still handle long-agentic dispatches before recommending. The operator is depending on you to maintain team coherence as new models arrive and hardware changes.

5. **The success criterion is recursive.** A fresh agent session, given only a clean-slate darkmux install + these docs + the bundled skills, should reach the same conclusion about *"what is darkmux for?"* as the rest of these doctrine entries name. If you find yourself uncertain or having to infer from primitives, **the docs have drifted from the vision** — surface that to the operator. Doc drift is a bug, not a footnote.

These claims compose with the existing **Anti-patterns** section below: anti-patterns are *what not to do*; the vision is *what to do instead*. If a request would violate both at once (e.g., *"silently roll back the compactor without telling me"*), the vision wins — surface the conflict and let the operator decide.

## Project posture

**darkmux is an AI-first local-AI orchestrator.** It uses local-AI internally to manage your local-AI workflows. The CLI binary embeds dispatch logic to call into LMStudio-loaded utility agents for its own routine bounded jobs: compaction inside every role execution, and routing for `darkmux radio`. The frontier-AI orchestrator (your Claude Code, Cursor, or OpenClaw session) remains the strategic reasoner; darkmux operates the local tier as a self-contained capability.

The recursive shape is the point: **darkmux uses local-AI to manage your local-AI.** Operators running darkmux are running local-AI dispatches whose orchestration is itself done by local-AI. That's the AI-first move — not "AI bolted on," but AI as the obvious built-in capability of a tool whose reason for existing is local-AI orchestration. Earlier framings of darkmux as *"infrastructure, not an agent framework"* were honest at the time (one-thing-only swap tool, saturated agent-X namespace) but are now aspirational. The current posture matches what the binary does.

### Role families

Two role families compose to make this work, and the distinction matters when picking models or proposing additions to a profile:

- **Utility agents**, small model (4B-class), bounded I/O, high throughput, structured output. darkmux's own jobs on the machine's one utility model (#2914): today compaction and the radio router, and the one definition of which calls those are is `darkmux_crew::usage::utility_job` (`call_purpose` derives from it) (the scribe and mission-compiler roles that used to sit here were retired in #2912/#2913). Each capability is asymmetric to its compute cost, one small model fills every utility job. darkmux dispatches utility agents internally for its own operations; the operator rarely invokes them directly. Defined by: bounded inputs + structured outputs + low per-call failure cost + throughput matters + bounded reasoning rather than strategy.
- **Specialist agents** — larger model (35B-class+), judgment-dependent, lower throughput, free-form output. Coder, code-reviewer, analyst. Operator's call: which specialist for which phase, with what tilt. darkmux makes them addressable via `dispatch <role>` but doesn't substitute its judgment for the operator's.

CLI primitives stay small and composable; the local model's built-in jobs are compaction (inside every role execution) and radio routing (`darkmux radio`), both utility-agent dispatches darkmux makes on its own behalf. Structuring work the operator used to get from built-in verbs (proposing a mission config, drafting a notebook entry) is the frontier orchestrator's job now, through bundled skills such as `darkmux-lab-notebook` (#2912/#2913). Both surfaces are part of the same project — the dual posture (small primitives + AI-first internals) is deliberate.

`darkmux dispatch` and `darkmux lab run` both use the internal Docker-bounded runtime — the only dispatch path (#1405 removed the legacy openclaw shell-out alternative).

## Operator sovereignty (architectural principle)

The operator is the agent of intent. The system surfaces, suggests, records, and supports — but does not substitute its judgment for the operator's at any decision point. Every default is overridable; every automatic action is auditable; every suggestion is explainable.

Compressed to one rule: **the operator never has to wonder where a decision came from.**

This is the principle that ties the anti-patterns above to darkmux's grand vision. Anti-patterns are *don'ts*; the grand vision is the *why*; operator sovereignty is the *architectural principle* every new design decision should test against. When designing any new surface — CLI, config file, agent doctrine, file layout, data model — ask: *"does this leave the operator in the loop, with provenance and override?"* If yes, the design fits. If no, it doesn't — even when it would be more "efficient" or "smart."

Exemplars across darkmux's current surface:

- **Anti-patterns** — every rule is operator-sided (don't assume, don't silent-rollback, check before filing)
- **Preference fallthrough with provenance** — operator's intent at each layer; system never silently substitutes; unknown keys surfaced as typo warnings
- **Allocator 80/20** — algorithm proposes; operator stays in the 20% of decisions that matter; override is always available; allocator emits reasoning + alternatives + confidence for orchestrator audit
- **Confidence threshold per expertise** — operator self-rates per capability; system adjusts how often it asks vs decides
- **Role + Crew (not Team)** — composition is operator's call per mission; no fixed membership
- **JSON source-of-truth + SQLite derived index** — operator hand-edits any source file; system rebuilds derived state on demand; deleting the index is recoverable
- **Don't mutate user state without confirmation** — `~/.darkmux/profiles.json`, anything operator-owned. Read + propose; never write silently.
- **Namespace everything darkmux brings up in shared state** — LMStudio loaded models, anything else darkmux writes into a system other systems also use. Convention: LMStudio identifiers under `darkmux:<model-id>` (e.g. `darkmux:qwen3.6-35b-a3b`). Then darkmux's own state-mutating operations only touch the namespaced subset — user state is off-limits by construction, not by careful coding. The namespace is the contract.
- **Keyword vocabulary hybrid** — ship a starter; operator augments; system logs misses but never auto-mutates the vocabulary
- **Operator-tunable preferences are numeric scales, not hidden enums** — discoverable via example values; supports continuous tuning; UI-ready

The principle is recursive. It applies to documentation surface (this CLAUDE.md, READMEs), to CLI verbs, to data shapes on disk, to the architecture of future features. When a design decision feels like it should be made automatically by the system, that's the moment to surface it back to the operator instead.

Tracked as #44.

## Anti-patterns — failure modes observed on fresh sessions

Live findings from cross-machine testing (M1 Max Studio fresh-Claude session, 2026-05-11). Future fresh-Claude sessions: read these BEFORE acting on a user request that touches model selection, version compatibility, or issue tracking.

- **Don't assume models — read the profile registry first.** Models live in `~/.darkmux/profiles.json` (or wherever `darkmux profile list` reports). If an agent role needs a model and one isn't declared, **ask the user**; do NOT pick a model from the LMStudio catalog at random. Older gemma family in particular ("gemma-4-e4b", "Gemma 4 26b", etc.) is known to produce looping garbage that poisons sessions across model swaps — never default-assume gemma. If the user has saved a memory about model selection, that supersedes any inference you'd make from the catalog.

- **Don't silently roll back on regression.** If a feature appears to regress on an unfamiliar LMStudio version, **surface the finding to the user** with the version numbers you observed. Don't quietly revert config overrides "to make things work" — loud beats quiet. The user is debugging an unfamiliar env and needs the signal; a silent rollback hides the real bug.

- **Check existing issues before filing.** Before `gh issue create`, run `gh issue list --search "<keywords>"` (include closed issues with `--state all`) and skim. Duplicates clutter the project board and dilute the eureka-detection roadmap. Default to **commenting on an existing issue** over filing a new one. If you're not sure whether something is a dupe, **ask the user**; don't file-and-hope.

- **Empirical defaults are load-bearing, not decorative.** When choosing compaction modes, context windows, or compactor pairings, the shipped profile defaults (`default` mode beats `safeguard` for local; small dedicated compactor at ~68K cuts wall-clock substantially) reflect measured configurations, not arbitrary picks. Don't deviate from a profile's settings without acknowledging the empirical reason — the operator has chosen them deliberately.

- **Name the model-on-test when characterizing local-AI behavior.** darkmux uses a bake-off methodology to validate model hires per hardware tier — a documented head-to-head comparison with criteria written before the runs (documented in the lab + notebook; the static per-tier recommendation registry from [#159](https://github.com/kstrat2001/darkmux/issues/159) retired in #1426). But what's actually loaded in LMStudio at any moment may differ from the registry's pick — operators swap for reasons (debugging, A/B comparison, evaluating a new candidate, defensive escalation, or simply not having swapped back after a focused test). When you (the orchestrator) characterize behavior from a dispatch — *"the local layer's response was X"* — **know which model produced it**. `darkmux doctor` shows the active profile; `lms ps` shows the loaded models. If the loaded model differs from the recommended hire and the analysis is making generalizable claims about *the local layer*, name the model explicitly. Silent misattribution (analyzing dispatch outputs as if from the recommended model when they're actually from a reserve / candidate) inherits class-wide errors into every downstream claim. Per-role `agent.model` pinning is tracked as [#160](https://github.com/kstrat2001/darkmux/issues/160); this anti-pattern is the awareness layer until it ships. *Not restriction — operators have preferences and models evolve.* Just awareness, surfaced.

## Loop policy — recheck vs rethink (escalate, don't re-ask)

When a dispatch's output needs verification, **re-asking the same agent to re-check its own work in its own context is near-worthless.** The Self-Verification Dilemma (arXiv 2602.03485) measured that the vast majority of an agent's self-rechecks are *confirmatory*, not corrective — the agent re-derives and entrenches its original answer. Correction value comes from cross-context **re-thinking** by a *different*, ideally higher-tier reviewer.

Codified policy (not orchestrator discretion):
- **Invariant-bearing or security-bearing diffs → escalate to a fresh-context / higher-tier (frontier) review.** Never sign off on the dispatching agent's own self-recheck for these. Lived at the s3 gate: a coder's 271/271 tests + clippy were all confirmatory of its own broken work; only the fresh-context frontier review caught the four regressions (same shape as #975).
- The escalation **raises the review tier; it never lowers the gate** (operator sovereignty #44). Hygiene-only diffs may stay at the local tier.
- Pairs with #799 (terminate on a verifiable mechanical check, never self-assessment) and the persisted-corrections brief injection (#849 half 1 — a correction made once is carried into the next brief, not re-derived).

## No blind runs — instrument before you measure (operator mandate, 2026-07-09)

**darkmux exists to observe local-AI work. A darkmux run that cannot be observed refutes the product.** This is the recursive success criterion applied to the project's own development: if operating darkmux means watching `tail -f` and `lms ps`, the observability claim is failing at home — and every gap felt while operating darkmux is a P0 feature request, not an inconvenience to work around.

**The rule: no measurement-grade run launches until its observability surfaces exist.** A run whose only yield is a verdict line is a wasted run — the DATA is the product. Before any multi-hour or decision-bearing run, verify:

1. **Per-event records stream to durable per-run-local files as they happen** — never end-of-run-only writes. A killed run keeps everything completed so far (per-case envelope streaming + `funnel-events.jsonl`, #1248).
2. **Host telemetry samples alongside the work** (cpu/ram/load at ~2s cadence) so "when did it slow down and what else was the machine doing" is answerable from the artifact, not reconstructed from another tool's server logs (#1247).
3. **The knob config is snapshotted into the artifact** (resolved staffing/model/k/max_tokens — `FunnelEnvelope.staffing`), so every run is self-describing for later series comparison.
4. **A live observing surface is available** (the lab view when it lands; at minimum a live-tailing event file) — the operator must be able to SEE the run, not infer it.

If a surface on this list doesn't exist for a new run type, **building it comes before the run**. Observability work precedes measurement work in priority; it is not polish.

**Origin (2026-07-09, Phase B validation day):** a full day of funnel validation ran blind — a heavy corpus run was killed after case 1 and lost its entire envelope (end-of-run-only artifact writes); a ~10–15% inference slowdown from concurrent builds was invisible until reconstructed forensically from LMStudio's own server logs; overnight runs were nearly launched whose total observable yield would have been seven console lines. Operator: *"darkmux is fully designed to observe everything and we aren't... No data, no ability to pinpoint when things got slow because of another process. Make it doctrine or this whole project won't work."*

Composes with: single-run-full-picture-first (verify a system with ONE complete instrumented run before corpus sweeps), smoke-before-long-runs, quiesced-machine for canon runs (until host sampling ships, measurement runs get no concurrent builds), and the lab-vs-fleet boundary (bench records stay per-run-local; engagement records ride the flow stream).

### The observer must not join the observed (operator lesson, 2026-07-10, #1286)

Observing local-AI work must not perturb it. The prior art is the AMD/OpenGL stats-render paradox: on-screen debug charts could only be drawn by the very graphics engine being measured, so *rendering the stats made the stats worse* — one line of provenance for a system-design requirement, not an optimization. Getting the numbers, and displaying them, has to happen OUTSIDE the measured system. Four binding constraints on every darkmux observability path (the memory ledger + `#lens=machine` are the first consumers):

1. **Observability paths contain ZERO model dispatches.** A measurement path reads kernel counters (`vm_stat`, `sysctl`, `ps`) and `lms` metadata only — zero tokens, zero Metal work. Using the LLM to observe the LLM (e.g. a utility agent summarizing stats mid-run) is the forbidden pattern; it is the modern form of rendering charts with the measured engine.
2. **The display renders off-machine by design.** The serve daemon emits JSON; chart-rendering cost lands on the CLIENT — the phone over the tailnet, another machine, any browser that isn't the measured host. Watching a canon run from the measured host's own browser is the anti-pattern (a Chrome tab is a real RAM/CPU consumer); the quiesced-machine doctrine extends to *watch measurement-grade runs off-box*.
3. **Samplers/gatherers stamp their own cost into the artifact/payload.** The gather records its own wall-clock (`gather_ms`); a host-telemetry sampler records its own CPU time alongside the samples — so "the observer was negligible" is a VERIFIABLE claim in the data, not an assumption. We already measured this failure class from the other side: concurrent cargo builds taxed judge throughput 10–15%, invisible until reconstructed forensically. The observer must be provably not that.
4. **Cadence is a recorded knob, never adaptive-silent.** The sampling interval / cache TTL is written into the payload (`cache_ttl_ms`) at its default (~2s); if someone tightens it for a debug session, the artifact says so.

## No compliance claims — mechanism, not outcome (operator standard)

darkmux is OSS for exploration and tooling, not a claim on any legal framework. Operator's own words: *"it shouldn't violate any laws, but it should also not claim to make you compliant under a framework for using it."* Producing evidence and being compliant are different things, and only the second one needs a lawyer.

- **Never name a regulatory framework** (ISO 27001, HIPAA, AI Act, SOC 2, GDPR, or any other) as something darkmux helps satisfy, on ANY user-facing surface — docs, the website, `--help`, doctor hints, skills, README, packaging. Naming a framework at all reads as an inducement; leave it out entirely rather than hedging around it.
- **Describe the mechanism, not the outcome.** "Recomputes each chain and reports the first divergence" stays true as the implementation changes; "proves records were not edited" is an outcome claim that rots into a falsehood the moment a gap is found — and then has to be publicly retracted. Prefer the mechanism form everywhere, not just in the audit sink's copy.
- **Avoid universals.** "Any modification is detectable" is a specific, testable proposition — one counterexample falsifies it. Say what the check does and name its known gaps instead of asserting completeness.
- **Internal code comments describing intent are fine.** The test is whether a reader could mistake the sentence for a claim about *their own* qualification, not whether the word "compliance" appears at all — a comment like "an audit-sink failure is a compliance gap" is aspirational context for a maintainer, not a promise to a user.
- **A false factual claim outranks an overstated feature claim.** Check the disclaimer's factual recitals (network egress, data locality, what talks to what) before polishing feature copy — a wrong fact is worse than an overclaimed feature.

Origin: a 2026-08 legal review found the audit sink's docs claiming tamper-evidence and compliance support the chain could not back — including wording this project itself introduced while trying to correct an earlier overclaim. The rule was learned from that correction, not designed in advance.

## Engagements (operator-defined dreamscapes)

**An engagement is operator-defined, never system-defined.** darkmux does not
enumerate engagements, impose a directory shape, or have an engagement config
format. The operator decides what counts as one and how much to describe it —
a repo path, a trip, a book, a fitness goal, a URL, classified work they will
not describe, or nothing written down at all.

**The orchestrator's bridging job**: read (or ask for) the engagement context in
whatever form it takes; offer to capture it durably as an `.md` if wanted, in a
location that is the operator's call; translate the soft context into the
structured concepts darkmux models in code (Mission, Phase, role tilts,
preferences) — proposing that translation is the job, not an overstep. **Do not
pry for structure the operator did not volunteer**: offer once, let it land or
get redirected, then drop it.

### The one hard rule: engagement never enters the CLI arg surface

Engagement context lives in the frontier orchestrator layer — CLAUDE.md files,
skills, conversation. It **never** becomes a `--engagement <hint>` flag on any
darkmux verb. No `--context`, no `--vibe` either.

Three reasons, and the third is the load-bearing one:

- **CLI args quantize.** `--engagement "wife time"` forces a dreamscape into one
  string-token. *"This is my marriage time, not a work trip — relaxation, no
  aggressive sightseeing"* threaded through the intent text carries what the
  flag cannot.
- **Utility agents are the wrong layer to interpret it.** A 4B utility agent
  asked to interpret the operator's relationship to an engagement is the exact
  capability mismatch the utility/specialist split exists to prevent.
- **Vision dies in translation, and a 4B agent cannot hold a contradiction — it
  resolves it.** That resolution is where the operator's intent gets lost. The
  pattern predates AI: when an admin layer translates vision into tasks, the
  vision quietly disappears, and the cost scales with org size. The frontier's
  role here is **vision guard** — protecting engagement-level intent from being
  compressed before it has been translated into structure the utility layer can
  handle.

For a verb that would benefit from "context-aware" output, the operator carries
that context in the verb's primary input, where a utility agent reads it as part
of its bounded structuring job.

Surfaced 2026-05-14: `--engagement` was added to the mission-proposal verb
(since retired, #2912) and caught pre-merge as a doctrine violation. The full reasoning — every engagement shape,
the bridging role in detail, the complete lost-in-translation argument — is in
[`docs/ENGAGEMENTS.md`](../ENGAGEMENTS.md). Tracked as #49.
