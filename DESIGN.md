# darkmux design notes

This document is the **why**: how darkmux is built, the decisions behind the shape, and the measurements that forced them. It is written against the code at the 5.0.0 release. Where a section names a symbol, a test or a golden file, that is where to check the claim, and where a claim is a known gap or an unmeasured guess, the section says so. History is kept only where it still explains the code; the record of what changed release to release is [`CHANGELOG.md`](CHANGELOG.md), and the doctrine that binds every change is [`CLAUDE.md`](CLAUDE.md).

**Versions.** darkmux ships as 5.0.0, and the numbers that matter for compatibility are the data shapes' own: the flow schema (`FLOW_SCHEMA_VERSION`), the config and profile registry schemas (`CONFIG_SCHEMA_VERSION`, `PROFILES_SCHEMA_VERSION`), the mission config schema (`MISSION_CONFIG_SCHEMA`) and the fleet work wire (`WORK_JOB_SCHEMA_VERSION`). Each changes by its own semver rules (contract 5). The daemon's HTTP routes and every verb's `--json` output are semver-bound to the binary, and each is pinned by a golden file.

**What this is and how it got here**

- [What darkmux is](#what-darkmux-is)
- [What darkmux is NOT](#what-darkmux-is-not)
- [How it got here: the evolution](#how-it-got-here-the-evolution)
- [How we decide](#how-we-decide)

**Vocabulary and identity**

- [Work units: the ladder and its words](#work-units-the-ladder-and-its-words)
- [Identities: runs, sessions and executions](#identities-runs-sessions-and-executions)
- [The flow vocabulary: one closed list, one spelling per event](#the-flow-vocabulary-one-closed-list-one-spelling-per-event)
- [One reader, and what an old archive reads as](#one-reader-and-what-an-old-archive-reads-as)
- [Typed flow payloads](#typed-flow-payloads)
- [Two logs: the flow stream and the trajectory](#two-logs-the-flow-stream-and-the-trajectory)
- [One token truth](#one-token-truth)

**Configuration and the user's files**

- [Configuration: visible defaults, gated features, secret carve-outs](#configuration-visible-defaults-gated-features-secret-carve-outs)
- [User files: the unknown-key gate and enum settings](#user-files-the-unknown-key-gate-and-enum-settings)
- [Endpoints: what darkmux does there, not where they are](#endpoints-what-darkmux-does-there-not-where-they-are)
- [Endpoint budgets](#endpoint-limits-and-budgets)
- [The utility model and lean utility jobs](#the-utility-model-and-lean-utility-jobs)
- [Public surfaces: the daemon's HTTP routes are a contract](#public-surfaces-the-daemons-http-routes-are-a-contract)
- [CLI `--json` is a contract](#cli---json-is-a-contract)

**The runtime**

- [Schema isolation: darkmux owns its own config](#schema-isolation-darkmux-owns-its-own-config)
- [Scope of the internal runtime: workflow-fit, not feature creep](#scope-of-the-internal-runtime-workflow-fit-not-feature-creep)
- [Compaction: tiers, structured slots, and graceful degradation](#compaction-tiers-structured-slots-and-graceful-degradation)
- [Runtime resilience: struggle detection + feedback injection](#runtime-resilience-struggle-detection--feedback-injection)
- [The check-in: observing a stream instead of truncating it](#the-check-in-observing-a-stream-instead-of-truncating-it)
- [Lab reproducibility: fixtures + content hashing](#lab-reproducibility-fixtures--content-hashing)
- [Residency: a pure planner, leases and an owned namespace](#residency-a-pure-planner-leases-and-an-owned-namespace)
- [Thermal and power governors: pause, never kill](#thermal-and-power-governors-pause-never-kill)

**Running a mission**

- [Seat classes: every step says what it consumes](#seat-classes-every-step-says-what-it-consumes)
- [Phase status: a set of tasks is not one unit of work](#phase-status-a-set-of-tasks-is-not-one-unit-of-work)
- [The command gate: darkmux runs your shell-outs, not its own](#the-command-gate-darkmux-runs-your-shell-outs-not-its-own)
- [Mission configs](#mission-configs)
- [The shared run lifecycle](#the-shared-run-lifecycle)
- [Crawl as a mission: the shapes and how data flows between them](#crawl-as-a-mission-the-shapes-and-how-data-flows-between-them)
- [Findings and mods: what was observed, and how it could change](#findings-and-mods-what-was-observed-and-how-it-could-change)
- [Code review as a second config on the crawl's building blocks](#code-review-as-a-second-config-on-the-crawls-building-blocks)

**Many machines**

- [Multi-machine substrate](#multi-machine-substrate)
- [Hooks: how records leave the machine, and who is allowed to hold a credential](#hooks-how-records-leave-the-machine-and-who-is-allowed-to-hold-a-credential)

**Editors and the terminal**

- [ACP: darkmux inside the editor](#acp-darkmux-inside-the-editor)
- [Radio: free text onto one command, and a confirmation before it runs](#radio-free-text-onto-one-command-and-a-confirmation-before-it-runs)

**Keeping it true**

- [Guardrails: what CI holds so the design stays true](#guardrails-what-ci-holds-so-the-design-stays-true)
- [Composability](#composability)

## What darkmux is

darkmux is an **AI-first orchestrator for local LLMs**. It does three things, and the notes below trace how each one earned its place:

1. **Mission orchestrator (the dispatch-to-PR loop).** `darkmux dispatch` / `mission launch coder-phase` run a coder in a container-bounded runtime, review it in a fresh context, gate it on operator sign-off, and ship a PR. The defining capability today, and the headline of the 2.0 identity: config-defined missions that run as a live task graph.
2. **Lab + unified observability.** `darkmux lab run` measures how a workload runs on your own hardware; every dispatch emits a typed flow record, and one daemon (`darkmux serve`) serves the stream, a drill-down viewer, and per-machine introspection across a fleet. The empirical half that grounds the config choices behind the missions.
3. **Model residency (internal).** A dispatch loads the models a named profile declares (model + context window + compaction settings) under the resident budget. darkmux *began* as this capability, the profile multiplexer, a manual `swap` tool; in 2.0 it moved inside gestalt and the `swap` verb retired. It is the floor the loop stands on, but it is no longer a verb the operator drives by hand.

The through-line is the doctrine in [`CLAUDE.md`](CLAUDE.md): *optimization, not replacement; the harness before the model; the operator always in the loop with full provenance.* darkmux uses local AI to manage your local AI.

darkmux's architecture wasn't designed up front; it was **measured into existence**. Nearly every section names the lab run, dogfood, or research finding that drove the choice, because [that's how we decide](#how-we-decide).

## What darkmux is NOT

- Not a model-swap optimizer (LMStudio handles the actual load; we orchestrate).
- Not an inference framework (vLLM/SGLang have that covered).
- Not an agent framework (LangChain/AutoGen have that covered).
- Not a prompt router across cloud providers (LiteLLM has that covered, and it's cloud-oriented).
- Not *designed* for multi-tenant deployment. **darkmux is single-operator, multi-machine.** A hobbyist or individual engineer's "few Macs joined over a mesh VPN" is the natural deployment shape. The trust boundary is the operator-controlled tailnet, plus one authenticated path: running work on another machine needs the fleet token, a network-verified sender and an allow-list entry, and is denied by default ([Fleet](#fleet-addresses-trust-and-the-execution-channel)). Everything else rests on the tailnet and the operator's own assertions: `DARKMUX_REDIS_URL` carries no auth beyond what the underlying mesh and Redis ACLs already provide; `DARKMUX_MACHINE_ID` is operator-asserted provenance, not authenticated identity; reads of the daemon are open to the tailnet unless `serve.read_auth` is on ([Read auth and execution auth](#read-auth-and-execution-auth)); cross-machine state on the shared substrate assumes all participants are the same operator. Fork-friendly if multi-tenant matters to you: the substrate is a reasonable starting point, and the missing pieces (per-user identity, ACLs on reads, fairness across distrusting users) are well-trodden elsewhere.

## How it got here: the evolution

darkmux's shape is the record of a sequence of decisions, each one forced by data. Kept here as history because the *why* is easy to lose once the *what* is built.

### v0.1: the swap tool (the smallest useful thing)

darkmux started as ~200 lines that collapsed a manual sequence:

```
lms unload <model> ; lms load <model> --context-length N --identifier <id>   # repeated per model, per profile
```

into one `darkmux` profile-multiplexer command. No proxy, no classifier, no daemon. The bet was modest: switching local-model *stacks* (model + context window + compaction settings together) is enough recurring friction that a one-command profile multiplexer earns its keep. It did. In 2.0 that multiplexer moved inside gestalt (the residency arbiter loads what each dispatch's staffing declares; the manual `swap` verb retired, #1426). The profile stack remains the floor, and everything since builds up from it rather than replacing it.

### The pivot: "check the harness before the model"

The first real finding reframed the whole project. Measuring local-agent runs (Genesis [Articles 1–2](https://darklyenergized.substack.com)) showed that **large wall-clock regressions that look like model problems are usually *harness* problems**: a compaction misconfig, a context-window mismatch, loaded-state drift between the profile and what's actually resident. The model wasn't slow; the harness was wrong.

That inverted the priority order and gave darkmux a reason to exist beyond swapping. If the harness is the dominant variable, then the tool that *owns the harness* and makes it measurable is where the leverage is. "Harness before model" is doctrine now ([`CLAUDE.md`](CLAUDE.md)); it's why `darkmux doctor` exists and why darkmux became a lab, not just a switch.

### Compaction: the biggest lever, measured

Of all the harness knobs, **compaction had the largest measured wall-clock impact**, so it earned the most defensive engineering. The data pointed somewhere specific: a small, dedicated compactor at a modest context (~68K) cut wall-clock substantially versus reusing a large all-purpose model, and the `default` strategy beat the more conservative `safeguard` one for local models (the two modes of the runtime darkmux replaced). Those aren't taste; they're [measured defaults](#compaction-tiers-structured-slots-and-graceful-degradation), the small dedicated compactor is why compaction runs on [the machine's one utility model](#the-utility-model-and-lean-utility-jobs), and the anti-patterns doc warns against deviating from them without naming the empirical reason.

The deeper bet, that **a small model fills labeled slots more reliably than it writes good prose**, produced structured-slot compaction. [That section](#compaction-tiers-structured-slots-and-graceful-degradation) is the template for how darkmux decisions get made: a hypothesis, a measurement, a typed design that degrades gracefully instead of failing.

### The bake-off: how models get hired

Choosing which local model fills a role isn't preference, it's a [documented head-to-head per hardware tier](https://github.com/kstrat2001/darkmux/issues/159) with criteria written *before* the runs. A 128GB-tier bake-off named a 35B-A3B MoE for routine coding (fast, only ~3B active parameters), held a larger dense model as a heavy-reasoning reserve, and kept a coder-specialist for single-shot prose. The methodology outlasts any single pick: models turn over constantly, so the *bake-off* is the durable artifact, not the winner. (When you characterize "the local layer's" behavior, name the model on test, because what's loaded may not be the recommended hire; see the anti-patterns in [`CLAUDE.md`](CLAUDE.md).)

### The internal runtime: owning the loop

Early dispatch shelled out to an external agent runtime (openclaw), through the 0.x line an opt-in alternative to darkmux's own. darkmux now ships its **own** container-bounded runtime: a Rust agent loop in a per-dispatch Docker container. Owning the loop is what makes everything downstream possible: kernel-enforced workspace isolation, a trajectory format darkmux fully controls ([Two logs](#two-logs-the-flow-stream-and-the-trajectory)), and telemetry emitted straight into the flow stream rather than scraped back out of someone else's logs. The shell-out path was removed on the 2.0 track ([#1405](https://github.com/kstrat2001/darkmux/issues/1405)) to keep the build and test surface small; the internal runtime is the only dispatch path. Two things outlived the comparison: the filter for what the runtime *adds*, [workflow-fit, not feature creep](#scope-of-the-internal-runtime-workflow-fit-not-feature-creep), and [schema isolation](#schema-isolation-darkmux-owns-its-own-config).

### The dispatch-to-PR loop: the defining capability

Owning the runtime turned darkmux from a configuration tool into a **work** tool. The loop:

```
mission launch coder-phase → coder → fresh-context review → fix → frontier/operator sign-off (gate) → PR
```

This loop is grounded in both research and dogfood: failures we *measured*, then found the literature that explained them.

- **Verification has to be real.** A production dogfood surfaced a *fabricated* sign-off: a coder reported a type-check "passed" when the slim sandbox couldn't actually run the project's toolchain, and a separate run reported the same failure honestly, so the fabrication was **nondeterministic**. You can't trust self-reporting to catch it. The fix: the runtime stamps the dispatch envelope when a verifier didn't run, so a claimed sign-off is mechanically contradicted ([#799](https://github.com/kstrat2001/darkmux/issues/799)). Process-reward-model research confirms step-wise verification catches the *silent errors* outcome-only checks miss ([arXiv 2604.24198](https://arxiv.org/abs/2604.24198)).
- **Self-review is mostly confirmatory.** At one gate a coder's full test suite + linter were *all green on its own broken work*; only a *fresh-context* review caught the regressions. The Self-Verification Dilemma ([arXiv 2602.03485](https://arxiv.org/abs/2602.03485)) measures exactly this: re-checking in your own context entrenches the original answer, while cross-context *re-thinking* corrects it. So the reviewer runs in a fresh context, and escalation is codified loop policy ([#849](https://github.com/kstrat2001/darkmux/issues/849)).
- **A wrong, confident diagnosis is worse than none.** A lab run caught a reviewer verdict that *sounded* authoritative but was wrong; it sent the next coder in circles for 600 seconds, zero net progress, then a watchdog timeout. The fix: detect the no-progress signature and escalate instead of looping ([#453](https://github.com/kstrat2001/darkmux/issues/453)).

darkmux drives this loop on **real production work** (the production services of a private fintech engagement) and on darkmux itself. The recursive case is the strongest evidence: darkmux's own observability features were built *through* `mission launch`, so the data those features visualize is the data the loop produced while building them. One self-building phase ran 106 turns and ~5.2M prompt tokens with **zero compactions** (context peaked near 70K of a 262K window), which retired a standing question by turning it into a measurement: on a window that large the compaction threshold is a *cost* knob, not a correctness one.

### Observability: from a telemetry sketch to a unified stream

The original observability idea was a per-request telemetry hook: useful, but bolted on. It became something better: a **single typed flow stream** every dispatch emits into (tokens, context occupancy, detector firings, runtime events), read by one daemon and one drill-down viewer ([#557](https://github.com/kstrat2001/darkmux/issues/557)). The stream is one closed vocabulary through one reader ([The flow vocabulary](#the-flow-vocabulary-one-closed-list-one-spelling-per-event)), and it is separate from each execution's own trajectory. The token view is the payoff: usage is one record per model call, summed and split by the endpoint and model darkmux invoked, never by a local-or-cloud label ([One token truth](#one-token-truth)). **Tokens only, never currency**: claiming to save or cost another person money is a liability we don't take on; the operator multiplies by their own rate. A dogfood day's data taught its own lesson, that the bulk of a long dispatch's tokens are *re-read* context, not generated output, which is itself a compaction-design input.

### Fleet: many machines become one

The multi-machine substrate lets a single operator's couple of Macs over a tailnet function as one development environment, [detailed below](#multi-machine-substrate), with an authenticated channel for running a dispatch on another machine. The design target is deliberately **heterogeneous**: a high-memory laptop as the inference peer, a smaller always-on machine as the hub. That heterogeneity is the white space. Nearly all distributed-agent research assumes cloud or homogeneous hardware, so a heterogeneous local fleet of Apple-Silicon Macs is darkmux's to define rather than follow (see the [roadmap](ROADMAP.md)).

## How we decide

darkmux's design decisions are **grounded in data and in published research where it exists**: we'd rather cite a measurement or a paper than assert from intuition. The framing is *convergence, not priority*: independent research and this project keep arriving at the same architecture (fresh-context review, verifiable-check termination, structured compaction), and the citations explain *why* it works. The citation-verification discipline: every cited source is re-fetched and confirmed, because a confident citation under a correctly-recalled label is exactly where fabrication hides.

The data comes from three places, and the lab notebook captures the *evidence* behind each call so the reasoning survives even when the underlying work is private:

- **Lab runs**: reproducible workloads against registered fixtures, with content-hash proof that two runs started and ended in the same state. This is where harness hypotheses get tested one variable at a time (baseline → single change → re-measure → compare → record).
- **Bake-offs**: documented per-hardware-tier model comparisons with criteria fixed before the runs.
- **Dogfood**: darkmux run against real work, including darkmux building itself through `mission launch` and a private fintech engagement's production services. The failure modes those runs surface (a fabricated sign-off, a confidently-wrong review, a doom loop) are the specs for the next hardening pass. The *data* is what's load-bearing; the sensitive work behind it never has to appear here.

When a decision can't point to a measurement, a citation, or a dogfood observation, that's a flag, not a reason to ship it on intuition.

---

The rest of this document is **reference**: how the current architecture works, section by section. The decisions above are why it's shaped this way.

## Work units: the ladder and its words

Every piece of work darkmux performs (a mission with forty steps, a one-shot `dispatch`, a lab run) is the same substrate at a different scale. Which word names which grain is contract 8 in [`CLAUDE.md`](CLAUDE.md)'s registry; this section is why the words are what they are, and where each grain lives in the code. The identities that carry the grains on the wire are in [Identities](#identities-runs-sessions-and-executions).

### The ladder

**mission › phase › task › step › role execution.** A *run* is the umbrella, never a grain: one top-level unit of work the operator started, in exactly three kinds (mission, dispatch, lab). A *step* is a graph node; a *role execution* is what the node did. A `procedural.shell` step has none; `dispatch.internal` has one; `dispatch.map` has one per collection item; a `dispatch.unit` step has one per draw. That zero-to-many cardinality is what makes the step layer real rather than a wrapper, and it is why no further noun sits between a step and a role execution: the many already have domain names that are not synonyms (a `dispatch.map`'s *items*, a seat's *draws*), and they all bottom out in one model-facing execution.

The inner unit is named for the **role**, not the model, because the model is derived rather than declared: `select_model(role, profile)` resolves it at dispatch entry, `DispatchOpts` takes `role_id` as required and `profile_name` as an optional override, and an endpoint-staffed seat has no local model at all. Role is the stable identity across local and remote; the model is a consequence of the profile. `dispatch` then names the top of the ladder only: the verb, and the run kind it produces, *a run consisting of exactly one role execution*. That is what stops one word naming both ends of the ladder.

The grains have their own bookends. `run.*` brackets a whole run and `dispatch.*` brackets one role execution, so a consumer never has to read a `source` field to tell them apart ([The flow vocabulary](#the-flow-vocabulary-one-closed-list-one-spelling-per-event)).

**A specialist change is an execution boundary.** Escalation mints a new role execution; it never puts a second specialist inside one. Utility work inside an execution (compaction above all) is a sub-execution attributed to its own model, never blended into the specialist's ([The utility model](#the-utility-model-and-lean-utility-jobs)). Getting this wrong is not cosmetic: filing the compactor's residency under the specialist is what makes a healthy run report a model swap that never happened, so a declared utility role going resident is not a swap.

### Where the substrate lives, and why that is the whole lesson

The substrate a serious run needs (host telemetry sampling, per-step records, budget accounting, liveness bookends, the resolved-knob snapshot) belongs in the **shared control-flow path every run already crosses**: the launcher and the scheduler. Not in an importable type that each mission may or may not adopt.

This is the measured outcome of trying it both ways in the same arc. When the telemetry sampler moved into the launcher and per-step records moved into the scheduler, every mission gained them with *zero changes in its own module*: a mission author cannot forget what they never had to remember. When the same arc left a piece as an importable type plus a paragraph of doctrine, it acquired exactly one consumer, the module it was extracted from, importing it back under its old name.

**Moving a type one crate over and importing it back is a relocation, not a layering.** The test of whether a capability is really shared is not where it is defined; it is whether a mission that never mentions it still gets it.

### The vocabulary, and why each word sits where it does

darkmux names a lot of layers, and the names were not arrived at freely: most of the obvious ones were already spoken for. This is the map.

**The containment ladder.** `mission › phase › task › step › role execution`.

| Term | What it is |
|---|---|
| **run** | The UMBRELLA: one top-level unit of work the operator started. Never a grain. Three kinds: `mission`, `dispatch`, `lab`. |
| **mission** | A whole task graph, launched from a config. |
| **phase** | A grouping of tasks within a mission (`Mission.phase_ids` → `Phase.task_ids`). |
| **task** | A group of steps, and where resource ASSIGNMENT lives (role, profile, workdir, image), which is why a step inherits staffing rather than declaring it. |
| **step** | One graph node. Contains **0..N** role executions. |
| **role execution** | The inner unit: one role, running many turns until it stops. Named by an `execution_id`. |
| **dispatch** | TOP-LEVEL ONLY: the verb `darkmux dispatch <role>`, and the run kind meaning *a run consisting of exactly one role execution*. |
| **crew** / **crew member** / **position** | Who staffs a mission, and where each member sits. |
| **role** | A stance, a tool palette, and a system prompt. The declared identity of a role execution. |
| **profile** | The registry entry that says which model, at what context, on which endpoint. |
| **seat** | A staffed model position within a run (`MemberRecord`), with a `draws` count. |
| **draw** | One invocation of a seat. |
| **item** | One element of a `dispatch.map` collection. Reserved shape: an object with exactly two keys, `system` (a string) and `item`, is a per-item persona override and its `item` value is the payload; every other value is the item itself. |
| **session** | An INTERNAL join key tying a family of flow records together. Never operator-facing. |
| **workload** / **fixture** | Lab-only: the thing being run, and the pinned sandbox it runs against. |

**The mission metaphor is deliberate, and it has to close.** darkmux's operator-facing vocabulary commits to the **NASA mission metaphor**. `Mission` and `Crew` were canonical from the start; the rest is named to *complete* the metaphor rather than to borrow from a software subculture.

| Term | What it names |
|---|---|
| **Mission** / **Crew** | The work, and who staffs it. |
| **Debrief** | The post-mission review ceremony (`Stage::Debrief`). |
| **Lessons** | The durable engagement-context store, previously "knowledge". |
| **Cautions** | The auto-detected loop pathologies. Already on-theme: spacecraft carry a *Caution & Warning System*. |

The metaphor closes, which is the point of it: **a mission's runs raise cautions → the debrief distills them into lessons → lessons brief the next crew.**

**Why a metaphor rather than accurate jargon.** Metaphors endure because they are *coherent and relatable*, not because they are literal. Xerox PARC and early Apple gave us the **Desktop**, the **Trash**, **Files**, none of which are literally inside a computer. They lasted because the metaphor was complete and drawn from a world people already knew. Software-tribal terms fracture it, and each carries baggage the metaphor does not: a *retrospective* imports Scrum, which not everyone practices and which means nothing outside engineering; a *post-mortem* imports death. A whole metaphor is something a person can hold. Half a metaphor is just inconsistency.

**How to apply it.** When naming any new operator-facing surface (a verb, a stage, a concept, a file), prefer the term that completes the mission metaphor, and check that it *completes* rather than merely coexists. Where a real NASA term exists, lean on it: "Lessons Learned" (NASA's LLIS) is the authentic version of what dev culture gestures at with "retro notes". **Reject "it is already there" as a naming argument.** Consistency with an unconsidered placeholder is not consistency: `Stage::Retrospect` was renamed to `Stage::Debrief` on exactly that basis.

**The boundary.** This governs the OPERATOR-facing surface only. Model-facing text (role prompts, skill descriptions, the autonomous-dispatch preamble, feedback-injection templates) defaults to AI-convention terminology instead ("the user", "system message", "tool calls"), because a local model under clean dispatch context has no darkmux history to ground a metaphor against. See the model-facing prompt doctrine in `CLAUDE.md`. The two rules do not compete; they apply to different readers. A naming rule that exists only in someone's memory is not a rule the project has, which is why this one is written down here.

**Names that are taken, and by what.** Every humanized word that reads naturally for "one crew member's bounded piece of work" turned out to already name a *different* layer of this same system, which is itself the finding:

- **task**: the grouping layer above steps.
- **job**: fleet work submission (`darkmux_fleet::WorkJob`, `WorkSubmission`), one dispatch sent to another machine's fleet listener. `WORK_JOB_SCHEMA_VERSION` makes it a wire contract, not just a word.
- **deployment**: the Azure hosted-model endpoint (`/openai/deployments/<name>`), surfaced in `darkmux doctor`'s own remedy text. Reusing it for the execution would re-fuse the exact thing choosing *role* over *model* was meant to keep apart.
- **activity**: the viewer's activity lanes.
- **assignment**: a Task's resource assignment.
- **turn**: one iteration of the agent loop; a role execution has many.
- **pass**: the review pipeline's probe, judge and verify passes.

`shift` and `stint` are genuinely unused and were weighed as more humanized alternatives; `execution` won on precision and on composing cleanly for sub-executions.

**"Rule" already sits at three grains, and none of them touch.** Three subsystems own the word, each with its own schema, and two of them even have a `match` field:

| Which "rule" | Where it lives | What it decides |
|---|---|---|
| **hook rule** | `config.hooks.rules[]` (`HookRule`: a `match` predicate + a target URL) | which FLOW RECORDS leave the machine, and to which receiver. Matched mechanically (`hook_match`); identified by position (`rule_index` on `hook.fired`). |
| **crawl pattern** | a crawl rule file (e.g. `swallowed-error`: `match`/`no_match` prose + `evidence`/`why_hint`) | what COUNTS AS A FINDING. Given to the model verbatim as `<pattern name="…">`; named by id in the manifest, the envelope, and a receiver's `rule` column. |
| **eureka rule** | `darkmux-eureka`'s `RuleDef`s (`RULES_SCHEMA_VERSION`) | what the detection engine flags, surfaced by `darkmux doctor`. |

The collision is survivable because the keys never meet (a hook rule's `match` is a record predicate, a crawl pattern's `match` is instructions for a model), but prose that says "the rule fired" is ambiguous in exactly the way this section exists to prevent. Say **hook rule**, **crawl pattern** (the model-facing tag already says `<pattern>`), and **eureka rule**. A fourth "rules" surface must pick a different word.

**The rule this leaves behind:** before naming a new layer, check whether the word already names a different grain in this system. A word at two grains is the defect that produced this whole section: `dispatch` meant both a top-level run kind and the innermost unit, and nothing said so, so every consumer picked a meaning and they disagreed.

### Lab stays separate, deliberately

Lab runs write per-run-local artifacts and stay off the fleet flow stream. That boundary is a measurement-integrity decision, not an inconsistency to be tidied away: the point of a lab run is that it is reproducible in isolation, and a bench that quietly enriched the shared stream would make its own numbers a function of what else the fleet was doing. What was unified is the noun on the operator's side. Recorded lab runs live under the darkmux root's `lab/` directory (`DARKMUX_LAB_DIR`, one resolver for the writer and every reader) and are read through `darkmux run`: `run list --kind lab` lists them, and `run inspect`, `run stats` and `run compare` read a lab run's recorded artifacts and refuse a mission or dispatch run id, naming where to look. `darkmux lab run <workload>` is the launcher only. Contract 8 makes "run" the umbrella over mission, dispatch and lab runs, so a `runs/` directory that held only lab runs and a read family beside `darkmux run list` were the umbrella's name on one kind.

## Identities: runs, sessions and executions

Three identities answer three different questions about a flow record, and each is a type rather than a convention.

| Identity | Answers | Type | Minted |
|---|---|---|---|
| **run** | which top-level unit of work the operator started | `RunId` in `darkmux_types::session_id` | at launch: a mission id, a lab run id, or a standalone run |
| **session** | which family of records belongs together | `SessionId { kind, run }` in the same module | by the code that opens the family; `wire()` is its only string form |
| **execution** | which role execution a record is about | `ExecutionId` in `darkmux_types::execution_id` | once per role execution, at the host entry that runs it |

**A session cannot exist without its run.** A `SessionId` is built from a `RunId` and a `SessionKind` (`Run`, `Phase`, `Task`, `Step`, `Adhoc`, `Relay`), so two launches of one config can never share a session, a presence key or a budget record: the task session of a review is `<run>.task.<task>`, not a string that hashes only the task id. `mission_id` on a record therefore always agrees with its session (a lab run and a standalone run stamp none). The wire grammar is `run [ "." run-kind ] "." kind { "." field }`, every component escaped so it never contains the `.` separator, and it is injective: two identities never share a string, and `SessionId::parse` reads one back exactly. Strings written before this grammar read only through `SessionId::parse_legacy`, the one place that knows their shapes. `session` stays an internal join key and never an operator-facing word; the `session_id` field keeps its name on disk because renaming it would strand every archive.

Two types have a run-kind role. `darkmux_types::session_id::RunIdKind` (mission, lab, standalone) says how a run's identity is built, and is internal. `darkmux_serve::runs::RunKind` (mission, dispatch, lab) is the operator-facing kind on the runs board, exported to the viewer, and `dispatch` there is a label on a mission, decided by `classify_mission` (`crates/darkmux-serve/src/runs.rs`): a mission whose spec names config id `dispatch` is a Dispatch, a mission with any other spec is a Mission, and only a mission with no spec falls back to shape (one phase, one task, one step means Dispatch).

**A fleet receiver runs submitted work under a relay.** A job another machine submitted runs under `SessionKind::Relay`, which carries the sender's own session and the peer it came from. Its run is the standalone twin of the sender's, so the receiver groups a sender's run together but never stamps it as one of its own missions, whatever id the sender names.

**An execution is named on every record that is about it.** `ExecutionId::mint` is the only way to a new one, and the host entries mint exactly one per execution: `crew::dispatch::dispatch` (which covers the hosted single-shot path), `dispatch_local_single_shot`, the `dispatch.single_shot` step kind, and each item of a `dispatch.map`. A `dispatch.map` step therefore writes one bookend pair per item and none of its own; the scheduler's step records cover the step. The id is stamped through one builder path (`FlowRecord::for_execution` and `for_execution_with`) on every action whose row declares the execution grain, and `CheckedRecord::check` refuses to write one without it. A resumed dispatch continues its id (it rides in the host-only `<out-dir>.resume_origin.json` beside the out-dir); a change of specialist is an execution boundary and mints a new one. A compaction is a sub-execution of the same execution, so its records carry the parent's id and its usage record says `purpose: utility`; a host-side utility job (radio routing) mints its own for its usage record. Consumers key on the execution: token sums, the DISPATCHES chip, `records_emitted`'s pairing, both lifecycle executors' attempts, and the finding store's key.

**Which step a record belongs to is stamped, not reconstructed.** A record of a step's session carries `payload.step_id`, set by the producer through the `Attribution` trait (`step_slot`) when the crew builds the record, and the viewer's `ingest.ts` has one cross-action read of it. A record with no `step_id` falls back to its step-scoped session (`SessionKind::Step`), and `mission_id` is the outer scope a reader filters by first. What no consumer may do is key a registry on a step's `kind` to infer a record's shape: that is the snowflake this design deleted, and attribution has to be inferable from the records alone.

**Archives get no identity, and no rewrite.** A record of an execution written before executions were named carries none, and none is invented for it (5.0, #3036): the reader and the viewer's `ingest.ts` stamp nothing. Only the token and run counts key such a record, by `(session, mission)` (`usage_sum::execution_key` and `executionOf`); that grouping is the one place the old shape survives.

Known gap: that grouping is as coarse as the old one was. Two executions that shared a session and a mission before the change read as one in the counts.

## The flow vocabulary: one closed list, one spelling per event

The flow stream is the record of what darkmux did, and every consumer (the daemon, the viewer, hooks, doctor, the lab, the audit chain) reads the same records, so the words in them have to mean one thing. An action used to be a free string, the same event had two spellings, and each consumer kept its own list.

**One list.** Every action is a `darkmux_flow::FlowAction` variant, declared once in the `flow_actions!` invocation in `crates/darkmux-flow/src/action.rs`. Its wire string follows one grammar, `<scope>.<event>[.<detail>]`: lowercase ASCII, two or three dot-separated segments. The first segment is the row's `FlowScope`, declared beside it and pinned to the string by a test. Producers build the enum; no constructor, builder or helper takes an action as a string, and consumers match on the enum.

**Bookends and grains are declared on the row.** A row that opens or closes a unit says so, and `FlowAction::bookend` and `FlowAction::grain` read that back, so "which actions bracket a run, and which bracket a role execution" and "which records must name an execution" each have one answer. `run.start`, `run.complete` and `run.error` bracket a whole run; `dispatch.start`, `dispatch.complete` and `dispatch.error` bracket one role execution. The two grains never share a spelling, which is what lets `dispatch` mean one grain. The run pair is written by a guard that closes it on every exit path, so a panic or an early return still writes `run.error`. The viewer's `bookendOf` mirrors the table, and the rules that read bookends (liveness, the lifecycle below) key on `Bookend` and never on a string.

**Writing is refused, not normalized.** Every sink write goes through `FlowSinkWrite::write`, which builds the `CheckedRecord` a sink accepts. It refuses an action darkmux does not write today (`FlowAction::Other`: one this build does not know, a retired spelling included), an execution-grain record with no `execution_id`, and a payload that is not its action's type. `darkmux flow record --action` accepts only known actions.

**Utility jobs sit outside the bookends.** A utility job (compaction, radio routing) writes `utility.start` and its usage record, and `utility.error` when a routing call fails, and nothing else: no session of its own, no bookends, no run. See [The utility model](#the-utility-model-and-lean-utility-jobs).

**A guard keeps it closed.** `scripts/flow-action-guard.py` runs in CI, reads its list from `action.rs` so it cannot drift from it (the retired spellings it refuses to see are listed in the script, since nothing in the build knows them), and self-tests before it scans. In production Rust it forbids writing an action by hand in any shape it lists (a literal, a format string that builds one, a prefix test, `concat!`, JSON inside a string). In test code, the viewer, docs, skills, templates and fixtures, it forbids any string that looks like an action and is not a current one. Recorded archives are exempt, and a comment may name a spelling. It reads source text, so an action assembled at run time in a shape it does not list is not seen.

**A fleet upgrades together.** A reader from before the closed vocabulary does not know the dotted spellings, so an old hub misreads a new peer's records: its missions never end and its step results are not folded. A current reader does not upgrade an old peer's records either: a record spelled the pre-4.0 way reads as an unknown action (next section), and nothing upgrades an old reader.

## One reader, and what an old archive reads as

Every consumer that reads records back goes through `darkmux_flow::reader`: the day files, the Redis stream, a peer's records, the audit chain's JSON bodies. `parse_record` returns a typed `FlowRecord`; `parse_value` serves the daemon, which passes records on to the viewer as JSON with every field intact.

The reader maps nothing (5.0, #3036). A record's action is a current spelling or it is `FlowAction::Other`, kept verbatim: the pre-4.0 spellings, the actions retired with no current equivalent and a newer writer's actions all read the same way. A retired `source` reads as `FlowSource::Unknown`, a retired payload key stays under its old name, a pre-run-bookend whole-run pair reads as the execution bookends it was spelled as, and a record of an execution that names none gets none. `UnknownActions` counts the unknown ones by name, so `darkmux doctor` and the viewer (`· N unknown` beside the event count) can say so instead of dropping them. A 3.x archive therefore still loads and still lists, but most of its records are unknown actions and the views built on actions show little of it.

**This is a rule with a guard, not a habit** (#3035). After 5.0 a flow record only grows: a newer darkmux may write an action, a payload field or an enum value this binary has never seen, and the reader is where that reads as `FlowAction::Other` instead of an error. A consumer that deserializes a `FlowRecord` itself (`serde_json::from_str::<FlowRecord>`, `from_value`, `from_slice`, `from_reader`, or a binding annotated `FlowRecord`) skips the leniency, so the same archive would read one way there and another through the daemon. `scripts/flow-reader-guard.py` runs in CI beside `flow-action-guard.py` and fails on any such call in production Rust (test code and the reader itself are exempt; a deliberate exception takes a `flow-reader-guard:allow` marker). It is a spelling tripwire (it also flags `into_iter::<FlowRecord>`, `FlowRecord::deserialize`, a `type` alias of `FlowRecord`, and a bare `use serde_json::from_*` in a file that names it), not a proof: a generic helper or a type inferred through a function's return passes it, and review is what catches those.

The reader is lenient because an archive outlives the binary that wrote it, and rewriting one would destroy the evidence it is. This is the one place the user-file gate's strictness deliberately does not apply.

## Typed flow payloads

A record's payload is one Rust type per action. `Payload` (`crates/darkmux-flow/src/payload/mod.rs`) has one variant per payload-bearing action, listed once in `flow_payloads!`. A record is built from a variant, so it takes its action from the payload and a producer cannot pair an action with another action's payload; an action with no row carries none, and the write check refuses one. The TypeScript twins are generated (`FlowPayloads.ts` and one file per payload type under `ui/src/types/generated/`), so the viewer reads a payload through the type the producer wrote.

- **Open payloads are declared, never implied.** `OpenPayload` holds keys someone other than darkmux chose: a mission config's outcome document on `mission.start`, `mission.close` and `mission.abort`, and a model's own `create_finding` arguments. Every action that carries one is named in the list, so "which payloads are free-form" is one grep.
- **One vocabulary on the wire.** Durations are `*_ms` and instants are `*_at_ms` (epoch milliseconds). An optional key with no value is omitted rather than written as `null`; `telemetry.tokens` keeps `null` for "not reported", and a hook rule that matches `payload.<key>: null` matches both a `null` and an absent key. Key order follows the type's field order. `FlowRecord.source` is a closed `snake_case` set, and `tier` says who acted (`operator`, `frontier`, `darkmux`), never where a model ran.
- **Reading an archive is tolerant.** A payload that does not parse as its action's type is kept as `UnreadPayload`: its JSON is preserved and re-serializes byte for byte, a typed reader treats it as absent, and it is never written. A field an older version never wrote reads as absent, never as zero, and a word in a closed set this build does not name (a result class, a detector kind, a seat class) reads as `unknown` instead of dropping the record.

Known gap: the types are the contract for new records, but a renamed field is a wire break for every archive already written. Nothing maps an old archive's spellings any more, so a renamed field reads as absent in a record written before the rename.

## Two logs: the flow stream and the trajectory

darkmux keeps two logs, and they are not the same thing.

| | Flow stream | Trajectory |
|---|---|---|
| What | typed records about the machine's work: bookends, steps, budgets, usage, hooks, machine samples | one execution's own event log: every stream, turn, tool call, checkpoint and detector firing |
| Written by | the host, through `FlowSinkWrite::write` | the runtime, inside its container, to `<out>/.darkmux-runtime/trajectory.jsonl` |
| Read by | the daemon, the viewer, hooks, doctor, the audit chain, other machines | the host's live tailer while the execution runs, and the lab afterward, both through one fold |
| Lives | append-only day files, optionally Redis and the audit chain, shared across a fleet | in the execution's run directory |

The runtime is not a workspace member and depends only on `darkmux-trajectory`, not on the flow crate. That boundary is the design: the runtime writes what it saw, and the host turns it into flow records. The tailer reads each trajectory event as it lands and writes the flow record that matters beyond this execution (`telemetry.tokens` for each call's usage, the detector and checkpoint records).

The two share words. `dispatch.checkpoint` is both a flow action and a trajectory event, and they are different records: the trajectory event is the runtime's own note, and the flow record is the host's, written from it. `dispatch.tool_call.discarded`, `dispatch.gate.abort` and `model.streaming.end` are trajectory events only. A reader looking for a record should first ask which log holds it; [The check-in](#the-check-in-observing-a-stream-instead-of-truncating-it) tabulates its records that way.

**The trajectory is the only per-execution record of turns, rests and per-call usage.** `darkmux_trajectory::TrajectoryFold` is the one reading of it: turns, tool calls, compactions, rests, tokens, checkpoints, detector firings and stream timing are all derived there, one event at a time. The host's live tailer applies each event as it streams and the lab folds a finished file, both through `TrajectoryFold::apply`, so a live number and a post-hoc number are the same computation.

## One token truth

A token count has one source and one sum.

**The unit of accounting is the model call.** Every call darkmux makes to a model endpoint writes exactly one `telemetry.tokens` record when its reply returns. `darkmux_crew::usage::usage_payload` builds it, so every producer (the container path's per-turn tailer, hosted and local single-shot, `dispatch.map` items, each compactor call) stamps the same vocabulary: the call kind, its purpose (`work` or `utility`), the endpoint darkmux invoked, the model it requested and the model the reply named, and the counts. A count the provider did not report is omitted, never written as zero, and `token_source` says whether the reply carried a usage block. `darkmux_trajectory::UsageCounts::total_tokens` is the one rule for a total: the provider's own total wins, and `prompt + completion` is used only when the provider reported the split and no total. A split missing a half is not a total.

**A total anywhere is a plain sum of those records.** The daemon's `usage_sum` module and the viewer's `sumUsage` are the two executors, and the shared golden `tests/usage-golden/` pins them to one answer. There is no execution keying in the sum, no estimate, and no local-or-cloud classification: the sum reports what darkmux invoked (the endpoint string and the model), and utility tokens are counted under their own chip. The one exception is data written before usage records existed: an execution with zero usage records counts its token-bearing completion once.

**A call with no prompt count has an unknown spend and is never charged as small.** A call's settlement against the dispatch's token cap (`limits.tokens_per_dispatch`) charges such a call (and one with no usage at all) at the whole granted `max_tokens` plus the prompt, estimated from the request at `CHARS_PER_TOKEN` (four) characters a token. Under an endpoint's window budget the halves that were reported still count, as a floor, and the window is flagged as not fully metered (`unmetered_calls` on `budget.warn` and `budget.wait`, and "spent at least" in doctor). A call that reported no usage at all adds nothing to a window's known spend, so it can warn but never make a token budget wait on its own. A calls budget is never flagged, since a call count is exact.

**`metrics.json` is gone.** The runtime used to write a totals file on a clean exit, and every reader had to be checked against the trajectory for the times the two disagreed: a killed run kept whichever run's copy was there before. In one measured archive 37 of 241 disagreed with their own trajectory, and every time the file was the wrong one. The file, the flags that policed the disagreement and the fields that mirrored it are deleted, and every count is a fold of the trajectory. A dispatch envelope's `metrics` block is written by the host from that fold.

Known gaps: the per-dispatch charge for an unknown spend is a stated estimate (four characters per token), not a measurement, and a window budget sees only this machine's usage records.

## Configuration: visible defaults, gated features, secret carve-outs

darkmux's settings live in one file, `~/.darkmux/config.json`, resolved with a single precedence everywhere: **env var > `config.json` > built-in default**. The env layer survives as a live override (CI, tests, a one-off shell); `config.json` is the durable surface; the built-in default is the floor. The whole precedence lives in one module (`darkmux_types::config_access`) so a reader never has to wonder where a value came from, the same *operator sovereignty* principle the rest of darkmux is built on: every default overridable, every value's source explainable.

Four choices shape it:

**Visible defaults, not hidden code-defaults.** `darkmux init` writes the common knobs *into the file* with their default values, rather than leaving them implicit in the binary. The cost is that a default written today doesn't silently change on upgrade, but that's the point: the operator can *see* what's configurable without reading source, and *change* a default with a file edit instead of a recompile. A config meant to replace env-var sprawl has to be discoverable, or it isn't a config at all.

**Off-by-default features are `enabled`-gated blocks, not presence-gated.** Redis coordination, the audit log and the fleet listener are written as complete blocks with `"enabled": false` and every connection knob populated; the serve daemon's `token_keychain` and `read_auth` are written visibly as `false`. The block's *presence* doesn't turn the feature on; the `enabled` flag does. So the whole surface is discoverable (you see exactly what Redis would need) and one edit from on, without darkmux guessing intent from whether a `host` happens to be set.

**Secrets are carved out, never plaintext config.** A `config.json` is a file an operator writes, edits, and might share or commit. So the one thing it never holds is a password: the Redis password and the serve-daemon bearer token live in the macOS Keychain, read at runtime and wrapped so they can only ever reach a log redacted. `config.redis` holds the non-secret connection bits; the Keychain holds the secret. (One other carve-out, for a different reason: `DARKMUX_HOME`, the pointer that *locates* the config root, stays an env var because it can't live inside the file it's there to find. It is the one relocation of the darkmux root: a `./.darkmux/` in the working directory is never adopted.)

**A `0` on a bound means unbounded, and a policy value names the action.** A `0` on a bound reads as "no limit" (`redis.maxlen`, `runtime.step_command_timeout_seconds`, an endpoint's `limits.concurrent_calls` and `limits.tokens_per_dispatch`, `runtime.thermal.episode_threshold`, `runtime.thermal.max_pause_ms`), never as "instantly". The exceptions are floored to 1 at read time because a zero would mean "run nothing, forever" or has no coherent meaning: `runtime.dispatch_free_concurrency`, `runtime.local_dispatch_concurrency`, and the thermal `speed_limit_hold_samples` and `ratchet_factor` (`config_access.rs`, each `.max(1)`). And a setting that says what happens at a limit says it in a word that names the action (`off`, `record`, `warn`, `wait`, `conclude`), never a guessed number: budgets ship off, and warn once set.

Loading is lenient (every field optional, unknown keys caught by an overflow map), so a hand-edited or malformed file never panics the CLI and `darkmux doctor` always runs. Consuming is not: a key the schema does not know is refused at every entry point's preflight and failed by doctor, naming the closest valid key, so a typo can never silently do nothing ([User files](#user-files-the-unknown-key-gate-and-enum-settings)). Additive schema changes are a minor version bump, and an older binary refuses a newer file's new key, so the binary is upgraded before the file is written.

## User files: the unknown-key gate and enum settings

A **user file** is a JSON document the operator writes and darkmux reads at run time: `config.json`, `profiles.json`, role, skill and crew manifests, mission configs, rule files, workload documents, lab fixture manifests, and a crawl's workspace spec (`darkmux_types::user_files::UserFileKind` is the set). None of them is compile-time, so a key the file's schema does not know (a typo, or a key a newer or older darkmux spelled differently) used to do nothing, silently.

**Loading never crashes on it; consuming refuses.** The typed load survives an unknown key (a `#[serde(flatten)] extras` overflow catches it, so one typo never discards the rest of the file) and `darkmux doctor` still runs against a file that is not even JSON. But every entry point that consumes the file refuses at preflight, before minting anything, and doctor reports it as a failure, one row per file. Both name the file, the key's dotted path and the closest valid key. A value of the wrong type, or a missing required key, is refused the same way, because one such value fails the whole typed load: for `config.json` every setting would fall back to its default (Redis and audit silently off), and for a user role, skill or rule the builtin of the same id would silently stand in.

**The valid keys are derived, never listed.** Each kind's keys and value types are its Rust type's derived JSON schema (`schemars::JsonSchema`), walked against the raw document by `key_issues`, so a new field is valid the moment it exists and no key list can drift. The schema honors serde's own attributes (`rename`, `flatten`, `tag`, `untagged`). The `extras` overflow is `#[schemars(skip)]`: it catches keys, it does not make them valid. A flattened map that is itself the schema (a hook rule's `match`) stays open, and each kind's `open_objects` test pins that set. `_comment` is valid in any struct-shaped object, as a note for the reader. `closest` is the only "did you mean" in darkmux: the gate, `darkmux config set` and `mission launch`'s undeclared-parameter warning all call it. Its structural guard is `whatever_the_gate_passes_the_typed_load_accepts`: for every shipped template and example, and for variants that insert a `_comment`, an unknown key or a value of each JSON type at every level, a document the gate passes must load with serde.

**One preflight chain.** `config_enum::preflight` (the config) runs inside `darkmux_profiles::preflight_with` (the registry), which runs inside `darkmux_crew::user_files::preflight_with` (roles, skills, mission configs, rules); every dispatch, mission launch, radio and ACP entry point calls it, and `darkmux_lab::user_files::preflight_with` adds workloads and fixtures for a lab run. A preflight refuses only over a file the operation would load: the effective copy of each mission config and workload id (a copy another tier shadows never loads), and the one fixture the run's workload binds. Doctor fails every file and says when one is shadowed. A workspace spec has no fixed location, so the launch preflight checks the one a launch input names, and `WorkspaceSpec::load` checks it again where the plan step reads it. A step's own `config` is checked the same way against its kind's struct ([Typed step config](#a-steps-config-is-checked)).

**Semantic validation stays at consumption.** The gate answers "is this a key and a type the schema has". Whether a known key's value makes sense (a profile name that resolves, a window that fits) is checked where it is consumed and in doctor, never on the load path, and a value that fails is refused there, never replaced by a default.

**Enum-valued settings follow one rule.** An unregistered value in an enum-typed setting is bad config and is never resolved to a fallback, in either direction. Two declarations in `darkmux-types/src/config_enum.rs` carry everything: a `ConfigEnum` (implemented with `config_enum!`, one row per value with its token and one-line meaning, plus any retired spellings, with an exhaustive `match` so the value list cannot drift from the Rust enum) and an entry in `ENUM_SETTINGS` (the dotted key, the env var, the shipped value and the entry-point scopes that could consume it). Storage stays a string parsed at the accessor, so a bad value never fails the load. Preflight refuses it at every entry point that could consume it (whether or not one particular run would read it), `darkmux doctor` fails it, `darkmux config set` refuses it, and help lists the valid values with their meanings. `--skip-preflight` does not waive it: that flag skips a Docker probe, and a bad config value is not a probe result. Policy values name the action (`off`, `record`, `warn`, `conclude`), never `enforce` or `observe`. Two settings are refused by no preflight, by design, and each registry entry carries a `no_scope_reason` that doctor prints instead of claiming a refusal: `fleet.mode` (no command that starts work reads it) and a hook rule's `match.level` and `match.category` (a bad value turns the hook sink off, loudly, while the run continues). Per-endpoint `profiles.json` enums (`managed`, `dialect`) share the value tables but keep their own refuse-at-use path through `Lenient<T>`.

**The exceptions are named.** The profile registry quarantines a mistyped profile or endpoint entry by name, loudly, instead of failing the file, and the gate leaves that to it. Flow archives stay lenient on read. Nothing else is.

Known gaps: the enum-registry conformance test cannot see a string compared with `==` against a literal outside the registered accessors; that stays a review question. Whatever the schema types as an open value (a mission-config `dispatch.map` collection, a hook rule's `match`) is open to the gate and is checked only where it is consumed.

### Version markers make additive-only real (5.0, #3035)

After 5.0 a change to a data shape is additive, so a file written by a newer darkmux can carry a key or a variant this binary cannot place. Three rules keep that from being hoped for.

**The authored and persisted shapes name their version.** Each marker is its own constant in `darkmux_types::data_version` (a data-shape version, independent of the release number): the role, skill, crew, rule, workload and lab-fixture manifests, `mission.json` and each phase, task and step JSON, `graph-report.json`, `lab-registry.json`, the `resume_origin` sidecar and the lab run `manifest.json` (`manifest_schema_version`, a separate key beside each provider's own integer `schema_version`, which versions that provider's fields and which the enricher raises). `config.json`, `profiles.json`, mission configs and the workspace spec already had one and are enforced the same way; `envelope.json` and a mission's config snapshot carry their own, and `fleet.json` keeps an advisory `version` that nothing enforces. The trajectory opens with a `trajectory.header` line (`darkmux_trajectory::TRAJECTORY_SCHEMA_VERSION`); the fold records its version and counts nothing for it, so a trajectory with only a header is still the empty one. `lessons.db` keeps its integer `user_version`. A marker is written on every save and read leniently: absent means the file predates it, and is accepted.

**A newer marker is refused with one message; the same or an older one changes nothing.** The unknown-key gate checks the marker first. A file whose `schema_version` is newer than `UserFileKind::schema_version` is refused as `Problem::Newer` (`this file was written by a newer darkmux (<kind> <file version>; this binary reads <known>). Upgrade darkmux.`) and its keys are not judged, because a newer darkmux may legitimately have added them. A file at the same version or older keeps the rule above: an unknown key is a typo and is refused. The state files go through `retired_state::parse_state`, the resume gate refuses a newer `resume_origin`, `lab inspect` a newer run manifest or trajectory, and `lessons.db` a newer `user_version` (it is never re-stamped down; an older one runs ordered migrations, then is stamped).

**What a future value reads as.** `MissionStatus`, `PhaseStatus` and `NodeStatus` take a `#[serde(other)] Unknown`, as do the enums a peer or a recorded run can send us (the trajectory's reason and outcome enums, the model ledger's, `HubLink`, `HealthState`, the lab's score and review rulings, `MissionSpecOrigin`, `AbandonReason`). An `Unknown` is never read as a success, a failure or work to run: no lifecycle verb moves a mission or phase that carries it, the scheduler neither runs the step nor counts it as a satisfied dependency, and the run reads `unparseable`. Enums an operator writes (a rule's `kind`, a profile's `dialect`, a config policy) stay closed, because the gate refusing a typo is the point and a catch-all would make a typo valid; `RunKind` is the work-unit vocabulary of contract 8 and a fourth kind is a contract change; `SubmissionMode` rides a `deny_unknown_fields` wire on purpose. `FleetRoster` keeps unknown top-level fields across every rewrite, as a machine entry keeps its own.

### Retired spellings are refused, and they name their replacement

A rename is not read as its old name. Each surface that can be misspelled has its own table of retired spellings, and each refusal names what replaced the spelling. There are no aliases.

- **Config keys** are `RENAMED_SETTINGS` and `RETIRED_SETTINGS` in `darkmux_types::config`, each with the line naming its replacement; each other user-file kind has a `RetiredLookup` for its own retired keys. `darkmux config set` refuses a retired key and names the new one.
- **Environment variables**: `retired_env_leftovers` is one check at CLI entry that refuses every command except `doctor` and `config` (and `--help` and `--version`) while a retired variable is set, and doctor lists each with what replaced it.
- **Verbs and flags**: `src/retired_verbs.rs` holds the one `RETIRED` table, and `refusal` runs on the raw command line before clap, because a retired spelling can still parse (a retired `lab run <read verb>` reads as the launcher for a workload of that name), so clap's own rejection cannot be the trigger. It exits 2 naming the replacement. `scripts/rs-drift-guard.py` scans Rust string literals for retired verbs, and the docs-drift job scans the docs.
- **Mission state files** in a retired spelling (a `sprint_ids` key, a `sprints/` directory) are refused by `darkmux_crew::retired_state`, naming the rewrite, and doctor fails each one.
- **Hook rules** that name a retired action spelling are refused: the hook sink does not load, and doctor fails the rule, naming the spelling to write.

The one place a rename is read leniently is a flow archive, which is never rewritten and now maps nothing ([One reader](#one-reader-and-what-an-old-archive-reads-as)).

## Endpoints: what darkmux does there, not where they are

darkmux records what it invoked, the endpoint and the model, and never classifies an endpoint by location or cost. The one distinction it draws is its own action (#2902):

| Kind | What darkmux does | What it knows |
|---|---|---|
| **Managed** (`"managed": "lmstudio"`, or no endpoint at all) | loads and unloads models with `lms`, dispatches to its own `darkmux:` instance (#2240), plans residency under the RAM budget | the loaded model and window, because it loaded them |
| **Unmanaged** (an endpoint with a `url`) | only sends requests | the model it requested and whatever the reply reports; what serves the endpoint can change without darkmux seeing it |

The kind is an enum so a further kind is additive. (A fleet machine is not one: running on another machine is a property of the profile address, [`profile@machine`](#fleet-addresses-trust-and-the-execution-channel), and the receiver's own endpoints decide what it does there.) The ROUTING decisions (the dispatch's hosted-or-container branch, the data-boundary check, the busy check's local target, the scheduler's seat placement, and a step's `config.endpoint`) match on it exhaustively with no catch-all, so a new kind is a compile error at each of them. The remaining consumers (residency, labels, doctor, `profile list`) ask the boolean `is_managed()`, and a new kind reads there as "not managed" until each is revisited.

**One resolver, one set of rules.** Endpoints are declared once in `profiles.json`'s `endpoints` map (url, `managed`, `dialect`, `auth` naming where the credential lives, `limits`) and named by id from a profile model. An endpoint declares its kind, either `"managed": "lmstudio"` or a `url` (one with neither is refused at use), and an inline endpoint object on a model is refused, with the exact rewrite named by every dispatching preflight and by `darkmux doctor`. Every path that turns (role, profile) into a request goes through `crew::target::resolve_in`, which returns the SELECTED model together with its own endpoint, dialect, chat URL and window. The endpoint rules themselves (classification, the chat URL, the host a label may show, the credential order) live on `darkmux_types::ModelEndpoint`. Before this there were three resolvers, three "is this remote?" tests, two URL builders, three body builders, three host extractions and the credential order twice, and fixes landed in one copy only (#2904, #2905); the compaction window even came from the profile's default model while selection had picked another. `step_kinds::endpoint_conformance` walks the workspace and fails when a new call site decides any of these on its own.

**Limits are the operator's and they are enforced.** An endpoint's `limits` is a rolling budget with a policy; it is parsed and validated at preflight, shown by `darkmux doctor`, and enforced before each call to the endpoint, managed or not ([Endpoint budgets](#endpoint-limits-and-budgets)).

## Endpoint limits and budgets

**Limits belong to the endpoint, whether darkmux manages it or not** (#3035). "Remote" was the wrong axis: a local server on the same machine is an endpoint too, and what matters is whether darkmux manages it. Four fields live in an endpoint's `limits`, and they split on that line:

| Field | Where | Meaning |
|---|---|---|
| `tokens_per_dispatch` | any endpoint | what ONE dispatch (one role execution) may spend there |
| `window` (with `policy` and `warn_at`) | any endpoint named by id | a rolling budget over the last `period`, summed from the usage records carrying the endpoint's id |
| `concurrent_calls` | an endpoint darkmux does NOT manage | how many of its calls run at once; absent, one at a time (within one darkmux process), said once per launch, never a guessed number |

On a MANAGED endpoint the scheduler owns parallelism (residency and the backend's parallel slots), so `concurrent_calls` is refused there at validation (preflight and doctor), naming the scheduler. The decision is `EndpointKind::is_managed()`, never "is it LM Studio". Spend limits apply to managed endpoints too, and a `wait` on one holds the dispatch in place, keeping its seat, by two mechanisms: between the turns of a container run it reuses the thermal governor's pace file (reason `budget`, written by the host sampler's pacer), and at the start of a dispatch and on a single-shot call it is #2902's polling loop (bounded sleeps that re-read the window and check whether the run was stopped). An inline endpoint (a step's own `config.endpoint` object) has no id to sum usage by: its `tokens_per_dispatch` and `concurrent_calls` work, and a window on one is refused, pointing at naming the endpoint under `endpoints`.

A budget is the operator's own limit on what darkmux spends at an endpoint, and what reaching it does. **Nothing runs unless the operator sets one**: darkmux ships no number, an endpoint with no `limits.window` (or with `policy: "off"`) is returned as no window budget before any file is opened, and a `limits` that cannot be used as written (unreadable, or a period that does not parse) is an error at preflight, never "no budget", so a typo cannot silently disarm one.

An endpoint declares `"limits": {"window": {"period": "1d", "tokens": 2000000}}` (tokens, calls, or both; the period is `<n>m`, `<n>h` or `<n>d`) and a `policy`:

| Policy | On reaching the budget |
|---|---|
| `off` | nothing is counted |
| `warn` (the default once a budget is set) | a CLI line and a Warn-level `budget.warn` record; the call goes ahead. An optional `warn_at` fraction warns once earlier |
| `wait` | calls to the endpoint pause until enough of the window has expired to leave room, saying how long (a CLI line and a `budget.wait` record), then resume and write `budget.resume` |

**A breach never stops the run.** A waiting call loses no work: the wait is recorded in `darkmux_types::run_pause`, which extends the run's wall-clock bound (the host-side twin of the thermal governor's pause). Every half second the wait checks whether its run was stopped: the process was interrupted, or its mission was aborted or finalized, or its phase abandoned, on disk. `darkmux mission abort` runs in another process and only writes that terminal state, so the waiter reads it rather than being signaled: a pid is not the operator's handle on a run. A stopped wait errors, writes `budget.stop`, and the call is never sent.

**The window is rolling, not calendar.** "The last `period` from now": its spend is the sum of this machine's usage records that carry the endpoint's id (`endpoint_id`), read with the same per-record reading the token sum uses, and both purposes count (a budget is about what the endpoint served). `0` is not a budget and is refused; set `policy` to `off` instead, because read either way a zero would be an eternal wait. "Room" is `spent < budget`, and the next call's cost is unknowable until it returns, so a call admitted with room can overshoot by itself: a soft ceiling.

**Cost was measured.** The window is read before every call to an endpoint with a counting window budget, so it must not rescan history each time. `Ledger` opens only the day files the window can touch and remembers, per file, the byte offset it has read to. The first read in a process pays for the whole window (measured on release builds: about 5 ms for a one-day window, 45 ms for seven days, 180 ms for thirty on a busy machine); every later read costs 0.1 to 0.7 ms.

**The per-dispatch cap is a different knob.** `limits.tokens_per_dispatch` caps what one dispatch (one role execution) may spend at the endpoint, under the endpoint's `policy` (`warn`, the default once a cap is set, or `off`). `wait` governs the window only: a dispatch's own spend never expires, so nothing would free room, and `wait` with no `window` is refused. It has no default number and `0` is no cap. A `dispatch.map` item is one dispatch with its own cap; the pre-5.0 allowance shared across a step's items (`bucket_group`) is retired, and a whole-run budget is the rolling `window`. A container dispatch settles each model call into its cap as the call lands, so the warning fires at the crossing.

Known gaps: another machine's spend at the same endpoint is not seen, since each window reads its own machine's flow log; records written before an endpoint had an id carry none and are not counted; and a call with no usage at all adds nothing to a token window's known spend ([One token truth](#one-token-truth)). Tokens only, never currency.

## The utility model and lean utility jobs

Some work is darkmux's own: compacting a long conversation, and routing free text onto one mission config for `darkmux radio` and the editor panel. These are bounded, structured jobs where a small model is enough (the utility half of the role families in [CLAUDE.md](CLAUDE.md)). They run on **the machine's one utility model**, declared once in `profiles.json` as `internal.utility`: `{ "id": "<model>", "n_ctx": <window> }`, an object with its window. The bare-string spelling is refused, and the registry does not load with it.

- **Profiles hold work models only.** Every task and step selection path sets the utility binding aside. A profile that still lists it puts work on its other model, a profile that lists only it is a loud error naming the fix, and `mission launch` refuses, before minting anything, a task whose staffing (or a `dispatch.single_shot` or `dispatch.map` step's `config.model`) resolves to it. The lab can still benchmark a candidate utility model through a profile that lists it.
- **Its window is declared, not inferred.** The compaction payload is bounded by the binding's `n_ctx`. An object with no `n_ctx` declares no window, falls back to a named default, and doctor nudges. With no utility model at all, compaction is off outright for the dispatch, disclosed at dispatch time, and radio and ACP routing refuse to route, naming the fix. There is no fallback to `default_profile`.
- **Utility jobs are lean.** What counts as one has a single definition, `darkmux_crew::usage::utility_job` (every runtime compactor call and every call by the radio routing role; `call_purpose` and `UtilityJobKind` read it). A job writes `utility.start` when it starts and its `telemetry.tokens` record (`purpose: utility`, `job`) when it ends, and nothing else: no session, no bookends, no run. The markers are emitted at the one chokepoint each half passes through (`darkmux_crew::utility::run_utility_single_shot` on the host; the runtime's compaction-start trajectory event through the tailer).
- **Accounted and visible, not listed.** The fleet total sums utility under its own chip, and the fleet card's utility strip and the "compacting" scope reading key on `utility.start`, but the runs board, the fleet card's activity and the status line's last dispatch key on bookends, so they show work only. That is the intent: the board used to list dozens of routing runs a day.
- **The trade is stated.** A routing call can wait behind a compaction on the one utility instance, by decision (#2914). Radio reports what LM Studio says while it waits ([Radio](#radio-free-text-onto-one-command-and-a-confirmation-before-it-runs)).

Radio's *answering* seat is work, not a utility job: it keeps its bookends and its run, staffed through `radio.answerer_profile` and `role_profiles.radio-host`.

Known gap: compaction records (`dispatch.compaction`, `telemetry.compaction`) still carry the specialist's role and model at record level and name the compactor only in `payload.compactor_model`; the per-call usage record is the one that attributes correctly.

## Public surfaces: the daemon's HTTP routes are a contract

The daemon's routes and response shapes are semver contracts, the same as the CLI's `--json` output (next section). A script, another machine's fleet view (`GET /fleet/view`), or the viewer binds to what a route returns, so it changes only on purpose.

**The route table is the one place a route exists.** `crates/darkmux-serve/src/routes.rs` builds the router from `table()`, and `route-table.golden` pins every route's method, path and response type. Adding, removing or renaming a route fails `route_table_matches_the_golden` until the golden is regenerated on purpose (`DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve routes`), which puts the change in the diff, and a CHANGELOG line goes with it. The fleet listener's two routes (`POST /fleet/work`, NDJSON, and `GET /fleet/card`) are in the golden too.

**Every JSON body is a named type.** A handler returns one of the types in `crates/darkmux-serve/src/wire.rs` (or a type that lives beside what it describes), through the `json!` and `json_list!` macros, which fail to compile if the type does not exist. A handler builds no `json!` object by hand. The TypeScript the viewer imports is generated from those types (`bun run types:regen`), `bun run types:check` regenerates and diffs so a stale twin fails CI, and `generatedOnly.test.ts` fails when the viewer's `fetchJson<T>` names a type that is not generated. There is no hand-written copy of a route's shape to drift.

**Fields the producer could not read are `null` on the wire**, never a zero or an empty string; an optional field the type marks `skip_serializing_if` is absent instead, and its generated twin is optional. `session` is an internal word (contract 8), so the daemon's own response types name no route or field with it: the drill-in route is `/flow-dispatch/:id` and its fleet twin `/fleet/dispatches/live`. The flow records that `/flow/<date>` and `/flow-dispatch/:id` relay are the stored records and still carry `session_id`, unchanged, because archives are append-only.

Known gap: a route's *behavior* (a filter, a status code on an error) is pinned only where a test asserts it; the golden pins the shape.

## CLI `--json` is a contract

A script or an orchestrator binds to what a verb prints under `--json`, so from this release that shape is a semver contract, the same as the daemon's HTTP responses (`crates/darkmux-serve/route-table.golden` pins those). Three things hold it, and each fails a test when it slips.

**Every output is a named type.** A verb prints through `cli_json::emit` (`src/cli_json.rs`), which takes only a `CliOutput`: a serialized Rust type that derives its schema. A hand-built `serde_json::Value` is not one, so it cannot reach stdout by that road, and `no_verb_serializes_json_outside_emit` scans the sources for the others: production code under `src/` may not call `serde_json::to_string*` (or `to_writer*`, `to_vec*`, however imported) outside a short, counted allowlist of uses that write a file, a fingerprint or a prompt, or quote a free-form field in a text line. Where the daemon already has the shape (`Run`, `MissionGraph`, `ModelLedger`, `MachineSpecsResponse`) the CLI prints that type, never a copy. The `cli_outputs!` table in `cli_json.rs` is the one place a type becomes an output and names the verbs that print it; `every_json_verb_is_in_the_table_and_every_row_is_a_verb` walks the clap tree, so a new `--json` flag with no type fails, and `always_json_verbs_and_the_clap_tree_agree` covers the verbs with no flag (`ALWAYS_JSON`) from the other side, so the table and the tree must agree in both directions.

**A golden pins each shape.** `tests/cli-json.golden` lists every verb and, for every type reachable from one, its fields in declaration order and their types (integers by width and sign, tuples by element), derived from the types by `schemars`. Changing a field, a type, a width, an order or an optionality fails `cli_json_matches_the_golden` until the golden is regenerated on purpose (`DARKMUX_REGENERATE_FIXTURES=1 cargo nextest run -p darkmux cli_json`), which puts the change in the diff, and a CHANGELOG line goes with it. A field the producer could not read is `null`, or absent when its type says `skip_serializing_if`; it is never a zero or an empty string.

**One rule for versions.** An output carries no in-band `schema_version`. Its version is darkmux's own: a shape change is a semver-visible change to the binary. The one kind of exception is a document that is also written to disk as an artifact, which outlives the binary that wrote it, so it names the schema it was written under (`RunStats`, `FindingRecord`, `ModRecord`, `ModelLedger`, `FlowStatus`, a mission-config document). Every output is one JSON object, so a field can be added without breaking a reader. The exception is a stream (`flow tail --json`), one record per line, whose record type is pinned by the flow schema.

**What is not an output.** The golden covers what a verb prints to stdout as a document. Flow-record payloads have their own schema and version (`FLOW_SCHEMA_VERSION`), `config list` prints the config document (`CONFIG_SCHEMA_VERSION`), and the JSON-RPC that `darkmux acp` speaks is the Agent Client Protocol's. Three fields of the `dispatch` envelope (`detections`, `bounds`, `host_window`) are the flow payload types themselves (`TelemetryDetectorPayload`, `RuntimeBounds`, `HostWindow`), so the golden pins them and the flow stream and the envelope cannot drift.

**The known free-form fields.** Every `any` in the golden is listed under its `# untyped` heading, and `every_untyped_field_is_explained_in_design_md` fails until each is named here with its reason. A script reading one of them gets JSON whose shape the golden does not pin:

- `Knob.value` (inside `DispatchEnvelope.bounds` and `RunStats.bounds`): a resolved runtime knob's value, which is a number, a boolean or a string by knob, or `null` for an uncapped one.
- `TelemetryDetectorPayload.context` (inside `DispatchEnvelope.detections`): the provenance a dispatch caller supplied, carried verbatim; darkmux never reads inside it.
- `FindingRecord.context` and `ForFinding.context`: the dispatch's `record_context` verbatim, or `null`. darkmux never reads inside it; its author owns the shape.
- `FindingRecord.emitted` and `ForFinding.emitted`: the model's tool-call arguments, verbatim and opaque by design.
- `InputJson.default`: a mission input's declared default, which may be a string, a number or a bool.
- `Profile.use_when`: the profile author's free-form routing hint, kept as written.
- `Lenient`, `Lenient2`, `Lenient3`, `Lenient4` (a `ManagedBackend`, `Dialect`, `UsageLimits` and `BudgetPolicy` as read from `profiles.json`): each is `T | any` because an unrecognized value from a newer darkmux or a typo is kept and printed as read rather than failing the registry (an enum value on a registry entry is refused where it is used, contract 9); `doctor` reports it.

## Schema isolation: darkmux owns its own config

Every field an operator sees in a darkmux profile maps to a darkmux-typed schema entry the internal runtime consumes: no decorative fields that look tunable but have no effect. The internal-runtime path (`crates/darkmux-crew/src/dispatch_internal.rs`, `runtime/src/`) reads only darkmux-native typed fields from `profile.runtime.*`; darkmux owns these field names, their semantics, and their evolution. An untyped `extras` map catches keys the type does not know so that a load survives them; nothing in the internal-runtime path reads from it (`from_profile_ignores_openclaw_custom_instructions_extras` and its neighbors pin that), and the [user-file gate](#user-files-the-unknown-key-gate-and-enum-settings) refuses an unknown key at consumption, so forward compatibility means upgrading the binary first. This discipline began when darkmux could shell out to a separately installed agent runtime: the two paths were deliberately schema-isolated (darkmux never translated its profile fields into the other runtime's config shape, and vice versa), so an upstream schema change had zero impact on darkmux. The shell-out was removed ([#1405](https://github.com/kstrat2001/darkmux/issues/1405)), and the rule now stands on its own as "the profile schema is purely darkmux-typed, full stop."

## Scope of the internal runtime: workflow-fit, not feature creep

When deciding what to add to the internal runtime, the filter is **workflow-fit**: does the feature serve darkmux's own workflow, not "does some other agent runtime have it." darkmux is shaped by three load-bearing decisions:

- **Mission-as-contract.** A phase is a bounded unit of work with explicit inputs (prior phase outputs, scope file), explicit outputs (typed text file persisted to disk), and explicit verify criteria. Cross-phase memory is file-mediated by design, so the frontier orchestrator sees what state moves between phases. Hidden session-state that survives across dispatches breaks this contract.
- **Utility/specialist split.** Utility agents (4B-class: the compactor, the radio router) handle bounded structured work at high throughput. Specialist agents (35B+: coder, code-reviewer, analyst) handle judgment-dependent work at lower throughput. Features that push specialists toward utility work (mid-dispatch planning, todo tracking, autonomous replanning) collapse the layering that makes the split valuable, turning judgment-bearing work into hidden utility work.
- **Operator sovereignty + frontier-as-strategic-layer.** The frontier orchestrator (Claude Code) holds the strategic context; utility agents structure under that context; specialists execute within it. Features that move strategic choices *down* into utility or specialist dispatches (opaque session state, automated replanning, scoped planning verbs) quietly relocate decision authority into layers that lack the context to make them well.

The filter for any proposed internal-runtime feature: **does this reinforce mission-as-contract, the utility/specialist split, and frontier-as-strategic-layer, or does it blur them?** Features that reinforce land cleanly even when they're small. Features that blur produce "works technically but feels wrong" outcomes that surface as bugs months later.

## Compaction: tiers, structured slots, and graceful degradation

Compaction is the harness lever with the largest measured wall-clock impact (Articles 1–2), so it gets the most defensive engineering. Two strategies coexist behind one config knob (`profile.runtime.compaction.strategy`):

- **Narrative** (default): prose summary, replaces the middle of the conversation with a synthetic `user`-role message. The Article-2-era shape.
- **Structured-slot** (tier-2, [#352](https://github.com/kstrat2001/darkmux/issues/352)): the compactor is called in JSON mode and emits a typed `StructuredCompactionOutput` (objective, current-truth, completed-decisions, errors-to-preserve, next-actions, verify-criteria), rendered as labeled markdown into a synthetic `system`-role message. Per-slot character caps bound each field. The default compactor prompt (the empirically-won "reality-discipline" prompt) frames every slot as *show, don't tell* to suppress the hallucination-class regressions earlier prompt versions produced.

The design bet behind structured-slot is that **a small model fills labeled slots more reliably than it writes good prose**, and that typed output degrades more gracefully. Three degradation layers make that real, in order:

1. **Lexical JSON repair** ([#401](https://github.com/kstrat2001/darkmux/issues/401) layer 1): a truncated compactor response (runaway escapes, an unterminated string, unbalanced brackets) is walked byte-by-byte and closed off, producing a parseable (if lossy) value rather than a dispatch bail.
2. **Schema patch** (#401 layer 2): if required fields are still missing after parse, safe defaults are inserted and `compaction_metadata.truncation_patched` is set so downstream analysis can flag the run.
3. **Escalation bound** ([#377](https://github.com/kstrat2001/darkmux/issues/377)): `reserve.bail_after_compactions` caps how many times one dispatch may compact; past the bound the runtime emits an `EscalationTriggered` terminal for frontier handoff rather than looping forever.

**Who compacts, and with what window.** Compaction is a utility job: it runs on the machine's one utility model (`internal.utility`), which declares its own window, and it writes `utility.start` and a usage record (`purpose: utility`) rather than a bookend pair ([The utility model](#the-utility-model-and-lean-utility-jobs)). The window the compaction *serves* is the selected work model's own, resolved by `crew::target::resolve_in`, not the profile's default model's. With no utility model registered, compaction is off for the dispatch and the dispatch says so; nothing falls back silently.

Two model-shape accommodations round it out: thinking-mode models route JSON to `reasoning_content`, so `extract_compactor_content()` falls back there when `content` is empty; and the JSON-mode request uses LMStudio's `json_schema` response format (decode-time shape enforcement), not OpenAI's looser `json_object`. The dispatch budget (turns/tokens used vs caps) is folded into the structured output's metadata so the model sees its remaining runway, framed as a *floor, not a ceiling*. Every field is darkmux-typed; `custom_instructions` is a typed field appended to the base prompt, not an `extras` passthrough.

## Runtime resilience: struggle detection + feedback injection

A local model in an agent loop fails in characteristic ways: re-reading the same file, re-reasoning the same dead end, hammering a tool that keeps erroring, emitting reasoning until it hits the token cap with nothing to show. The internal runtime carries a family of cheap, edge-triggered detectors for these, plus the recovery and budget machinery to act on them. Three design commitments shape the family:

- **Observability before intervention.** Each detector (cycle, reasoning-loop, tool-failure cascade, cadence-drift) writes a trajectory event and feeds the model a nudge, and nothing else changes: the default is *visible struggle*, not auto-bail. `MAX_TURNS` and the inactivity deadline catch genuinely-stuck dispatches *late*; the detectors exist to surface the struggle *early*, for the operator and (via feedback injection) for the model. The one detector that may act has its authority set by the operator, as a policy value that names the action (`runtime.detection.degeneracy.policy`, an enum setting): `off` does not measure, `record` measures and records silently, `warn` also surfaces a warning, and `conclude` (the shipped default) closes the model's thought so it answers from what it has, escalating if the output keeps repeating; nothing is discarded.
- **Recover, don't discard.** When a turn hits the per-call token cap but emitted well-formed tool calls, those calls are salvaged rather than treated as a failed turn. The check-in that used to create those cap hits no longer truncates anything: see [The check-in](#the-check-in-observing-a-stream-instead-of-truncating-it) below, which supersedes the mechanism described here while keeping the commitment. A `finish_reason=length` turn with no content and no tool calls (pure runaway reasoning) is dropped, nudged, and retried within a small budget before escalating. Tool calls the model wrote as plain text (bracket, harmony, or darkmux's XML extension) are promoted back to structured calls instead of being lost ([#406](https://github.com/kstrat2001/darkmux/issues/406)). Each recovery is itself a trajectory event so bail/recovery rates stay visible.
- **Feedback injection is the model-facing half.** Detectors and recovery paths queue synthetic `[darkmux-runtime]`-prefixed `system` messages drained into the next turn's prompt: telemetry the model can act on, not just telemetry the operator reads after the fact. The bracketed prefix is the term-provenance contract (see the model-facing-prompt doctrine in [`CLAUDE.md`](CLAUDE.md)); per-signal wording is overridable per role via the manifest's `feedback_templates`, and the whole channel is disable-able with `DARKMUX_FEEDBACK_INJECTION=0`. The deadline and budget caps (`--max-turns` / `--max-tokens`, opt-in; `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` with a 75% soft warning before the host's 100% hard kill) are the coarse backstops underneath the fine-grained detectors.

The unifying principle is operator-sovereignty applied to the runtime: every detector is observable in the trajectory, every nudge is attributable to a named signal, every bound is operator-tunable, and nothing silently changes the dispatch without leaving a record of why.

## The check-in: observing a stream instead of truncating it

A long model turn needs a point at which the runtime can ask "is this still productive?". Before [#2836](https://github.com/kstrat2001/darkmux/issues/2836) that point was created by stopping the model: the check-in interval was sent as `max_tokens` on the chat-completions request, the endpoint truncated the response at that many tokens, and the runtime inspected what came back and handed it forward as a prefill.

### Why that design failed

`tool_calls` and `content` are separate fields in the response, but they are produced by one generation pass. A cut at N tokens lands wherever the model happens to be. When it landed inside a tool call's `arguments`, the result was JSON that does not parse. Such a call cannot be dispatched, and it cannot be sent back either (an assistant message containing malformed `arguments` causes the next request to fail), so it was discarded. The model's own reasoning, which described the action it had just committed to, was handed back with the action missing. Measured: the model reads that transcript, concludes it has already answered, and stops.

Four runs on the same fixture, before the fix: **9 of 14 check-in firings destroyed a tool call (64%)**. Every one was an `edit` call cut after a single character of arguments. No run passed its verify step. The discards left no trajectory record at all, so a run that lost nine tool calls was indistinguishable, from its own artifacts, from a run that had not used tools.

### The design

The interval is no longer sent to the endpoint. It becomes an **observation cadence**: the runtime already receives the response as a stream, so it reads each chunk as it arrives, accumulates the generated text, and at each cadence boundary runs the same degeneracy check the old design ran after truncating. A clean verdict costs nothing: no truncation, no round trip, no re-sent prefill, and nothing the model can observe. Intervention happens only when the check fires.

| Bound | Sent to the endpoint | Enforced by | Purpose |
|---|---|---|---|
| Check-in interval (`reasoning_checkpoint_interval_tokens`, `generation_checkpoint_interval_tokens`) | no | runtime, per streamed chunk | how often the output is examined |
| Per-call ceiling (`max_tokens_per_call`) | yes, as `max_tokens` | endpoint | backstop against unbounded generation |
| Read timeout | n/a | transport | detects an endpoint that has stopped sending |

Four properties of the runtime-side check are load-bearing:

**The cadence counts characters, not chunks and not tokens.** A staged plan proposed counting chunks, based on a measurement that chunks tracked tokens roughly 1:1 on one engine. Across 61 later calls that ratio ran 0.02 to 1.00 (a 43x spread), because a speculative-decoding engine emits however many draft tokens the verify step accepted in a single chunk. Characters are read directly off the deltas and need no conversion. The token-denominated interval an operator configures is converted using a constant of 4 characters per token, and each call's measured ratio is recorded on `model.streaming.end` so the constant stays checkable. Being wrong there costs cadence, not correctness: a boundary landing early or late only changes how often a healthy stream is examined for free.

**The check is suspended for any call that has begun emitting a tool call.** The degeneracy metric scores JSON at 0.003 against a 0.25 threshold, so a call writing structured arguments reads as maximally repetitive. This is wider than "while the arguments are open" on purpose: once a tool call starts, the check does not run again for the rest of that call. A model that emits a call and then repeats in a long answer is left to the ceiling. Suspending costs nothing; a false positive costs committed work.

**The verdict sees the whole turn, not one call.** A turn is many calls (a check-in continuation deliberately does not consume a turn), and a model re-treading ground from three continuations ago produces calls that each look novel in isolation. The runtime-side check is therefore seeded with the turn's accumulated output before reading the first chunk of the current call. An earlier version judged only the current call and aborted one turn six times in a row while the post-turn check, looking at the full accumulation, returned "continue" every time.

**The runtime ends the stream itself, so the terminal state is synthesized.** `finish_reason` and `usage` arrive only on the endpoint's final chunk, which a runtime abort never receives. The abort therefore sets `finish_reason: "length"` (routing into the existing path that closes the reasoning region and hands the accumulation back) and records the cut source explicitly, because downstream predicates that used to infer "did we cut this?" from a token comparison have no token count to compare.

### Cut sources

Two predicates need to distinguish a bound the runtime imposed from the model's context window overflowing. Both used to compare `completion_tokens` against the cap that was sent, and they resolved an absent `usage` in opposite directions, several hundred lines apart, with nothing naming the difference. They now read a `CutSource`:

| Source | Meaning |
|---|---|
| `None` | the model stopped on its own (`stop`, `tool_calls`) |
| `ServerLength { measured_at_cap }` | the endpoint reported `length`; whether that was our cap is measured, or unknown when no `usage` arrived |
| `RuntimeAbort(Degenerate \| Silent)` | the runtime ended the stream |

The salvage path asks `is_ours_confirmed()`, where unknown reads as *no*: salvaging dispatches tool calls that may have been truncated. The overflow check asks `is_ours_or_unknown()`, where unknown reads as *yes*: the alternative is a hard error that ends the dispatch and discards every banked continuation, and diagnosing an overflow requires a measured count below the cap to diagnose from.

### The per-call ceiling

`max_tokens_per_call` was documented as a failure boundary against runaway generation, set to 10,000 because the only way to notice a runaway was to stop and look. The runtime-side check now looks continuously, and a runaway is repetition, which is what the check detects. The ceiling is therefore no longer the mechanism that notices anything; it is a backstop.

It is raised to **32,000** and left on the wire. Raised, because the largest productive call observed on this workload was 18,875 tokens and the runaway signature the constant was written against is roughly 50,000. Left on the wire, because the endpoint counts tokens and the runtime counts characters: measured across 85 calls, the true ratio of generated characters to `completion_tokens` spans 0.01 to 3.89, so no constant converts one to the other. An attempt to enforce the ceiling runtime-side failed live for exactly that reason: the runtime was still below its character threshold when the endpoint's own cap cut a tool call at 12,000 tokens.

Two premises in the constant's original rationale are also no longer true, and both were arguments for keeping it low:

- *A capped turn's reasoning is discarded entirely.* It is handed back as a prefill and the turn resumes.
- *The cap limits what a turn may spend.* It does not. A turn is many calls, so the same work arrives either way; a low cap only decides whether it arrives as one call or several, each re-sending the accumulated prefill. Measured: 8 calls and 20,000 completion tokens inside one turn.

Raising the ceiling also moved `max_generation_continuations`, which is `(max_tokens_per_call / generation_interval).max(4)`, from 4 to 8. That is past the first point at which the degeneracy check can return a verdict on a verbatim loop (a fixed k=5, since the metric's numerator and denominator scale together). A repeating turn now ends on the repetition that was observed rather than on a continuation budget expiring, which is an accounting fact true of any turn of that length. A regression test asserts that relationship directly, so retuning either constant back into collision fails with a message naming the constant to change.

### The silence guard

Repetition is visible in the stream. An endpoint that has stopped sending is not distinguishable from one that is merely slow, which makes silence the one failure this design cannot detect by reading output. It is bounded by the transport instead.

The HTTP client uses a **read** timeout, not an overall request timeout. The distinction matters: the overall timeout in use previously included body read, so it bounded how long a call could take rather than how long it could be idle. Nothing hit it while the check-in was chopping every call at 1,000 tokens; removing that chopping made a single 18,875-token call plausible, which at measured generation rates is close to the 900-second bound.

An idle stream ends the turn as `RuntimeAbort(Silent)` and the accumulation is handed back. Previously it propagated as an error, which produces a dispatch result of `error` with no envelope, no metrics and no deliverable: every banked continuation of a long turn lost because the endpoint went quiet at the end of it. Genuine transport failures still propagate. The two are distinguished by a typed marker rather than by matching on the error's text, because a text match fails silently when a dependency rewords its message and a failed match is indistinguishable from "not a timeout".

### What the records say

| Event | Log | Field | Answers |
|---|---|---|---|
| `dispatch.tool_call.discarded` | trajectory | `name`, `arguments_chars`, `cut` | a tool call was destroyed, which one, how much was written, and what cut it |
| `dispatch.gate.abort` | trajectory | `slice_chars`, `generated_chars`, `tool_call_in_flight` | the runtime ended a call, what it judged, and how much of that was this call |
| `dispatch.checkpoint` | trajectory (the flow record the host writes from it omits this field) | `judged_chars` | how much text the verdict was computed over |
| `model.streaming.end` | trajectory | `observations`, `chars_per_token` | how many times the stream was examined without being touched, and the measured cadence calibration |

The runtime writes these to the trajectory, not to the flow stream ([Two logs](#two-logs-the-flow-stream-and-the-trajectory)); a reader looking for one in the flow stream will not find it.

`judged_chars` exists because a verdict of "continue" could not previously be distinguished from a check that had nothing to examine. Both appear identical in the record. One turn produced six consecutive `continue` verdicts over **zero characters**: closing the reasoning region switches the examined region to the answer, a model that reasons entirely through the separate `reasoning_content` field never writes an answer, and the check read an empty string from then on. A pass computed over no input is not evidence of health.

`chars_per_token` is recorded only on calls that emitted no tool calls. The cadence counts text and deliberately excludes tool-call arguments; `completion_tokens` counts both. On a tool-calling call the ratio therefore collapses (median 1.58 against 3.93 on text-only calls), and publishing it would suggest the conversion constant is twice too high when it is correct.

### Measured outcome

| | before | after |
|---|---|---|
| tool calls destroyed by the check-in | 9 in 61 calls | **0** |
| share of check-in firings that destroyed work | 64% | 0% |
| verify passing | 0 of 4 runs | 3 of 4 runs |

### Known gaps

- **The non-streaming path is unchanged.** The interval can only come off the wire because the runtime can observe the stream instead; with `--no-stream` there is nothing to observe, so the endpoint-side bound remains the only check-in available.
- **A second degenerate verdict after the reasoning region is closed escalates immediately** ([#2839](https://github.com/kstrat2001/darkmux/issues/2839)). That is correct in that it hands off rather than looping, but it leaves one remedy between "continue" and "stop". Two of three runs in one series ended this way; whether the threshold is too eager is not yet measured at a useful sample size.
- **The conversion constant is a single value for all content.** It is checkable from the records but not yet adaptive.

## Lab reproducibility: fixtures + content hashing

The lab harness only earns the word "measurement" if a run is reproducible. The fixture cluster ([#487](https://github.com/kstrat2001/darkmux/issues/487)) closed the two gaps that made earlier `coding-task` numbers untrustworthy: runs mutating their own inputs, and no way to prove two runs started (or ended) in the same place.

- **Per-run COW isolation.** Each run operates on a copy-on-write clone of the source fixture, never the source. The clone is cheap on COW filesystems (`clonefile` on APFS, `--reflink` on btrfs/xfs/zfs) and falls back to a deep copy elsewhere. The provider trait is unchanged: providers see a sandbox path and don't know it's a clone. This eliminated the cross-run baseline drift observed in earlier lab runs.
- **Content hashing as proof, not policy.** `baseline_hash` (source state at clone time) and `final_hash` (post-dispatch sandbox state) are BLAKE3 over a deterministic walk that excludes derived dirs (`.git`, `node_modules`, `target`, `__pycache__`, `.darkmux-runtime`). Determinism is the point: same content + same layout → same hash, independent of mtimes or inode order. Equal `final_hash` across two runs is the strongest reproducibility signal the lab can emit. Hashing is best-effort: a failure logs and records `null` rather than aborting the dispatch.
- **Registry, not embedded sandboxes.** A fixture is an operator-owned directory with a `.fixture.json` manifest; the registry (`lab-registry.json`) is a name→path lookup plus integrity metadata. `lab fixture register`/`lab fixture unregister` never move or delete the directory (operator sovereignty: `lab fixture unregister` drops the *entry*, full stop). Workloads bind to fixtures abstractly via `requires_fixture: "<name>@<version>"`, resolved against each fixture's `satisfies` declaration. `lab doctor` makes drift detectable offline before a dispatch is wasted on it.

## Residency: a pure planner, leases and an owned namespace

darkmux began as the thing that loads the right models at the right context. That capability is now internal, and it is built so that "why did darkmux unload that model?" always has an answer.

**The planner is pure.** `darkmux-gestalt` takes facts in and returns a `Plan` out: the desired state (crew staffing needs), the observed residency, the resource facts, the ownership namespace and the machine's AI RAM budget go in, and a list of `Load`, `Unload`, `Reuse`, `Reconcile` and `Block` actions comes out. Each action carries a typed `Reason` and a machine-checkable `Precondition` that the executor re-verifies immediately before acting, because facts are snapshots: drift aborts the action and replans. Everything outside the `ports` module does no I/O, reads no clock and no environment, and orders its data deterministically, so a behavior is one `assert_eq!` table row. The seam to the world is two traits, `ModelHost` (load and unload, each with a bounded `Deadline`) and `ResourceProbe`. Two more pure surfaces sit on the same facts: a wave scheduler (`waves`) that realigns parallel and sequential dispatch to the budget, and an architecture-aware footprint estimator (`ArchEstimator`), because "4B does not mean 4GB".

**Ownership is absolute.** Every planned load, unload and reconcile targets only `darkmux:*` instances (the one other case is a placement that itself names an explicit alias, which `decide_residency` treats as owned once the placement names it). A resident darkmux did not load is user state: visible to the planner as pool consumption, and structurally unnameable in an action, since `OwnedTarget` has no constructor for a foreign identifier. When user state blocks a need, darkmux surfaces a reason naming the blocking instance and suggests; it never touches. A dispatch to a local instance puts the namespaced identifier on the wire, not the bare model key: under deliberate co-residency (a `darkmux:foo` beside a user-loaded `foo`) LM Studio's own resolution of a bare key across several residents is undocumented, and a user-loaded copy has unknown load configuration, which is how a confidently truncated response looks like a correct one. The trade is stated: the identifier exists only while darkmux's instance is resident, so a dispatch fails loudly (LM Studio answers a hard 400) if the instance disappears between the residency preflight and the call, instead of silently loading a fresh copy at LM Studio's default context. A remote endpoint dispatch, and one pointed at a non-LM Studio URL, never routes through this and stays bare.

**Leases give the planner liveness.** A planner plans on residency (`lms ps`), never on what another darkmux process is actively doing, so an exclusive reconcile from one command could unload a model a concurrent command is mid-dispatch on (a CI review runner overlapping a local dispatch). `darkmux_types::residency_lease` is the missing input: one JSON file per live process at `<darkmux-home>/residency/<pid>.lease`, holding the models the process desires and the subset it has confirmed loaded. The leases feed the planner's `AcquireOpts.pinned`, so a reconcile never touches a busy resident. Only a lease whose pid is verified to still be its writer counts, so a lease orphaned by a crash under a reused pid never names the wrong process.

Known gap: the sibling protection is seeded only from pins that pass a bare `darkmux:` prefix test, so any pin naming a non-`darkmux:` alias never enters the claimed set ([Seat classes](#seat-classes-every-step-says-what-it-consumes)).

## Thermal and power governors: pause, never kill

A long local run heats the machine and can drain a battery, and both need a response that loses no work. The response is a **rest**, not a stop.

**A pace file carries the decision.** The host samples the OS thermal state once per tick from the dispatch's host sampler, and the governor (`thermal_governor` in `darkmux-crew`) decides whether the in-flight execution should rest. It writes `pace.json` into the execution's out-dir, not the workspace (a crawl mounts the workspace read-only, and a coder run's workspace is the operator's own repo), and the runtime reads it at each turn boundary (`runtime/src/pace.rs`) and rests in bounded increments with its full conversation in memory. Nothing is killed and no checkpoint round trip is needed for the common case. Budget waits are the host-side twin of this pause (`darkmux_types::run_pause` extends the run's wall-clock bound by the time spent waiting).

**A five-tier ladder, from the operator's overnight-crawl experience.**

| Tier | Condition | Response |
|---|---|---|
| 1 | nominal | no delay |
| 2 | `fair`, sustained | duty-cycle: a turn delay rides the pace file |
| 3 | `serious` | full pause until back to the resume threshold, then resume with the delay doubled for the rest of the run (a one-way ratchet) |
| 4 | the Nth `serious` episode (`episode_threshold`, default 2; `0` is unbounded) | an indefinite pause that resumes only on operator intervention |
| 5 | `critical` | the breaker: a crawl's `STOP` file so no further unit dispatches, and the managed models are ejected once a turn boundary is reached or a short bound elapses |

An episode is a transition, not a sample, so a long stretch at `serious` counts once. A paused execution resumes with `darkmux dispatch <role> --resume-from <out-dir>`.

**A pause needs an active writer.** The runtime honors a pause only while the pace file is fresher than `max_pause_ms`, with no per-reason exemption: a `thermal-critical` stop gets no more lenience than any other. "Indefinite" is therefore expressed as "someone keeps renewing it", and the governor re-stamps the file every `max_pause_ms / 4` for as long as the state holds. A dead host writer cannot leave a container paused forever, and a torn read of the file never releases a live pause. A gap in thermal readings mid-pause is treated as time passing with no new information.

**A threshold comparison must not degenerate at its knob's end value.** That rule is what six review rounds on the ladder kept finding: a comparison that is unsatisfiable at one setting kills the behavior it gates, and one that is tautological kills the complement. The severity bands are validated types (`thermal_bands`), so an unsatisfiable or tautological tier is unrepresentable in its severity comparisons, and the soft tiers refuse to run without a usable band. The rest of the rule is covered by tests rather than types.

**The battery policy enforces what the operator wrote.** `power.min_battery_pct`, `power.refuse_start_below_min` and `power.pause_running_below_min` are two decisions with different costs: whether a run may start below the floor, and whether one in flight pauses when it crosses. A pause uses the same pace file, with the reason `battery`. **A machine with no battery is never gated**: absence is `None` all the way down, never read as 0% or as 100%, because in this fleet the always-on hub is a desktop and the battery-bearing laptop is the inference peer, and a misread would either block the hub forever or disable the gate on the one machine it protects. Every refusal names the observed charge, the floor and the config field that decided; darkmux describes and enforces the operator's threshold and never advises about battery health. A run type that cannot resume from a pause refuses to pause and says so, rather than pausing into a state it cannot leave.

Known gap: the governor acts on what the OS probe reports (a thermal state and a CPU speed limit), and with no reading it has no new information to act on, so it does not throttle a machine it cannot read.

## Seat classes: every step says what it consumes

A mission is a graph of steps, and the scheduler has to decide, for each one, how many of its siblings may run alongside it. That decision needs one fact: what does this step consume?

For a long time the question was asked in a form that could not carry the answer. A step kind was asked for an *optional model placement*, and the default answer (the one every kind got for free by never implementing the hook) was "none". But "none" was three different facts wearing one word: *this seat is on an endpoint darkmux does not manage*, *this step speaks to no model at all*, and *this was supposed to be a local model and resolution broke*. The scheduler could only act on the word, so it treated all three as unmanaged endpoints and bounded them by the one cap that existed to respect a hosted provider's rate limit (then a machine-wide `remote.concurrent_cap`, retired in 5.0, #3035).

That cap was 1 on a mission launch. So six `procedural.shell` steps waiting for six independent things (none of which involves a model, a network call, or a rate limit) ran strictly one at a time, each waiting out the previous one's full timeout. A nine-minute window became fifty-four minutes, and nothing in the run's records said why: six steps with the same start second, and one `sleep` running at any moment.

The fix is not a special case for shell steps. It is making the question answerable. A step kind now declares a **seat class**, and there are four:

| Claim | What it means | How it is scheduled |
|---|---|---|
| `LocalModel(placement)` | needs this model resident locally | gestalt plans a wave for it; it holds a residency lease so a concurrent darkmux command cannot pass-1-evict its model as not-desired, whether that command is running in ANOTHER process or is a same-process sibling dispatch (#2663): a live sibling's model at the SAME model key but an insufficient context is protected too (#2669), and two siblings that are BOTH merely racing to acquire the same identifier (neither loaded yet) no longer mutually Block each other either (#2672): the placement Blocks (with a bounded hold-not-fail retry, gated to the genuinely clearable case) instead of unloading it out from under the sibling. Qualifier: this protection is seeded only from pins that pass a bare `darkmux:` prefix test, so ANY pin naming a non-`darkmux:` alias identifier never enters the claimed set (#2672 CONSIDER 6, a known residual gap) |
| `UnmanagedEndpoint(slot)` | a call to an endpoint darkmux does not manage | bounded by that endpoint's own `limits.concurrent_calls`, one at a time when it declares none (said once per launch); calls to different endpoints never wait on each other; consumes no local pool |
| `NoModel` | dispatches nothing at all | bounded by `runtime.dispatch_free_concurrency` (default 8), and per command by `runtime.step_command_timeout_seconds` |
| `LocalModelUnresolved { reason }` | meant to be local; the placement would not resolve | runs one at a time, exactly as before, but loudly, naming the step and the reason on stderr and in the flow stream |

Four properties are worth stating, because each was chosen against an alternative.

**The endpoint is part of the claim (#3035).** `UnmanagedEndpoint` carries an `EndpointSlot`: the endpoint's id (an inline one, by its URL) and its declared `limits.concurrent_calls`. The executor runs one batch per endpoint, each as wide as that endpoint declares, and one at a time when it declares nothing: darkmux never guesses a number, and it says once per launch which endpoint is running serially, because that is the setting an operator would change. Serial means within ONE darkmux process: two missions, radio, or a fleet job against the same endpoint at the same time are not serialized together (declaring `concurrent_calls` does not make them so either; the provider's own limit is the only bound across processes). A machine-wide hosted cap could not say that: one number for every endpoint was right for none of them. A managed endpoint has no slot, because it is not an `UnmanagedEndpoint` claim at all: its parallelism is the scheduler's (the `LocalModel` wave and the backend's parallel slots), and `concurrent_calls` is refused there.

**There is no default.** The hook is required, with no body to inherit. A new step kind does not compile until its author says what it consumes. This is the same reason the fourth claim exists at all rather than folding back into the second: the old fail-open was *correct behavior* for a genuinely remote seat and a *silently lost safety guarantee* for a broken local one, and one return value could not tell an operator which had happened. Now the run's own records say.

**Extension is a variant, and the compiler finds every site.** Adding a fifth class is a new enum variant; the places that must handle it (the executor's partition into tracks, the label stamped onto the record) are exhaustive matches with no catch-all arm, so they fail to compile until they are updated. The old shape had the opposite property: a new case fell into an existing arm and behaved like something it was not, which is exactly how this bug lived.

**The caps stay separate.** It is tempting to make the dispatch-free track unbounded: nothing there is rate-limited by anyone. But `mods.gate` runs an operator-supplied test command per mod, and "as many test suites at once as the graph happens to contain" is a real machine load, not a free lunch. It gets a generous default and a visible knob, which is the same shape every other bound in darkmux has.

## Phase status: a set of tasks is not one unit of work

A mission graph has two levels that look alike and are not. A **task** is one unit of work: a chain of steps that either did the thing or did not. A **phase** is a *set of independent tasks* that happen to run together.

For a long time one predicate served both: *any `Error` wins; else any `Running`; else all-`Complete`; else `Abandoned`; else `Planned`*. At the task level that is exactly right: a task whose second step failed is a failed task, and no amount of earlier success changes it. Applied to a phase, the same words say something false. A review mission's Review phase read **ERROR** while one of twelve unit tasks had errored, seven had completed, and four were still running. The phase was not failed. It was not even over.

The envelope collapsed the same way from the other side: `errored > 0` became `Abandoned("N step(s) errored")`, so a debrief called a phase with eleven successes "abandoned".

The fix is not a softer threshold. It is asking a question a set can answer:

| The set | Status |
|---|---|
| any task running, or planned alongside terminals | `Running` |
| all planned, or empty | `Planned` |
| all complete | `Complete` |
| terminal, some complete | `Degraded` |
| terminal, none complete, some errored | `Error` |
| terminal, none complete, none errored | `Abandoned` |

Task level is untouched. `Degraded` is the word the mission vocabulary already used one level up, and it means there what it means here: **real output was produced, something was lost.**

Five properties are worth stating, because each was chosen against an alternative.

**`Degraded` is terminal, and it closes the phase on disk.** It drives `lifecycle::phase_complete`, exactly as `Complete` does. This looks like the bug returning (the CLI counting a mixed phase as complete), and it is not the same thing. The phase *is* finished and it *did* produce output; a lifecycle that reopened it would be lying in the other direction. What changed is that the reporting surfaces can tell the two apart. Where the lifecycle needs one bit (is this phase still consuming a seat?), the operator needs the distinction, and those are different questions that were being answered by one field.

**The counts travel with the status, as text.** `Degraded` alone cannot separate eleven-of-twelve succeeded from one-of-twelve succeeded, and those deserve different reactions. So the status carries `7 complete · 1 errored · 4 running` wherever it is shown, and *shown* means rendered, not `title=`. A tooltip does not exist on a phone, and this viewer is driven from one.

**Every reporting surface gets the distinction, or the fix is half-done.** The first cut taught the graph lens and the envelope about `Degraded` and left `mission status` and `mission debrief` counting it as plain `complete`. That is not a smaller version of the fix; it is the same defect with the volume turned down: the board moved from wrong-and-loud ("abandoned") to wrong-and-quiet ("complete"), which is worse, because nobody investigates a green board. A status vocabulary is a contract across surfaces or it is decoration on one. That is a claim about the surfaces that *read* a phase outcome, and deliberately not yet a claim about every *producer* of one: `src/coder_phase.rs`'s `finalize_mission_if_complete` still derives each `PhaseOutcome` from the persisted `PhaseStatus` alone (`Complete` maps to `Complete`, anything else to `Abandoned`), so a coder-phase mission's envelope cannot carry `Degraded` whatever its tasks did. Nothing reads wrong because of it (the word simply never arrives from that producer), but a surface can only draw a distinction its producer made, so that one is named, not finished.

**The envelope counts tasks, not steps: the same rule, one level down.** The first cut had the display roll up *tasks* while the envelope rolled up raw *steps*, and they disagreed on any phase whose mix lived inside a single multi-step task: `[Complete, Abandoned]` steps in one task read `Degraded` in the envelope and `Abandoned` in the lens, with `complete` on disk: three words for one phase. Counting steps is the mirror image of the bug this section exists to fix, applied one level too low: a task's steps are *stages*, not independent deliverables, so a half-finished unit is not "output shipped, something lost", it is a unit that did not finish. The envelope now groups by `task_id` first. One divergence is deliberately left: `[Complete, Error]` inside one task reads `error` on one side and `Abandoned` on the other, because `PhaseOutcomeKind` has no `Error` variant: that predates this change, and it is written into the test rather than papered over by forcing agreement.

**An additive enum variant on a persisted shape needs a catch-all in the same change.** `PhaseOutcomeKind` gained `Degraded` and, in the same commit, `#[serde(other)] Unknown`. Without it an older binary reading a newer envelope does not degrade one phase: `serde` fails the **whole document**, because `#[serde(default)]` on the `phases` vector covers a missing field, not a failing element. The lesson generalizes past this enum: on any shape that outlives the binary that wrote it, the catch-all is part of adding the variant, not a follow-up.

## The command gate: darkmux runs your shell-outs, not its own

Some mission configs exist to run a command that changes something outside darkmux: approve a pull request, merge it, apply a deployment. They are ordinary `procedural.shell` graphs an operator wrote, shelling out to a tool the operator already has installed and signed in, exactly like the `lms` and `zed` shell-outs elsewhere in the binary.

**darkmux holds no credentials of its own.** That is the whole security posture, and it is what makes the gate necessary rather than paranoid: darkmux is borrowing the operator's authenticated tool. A config that can run `gh pr merge` on your behalf is a config that can merge a pull request with your identity, and nothing in darkmux authenticated to earn that.

So a config may declare a `cmd` (a name) and darkmux refuses to run it until that exact name appears in the operator's own allowlist:

```json
{ "cmd": { "enabled": true, "allowed": ["pr-approve", "pr-merge"] } }
```

It **fails closed on both counts**: `enabled: false` blocks every declaring config regardless of the list, and a name absent from the list is blocked even when the gate is on. `darkmux init` writes the block visible, disabled, with an empty list: darkmux ships no opinion about which commands exist.

**The gate knows nothing about what it is gating.** It compares one string against a list. It has no model of pull requests, no knowledge of any tool's subcommands, no notion of what "merge" means. That is deliberate: the operator's configs name their own commands, and the operator opts each one in.

<!-- flow-action-guard:allow, a policy field, not an action -->
This is why the field is `cmd` and not `gh_verb`, which is what it was called until schema 3.0. The mechanism was always neutral, but the *name* was not, and a name is what people build on: a GitLab user was allowlisting `mr-merge` under `gh.allowed`, and a config gating `terraform apply` (which wants this gate exactly as much) had to declare a GitHub-shaped field to get a check that has nothing to do with GitHub. Renaming it cost one schema major and zero migrations, because no document had declared it yet. Waiting would have cost both.

**One asymmetry is worth stating plainly, because it drove the migration's design.** The gate fails *open* for configs that declare nothing: a config with no `cmd` is never blocked, which is correct: most configs dispatch models and touch nothing outside darkmux, and requiring every one of them to declare a name would make the gate noise. But it means a config that *loses* its declaration silently loses its gate. So a document still carrying the old `gh_verb` key is refused by name, as a retired key of the [user-file gate](#user-files-the-unknown-key-gate-and-enum-settings) (its message says the key is ignored and the config would run without the allowlist it asked for), never a quiet overflow. An unrecognized field is usually harmless; this one would specifically un-protect the thing it was added to protect.

## Mission configs

A mission config is a JSON document that declares a whole graph as data: `inputs`, then phases holding tasks holding steps. `darkmux mission launch <config>` is the only way a mission graph is made from a config (a one-shot `darkmux dispatch` mints a crew-of-one mission of its own), and the generic launcher (`src/mission_launch.rs`) is the only path a config takes to a graph: it interprets the config, mints the graph, and drives it one phase at a time through the scheduler. The verbs that built missions by hand are gone, and so is every bespoke launcher; `review` and `crawl` are ordinary configs (the shipped four are `coder-phase`, `review`, `crawl` and `machine-status`). What follows is what makes a config safe to author and predictable to run: its step configs are checked before anything starts, a disabled step never exists in the run, a step's output can grow the graph, and a task can declare which failures it survives. `MISSION_CONFIG_SCHEMA` is the config's own semver, and a major bump refuses documents the older shape accepted.

### A step's config is checked

A step's `config` is a JSON object whose keys belong to its `kind`, and each of the fifteen kinds darkmux ships has ONE typed struct (`darkmux_crew::step_config`, `ConfigKind` is the closed list) that is both its schema and the only way the kind reads its config. The unknown-key gate (`darkmux_types::user_files`) derives its schema from that struct: a mission config whose step config has a misspelled, wrong-type or missing key, or whose `kind` is none of the fifteen, is refused at preflight and failed by doctor, naming the file, the key path and the closest valid key, and whatever the gate accepts the kind's own load accepts (`whatever_the_step_gate_passes_the_kinds_load_accepts`). "Load" includes the kind's value rules (`ConfigRules`: a `draws` bound, a diff plan's required `diff_file`, the records a deliver step embeds): the struct owns each rule as one function, the kind's reader and the gate both call it, and the launch runs it again on the static graph with the launch's params substituted (before `--dry-run`'s short-circuit and before any mint) and on each grown copy as it is minted. The launch substitutes params as the mint does, including the `mission_id` it mints before checking, and every refusal after substitution names the step and its kind. What no config alone decides stays a run-time refusal: a role named by neither the task nor the config, a profile-registry endpoint id, a rule id that names no known rule (or a diff-only rule on a tree plan), a path that must exist (a workspace spec, a diff, a plan or an intent file, a workdir), a collection read from a dependency's output, and a deliver step with no embedded `findings` and no `records.gather` output to read. Each crawl kind's own reader is also swept against the gate (`every_config_the_gate_accepts_is_one_from_step_reads`); the kinds that read inside `run` next to their I/O (`dispatch.*`, `procedural.*`, `mods.gate`, `records.gather`, `mission.coder`) are covered only by the `ConfigKind::loads` comparison. A struct is closed to the gate but tolerant at load, because a task's `grow.config` merges every key into EVERY step of its copies (and the scheduler adds `grown_from`); the gate therefore checks a step as its own `config` overlaid with the grow keys ITS kind names, and refuses a grow key no step of the task names.

An open map is declared, never implied: a field that holds free-form data is typed as an open value in the struct with a doc line saying so (`dispatch.map`'s `collection`, `deliver.github_review`'s `findings`/`mods`/`scope`), and every other key of that config stays closed. Numbers and flags are `Count` and `Flag` (`darkmux_types::param_scalar`), which read both `3` and `"3"`, because a `--param` value reaches a step as text and a `{{param}}` reference must pass the gate before it is substituted. Adding a kind means adding its struct and its `ConfigKind` arm; `every_registered_step_kind_has_a_config_struct_and_every_struct_a_kind` fails until both exist.

### A disabled step never exists in the run

A phase, task or step in a mission config may carry `enabled: false`. It is pruned when the run is minted, before anything is interpreted or persisted, so the run's graph is exactly what will execute. There is no gray state: a nightly crawl config with ten plan tasks and six enabled shows six. A task whose steps were all disabled goes with them, a phase whose tasks all went goes too, and a task whose every dependency was pruned is pruned in turn, while one live dependency keeps it and it simply sees fewer inputs. Provenance is the resolved-config snapshot the run already keeps, which carries the flags verbatim, plus a `graph-report.json` beside it naming what was declared, what was minted, and each pruned item's reason; `mission status` prints the one-line count and the `mission.start` record carries the same report. There is deliberately no CLI override. The config is the only place a run's shape comes from: edit the JSON, run, and the snapshot records it.

### A step's output grows the graph

A task may declare `grow` instead of being a task:

```json
"grow": { "from": "plan-task", "items": "units", "id": "{{item.id}}",
          "config": { "unit": "{{item.id}}", "rule": "{{item.rule}}" } }
```

The task is then a **template** and is never minted. After the phase containing `from` completes, the launcher reads that task's last step `output` as a path to a JSON file (the contract every producing step honors), loads it, takes the array at `items`, and mints one copy of the template, with all its steps, per item. `{{item.<field>}}` renders from the item's own top-level scalar fields, into the copy's id suffix and into every step's config; a whole-string placeholder keeps the field's JSON type, so a number stays a number. Zero items mints zero copies, and the phase is explicitly started and completed with `grew_nothing` recorded rather than failing: a phase with no steps is invisible to the step-driven phase open/close, so without that it would sit `Planned` all run and be swept to `Abandoned` by the finalize backstop, recording a failure where the plan simply planned nothing. Every other way this can go wrong (the producer never ran, produced no output, named a path that isn't there, or wrote a shape the template didn't ask for) is a loud error naming the task and the path, never a quiet zero. That is deliberate: the retired `expand` primitive (schema 1.1–1.4) shipped for two schema versions silently expanding to nothing, and the whole point of `grow` is that its input is produced by the run rather than handed in at launch.

**Growth happens at a phase boundary, and that is a real trade.** `run_step_graph` takes its task map by shared reference, so the graph cannot grow mid-run. The generic launcher therefore runs one graph call per phase, in config order, and expands a phase's templates just before minting it. The cost: two phases with no edge between them no longer overlap. That is acceptable because phases are already sequential by design here (`phase_order` and the lazy phase-close logic both assume a strictly linear order), and parallelism lives *inside* a phase, where the wave scheduler still runs every independent task concurrently.

Provenance is on the record, not in the operator's head: every grown step's `config` carries `grown_from: {task, item, index}`, and the item's `rule` lands on `config.rule` through the template. Neither is *rendered* yet (the graph lens builds its step rows without `config`, and `finding list` reads the unit and rule out of a finding's own context), so surfacing them in the viewer, which is what would let an operator filter a run by track, is follow-up. What is readable today: the run's `graph-report.json` gains a `grown` entry per growth event naming the template, the `from` task whose output was read, the producing step's id, the item count and the real task ids minted: `source` holds that step's id and never the artifact's path, since `mission.grow` rides the fleet stream and an absolute host path means nothing on another machine; the artifact stays reachable as the named step's own `output` on its step record; one `mission.grow` flow record carries the same facts live; the phase record's `task_ids` lists the grown tasks alongside the phase's declared ones; and `mission status` prints "grew N task(s) from `<from>`".

### A task's `run_on` decides which of its dependencies' failures it survives

A task may declare `"run_on": ["complete", "error"]`. The default is `["complete"]`: a task becomes ready only once every task it `depends_on`/`reads` reaches `Complete`: the behavior the scheduler always had. A task that adds `"error"` becomes ready once each of those dependencies reaches ANY terminal status: `Complete`, or `Error`, or `Abandoned`. `Abandoned` is folded into the same `"error"` acceptance rather than a literal an operator could name on its own, because a task only ever reaches `Abandoned` as the *transitive* form of some ancestor's `Error`: there is no scenario where accepting one but not the other is what was meant.

The cascade is what makes that transitive form exist. The moment a step's failure makes its owning task's derived status `Error`, the scheduler walks forward over every OTHER task that names it in `depends_on`/`reads`, direct and transitive, and rolls each one to `Abandoned` immediately (in the same pass, not lazily on the next readiness check) *unless* that task's own `run_on` accepts `"error"`, in which case the walk stops there: that task gets a real chance to run, and its own fate (complete, or error and cascade further) is decided only when it actually does. This is why declaring `run_on` on one summary/report task at the end of a chain is enough to unwedge it even when several tasks separate it from the failure: each intermediate task, left at the default, is cascade-abandoned in turn, and the terminal status that finally reaches the summary task is `Abandoned`, which its `"error"` acceptance treats exactly like the `Error` that caused it.

**The walk is scoped to other tasks, never to the errored task's own remaining steps.** A multi-step task whose first step errors already reads as `Error` (a task's derived status is `Error` if ANY of its steps is): its own later, still-`Planned` steps are left exactly where they are; `step_is_ready`'s intra-task rule (each step needs its immediately-previous SAME-task step `Complete`) already keeps them from ever becoming ready, and they are swept to `Abandoned` only by the ordinary close-time reconcile (`lifecycle::reconcile_phase_steps_terminal`), same as any other stranded step on a stopped run, never by the cascade itself. The cascade's whole domain is CROSS-task edges (`depends_on`/`reads`); it does not reach inside the task that actually failed.

Also scoped, deliberately: `cascade_abandon` propagates exactly ONE originating step's id and reason text per abandonment chain (the step whose error triggered that walk), not a running tally across every independent failure a run might contain. Two unrelated tasks erroring independently each start their own walk, and a downstream task reachable from both keeps whichever reason the LATER walk wrote. This is a known, accepted simplification (every review.json scenario today has exactly one true origin per run) rather than a general N-origin aggregator.

A task rolled to `Abandoned` this way is never silently dropped: its `Step.output` carries the ORIGINATING step's own id and failure text VERBATIM (an `"<origin-step-id>: <origin's own recorded message>"`-shaped string), not a generic "depends on X" placeholder that would lose the real reason a single hop downstream. Every task the cascade rolls in one walk carries the SAME origin text, so a task several hops from the actual failure still reads the true root cause directly off its own dependency's forwarded output, without needing to trace the chain by hand. It is persisted through the same `persist` hook every other transition uses, and its status is visible to any reader of the run's step state (`mission status`, the mission-graph lens) exactly like `Complete`/`Error` are. What it does *not* get is a fourth live flow-record action: `STEP_LIFECYCLE_ACTIONS` stays the three-action contract (`[FlowAction; 3]` in `scheduler.rs`: step start, complete and error) the mission-graph lens's SSE matcher is keyed on; the same convention `lifecycle::reconcile_phase_steps_terminal` already uses when a phase closes around a still-live step (same status, same output-names-why discipline, no new live action either).

The built-in `review.json` uses exactly one `run_on: ["complete", "error"]` declaration, on its `deliver` task: when any upstream stage errors, every task between it and delivery is cascade-abandoned, the delivery task becomes ready anyway, and its step kind (`deliver.github_review`) renders a degraded, self-describing payload rather than the run silently producing no comment at all: the graph-native replacement for what used to be a separate fallback render the bespoke launcher ran *outside* the graph when its report step never started.

### Typed step outputs

**Every value one step kind hands to another is a typed serde struct with a `schema_version`**, required fields plain, optional fields `#[serde(default)]`, never a free-form JSON blob and never a string protocol. The consumer deserializes through that struct, and **the read IS the check**: a producer that drifted fails at the read, by field name, instead of being silently summarized as zeros. Required-versus-optional is expressed in the body struct itself, so there is one place to look. Comparing two schemas at CONFIG time is only worth building when composition can wire two different families' outputs together; until then the read is enough.

The body rides in a thin envelope (`darkmux_crew::step_output::Output<T>`), so a consumer knows what it is holding before it looks:

```json
{ "schema_version": "1.0", "kind": "dispatch.unit",
  "producer": { "mission": "crawl-…", "task": "unit-…", "step": "unit-…-step", "machine_id": "laptop" },
  "produced_at": "2026-09-04T…Z",
  "hash": "9f2c…",
  "body": { "schema_version": "1.0", "unit": "u-0001", "result": "stop", "findings": 2, … } }
```

`kind` is a CONTENT id the reader checks against the value it expects **before** deserializing `body`; a mismatch is a refusal naming both, which turns a mis-wired graph into one clear error instead of a confusing field error deep inside a body struct. A **data port's label is the same string as the `kind` its output carries** (`crawl.plan` and `plan.sites` provide `plan.sites`, `dispatch.unit` requires `plan.sites` and provides `dispatch.unit`, `dispatch.summary` provides `dispatch.summary`; `procedural.shell` and `dispatch.internal` provide untyped `text`), so the config's wiring can be checked against the kinds directly, with no rename table in between. The two concepts stay distinct (a port says where a value flows, `kind` says what is in it); they just agree on their spelling. The labels live in one place, `step_output::labels`, which also maps the pre-5.0 spellings (`crawl.plan`, `crawl.unit-outcome`, `crawl.summary`) on read so an archived output still opens.

**The wiring is checked before anything is minted** (#2312, `MissionConfig::validate_with`). For every task, each data port its FIRST step `requires` must be `provides`d by the LAST step of at least one task named in `depends_on` or `reads`, or by the `grow.from` producer. A miss is an error naming both tasks and both kinds, and `mission launch`, `mission config show` and `darkmux doctor` all run it. The ports are declared on the registered kind, never in the config. `Output::read` still checks kind, hash and fields at run time; this check refuses earlier a config the read would fail anyway (measured: a `dispatch.unit` grown from a `procedural.shell` producer mints, then every unit fails reading its plan with `missing field kind`). `dispatch.summary` declares no requirement because it folds the unit step records the mission already holds; the units are grown at run time, and a task cannot name a grow template in `depends_on`.

**Retired kind ids.** `crawl.unit` and `crawl.summary` were renamed `dispatch.unit` and `dispatch.summary` in 5.0 (#2430). A config naming an old id is refused, naming the new one; it is not aliased. A step record an older run left on disk still reads: `Step.kind` maps old ids to new ones on read (`step_config::current_kind_id`) and nothing writes the old id back.

`hash` is blake3 over the body written in a canonical form, every object's keys emitted in sorted order, all the way down, arrays left in their own (meaningful) order, so field order can never change the digest, and `Output::read` recomputes it and refuses a mismatch. The sort is explicit rather than inherited from `serde_json::Map`'s default `BTreeMap`, because serde_json's `preserve_order` feature makes that map insertion-ordered and cargo unifies features across a workspace: in darkmux's own tree `agent-client-protocol` enables it, which made the digest stable under `cargo test -p darkmux-crew` and unstable under `cargo test --workspace` until the canonicalizer sorted keys itself. A hash whose value depends on who else is being compiled is not a hash. The reason is not tampering: a consumer must be able to tell a **complete** file from a partial one, and a **stale** copy from the current one, whatever moved it there. A length check cannot and a timestamp lies. Bodies (and plan files) are written once via tmp + rename and never rewritten, so a body whose hash disagrees is a truncated write or a copy that is not the one this run produced. A synced or shared filesystem (iCloud, a network share) is **never** the transport for a `ref`: those deliver partial files as ordinary reads, which is exactly the case this check names. When `body` lives in a file, the hash is of that file's body bytes.

A step's `output` is a string, so `Output::read` accepts an inline envelope, a `{"ref": {"path": "…"}}` pointer, or a bare path (the shape `crawl.plan` still writes). The grow seam's `items_from_artifact` looks inside `body` when it finds an envelope and at the top level when it does not, so a producer that does not wrap keeps working.

**Who wraps today:** the crawl kinds (`crawl.plan` → `Plan`, `dispatch.unit` → `UnitOutcome`, `dispatch.summary` → `CrawlSummary`), `plan.sites`, and review's `records.gather` and `deliver.github_review`. The coder-phase kinds and `dispatch.*` steps do not wrap yet. Fleet transport comes after the wrapper exists everywhere: once every producer wraps, a `ref` can name a MACHINE as well as a path and be fetched from the producing machine's daemon, with the hash as the completeness check on arrival. No body struct changes when that lands.

**The drift guard is the exported types**, not a second hand-written schema. These structs derive `ts_rs::TS` behind each crate's `ts-export` feature and export into `ui/src/types/generated/`: the same generated file the viewer already consumes and CI already diffs (`bun run types:check`). No `schemars`, no hand-written zod: one definition in Rust, one generated TypeScript file, one `git diff --exit-code`.

## The shared run lifecycle

"Has this run started, is it running, waiting, finished, or gone quiet" is asked by the fleet card, the activity timeline, the run page, the mission graph's step meter, playback, and the daemon's `/runs`. It has one answer: one rule set with two executors, `ui/src/lib/lifecycle.ts` for the viewer and `crates/darkmux-serve/src/run_lifecycle.rs` for the daemon, judged by one corpus, `tests/lifecycle/cases.json`. The two share no code (one is TypeScript, one is Rust), so the corpus is the contract: each case is a set of records, an as-of instant and the phase and status every surface must state.

The rules, in the order they apply:

1. **Attempts.** A session's records segment into attempts in time order. An attempt opens on its first opening record (a bookend start at either grain, a `budget.wait`, `mission.start`, `step.start`, or, when nothing opened yet, a turn, heartbeat, tool call or rest). A record of an execution joins the latest attempt of that execution, so a `dispatch.map` session holding several keeps each one's close apart; any other record joins by mission. A second bookend start in an attempt, or a reopening record after it closed, starts the next attempt (a relaunch under one id).
2. **Close.** An attempt closes on its earliest closing record. A close stamped before anything opened (clock skew across machines) closes the first attempt that has none and is marked skewed.
3. **Outcome.** How it ended comes from the attempt's bookend terminal when it has one, so a `session.end` that lands first does not erase a clean `run.complete`.
4. **Waiting.** An open `budget.wait` holds the attempt live until its announced resume time plus a grace, then the staleness clock runs from there.
5. **Stale.** An open attempt silent for longer than the window, or superseded by a later attempt of its own mission, has stopped with no ending recorded. Another mission's later attempt on the same session does not supersede it, so a session two missions share is two runs (#2125). A session names its run, so new records cannot share one that way; the rule stays because archives, and peers on an older darkmux, hold sessions that two missions launched from one config shared.

Presence is the one input the daemon lacks. In the viewer it can only add: it holds a session's current run open against the staleness clock, never one a later attempt superseded, never one that has not started, and never against a record that closed it.

The window is twice the inactivity budget (`darkmux_serve::runs::stale_after_ms`, from `runtime.inactivity_timeout_seconds`), and the wait grace is a fixed minute. `/runs` publishes the numbers it judged by as `policy`, and `/health` publishes the same object as `lifecycle_policy`, which is where the viewer reads them, so no client hard-codes a threshold. Every other surface is a projection of this lifecycle, not a second judgment.

Known gap: the daemon cannot judge a run held open by presence, so the corpus marks that one input as viewer-only.

**The CLI reads the same derivations.** `darkmux run list` is the union of mission, dispatch and lab runs (the CLI twin of `GET /runs`, folded from the same records by the daemon's `build_runs` family), `darkmux mission status` is the read-only board that surfaces drift and suggests the reconcile commands without running them, and `darkmux mission show <id>` prints one mission in full from one derivation: the config it was launched from and its declared inputs, every phase, task and step with status, tokens, turns and model, its runs, its total tokens and a viewer link. The editor panel's `/mission show` prints the same thing, and its `--json` is a semver-bound shape (`MissionShow`).

## Crawl as a mission: the shapes and how data flows between them

The crawl (#2297) is an ordinary mission config on the generic launcher: a plan step per rule, a grown set of unit tasks, a summary, and an optional create-mods phase. This section is the map: every record the crawl touches, its shape, who writes it, who reads it. Where a piece is not built, it says so, so a reader can tell design from delivery.

### The two documents: config is the shape, plan is the instance

**Mission config** (`templates/builtin/mission-configs/crawl.json`) is the shape of the work and is the same file for every crawl of every repo. It declares `inputs` (the workspace spec path, the rule ids, sizing knobs), and phases holding tasks holding steps. The `plan` phase holds **one task per rule**, explicitly, each with a single `crawl.plan` step:

```json
{ "id": "plan-unnamed-predicate", "enabled": true,
  "steps": [{ "id": "plan-unnamed-predicate-step", "kind": "crawl.plan",
              "config": { "rule": "unnamed-predicate", "workspace": "{{workspace}}" } }] }
```

A task with `"enabled": false` is pruned when the run is minted and never exists in the run (see [A disabled step never exists in the run](#a-disabled-step-never-exists-in-the-run)). N rules are N tasks; a nightly that wants a subset disables the rest; the run shows only the live ones. The config is the only place a run's shape comes from: no CLI override, edit the JSON and run, the snapshot records it.

**Plan** (`<missions>/<mission-id>/plan/<rule>.json`, plan schema 1.1) is the instance data for one run of one rule, and it is a **step's output**, never a mission input. Its shape:

```json
{ "schema_version": "1.1", "workspace": "darkmux-ui", "planned_at": "…",
  "rules": ["unnamed-predicate"],
  "params": { "max_sites_per_unit": 40, "max_est_tokens_per_unit": 16000 },
  "sources": [{ "id": "app", "sha": "20c7750…", "ref": "main", "tree": "…", "files_walked": 412 }],
  "units": [{ "kind": "site", "id": "u-0001", "rule": "unnamed-predicate", "source": "app",
              "sites": [{ "file": "src/x.ts", "line": 64, "start": 44, "end": 84, "hits": [64] }],
              "est_tokens": 3900 }],
  "totals": { … } }
```

The test for which document a field belongs to: would you change it without changing what the run is about? Sizing knobs, model, profile, rule ids are config or launch parameters. Units, sites, sha are plan. `rules` and `params` ride the plan so it is self-describing for later comparison, not so anyone edits them there.

### The `crawl.plan` step kind: one kind, never one per rule

Control flow: load the workspace spec, materialize it (bare mirror plus checked-out tree, per source, at a recorded sha), run the rule's prefilter over the files its globs admit, cut a window around each hit, pack sites into units under the sizing knobs, write the plan, hand the plan's path downstream as the step's `output`. That flow is new, so by #1352's test it is a kind. The windowing and packing half is the Tier 2 `plan_sites` pattern (`crates/darkmux-crew/src/step_kinds/patterns/plan_sites.rs`), which crawl plugs a tree walk into (`TreeSource`) and review's `plan.sites` step plugs a diff's hunks into (`DiffSource`); the survey, the prefilter and the materialization stay in the crawl module (`crates/darkmux-lab/src/crawl/plan_step.rs`). Materialization itself is serialized and self-checking (#2399): a `materialize` call takes an advisory `flock(2)` on `<root>/.materialize.lock` and hands the guard back inside the `Materialized` value, so the workspace stays held for as long as the caller reads the trees it named: the 8-wide `plan.sites` steps #2397 made concurrent therefore take one workspace in turn rather than tearing each other's trees down mid-walk (a step that dropped the value early would see missing files recorded as `skipped`, under-reporting its own coverage), and a tree already checked out at the resolved sha with nothing modified is reused rather than rebuilt. A mirror that already exists is verified bare and pointing at the spec's own origin before anything fetches into it; one that fails is moved aside to `<mirror>.corrupt-<unix-ts>`, announced on stderr, and re-cloned, unless `--no-fetch` means no re-clone could follow, in which case it is left untouched and the step refuses. `darkmux doctor` lists what has been quarantined.

Rules vary in **what a site is and who finds it**, not in how planning runs, so there is never a kind per rule. The site producer is a mux keyed by the rule's declared `prefilter` shape:

| shape | today | example |
|---|---|---|
| `["<regex>", …]` | implemented; the planner compiles and runs it | `unnamed-predicate`, `swallowed-error` |
| `{"command": "…"}` | **reserved, refused at rule load by name (#2297)** | semgrep, ast-grep, a linter emitting SARIF/JSON |
| none | whole files, sized by tokens | `read`-kind rules |

Semantic rules with no cheap prefilter (`doc-contradicts-code`) are the part a linter cannot do and the model is for. When the command shape lands, it is a `procedural.shell` step ahead of the plan step whose output is a site list; the plan step consumes sites in one shape regardless of who produced them.

### How data flows, phase by phase

```
crawl.json ──prune(enabled)──▶ minted run: plan phase, one task per LIVE rule
                                  │  config-snapshot.json (declared), graph-report.json (what was left out)
                                  ▼
  crawl.plan (per rule) ──▶ plan/<rule>.json ─── output = path ──▶ unit tasks GROWN per plan unit,
                                                                     each tagged with its rule (a TRACK)
                                                                             │
                              crawler role reads the rule's match/no_match/evidence text, one unit per task
                                                                             ▼
                                              create_finding ──▶ dispatch.tool record (payload.emitted, emit_seq)
                                                                             │
                     ┌───────────────────────────────────────────────────────┼─────────────────────────┐
                     ▼                                                       ▼                         ▼
      hook rule → jq transform → external tracker           finding sync → ~/.darkmux/findings/    create-mods step per finding (OFF)
      (metadata + emitted, destination owned by the hook)   <execution_id>/<seq>/finding.json  (brief_refs: [{finding, key}])
                                                                             │
                                                        dispatch --finding / --mod, or a mission step with brief_refs
                                                                             ▼
                                                        create_mod / mod create ──▶ ~/.darkmux/mods/<key>/ (kit + attachments)
```

Every arrow above is built. A task may declare `grow` and the generic launcher expands it at the phase boundary ([A step's output grows the graph](#a-steps-output-grows-the-graph)), which is what lets `crawl.json` declare the whole crawl: a `crawl.plan` task per rule, a `dispatch.unit` grow template per rule, and one `dispatch.summary`. `darkmux mission launch crawl --param workspace=<spec.json>` is an ordinary generic launch. Its inputs are `workspace` (required), `rules` (which rule tracks to mint), `max_sites_per_unit`, `max_est_tokens_per_unit`, `no_fetch` and the generic `dry_run`; a one-shot crawl is a one-source spec file. `--param rules=a,b` prunes the tracks it does not name at mint, with reason `not_selected` in `graph-report.json`, the same mechanism `enabled: false` uses, so a run's graph is always exactly what will execute. Grown unit task ids are unprefixed (`unit-<rule>-<unit-id>`) while declared ids carry the mission-id prefix; that is the shape the phase record's `task_ids` holds. Tracks run in parallel (the scheduler's wave admission places each on the machine's resources, see [Seat classes](#seat-classes-every-step-says-what-it-consumes)), fail independently, and resume alone: per-unit reuse is the scheduler's step-output reuse. A minted step records the plan it came from, so the operator never wonders where a step came from.

The last phase, `create-mods`, has one grow template over the summary's `finding_refs`: one `coder` `dispatch.internal` step per finding, each carrying `brief_refs: [{"kind": "finding", "key": "<execution_id>/<seq>"}]` and the materialized tree the finding was observed in as its `workdir`, and each asked to record its change with `create_mod` naming that same key in `for`. **It ships OFF** (`"enabled": false` on the task, which prunes the task and then the emptied phase at mint), because the hook → tracker path is the default exit. To turn it on, copy `templates/builtin/mission-configs/crawl.json` to `~/.darkmux/mission-configs/crawl.json` and set `"enabled": true` on that one task; `darkmux mission config show crawl` reports the gate per task either way. Two consequences are worth stating before an operator flips it. First, the enabled create-mods becomes the LAST phase, and the close-payload rule below promotes that phase's last step output only when it is a JSON OBJECT. A coder `dispatch.internal` step's output is the model's final text, not an object, so an enabled create-mods leaves `mission.close` with `payload: null` and the `CrawlSummary` stops reaching it (the summary is still its own step's output on disk, and a copy that wants the payload back ends the create-mods phase with its own summarizing task). Second, a `brief_refs` key that addresses no stored record REFUSES the step, loudly and before any container work, so a create-mods step that outran the finding tailer fails naming the key rather than dispatching a coder that never saw the finding.

### What each record is for

| record | written by | read by | key |
|---|---|---|---|
| `config-snapshot.json` | mint | provenance; `mission status` | mission id |
| `graph-report.json` | mint (prune) | `mission status`; the `mission.start` record carries the same object | mission id |
| `plan/<rule>.json` | `crawl.plan` step | the grow seam (`grow.from`, #2300); `dispatch.unit` (via `config.plan`) | mission id + rule |
| `Output<T>` envelope | every typed producer (#2301) | every typed consumer, through `Output::read` | `kind` + `hash` |
| `UnitOutcome` (`dispatch.unit`) | `dispatch.unit` step | `dispatch.summary`; the run-detail lens (step output) | unit id |
| `CrawlSummary` (`dispatch.summary`) | `dispatch.summary` step | the `mission.close` payload; the viewer's crawl surfaces | mission id |
| `FindingRef` (on `UnitOutcome.finding_refs` / `CrawlSummary.finding_refs`) | `dispatch.unit`, from the same one read of the dispatch's `findings.jsonl` that counts them | the create-mods grow template (`items: "finding_refs"`); `brief_refs` resolution, by `key` | `<execution_id>/<seq>` (and `id`, the same key with `/` → `-`, because a task id is one segment) |
| `grown_from` on a grown step's `config` | the grow seam | the on-disk step record; `graph-report.json` carries the same triple per copy | `{task, item, index}` |
| `graph-report.json`'s `grown[]` | the grow seam (appended post-mint) | `mission status`; the `mission.grow` flow record carries the same facts | mission id |
| `dispatch.tool` record with `emitted` | the runtime, via `create_finding` | hooks (external trackers); `finding sync` | execution + `emit_seq` |
| `findings/<execution_id>/<seq>/finding.json` | `finding sync` (the tailer) | `finding list/show`; `dispatch --finding`; brief refs | `<execution_id>/<seq>` |
| `mods/<key>/mod.json` + `attachments/` | `mod create`; `create_mod` | `mod list/show`; `dispatch --mod`; the integration mission | minted `mod-<secs>-<hex>` |

The close payload is a **generic** rule, not a crawl one: the launcher promotes the LAST phase's last step `output` to the `mission.close` payload whenever that output is a JSON object (unwrapping an `Output` envelope's `body` when it is one). Any config whose final step emits a JSON object has a payload, so read the config id, never infer the crawl shape from a payload's presence.

## Findings and mods: what was observed, and how it could change

This design was settled after the first crawl findings reached a tracker and an agent that knew nothing about crawls made the first PR from them.

### darkmux is the worker

Imagine the tracker is GitHub. The orchestration layer above darkmux knows its job is to post a PR with every change that came out of crawl X. darkmux does not: it is the worker. It is asked to observe (a crawl, a review, a one-off dispatch with the right tool granted) and it is asked to make a change for a given observation, and it records both. It never reads a tracker, never decides which observations deserve a change, never opens a PR. Those are the orchestrator's, whether the orchestrator is a frontier session, a person, or a scheduled darkmux mission later on. The reason is modularity: every step of the loop has to be staffable independently, so that a local model can do the generation and thinking and a frontier model does only the packaging, or the reverse when a step turns out to need it.

### Two records, both opaque

A **finding** is what was observed. It is an event: it happened at a moment, in a role execution, and it is never rewritten. Its key is `<execution_id>/<seq>`: the role execution that produced it and the ordinal of the acceptance within that execution, which every finding has, crawl or not. (Two items of one `dispatch.map` step share a task session, so a per-session ordinal would have conflated their seats; the execution does not. A finding filed before executions were named keeps its old address.) A crawl adds context (mission, unit, rule, source, sha) when it launches the dispatch; nothing about a finding requires a crawl. The runtime tool that produces one is `create_finding` (the tool *creates* a record; a hook is what *reports* it, which is why it is not named `report_finding`). darkmux does not interpret the emission: the record is metadata plus the model's arguments verbatim (`emitted`), and a hook's transform composes whatever a destination needs from that. A finding's location is domain-specific (a line for text, a page for a PDF, a rect for an image) so no field for it exists on darkmux's side.

A **mod** is how something could change. It is a *kit*: instructions plus data, in whatever form the proposer chose (a diff, a sentence, pixel data, a config value), enough for an AI to make the change correctly later, given the mod's own context. darkmux never types a kit and never opens it. A mod has its own minted key and its own store; it may carry provenance, `for`: zero or more finding references. That is the only stored link between the two records, it lives on the thing created later, and it is a list, because one change can address three observations and one observation can attract three competing changes. The view from a finding to its mods is derived by scanning mods, never stored on the finding.

Two producers write the same mod record, and both exist: the CLI, for a change made outside darkmux (`darkmux mod create --by <actor> [--for <finding>]... --kit ... [--attach ...]`), and the runtime tool `create_mod`, for a change made inside a dispatch (its emission rides the same `dispatch.tool` record a finding's does, and the host materializes the record from it: attachments included, since no host path reaches the container's copy of the file). Whoever made it, the record names the proposer and the time. A mod is written even when part of it could not be kept (an attachment that did not decode, a `for` key that addresses no finding), with the reason recorded on the mod itself, because the kit is the product of the work and a malformed sibling field is not a reason to lose it.

### Why the key is minted per mod

Two agents review the same finding at different times. One proposes the code change; the other recommends a comment. Both are valid; they may overlap, conflict, or compose. The record keeps both, judges neither, and leaves the question to whatever integrates them. A key derived from the finding would have made the second overwrite the first.

### Verbs, and what is deliberately absent

`finding list` / `finding show` read the store (the flow stream stays the audit trail; the directory is the queryable copy: JSON on disk is the truth, as with roles). `finding sync` is that store's second producer: it replays the flow stream into the store for anything the live dispatch tailer missed (an older binary, a killed process) and is idempotent because the store is write-once, so the two producers can race without ever disagreeing. `mod create` / `mod list` likewise, and `mod show <key>` prints one mod whole with its kit raw. **Verbatim means byte-exact**: a kit is stored as the text that was written and is never parsed on write, because parsing and re-serializing a JSON-looking kit silently collapses duplicate keys and rounds large integers: a kit is not darkmux's data to normalize. A `for` key is canonicalized to `<execution_id>/<seq>` on create, so one finding has one address; a key that can address no finding is refused rather than stored as a link nothing can follow. `dispatch <role> --finding <key>` appends the finding's stored record to the brief, verbatim, so a role has the *what*; its palette decides whether it may `create_mod`.

Both record kinds reach a brief the same way, and the mechanism is a **step config field, not a verb**. A `dispatch.internal` step carries `brief_refs: [{"kind": "finding"|"mod", "key": "..."}]`, and the step kind that runs it is where each ref is resolved against its store, rendered as a block the model can ground, and appended verbatim after the user's own message, in the order given. That placement is the whole point: the step kind is the single place every producer converges on, so a mission graph that sets the field gets exactly the brief the `dispatch` verb does. The verb's `--finding` / `--mod` flags only write the field (and check the keys early, so a typo refuses before the acknowledgment gate rather than one layer down); the rendering happens once, in one place. A key that addresses no stored record fails the step before any container work, so a dispatch never runs on a silently missing block. A mod ref also bind-mounts that mod's `attachments/` read-only at `/darkmux-mods/<key>/attachments` (the path its block names, from the same constant), after the key is re-validated and the host path is proven to resolve inside the mod store. The refs are **darkmux record kinds only**, never an arbitrary file: the workspace mount is the file channel, and a ref is provenance-bearing by construction. One gap is named rather than papered over: the fleet work job's shape carries no refs, so a `profile@machine` dispatch that names one is refused instead of routed without its blocks.

There is no `integrate` verb. If darkmux integrates mods, that is a mission: a shared workspace, one step per mod applying its kit onto the accumulating change and handing the workspace to the next, a failed step failing itself and not the mission. It composes from existing pieces (a worktree step, shell or coder steps) so the concept lives in a mission config, not in the CLI. Each step names its seat, which is what lets the operator decide, per integration, whether a local model or a frontier model does it.

### What this makes measurable

Tokens on the local seat versus the frontier seat per crawl PR. The frontier-only baseline (a Sonnet agent doing both creation and integration for seven findings, PR #2285) is the number every local-seat experiment is compared against.

## Code review as a second config on the crawl's building blocks

`darkmux mission launch review` is a mission config on the crawl's shared blocks, run by the generic launcher. It was designed while retiring the bespoke review launcher (#2310), whose ten step kinds, ~3.6k-line launcher and funnel document were deleted (P4d) when this config took the `review` id. It is: **the ladder** (plan by rule → detect per unit → confirm in isolation → deliver), **the seat boundary as a hook** (detection is local, writing the patch is a frontier mod the run waits for, bounded, or does without), and **two seats** (a small reviewer seat per unit; a coder seat only for the optional unattended mod). One thing stays unmeasured and is the go/no-go: whether a rules catalog on small seats finds things worth saying. Everything below says for itself what is built.

### What the retirement measured

The review path *was* a funnel: probe the diff k times with an open "find anything wrong" prompt, dedup, judge, verify, synthesize prose, render. Ten bespoke step kinds and a launcher, about eleven thousand lines. Retiring the launcher onto the ten kinds went through three review rounds, and every defect those rounds found lived in the launcher: an errored run rendered nothing, the saved-flags re-judge path rendered nothing, the fallback ignored a document's own emit path. None were in a step. The conformance harness built for the retirement exercises steps, and the launcher sat outside it, so the one path that could be broken by a neighboring change without a test noticing was the bespoke one. That is the measured form of a rule this project already holds: well-tested building blocks execute many patterns; a bespoke path is where defects hide.

Looked at beside the crawl (the section above), the funnel is the crawl's ladder re-derived under other names. A probe flag is a finding. Judge plus verify is confirmation. Synthesis is delivery. The operator's reading: review was never going to work as a wide-open probe; it was always going to be detections by rules, inference to confirm each finding in isolation, and a mod as the recommendation. Gemini Code Assist's rule files land in the same place from the other direction.

### Separate config, shared blocks

**Crawl is separate from code review, and stays general.** A crawl walks any corpus, code or not. Review is code-only and diff-scoped, and those two facts are advantages to exploit rather than a subset to trim crawl down to. Review therefore becomes its own mission config built from the same blocks, and neither config learns the other exists:

- **Planner.** Crawl's plan step does rules times source, then windows, then units. The control flow is the same for review; only the source enumeration differs (a tree walk against a diff's hunks). The planner's source becomes a strategy on one shared pattern. Built in #2353: `step_kinds/patterns/plan_sites.rs` owns the merge-windows-and-pack-units procedure and takes the enumeration as a caller-supplied `SiteSource`, with `crawl::plan`'s `TreeSource` (the tree walk) and `DiffSource` (a diff's hunks) as its two impls.
- **Units.** Already generic: a map step over units with a role that carries the finding tool. Review supplies a reviewer role and its rules files. A draws-per-unit knob, off by default, ports the measured k-draw recall technique from the funnel. Built in #2357: `dispatch.unit`'s `config.draws` dispatches the same unit N times (absent means 1, one dispatch per unit), and a unit's `finding_refs` are deduped on `(rule, file, line)` before anything grows from them.
- **Findings and mods.** The stores, the runtime tools, and the hooks are shared infrastructure today (see "Findings and mods"). Review reused crawl's create-mods phase verbatim until #2310 P4e; it now waits for a mod instead of dispatching a coder to write one (see "The seat boundary is a hook" below), while crawl's own create-mods task is unchanged.
- **Delivery.** Review's own kind: mods and findings in, the GitHub review payload out. Pure render, no model, so the harness covers it. Built in #2353 as `deliver.github_review` (`step_kinds/deliver_github_review.rs`), and hardened in #2365: model text cannot break out of its markdown container, and a run that covered less of the diff than it planned to renders degraded rather than noop. The retired funnel's own report kind was its ancestor.
- **Scheduler.** A delivery task has to run after an upstream error. That is the `run_on` contract (see [A task's `run_on`](#a-tasks-run_on-decides-which-of-its-dependencies-failures-it-survives)), needed by both configs. Built in #2350.

Working hypothesis, deliberately undecided: crawl may turn out to be *the pattern* (plan, detect by rule, confirm in isolation, mod) rather than a general tool, with crawl and review both configs on it. If the planner extraction produces exactly that shape, it belongs in `step_kinds/patterns/` as a named Tier 2 pattern. Decide from what the extraction looks like, not up front.

### What is code-specific, so far

- **The diff carries intent.** A PR body or intent file plus before and after per hunk. Intent-versus-diff is a rule review can run and crawl cannot.
- **Hunks are natural windows.** No prefilter is needed to find sites; every hunk is a bounded site of the right size for a small seat.
- **Changed files name the test targets.** A mod's gate can run the relevant tests rather than the suite, which is what makes confirmation cheap enough to do per finding.
- **The PR is the delivery surface.** One-click suggestion blocks, review tiers, a summary line with honest partial counts. Crawl delivers to a tracker.
- **The tree is the confirmation surface.** The diff is where triggers are detected; the whole worktree, with a search tool, is where they are confirmed. Review units keep both.

### Confirmation is a mod, a search, or a question

The funnel confirmed a finding with a second model pass. Here, confirmation takes one of three forms, and the delivery form says which one happened:

- **A mod.** The finding is confirmed by producing a patch that passes its gate. Mechanical, per #799. Delivered as a GitHub suggestion block where the mod sits inside the diff's lines, a patch comment where it does not. A suggestion block always means "gated patch".
- **A search.** Some findings are an intuition that an architectural issue may exist, whose confirmation is enumerating instances across the corpus, not reasoning inside the hunk: a shared auth middleware changed (correct in intent, but which endpoints use it?), a new string union introduced (does an enum already exist?). The rule declares the search; the unit runs it as a tool call; the delivered thread is the list ("fourteen endpoints use this; confirm each"). This is the review-side neighbor check and the typical human PR comment. The future contextual map of a corpus's relationships is the precomputed form of this search.
- **A question.** "Did you check whether the repo already has this?" with candidates attached. Cheapest to produce, honest about being unconfirmed, and often the comment that changes what the author does next. Findings whose mod could not be built or failed its gate for a reason other than "the finding was wrong" also deliver as a thread: finding key, window, claim, what to check. That thread is an identifiable artifact the orchestrator can escalate by id to a stronger model, or to a person when the claimed severity warrants. Escalation stays outside darkmux (see "darkmux is the worker").

Refused and rejected findings are counted in the summary line and never posted.

### The seat boundary is a hook

Written after measuring the one step above that a small seat could not do. Detection held up: a local seat reading one hunk against one written-down rule produces findings. **Writing the patch did not.** Across three local coder seats given the same create-mod message and the same finding, applying unified diffs came back 1 of 4, 0 of 3 and 1 of 10 (container paths instead of repo-relative ones, miscounted hunk headers, code fences, missing trailing newlines) while a clean-context frontier session given the identical instructions wrote 4 of 4. The gate had already been taught to absorb three of those shapes mechanically; the residue is judgment about what the smallest correct change is, and that is the thing the tier split exists to route.

So the seat that writes a mod moved to the frontier, and the operator's own words name the mechanism: *we already created hooks for this reason.* darkmux does not gain a frontier client, an API key, or a "call Claude" step kind: it would be the one place in the system where darkmux stopped being the worker. What it already had was a sink that fires on a matched flow record, and `create_finding` is a matched flow record. The operator adds one rule to their own config; their orchestrator session sees the fired match; the bundled `darkmux-mod-create` skill sends a subagent to read the finding, open the pinned checkout and write the diff; and that subagent records it with the ordinary `darkmux mod create --for <key>` verb. Every darkmux-side surface in that sentence already existed. **A tier boundary is an integration point, not a dependency**, which is the same reason escalation leaves the system as an identifiable artifact rather than a stronger model being called from inside it.

What review's `create-mods` phase does now is WAIT, bounded, for that mod: a Tier-1 `procedural.shell` poll of `mod list --for <key>` on a ~5s cadence, then the unchanged `mods.gate` and `deliver.github_review`. **A wait that ends with no mod is a clean outcome**, exit 0 with `found: false`: the gate records its ordinary no-mod skip and the finding delivers as a question. That is deliberate and was a review correction: the frontier reading a finding and declining to write a mod is the CORRECT answer for a search-form finding and for one that does not hold (the skill instructs it explicitly), so erroring there would have marked every run that took the design's own advice Degraded. Only a malformed bound and a genuine infra failure error the step. The wait's own bound, `mod_wait_seconds`, **defaults to 0, meaning do not wait**, because the unattended path is real and is not a degraded version of the attended one: the self-hosted runner that runs review on every PR has no orchestrator session to receive the hook and no frontier seat, so waiting there buys nothing and costs a per-finding stall. Unattended runs therefore deliver **detections only**: every finding as a question, honestly labeled. An attended run opts in with `--param mod_wait_seconds=<N>` and gets suggestions for the findings whose mods pass their gate. The same config, two honest modes, distinguished by one number rather than by a second document.

Two consequences worth stating. First, the frontier is now inside the run's latency, which is why the bound is an operator knob and why `runtime.step_command_timeout_seconds` (the bound every step command already had) stays the outer limit: a wait that outruns it is killed by the step bound and errors naming that, not `mod_wait_seconds`. Second, `crawl.json` keeps its coder dispatch. Crawl's findings are proposals over a read-only corpus with no test target implied and its create-mods task ships off by default; review's are diff-scoped with the changed files naming the tests. The two configs diverged here for the reason the section above predicted they would diverge anywhere, because what is code-specific about review is real.

**The unattended seat (#2310 P4f).** The wait needs a session watching the hook, and the runner has none, so for the unattended path the answer is not a shorter wait, it is a different SEAT. What the measurement actually found was a TIER boundary, not an attendance one: a frontier-class model wrote applying diffs and the small local seats did not, and a hosted frontier-class endpoint is a seat a runner can staff. So `create-mods` ships a SECOND grow template beside the first, `create-mod-dispatch`: a `coder` dispatch on an endpoint profile named by a new `mod_seat_profile` input, off by default, carrying the create-mod message `crawl.json` carries: byte-identical again, and pinned as such by a test, because it is the same job specified for a different tier. The two templates are mutually exclusive through a new schema-3.5 `excludes` field: exactly one may be enabled, both is a validate-time Error naming both, and the dry-run graph shows whichever is live because the other is pruned at mint. This does NOT give darkmux a frontier client: an endpoint profile is operator config the profile-uniformity contract already covers, and the dispatch is the ordinary `dispatch.internal` step kind reading the ordinary `profile_name` override; nothing here knows what a frontier is. Two limits stated rather than papered over. The seat sends the finding and the source it names to a third party, so it is for public repositories only: work under a client boundary keeps the local seat and the attended hook. And the agentic container path (#1187) is metered like every other path: the endpoint's `limits.tokens_per_dispatch` settles each model call as it lands (a turn that reports no usage is charged the granted per-call cap), and its rolling `limits.window` pauses the run between turns. With neither set, the inactivity budget and the turn cap are its only ceilings; the guide says so in the same breath as the recipe for measuring what the seat actually costs per diff, which is tokens read off each `dispatch.complete` record, never currency.

### A rule is a procedure, because a small seat has no intuition

A small model will not think "this might exist somewhere else". The rule file carries that idea as a standing procedure, so the model never has to have the intuition:

1. `detect`: a concrete shape to recognize in the hunk (a new function or type whose name or body matches a utility class; an enum-like set of string literals; a shared symbol changed).
2. `search`: a recipe the unit runs verbatim (grep and symbol search for the same verbs and nouns; existing enums with overlapping members; the package manifest for a known library).
3. `compare`: the only inference, bounded ("does candidate X do the same job as the new code? yes, no, partly, one line why").
4. `deliver`: mod, list, or question.

For "a well-known package does this", the model's own knowledge is the unreliable part, so a rule reads per-repo data instead: a curated known-solutions list, operator-maintained, grown with every catch. The rules directory is where a codebase's tribal knowledge lives, and it is the review's quality lever: every shipped defect becomes a rule, a noisy rule is one flag from off, and the review gets better monotonically as standards are written down. The hit rate per seat is a measurement, not a promise; the conformance fixture should plant a re-implemented helper and a union with an existing enum, and the go/no-go run reads what each seat does with them.

### The honest limit

A rule-shaped review is narrow by construction: very good catches, rarely broad. A good comment on a big diff can need architectural knowledge that no window carries; out of context is out of knowledge, for a local seat and a frontier model alike. Two consequences. The review's summary must state its scope (rules run, windows covered, what it did not attempt) so a narrow review never reads as complete. And breadth stays where the two-tier review policy already puts it: the local rule-based review is the always-on first tier that logs onto the PR; the frontier gate keeps the architectural read.

### History: how the review config was built

The order was: the `run_on` scheduler contract first, because both configs need it (#2350); then the planner source extracted into a strategy and the model-free deliver kind (#2353); then the review config with its first rules catalog (intent-vs-diff, existing-solution, shared-symbol-callers, union-vs-enum, swallowed-error, unnamed-predicate, test-gap), each rule carrying a scope and one of the three confirmation forms (#2354); a declared input substituting into any step config at mint, which is how the launch's `intent_file`, `test_command` and `draws` reach a grown step (#2355); and the mod gate, the draws knob and the gather-and-deliver task that turns a run's own stores into a GitHub review payload (#2357).

Then it ran for real, which is the part no amount of golden testing substitutes for. The first live run planned and dispatched the catalog against a live diff end to end and found three seam defects nothing in the suite could have: per-rule plans collided because every plan named its first unit `u-0001` and they shared one on-disk home; the delivered scope line miscounted what the run had covered, reading a completed step as a covered hunk and carrying container paths out to the host; and a run under a relocated `DARKMUX_HOME` wrote its flow and audit records into the real `~/.darkmux`, a config-wide leak the review config happened to be the first thing to trip. That is why a live run, not a green suite, gates a release. The deletion of the bespoke launcher (P4d) left every frozen `--param` name declared, the ones with no consumer marked `ignored` rather than dropped, so the self-review workflow's command line is unchanged, and `plan.sites` learned to derive its own one-source workspace from `github` plus `head_sha` so a runner with git credentials and no workspace spec plans against the checked-out head. The catalog's own hit rate, whether a small seat with a written-down procedure says anything worth reading, is the measurement still owed, and it is the go or no-go.

## Multi-machine substrate

Single-operator multi-machine is the design target. The operator owns a couple of Macs on a tailnet they control, and darkmux makes them function as one development environment without becoming team tooling. The substrate is four things: a daemon on each machine that serves the machine's records and introspection ([The daemon](#the-daemon-and-its-read-surface)), two separate authorizations for reading that surface and for running work through it ([Read auth and execution auth](#read-auth-and-execution-auth)), the fleet channel that lets one machine run a dispatch on another ([Fleet](#fleet-addresses-trust-and-the-execution-channel)), and the shared record stream with its optional audit chain ([Provenance and the audit chain](#provenance-and-the-audit-chain)).

### The daemon and its read surface

`darkmux serve` is one process per machine. It serves that machine's flow stream (`GET /flow/<date>`, and a live SSE tail at `GET /flow/<date>/stream`), the bundled drill-down viewer (`crates/darkmux-serve/assets/next.html`, built from `ui/src` and compiled into the binary; the public demo is the same viewer in playback mode, and CI regenerates it and fails on a diff), and per-machine introspection. `GET /machine/specs` returns the version, `machine_id`, RAM, CPU brand, OS, loaded models from `lms ps` and the redacted Redis URL, and `GET /fleet/view` publishes the fleet as typed rows, this machine's own card among them ([Fleet awareness](#fleet-awareness-one-view-every-machine-reads)); a machine's card travels to its peers on the fleet listener, not on the daemon. The routes are a contract ([Public surfaces](#public-surfaces-the-daemons-http-routes-are-a-contract)).

- **A misbehaving viewer tab cannot exhaust the daemon.** The Redis tail behind the SSE route is bounded: connect wedges are bounded by `REDIS_CONNECT_TIMEOUT`, a persistent failure ends the stream cleanly with a synthetic `stream.error` record, and the producer-to-consumer channel is capped with drop-newest semantics. Concurrent SSE streams are capped (`MAX_CONCURRENT_SSE`, held by an `SseSlot` guard), and every non-streaming route has a request timeout; the event stream is kept apart so the timeout never applies to it.
- **CORS is deny-all by default.** No origin, `null` included, is allowed until named (#2155). `null` was once allowed so a `file://` viewer worked, but it is also the origin of a sandboxed iframe, so any site could embed one and read the daemon's routes. The operator opts in to specific origins through `DARKMUX_DAEMON_CORS_ORIGINS` (exact match after normalization; a literal `*` is rejected with a hint on stderr).
- **A live channel carries sub-second model state and persists nothing.** While an execution runs, its dispatch process sends samples (the model's state 250 ms apart, and every transition) over a unix datagram socket to the local daemon (`darkmux_flow::live::local_socket_path`), and `live_hub` fans them out to open viewers as SSE `live` events on the same stream. A bounded broadcast ring drops a slow viewer's oldest samples instead of holding the others back; the receiver accepts only what `LiveSample::from_datagram` accepts and re-serializes it, so a local process cannot push arbitrary bytes into a viewer. Nothing in the path writes a day file, Redis, the audit chain or the trajectory, so the durable heartbeat stays at two seconds and no history grows. It is local only (another machine's cards stay at the heartbeat), and its cadence is `runtime.live_sample_ms` (`0` is off). The observer must not join the observed: a slow or absent daemon never slows a dispatch.
- **The unit of the daemon is the machine.** Mission and phase state live in each machine's own filesystem. Replicating them across machines is out of scope, tracked as a future architectural pivot (#280), as are mission priority with cross-fleet pause (#282) and elastic-hub failover, which would close the single point of failure of a fixed hub.

### Read auth and execution auth

Running work and reading records are different powers, and they are authorized by two separate switches.

- **Execution** is fleet work submission, the only surface that starts work. It always requires the fleet token plus a network-verified sender, whatever else is set ([Fleet](#fleet-addresses-trust-and-the-execution-channel)).
- **Reads** (the viewer, every JSON route, the live stream) are governed by `serve.read_auth`, default `false`: reads stay open to whatever reaches the daemon, by design, because the tailnet is the trust boundary and a viewer over `tailscale serve` is the point. With it on, only a request from this machine stays open; every other read needs `Authorization: Bearer <token>`.

They were one switch until a token that closed reads made it impossible for a hub that takes fleet work (which needs the token) to also serve its viewer to the tailnet. The token itself is a Keychain item (`serve.token_keychain` gates whether it is read; `DARKMUX_SERVE_TOKEN` overrides), wrapped in `RawServeToken` so it can only reach a log redacted.

**"This machine" is one predicate**, `is_local_request` in `darkmux-serve`, used for every local decision. A request is local when its peer is loopback (an IPv4-mapped loopback included), it carries no reverse-proxy header, and it has a single `Host` naming the daemon (`localhost`, `127.0.0.1`, `[::1]` or the bound address, with no port or the bound port). A request through `tailscale serve` arrives from loopback with proxy headers (`X-Forwarded-For`, `Tailscale-User-*`), so it is not local and needs the token; a page rebound by DNS to loopback has a loopback peer and the attacker's `Host`, so it is not local either; a request with no `Host` or no peer address is not local. The failure direction is deliberate: a local process that fakes a header only makes itself look remote, which shows less.

Two panels describe the execution surface (`doctor` shows the fleet listener's overlay address, port, busy policy and the allow-list's node names and roles; `config-list` shows the whole `config.json`, allow-list included), so they are served only to a local request or a token holder even with read auth off, and `/health` withholds the same facts. `GET /fleet/view` uses the same predicate (`caller_is_local_or_holds_token`) for each card's seat block; what each peer lets this machine run goes to every reader ([Fleet awareness](#fleet-awareness-one-view-every-machine-reads)). `darkmux serve` runs the same config gate as every other entry point before it binds, so a wrong-typed `serve.read_auth` or a retired key refuses the start instead of dropping the file to defaults, and it refuses to start with read auth on and no token. A non-loopback `--bind` requires read auth on.

Known limit: `Host` is client-set, so a non-browser client behind a TCP forward that adds no headers can send `Host: localhost` and cannot be told apart from this machine. For that setup use the HTTPS `tailscale serve` (it adds headers), or keep read auth on with a non-loopback bind. `darkmux doctor` and the `serve` banner state both postures.

### Fleet: addresses, trust and the execution channel

A machine's work runs on another machine only through an authenticated channel, and the address that names it is part of the profile.

**The address.** `darkmux dispatch <role> --profile <profile>@<machine>` runs the dispatch on `<machine>`. `darkmux_types::profile_address` is the one parser: it splits at the last `@`, the machine part is a `machine_id` (letters, digits, `-` and `_`, at most 64, compared case-insensitively), and a profile name that itself contains `@` cannot be addressed. The sender sends the owner's bare profile name, and **the owner is the only judge of it**: the receiving machine resolves the name against its own registry, and an undefined name is refused by name, never replaced by its `default_profile`. A path that runs only on this machine (the lab, a mission step) refuses an address instead of reading it as an undefined local name. The address is a profile property, not an endpoint kind.

**The listener.** With `fleet.listener.enabled`, the daemon runs a second listener bound only to the address the identity provider reports for this machine: never `0.0.0.0`, never loopback, and never behind a `tailscale serve` front, which would make every caller arrive as loopback and unidentifiable. It serves two routes, `POST /fleet/work` and `GET /fleet/card`, and every request on it is authenticated first (`darkmux_fleet::authenticate`: the fleet token, the sender's node, not this machine). A job is then authorized (`darkmux_fleet::authorize`, the allow-list); a card read reports the allow-list's answer as data instead of refusing on it ([Fleet awareness](#fleet-awareness-one-view-every-machine-reads)). `darkmux_fleet::admit` is `authenticate` followed by `authorize`, the one path a job takes.

**Three checks, in this order, deny by default.** The first two authenticate the caller; the third authorizes it.

1. **The fleet token**, compared in constant time. A caller without it costs one compare and never makes the receiver spawn anything. The token is the serve token, one shared secret on every machine.
2. **The network identity of the sender.** `fleet.identity.provider` (a registered value, `tailscale`) answers `whois` for the connection's peer address. No answer is a refusal (fail closed). A job's lookup has no cache (about 25 ms a call, measured), so a node leaving the network takes effect on the next job, and the allow-list is read from `config.json` per authorization, so `untrust` does too. A card read may reuse a lookup for the same address for 10 s: it states a card and a grant to a caller that already holds the fleet token.
3. **The allow-list** (`fleet.accept_work.<machine>`). An entry names the node (`node_id`, which `darkmux machine trust` resolves through the provider and an operator never types), and scopes what the peer may run: the *resolved* profile must be one it lists and must not be utility-only; roles are an explicit list (a role is a tool palette and a system prompt, so "any role" would grant every palette this machine has); images run on darkmux's own runtime image unless listed; a `workdir` needs `workspace`. `machine_uid` is never an input, and machine names compare ASCII case-insensitively.

**Connections are bounded.** A tokenless peer could open many half-sent requests and exhaust the daemon's descriptors, taking the viewer port down with the listener. The listener serves at most 32 connections at once (one more is closed on accept), at most 6 per peer address (the 4 requests a node may have in flight plus the 2 card reads it may make, so a peer whose waited jobs hold their connections still reads this machine's card), gives each 3 seconds to send its headers, and caps a connection's lifetime; the daemon raises its open-file limit at start, and refusals are logged at most five times per peer address per minute.

**Busy is decided per seat.** A local model serves one request at a time, so a job on a local model holds that model for its run, and a second job for the same model is busy while a job for another local model runs beside it. A job on an endpoint the receiver does not manage runs beside others on that endpoint up to its `limits.concurrent_calls` (`0` is unbounded; absent, one at a time), and jobs on different endpoints never wait on each other. Past a limit the receiver's `fleet.busy_policy` answers: `refuse` (the default) says so at once, naming what runs; `queue` holds the job first come first served per seat, at most four per sending machine (`NODE_CAP`), with a `queued` line every 20 seconds so the connection never looks dead, and a queued job passes every admission check again when its seat frees, so `untrust` also stops jobs already waiting. Only jobs from other machines count; this machine's own dispatches are not seen by the listener.

**The sender verifies the receiver too.** Before the token leaves, the roster address must resolve to a tailnet node, pinned per roster entry on first contact (`PeerTarget::pinned_ip` and `newly_pinned`), and a loopback target gets no token. A roster entry's id is the machine's own `machine_id`, and `machine add` refuses an address that reaches only the reading machine (loopback forms) unless `--allow-loopback`, because other machines read the roster and a loopback address reaches whichever machine reads it. Remote output is stripped of control characters before it prints.

**What a routed dispatch records.** The sender writes `dispatch.route` (`target_machine`, `decision`, `profile_address`) and never a token count: the machine that ran the model counts tokens. The receiver runs the job under a relay session in a standalone run, so it is never one of the receiver's own missions, and it never mounts the shared toolchain cache.

**The wire is versioned, and it grows by minors.** `WORK_JOB_SCHEMA_VERSION` is `major.minor` (`"8.1"`, `WorkVersion`), and a reply body is newline-delimited (queued lines, then the answer). A receiver takes a submission of its own major with a minor at or below its own; a newer minor, another major, and a schema that is not `major.minor` are refused with the `version` code, naming both versions. `WorkSubmission` and `WorkJob` keep `deny_unknown_fields` (a sender cannot smuggle a field a receiver might start interpreting), so the minor is what makes an addition legal: a new optional field ships as a minor bump, and the receiver's version gate is the one authority on what it takes. A change that is not additive is a major. The 8.0 shapes are frozen by fixtures and a shape hash, and 8.1 (the job's optional `target_machine_uid`, #3028) has its own beside them (`crates/darkmux-fleet/tests/fixtures`, `work-shape.golden`), the way the card's are.

**The data boundary is the receiver's.** A job may carry `boundary: managed_only`: its message may go only to a model the receiver serves itself. The receiver enforces it in `check_scope`, against the profile the job RESOLVES to (`seat` from `classify_profile`, which reads `darkmux_crew::target::target_for`'s endpoint kind, the same resolution the card reports as `endpoint_kind`), so a profile that is hosted is refused with the `boundary` code and nothing reaches its model. `check_scope` runs when a job arrives and again at a queued job's recheck, so a profile repointed at a hosted endpoint while the job waited refuses it too. A boundary a receiver does not know (`Boundary::Unknown`) is refused: it never runs a job whose boundary it cannot check. The sender's view of the peer (its card) is only the first guess; radio's peer answering seat sends full grounding under `managed_only`, and on a `boundary` refusal asks once more with the hosted-safe grounding and no boundary.

**Check mode is the one "would this route work".** A job with `mode: check` runs every gate a run meets in the listener's own order (the fleet token, the network identity, the allow-list, the scope, the boundary, the seat under `fleet.busy_policy`, the version) and answers `checked` (the resolved profile in `profile`, and `check`: `endpoint` `managed` or `unmanaged`, `seat` `free` or `would_queue`) or the refusal a run would get. It mints no session, takes no seat and no queue slot (`SeatBook::peek`, `KeySlots::has_room`), starts no worker and writes no dispatch records; a successful check writes no log line, since a poller may repeat it. `darkmux_fleet::check_route(profile_address, role, boundary)` is the sender side and returns a typed `CheckOutcome` (`Routable`, `Refused { code, reason }`, `Unanswered`). Doctor and radio read it instead of re-deriving the decision from allow-lists or cards, which would diverge on profile resolution through `role_profiles`, utility-only profiles, images and `workspace`.

**Refusals are typed.** Every `Refusal` maps to exactly one `RefusalCode` (`Refusal::code`, the one classification), and the reply carries it beside the sentence in `refusal`. A sender's `submit_work` returns a refusal as a `SubmitRefused { code, reason }` error, and consumers match on the code, never on the sentence. A code a newer darkmux sends reads as `unknown`, never as a known kind.

Known gaps: the identity provider is one value today (`tailscale`); a mission's own steps cannot yet name a `profile@machine` (only `dispatch` routes); a work job carries no `brief_refs`, so a routed dispatch that names one is refused instead of routed without its blocks; rotating or removing the fleet token takes effect when the daemon restarts, since it reads the token once; and a sender that vanishes without closing its connection (a laptop that sleeps) keeps its queue place until TCP gives up.

### Fleet awareness: one view every machine reads

"All machines as one" needs one fleet-scoped view that every consumer reads, whichever machine is asking. Before it, the facts were scattered: the roster held who is in the fleet, presence held who is alive, `/machine/specs` held hardware and loaded models, and the facts that matter most for routing (which profiles a machine has and whether darkmux manages their endpoint, what it accepts from whom, whether its seats are busy) were not published at all.

**The channel.** A machine's card travels between machines on one channel: its fleet listener, `GET /fleet/card`. The listener is the only surface that can say who is calling, because it binds the overlay address and verifies the caller's node from the socket; the daemon behind `tailscale serve` sees every caller as loopback. There is no second channel and no fallback. The daemon serves no card route (`GET /machine/card` is retired), and a machine reads its own card from its own row of `GET /fleet/view`. A peer whose listener is off is shown as unreachable with a typed reason (`listener_off`), and a machine is visible to the fleet while its listener runs.

**The card.** `MachineCard` (`crates/darkmux-serve/src/machine_card.rs`, with `CARD_SCHEMA_VERSION`). Every block is read from the thing that owns the fact, never re-derived: identity, hardware and loaded models come from the same gather as `/machine/specs` (the card embeds its type); each profile's endpoint kind comes from the dispatch target resolution (`darkmux_crew::target::target_for`), the one every dispatch, residency and doctor path runs (a profile of both kinds is `mixed`, one whose target cannot be built is `unresolved`, never guessed); governor state is the host sampler's own reading (the OS thermal word, verbatim) plus `power_policy::start_decision`, the decision the launch pre-flight runs, which is `null` when no sampler ran in the process that built the card (not observed, never `false`). The card stamps its own gather cost and the TTL it is served under.

**The fleet role.** Since card schema 1.1 a card states the position its machine declares (`fleet_mode`: `standalone`, `hub`, `peer`, or `unknown` for a value this darkmux does not know), whether the Redis this machine is configured to use runs on this machine (`hosts_fleet_redis`), and, on a hub's card only, the fleet defaults ([The declared hub](#the-declared-hub-and-what-it-hands-out)). Presence beats and telemetry records carry the same `fleet_mode`. A setting that changes how the fleet behaves is visible from every machine; nobody has to ask a machine which role it plays.

**Seats state facts, not availability.** The seat block comes from the fleet listener's `SeatBook`, which sees only jobs other machines submitted: this machine's own dispatches never pass through the listener. The card says so (`counts_own_work: false`) and no field reads as "free": `local` lists each local model a peer's job could seat with `held_by_peer_job`, `hosted` has `held_by_peer_jobs` and the `cap`, and a model that only a utility-only profile names is left out, since the listener refuses that work. LM Studio's own per-instance `queued` count rides in the specs. When one seat derivation shared by the listener and radio exists, `counts_own_work` flips to `true` with no shape change.

**Authenticate, then authorize.** The listener's gate authenticates every request (`darkmux_fleet::authenticate`: the fleet token, the connecting node, not this machine). A job then authorizes (`darkmux_fleet::authorize`: the allow-list entry for that node). A card read stops after authenticating and answers `ListenerCard { card, grant }`, where the grant is the allow-list's answer stated as data: `listed` with the caller's own entry (`profiles`, `roles`, `images`, `workspace`) and no other, `not_listed`, or `unknown` (the allow-list could not be read or names the caller twice). Read awareness is therefore not coupled to execution grants: an empty `accept_work` means "visible, takes no work", and a machine that does not list the asker still shows its card to it.

**The view.** `GET /fleet/view` returns a `FleetView` (`fleet_view.rs`): one row per roster machine, every roster peer dialed, and always a row for this machine. A row carries:

- `entry` (the roster entry; `null` only for this machine's own row when the roster has no entry that is this machine) and `is_this_machine`, decided by the verified node behind the entry's address against this machine's own node, else by hardware uid, else by name. Evidence that exists and disagrees decides "no". A roster alias that verifies to this machine's node folds into this machine's row and is not dialed; an entry that is not this machine stays a row, unreachable with its reason.
- `machine_uid` and `uid_source` (`card`, `declared` or `flow_history`): the verified card's uid, else the entry's declared uid, else the one derived from flow history under the entry's name. The roster reader is shared with `GET /fleet/roster` (`resolved_roster`), so an entry has one uid on every surface; the flow-history scan, which reads every day file, runs at most once every five minutes.
- `liveness` (`live`, `no_beat`, `unknown`), from presence, and `last_beat_ms` on the peer's clock. Presence is a display column and never a gate: an unreadable or forged Redis cannot hide a machine that is up, and `no_beat` says only that no beat was found.
- `received_at_ms` (when this machine got the answer, on this machine's clock: ages are computed from it) and `fetch_ms` (what producing the row cost).
- `card`, one of: `available` (the card, with `source` `local` or `listener`), `unavailable` (with a `why`: `no_card_route` for a listener of an older darkmux, `other_schema_major`, or `unparseable` for a card of this major that does not parse, which is a writer bug and not "older"; and the `peer_version` with whose word it is, the peer's own answer preferred over the presence beat's), `mismatch` (the answering machine is not the one the roster entry names, so its card and its grant are dropped, not attributed), or `unreachable` with a typed reason (`bad_address`, `dns_failed`, `identity_unavailable`, `not_on_overlay`, `pin_mismatch`, `pin_not_saved`, `listener_off`, `auth_required`, `refused_by_peer`, `listener_unavailable`, `bad_answer`, each with its own remedy text).
- `accepts`, beside the card: `granted` (with the one entry), `not_listed`, `this_machine`, or `unknown`. It is a fact about the row's machine, not about how its card arrived. A grant is a permission, not a route: several peers may grant this machine the radio seat, and which one answers is this machine's radio setting. `accepts` stays on the wire for the CLI (`profile list --machine`/`--remote`, `machine list`), and the viewer does not draw it on a fleet card (see below).

Every daemon gathers its own view, so there is no hub dependency, and the row set is that machine's own roster plus itself: rosters are not compared across machines, so two machines' views agree only as far as their rosters do. `darkmux machine list` prints the view its own machine's daemon gathered; with no daemon running it gathers one in process and the view says so (`gathered_by: cli_process`), because seats, thermal state and battery are the running daemon's and are then not observed rather than absent.

**Where a card comes from.** From the peer itself, over the verified peer path (`peer_target` + `fleet_get`: the roster address must be the pinned overlay node before the token is attached), never from Redis, and never from an address no node stands behind (a loopback entry is not dialed). The roster address names the viewer daemon, which may sit behind `tailscale serve`, so its port and scheme are dropped and the listener's port is used over plain http, as work submission does. A peer's card and refusal sentence are sanitized (control characters, bidi overrides and zero-width characters removed, strings cut) and size-bounded before they are typed.

**Compatibility.** Every enum a card or a view carries has an `unknown` arm (`#[serde(other)]`): a card that a newer darkmux writes with a value this one has never seen still reads, and every other field is shown. `Unknown` is never read as any known value: an unknown grant is neither listed nor not listed, an unknown busy policy is neither `refuse` nor `queue`, and an unknown reply status on the work wire is neither a success nor a refusal. `CARD_SCHEMA_VERSION` is tied to the card's shape by a golden hash (`crates/darkmux-serve/tests/fixtures/card-shape.golden`): a shape change without a version bump fails a test, and a released version's line in that file is never edited. The fixtures of every released version are frozen: the current version's are checked against the writer, and an older version's are only read, through today's parser. A field added in a minor is optional on read, and its absence states nothing. A 1.0 card predates the fleet-role fields, so it reads as stating no role, never as `standalone` or as not hosting Redis. (Found live: with those fields required, every peer still on the previous build read as `unparseable`, and the 1.0 fixtures that should have caught it had been regenerated in place.) A peer on another schema major shows as `unavailable`, never as an error.

**What bounds a peer, and what a card read costs.** Each card request has one overall timeout (2 s: connecting, sending and reading the answer, so a peer that trickles bytes is still cut off), and there is one attempt per peer. Before it the roster address is resolved (DNS, bounded by its own 2 s limit) and verified (the identity tool's own 3 s bound per resolved address). Peers are asked in parallel, the whole view is single-flight and cached for 5 s (`cache_ttl_ms` is recorded in the view, a knob and never adaptive), and the gather takes as long as its slowest peer. The serving machine keeps its card for 2 s (`cache_ttl_ms` on the card, single-flight), so a card read does not run a specs gather while the cache is warm, and keeps identity lookups for card reads per address for 10 s (a job never reads that cache). A card read takes a per-node slot of its own (2), so a sender whose jobs fill its request slots still gets its card, and the per-address connection cap (6) leaves room for those reads. A caller with the wrong token costs one compare and its refusals share the per-address log budget (5 a minute, the rest counted and reported once), so a peer that cannot be placed cannot grow this machine's log.

**What a fleet card may show.** A fleet card shows only facts about its own machine. A relationship with the machine serving the viewer (a grant, a radio permission) is not a card fact: the same fleet must read the same from any server. Relationships live in the console, which runs commands on the serving machine, and whose command line names that machine. A card states what its machine SERVES without naming who may use it: `serves_radio` (this machine's `fleet.accept_work` grants `radio-host` to at least one peer) is named by the card's serves line in words ("serves radio"), and `serves_profiles` (card schema 1.2, the number of distinct work-class profiles in its registry that the allow-list grants to at least one peer) by the same line ("serves 3 profiles · radio"), a line whose height is reserved whether or not it has words. Neither is ever read from `accepts`. Absent, or 0, means nothing is shown. Whether YOU are allowed is a relationship, and checking it is the console's job: `darkmux profile list --machine <peer>`.

**Who sees what.** The view a reader gets from a daemon shows each card's seat block only to a local request or a token holder (the doctor panel's audience), and `accepts` to every reader. Reads stay tailnet-open, and a grant is a read: it states which profiles and roles a peer runs for this machine, and carries no address and no token. An earlier rule served `accepts` only to a local request, on the reasoning that the fleet token is shared fleet-wide. It hid every grant from the operator's own bookmarked viewer, which reaches the daemon through `tailscale serve` and so is never local, and it was dropped (operator, 2026-10-01). Execution is unchanged: it still needs the token and a verified sender. Reads of the daemon's own routes otherwise follow the existing split: open to the tailnet with `serve.read_auth` off, token-gated with it on.

**Freshness.** A card is as old as `received_at_ms` on the reading machine, whatever its own `generated_at_ms` says, and at most `cache_ttl_ms` older than that; a view is as old as `fetched_at_ms` plus up to its `cache_ttl_ms`. Nothing is pushed: a change on a peer shows on the next gather after the caches expire.

Known gaps: a gone peer's last-known card is not kept; runs a governor has paused are per-run state (each run's pace file), so the card states the thermal word and the battery gate but not "resting"; a peer's version comes from its presence beat when its listener has no card route, and the row says so; and the data-boundary decision a sender makes from a card (a peer's profile is `managed` or not) is only a first guess: the receiver enforces the boundary against the profile it resolves at run time ([Fleet: addresses, trust and the execution channel](#fleet-addresses-trust-and-the-execution-channel)).

### The declared hub, and what it hands out

One machine declares `fleet.mode hub`. It runs the Redis the fleet's records and presence go to, and it holds the fleet defaults: settings a machine takes when it has none of its own. Before the role existed the hub was informal. Every machine pointed `redis.host` at the same machine, and from any other machine the only way to learn which one was to ask it.

- **The role is visible, never asked for.** The hub's card states `fleet_mode: hub` and `hosts_fleet_redis`, presence and telemetry carry `fleet_mode`, and the viewer shows a HUB badge on that machine's fleet card and in its machine lens header.
- **One decision, every reader.** `FleetView::declared_hubs` is the one place that decides which machine is the hub (none, one, or several). `MachineCard::hub_defaults` is the one place that decides what a card may hand out: a defaults block that is present, written in a shape this darkmux reads (`FLEET_DEFAULTS_VERSION`), on a card that declares `hub`. The badge, doctor and every default resolution read these two (`src/fleet_defaults.rs`), so they cannot disagree.
- **Defaults travel only in the hub's card,** over the verified peer path, never in Redis. Redis is writable by anything that reaches it, and a default like radio's answering seat decides where full grounding goes.
- **A machine's own setting wins.** Radio's answering seat resolves in one order (`SeatSource`): the session's radio host pick, this machine's own setting (`radio.answerer_profile` or `role_profiles.radio-host`), the hub's default, then the built-in default. The radio line names which one answered.
- **A hub that cannot be asked does not move a default.** Each machine keeps the last copy it read (`<home>/fleet-defaults.json`, with the time it read it) and uses it only when the hub cannot be asked; the radio line then names the copy and its age.
- **Doctor checks the shape.** "fleet hub" fails when several machines declare `hub`, warns when none does in a fleet of more than one machine, and checks that the hub hosts the fleet's Redis and that this machine's `redis.host` reaches it. "fleet defaults" names the answering seat and where it came from.
- **Heartbeats stay local; the fleet stream carries work records and machine telemetry (#2101).** `dispatch.turn.heartbeat` fires every two seconds per running execution, which on a long mission was most of its records and flushed other machines' work out of the capped stream. The Redis sink skips it, live and in the outage backfill (`reaches_fleet_stream`, `darkmux-flow/src/lib.rs`); the local day file keeps every one. A peer's run stays live through its other records (`dispatch.turn`, `dispatch.tool`, `dispatch.rest`), and presence is a TTL key, not a stream record.
- **The hub going down loses no record.** Each machine's own records stay complete in its local day files; what stops is cross-machine visibility. The daemon's Redis sink is long-lived (`SinkPolicy::LongLived`, `darkmux-flow/src/hub_link.rs`): it keeps probing on a capped backoff, and when the hub answers again it publishes the records written locally during the outage, marked as late (heartbeats excepted). A CLI run gives up after a few failures (`OneShot`). `/health` and doctor report the link (`hub_link`).

Known gaps: declaring `hub` starts nothing (darkmux does not run or supervise Redis), and a machine joins the fleet by hand-editing rosters on every machine; pairing is #3024.

### Machine identity: a name is a label (#3028)

A person reads and writes machine names: `phi4-review@studio` in config is legible, and a uid there could not be mapped by eye. So names stay the handle in config, and a name resolves to a machine whose identity is its hardware uid.

**Where the name used to be the identity,** in three places: the roster was keyed by name with no `machine_uid` for a peer, a `<profile>@<machine>` address resolved through that name, and the receiver compared the job's `target_machine` to its own name. After a rename, a job addressed to the right node under the former name was refused as misaddressed (found live, 2026-10-01). The trust list is not one of the three: `fleet.accept_work` matches the sender by its network node id (`match_entry`), so its key is only a label.

**What exists now:**

- **The roster entry learns its peer from the peer's card.** `learn_identity` (`crates/darkmux-fleet/src/peer.rs`) writes the card's `machine_uid` and `machine_id` onto the entry as `machine_uid` and `current_name`. It is called from the card read the fleet view already makes (`fleet_view::learn_from_card`, the daemon's gather and `machine list` alike), so it adds no network call, and only when the target carries a verified node: the card is the pinned node's, and only a card the view attributed to the entry. The write follows the pin's pattern: compare-and-set under the roster lock against the entry that was verified (`check_saved_entry`), and no write when the card adds nothing. The entry's `id`, the key the operator wrote, is never rewritten. What it learns depends on what the entry knows: with no uid, only a card whose name is the entry's id or its learned name (a card under another name is a mismatch and teaches nothing, so a mis-pointed entry is not adopted); with a uid, a card stating the same uid under a new name is a rename and the name is learned; a card stating a different uid is a mismatch and is never written over it. A name another entry's id already carries is not taken as this entry's current name. Repointing an entry at a new address forgets both, like the pin.
- **`@name` resolves at use, in one function.** `find_machine` (`roster.rs`) matches an entry's `id` or its `current_name`, ASCII case-insensitively. Dispatch, the radio's answering seat, `check`, doctor's route probes and `machine status` reach it through `send_job`, `fetch_peer_json` and the trust command. Two entries that answer to one name are an error naming both. `find_machine_key` stays id-only, for `machine add` and `remove`, which edit the key the operator wrote.
- **A job carries its target's uid.** `WorkJob.target_machine_uid` (work wire 8.1) is filled in `send_job` from the entry's `machine_uid`. The receiver's one comparison is `WorkJob::is_addressed_to`: when the job and the receiver both have a uid, the uid decides, case-insensitively, whatever the name; otherwise the name decides as before. A different uid is `Misaddressed`, and the refusal sentence prints neither uid. A job from an 8.0 sender, or from a sender whose entry has learned no uid, still works by name. A submission is written at the lowest version that can say its job (`WorkJob::wire_version`): 8.0 when no uid is carried, so an 8.0 receiver still takes it, and 8.1 with the uid, which an 8.0 receiver refuses with the version remedy. The 8.0 fixtures are frozen and an 8.1 receiver is tested against them.
- **Doctor says what differs.** `roster identity` warns when an entry's `current_name` differs from its `id`, names both, and says that addresses using either name work; it is a warning because nothing is broken. When another entry already goes by that name, it is the duplicate repair instead.
- **The viewer labels a fleet card with the machine's own current name** when the view read its card (`specs.machine_id`), and with the roster id when it did not.

**Limits.** A sender that has not yet read the peer's card since the peer was added holds no uid and falls back to the name check, so the first gather after a rename (or after adding the peer) is what teaches it. The sender's `profile@machine` comparison against an already-addressed `opts.machine` still compares names, so one dispatch addressed under both names of one machine is refused as two machines.

### Provenance and the audit chain

Two optional sinks sit beside the local day file in one `TeeSink`.

- **Redis Streams**, through `RedisSink` (`redis.enabled`, or `DARKMUX_REDIS_URL`): `darkmux:flow` is the fleet-wide event log, and every machine's tee includes a Redis leg that appends each record. The daemon's `/flow/<date>` reads it for the decentralized topology view. The password is a Keychain item, never config.
- **The audit chain**, through `AuditFileSink` (`audit.enabled` and `audit.dir`): a per-machine, per-day file in which each line is a BLAKE3 hash of the record's exact bytes chained to the previous line, serialized with `flock(2)`. `darkmux flow integrity-check` recomputes each chain and reports the first divergence: exit `0` when every chain walked clean, `2` when a break is found, and `3` under `--strict` when a file could not be content-verified at all (a pre-chain legacy file). A break outranks an unverifiable file.

A record's `machine_id` is **operator-asserted**: named by the operator and stamped from the environment, with no authenticated identity behind it. The chain's stated limit is that it is anchorless: a writer with access to the files can rewrite a whole file (fresh header, recomputed hashes, relinked `prev_hash`) and reach exit `0`, and a legacy file carries no evidence either way. darkmux describes what the check does; it makes no claim about what a clean run proves.

## Hooks: how records leave the machine, and who is allowed to hold a credential

A hook is not a feature bolted onto the crawler or the review pipeline. It is a **`FlowSink` like every other**: the fourth child of the same tee that already fans a record out to the local day file, the audit chain, and Redis. Its `write` matches the record against operator-configured rules and appends matches to a per-rule on-disk outbox; a drainer thread POSTs them and advances a cursor only after a success.

Two consequences fall out of that placement, and both are the reason it was placed there. Every record kind is hookable with **zero producer-side awareness**: thermal transitions, tool calls, and mission bookends all became deliverable without one line of change at the site that emits them. And delivery is **at-least-once, durable across restarts**: the queue is a file, the cursor moves after the 2xx, and a receiver that is down is an outage to wait out rather than data lost.

**Rules match the closed vocabulary.** A rule's `match.action` is checked against the flow vocabulary ([The flow vocabulary](#the-flow-vocabulary-one-closed-list-one-spelling-per-event)): a rule that names a retired spelling, an exact one or a spaced glob, matches no action darkmux writes, so it delivers nothing. The hook sink warns at load and `darkmux doctor` warns `CANNOT MATCH`, the same as for a typo. A rule's outbox is keyed by a hash of its `match`, so a rewritten rule starts a new outbox, and records still pending under the old spelling are not delivered; the key is not derived to survive the rewrite, because that would mean hashing the retired spelling forever.

**The receiver cannot always be adapted, which decides where transforms live.** When darkmux owns the receiver (the local crawl tracker), the honest shape is a thin adapter in the receiver: darkmux ships one wire contract (the flow record verbatim, schema-versioned, lenient on read) and the receiver projects it into whatever it stores. That stops being available the moment the destination is somebody else's SaaS. You get an API; you cannot put code inside Jira. So for anything not your own, the transform has to live on the **sending** side.

**Two orthogonal axes, and only one of them ever holds authority.**

| Axis | Job | Authority |
|---|---|---|
| **transform** (a `.jq` adapter) | the *shape*: record → request body | **none, by construction** |
| **transport** (`http` · `file` · `cmd`: `cmd` planned, not yet built) | the *destination and its credentials* | http: one Keychain header value · cmd: the operator's own CLI |

The split is the whole design. A transform is a **pure function** (it needs no filesystem, no socket, no subprocess), so it is given none of those. jq is the language because it *is* JSON-to-JSON with no I/O in its grammar: there is nothing to sandbox. Three properties follow. A crawl finding's `evidence` is a source line copied verbatim out of a repo under audit; through a shell-spawning adapter that is an injection target, and through jq it is a string, so the hostile-data class disappears. The transform receives only the record, so it **cannot** read a credential: the delivery path resolves those separately and the two never meet. And because the evaluation is in-process, the outbox's guarantee extends all the way to the real destination rather than stopping at a hop.

**What was rejected, and why, since each looks reasonable from a distance.** A *declarative template* with `{dotted.path}` substitution is safe and becomes a bad programming language the first time someone needs a conditional or a nested document: Jira's ADF `description` alone is enough to break it. *Executing an operator script as the transform* hands a pure function full operator authority (filesystem, network, spawn, Keychain) to do a job that requires none of it, and it walks around the command gate that already exists for exactly this class of thing. *Shipping named adapters for Jira, Slack, and friends* is the safest option of all and re-creates precisely the coupling this whole design refuses: the sender would own N destination schemas and every new API would be darkmux's maintenance. A *sidecar adapter service* on loopback works today with no new code, and quietly breaks the delivery guarantee: darkmux's `hook.fired` would mean "handed to a process that may have dropped it," so the retries and quarantine records would describe the wrong hop. It stays documented as the escape hatch for integrations that need their own state or batching, with the honest note that the operator owns delivery from that point on.

**The transport set is closed at three, and `cmd` is what makes closing it possible.** Two of the three ship today: `http` and `file`; **`cmd` is designed but not yet implemented**, a separately-gated packet, and the paragraph below describes the intended shape rather than current behavior. A rule naming only `cmd` is refused at load today, the same as any rule with no destination. `http` covers the ninety percent with a static credential: Jira, Slack, Telegram, PagerDuty, a webhook, an Azure Function key. `file` writes the delivery to disk instead of sending it, which is the no-network tier for testing an adapter end to end. `cmd` pipes the transformed body to an allowlisted program on stdin and **treats its exit code as the delivery ack**: zero advances the cursor, non-zero enters the existing retry and quarantine policy. That is what keeps at-least-once intact through an arbitrary destination, and it means the protocol library is the operator's own CLI rather than darkmux's source tree. SMTP is a script piping to `mail`; SQS and anything else SigV4-signed is `aws`, which already implements SigV4; gRPC is `grpcurl`; Postgres is `psql`; an Azure AD-protected endpoint is `az account get-access-token` and a curl. darkmux will not learn SigV4, OAuth refresh, SMTP, or gRPC natively: each would be the same coupling wearing a different hat. The one extension planned in advance is `auth: { cmd, ttl_seconds }`, an allowlisted command that prints a header value and is cached for its TTL, which covers every token-refresh case without darkmux implementing OAuth.

**Credentials follow the posture the command gate already states: darkmux holds none of its own.** A rule names a Keychain item; the item holds the **complete header value**, `Basic <base64(email:token)>`, `Bearer …`, whatever the destination wants, not a raw token to be assembled. darkmux therefore has no credential-formatting logic to get wrong and stays scheme-agnostic, and the value is redacted in every record, log line, doctor row, and dry-run dump. The exec transport inherits the same gate as any other shell-out: a **name**, not a path, refused until that exact name appears in the operator's own allowlist, spawned directly rather than through a shell, with the record on stdin only so that nothing derived from a crawled repository can reach a shell parser.

**One limit stated plainly rather than discovered later.** At-least-once is not idempotent, and a create-issue API has no idempotency key, so a lost response can produce a duplicate. The delivery id is stable across retries and an adapter can write it into a searchable field, but a genuine check-then-create needs two requests, which is the `cmd` transport's job, not the transform's.

## ACP: darkmux inside the editor

`darkmux acp` speaks the [Agent Client Protocol](https://github.com/agentclientprotocol/agent-client-protocol) over stdio, so an editor like Zed can drive darkmux from its own agent panel. You type `/mission launch review` in the editor and a local crew works the PR, with the result rendering in the panel rather than a terminal you have to go find. `src/acp.rs` owns the wire-protocol plumbing, and `src/acp_panel.rs` owns the panel's grammar, its launch planning and the ephemeral runner.

**The panel has one command, and it is generic.** `/mission list` lists every config `darkmux mission launch` can start, `/mission launch <config> [name=value ...]` launches one, and `/mission show <id>` prints one mission from the same derivation `darkmux mission show <id>` prints (`mission_show`, and its `--json` is a semver-bound shape). No config names itself into the panel: adding a launchable command is writing a JSON file, with no rebuild, no registration call and no darkmux release. A mission config that still carries the retired per-config `panel` block is refused by the user-file gate, naming `/mission launch <id>`. Words after the config id map onto the config's declared inputs the way `--param` does (a `name=value` token naming a declared input is a parameter), and any other text goes to the config's `__panel_args__` reader and is refused when the config has none, rather than dropped. Values have no escape syntax: a backslash right before a closing quote is refused, naming the input, instead of being guessed at.

**The panel and the CLI share one launch path.** `plan_launch` resolves the config through `mission_launch::resolve_config` (the CLI's own load and its refusal text), so `/mission list` and radio's catalog run the same first check a launch does and list configs exactly when a launch could start. How an invoked command runs is decided **structurally, never by matching an id**:

- a config whose graph dispatches **no models at all** runs as an ephemeral in-process graph (`run_ephemeral`, through the scheduler's `run_step_graph`): no mission record, no run artifacts, because a command that shells out and prints a result is not a mission and recording it as one would pollute the mission board. It is also what lets an operator-gated step ask the editor for approval through `session/request_permission`.
- anything else is a real `darkmux mission launch <id>` subprocess.

A panel invocation types no diff, so `prepare_launch` fills a declared required `diff_file` (plus `workspace` and `head_sha`) from the session's working directory when the operator passed none. The trigger is the config's declared inputs, never its name, and the CLI does not synthesize: a terminal user names the diff.

**A long-lived agent process needs a way to stop.** ACP gives an agent no disconnect notification, so a naive implementation leaks a session per editor thread forever. `session/close` is advertised and handled: it aborts anything in flight for that session and drops its state, and `session/cancel` shares the same abort-handle registry, so a canceled command is genuinely aborted rather than left running with its output discarded. Because a client may never send a close, there is also a two-tier process-level backstop (#1781). A process no client ever attached a session to (spawned and abandoned) exits once it has been idle for `runtime.acp_idle_exit_minutes`; a process that has had a session attached is reclaimed only after a week-scale hard ceiling with no traffic. The predicate is "has a session ever attached", never "is one attached right now": the latter goes true again the moment `session/close` prunes the map, so a client that closes one thread and opens another minutes later would have its live process exit in between.

Known gap: there is no structured progress channel that every command can render; a launch reports that it started and then its final output.

## Radio: free text onto one command, and a confirmation before it runs

`darkmux radio` is the terminal twin of the panel's free-text channel: it routes text onto exactly one launchable config, then (after the operator confirms) runs it. The core is surface-neutral (`src/radio.rs`), and the CLI verb and the editor channel both call into it; both call into `acp_panel` for the catalog and the launch plan, so there is one place that decides what is launchable and one place that decides how a launch runs.

- **Routing is a utility job.** One bounded classification call runs on the machine's utility model, through the lean utility path: free text and the catalog in, one config id and its arguments (or a refusal) out. The router requires `internal.utility`; with none registered it refuses, naming the fix. It reads the first sentence of a config's `description` (else its `name`) to describe each candidate, so a config that should be routable leads with one plain sentence.
- **Selection, never composition.** A route can only name a command that was in the catalog the call was given; `validate_router_output` re-checks the model's claimed id against the catalog after parsing. Text that does not map cleanly onto one command refuses and lists the options rather than guessing, because a router that guesses wrong on a command that changes external state is the failure the command gate exists to prevent.
- **It asks before it runs anything it chose.** Radio prepares the launch's inputs first and prints the exact `darkmux mission launch <id> --param ...` command with every parameter that will run (for `review`, the temporary `diff_file` and `workspace` and the `head_sha` it made from the current directory), then asks `Run it? [y/N]`. With no interactive terminal it prints the command, says it was not run and exits 1; an interrupt at the prompt runs nothing and removes the temporary files. A routed input holding a control character or an invisible formatting character (a bidi override, a zero-width space) is refused. The editor channel does the same for free text: the pick is shown in the panel's permission dialog and only Allow runs it. An explicit `/mission launch <id>` is the operator's own command and is not asked again.
- **It says when the model is busy instead of queueing silently.** One LM Studio instance serves one request at a time, and darkmux caps concurrency only within one process, so a radio call fired while a coder runs used to queue inside LM Studio with no visibility and fail at the call's ceiling after minutes of silence. Two facts something else already holds decide it: LM Studio's own status and queue for the instance, and the residency leases other live darkmux processes hold. The answering seat asks before it sends and answers "busy" at once, naming the occupant when darkmux knows it. The routing seat waits behind whatever occupies the one utility instance, by decision (#2914), but after a delay the surface says what LM Studio reports and keeps waiting to the ceiling. The check runs just before the send, so work that starts in between can still queue a call.

The *answering* seat (grounded answers over the catalog, the live config, the mission board and the last few command outputs the panel rendered) is ordinary work: a full dispatch and a run, staffed through `radio.answerer_profile` and `role_profiles.radio-host`.

## Guardrails: what CI holds so the design stays true

A rule that only lives in a paragraph gets skipped under time pressure, so most of this document's invariants are held by something that fails a build. The principle is the same for each: a guard that should block a merge lives where the merge already waits, it reads its list from the code it guards so it cannot drift, and it proves it can fail (`--self-test`) before its pass is trusted.

**The static guards are one required job, with one exception.** The `docs-drift` job in `.github/workflows/ci.yml` is a required check on `main`, and it holds the Rust half of the complexity ratchet. The TypeScript half (`--lang ts`) runs in the `ui` job instead, only when the pull request touches the viewer (a path filter). A guard that should block a merge lives in that job rather than in a job of its own, because a new job is not required until the ruleset names it.

| Guard | What it forbids | What it cannot see |
|---|---|---|
| Retired terms in docs (`docs-drift`) | a retired verb spelling, a retired role family name or a retired identity phrase in the user-facing docs, `DESIGN.md`, `CLAUDE.md`, `packaging/`, `skills/` and `templates/`; a line that narrates a removal must carry its issue number | prose that misdescribes a live feature without using a retired word |
| `scripts/rs-drift-guard.py` | the same retired verbs inside Rust string literals (help text, error hints, banners); an inline `// drift-guard:allow` marker with a reason covers a deliberate one | a retired verb assembled at run time |
| `scripts/flow-action-guard.py` | a flow action written by hand in production Rust, and an old or made-up action in tests, the viewer, docs, skills, templates and fixtures ([The flow vocabulary](#the-flow-vocabulary-one-closed-list-one-spelling-per-event)) | an action assembled at run time in a shape it does not list |
| `scripts/complexity-ratchet.py` | a new function above cyclomatic complexity 15, and any function whose complexity rises | complexity that lives in data or in a test |
| `scripts/engagement-sentinel-guard.py` | an engagement-private identifier (an employer, a private repo, a hosted endpoint, a tracker key) entering this public repo, and a UUID-shaped string outside the fake fixture form | a fixture that reproduces private content without spelling a sentinel word; field-policy review is still a human job |
| `cargo machete` | a dependency a crate declares and never uses | what only a build would show: it reads sources, not the build, so a false positive is listed under the crate's `[package.metadata.cargo-machete] ignored` with a reason |
| `scripts/verify-tap-pin.py --self-test` | its own logic drifting; the live half runs at release time against the tap | (a pull request has no tag to check) |
| tracked-but-ignored files, demo sync | a file `.gitignore` says to ignore but git still tracks; a demo page that has drifted from the served viewer | |

**The complexity ratchet** measures Rust with `rust-code-analysis-cli` (pinned) and TypeScript with ESLint's `complexity` rule, both over production code with test code excluded. A function is keyed by name and never by position (its path, its inline `mod` blocks, its impl or trait, and any function it is nested in), so an edit above it does not move it and a new function cannot inherit another's baseline. `scripts/complexity-baseline.json` records the debt measured when the ratchet was set; only `--prune` rewrites it, and prune only lowers an entry or drops it. `scripts/complexity-allowlist.json` holds deliberate exceptions by hand, each with a reason and a maximum of 25. A gain never fails: a function that got simpler, or was split or removed, passes with a notice naming `--prune`, which keeps the gain.

**Generated and derived artifacts are regenerated and diffed.** The TypeScript twins of the daemon's types and the flow payloads are regenerated by `bun run types:check` and fail on a diff; the committed viewer bundle is rebuilt from `ui/src` and diffed; `route-table.golden` and `tests/cli-json.golden` are derived from the types and fail their test until regenerated on purpose (`DARKMUX_REGENERATE_FIXTURES=1`), which puts the change in the diff. A golden is a review surface, not a snapshot to accept.

**Test isolation is checked by its effect.** The isolation leak check runs each test unit under a sentinel state tree and counts what it wrote, and the verdict is the file count, never the child's exit status (three of four leaking targets once went red and one stayed green while leaking). It runs on every push to `main` and on a pull request labeled `full-ci`; without the label a leak is caught on `main` after the merge. A source scan in `tests/cli.rs` is the faster pre-check that names the line, and the effect check reaches the shapes the scan cannot see.

**Two CI tiers, and what "green" means.** An ordinary pull request gets the light gate: build, the full nextest suite, clippy, the runtime crate, and the static guards above. Mutation and coverage are advisory about their findings (a surviving mutant is a number and a diff, never a block) but gate on their own integrity: a run that did not actually run, or reported numbers that contradict its exit code, fails, because a check is allowed to say nothing and is not allowed to say "clean" when it never ran.

**Known gaps.** `plugins/darkmux-bundler-rust` is outside the workspace, so the workspace suite never runs its tests: they run only in the PR-diff mutation job, and only when the pull request changes a mutable-looking line in the plugin's `*.rs` files (`bundler_changed_lines` in `quality.yml`), so a `runtime/`-only change or a change to the plugin's `Cargo.toml` alone runs none of them. Every guard above reads text, so each has the blind spot named in its row; a passing guard narrows the class of defect that can ship, and it does not prove none can.

## Composability

darkmux is designed to live BELOW agent frameworks and ABOVE inference backends:

```
[ agent framework / frontier orchestrator: Claude Code, OpenClaw, Aider, Cline, … ]
                    |
                    v
          [ darkmux ]   (dispatch · missions · observe)
                    |
                    v
[ inference backend: LM Studio ]
```

darkmux is **not** a proxy that sits in the request path (an OpenAI-compatible router was the v0.2 plan and was deliberately *not* built; see the evolution above). It operates the layer instead of intercepting it: it loads what each dispatch's staffing declares, dispatches work through a runtime it owns, and emits the observability stream. No changes to the inference backend; the frontier orchestrator drives darkmux rather than routing through it.

**The backend is LM Studio, and only LM Studio.** The residency arbiter, the `darkmux:` namespace convention, the empirical profile defaults and every `lms` shell-out in `darkmux-gestalt` are LM Studio-shaped, and an earlier claim that darkmux drives "LM Studio, Ollama and llama.cpp" was an aspiration, not a capability. A model-backend abstraction is tracked (#316) and deliberately not built; it gets revisited when a real second backend has a real user. Until then: do not deepen the coupling gratuitously, and do not pretend the abstraction is there. What is not LM Studio is an *unmanaged* endpoint ([Endpoints](#endpoints-what-darkmux-does-there-not-where-they-are)): any URL darkmux only sends requests to, on which it loads and unloads nothing.
