# darkmux design notes

## What darkmux is

darkmux is an **AI-first orchestrator for local LLMs**. It does three things, and the notes below trace how each one earned its place:

1. **Mission orchestrator (the dispatch-to-PR loop).** `darkmux dispatch` / `mission launch coder-phase` run a coder in a container-bounded runtime, review it in a fresh context, gate it on operator sign-off, and ship a PR. The defining capability today, and the headline of the 2.0 identity: config-defined missions that run as a live task graph.
2. **Lab + unified observability.** `darkmux lab run` measures how a workload runs on your own hardware; every dispatch emits a typed flow record, and one daemon (`darkmux serve`) serves the stream, a drill-down viewer, and per-machine introspection across a fleet. The empirical half that grounds the config choices behind the missions.
3. **Model residency (internal).** A dispatch loads the models a named profile declares (model + context window + compaction settings) under the resident budget. darkmux *began* as this capability, the profile multiplexer, a manual `swap` tool; in 2.0 it moved inside gestalt and the `swap` verb retired. It is the floor the loop stands on, but it is no longer a verb the operator drives by hand.

The through-line is the doctrine in [`CLAUDE.md`](CLAUDE.md): *optimization, not replacement; the harness before the model; the operator always in the loop with full provenance.* darkmux uses local AI to manage your local AI.

This document is the **why**: the decisions, and the data behind them. darkmux's architecture wasn't designed up front; it was **measured into existence**. Nearly every section names the lab run, dogfood, or research finding that drove the choice, because [that's how we decide](#how-we-decide).

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

Of all the harness knobs, **compaction had the largest measured wall-clock impact**, so it earned the most defensive engineering. The data pointed somewhere specific: a small, dedicated compactor at a modest context (~68K) cut wall-clock substantially versus reusing a large all-purpose model, and the `default` strategy beat the more conservative `safeguard` one for local models. Those aren't taste; they're [measured defaults](#compaction-tiers-structured-slots-and-graceful-degradation), and the anti-patterns doc warns against deviating from them without naming the empirical reason.

The deeper bet, that **a small model fills labeled slots more reliably than it writes good prose**, produced structured-slot compaction. [That section](#compaction-tiers-structured-slots-and-graceful-degradation) is the template for how darkmux decisions get made: a hypothesis, a measurement, a typed design that degrades gracefully instead of failing.

### The bake-off: how models get hired

Choosing which local model fills a role isn't preference, it's a [documented head-to-head per hardware tier](https://github.com/kstrat2001/darkmux/issues/159) with criteria written *before* the runs. A 128GB-tier bake-off named a 35B-A3B MoE for routine coding (fast, only ~3B active parameters), held a larger dense model as a heavy-reasoning reserve, and kept a coder-specialist for single-shot prose. The methodology outlasts any single pick: models turn over constantly, so the *bake-off* is the durable artifact, not the winner. (When you characterize "the local layer's" behavior, name the model on test, because what's loaded may not be the recommended hire; see the anti-patterns in [`CLAUDE.md`](CLAUDE.md).)

### The internal runtime: owning the loop

Early dispatch shelled out to an external agent runtime (openclaw). darkmux now ships its **own** container-bounded runtime: a Rust agent loop in a per-dispatch Docker container. Owning the loop is what makes everything downstream possible: kernel-enforced workspace isolation, a trajectory format darkmux fully controls, and telemetry emitted straight into the flow stream rather than scraped back out of someone else's logs. The openclaw shell-out path stayed first-class for a while as an opt-in alternative, but was removed on the 2.0 track ([#1405](https://github.com/kstrat2001/darkmux/issues/1405)) to keep the build and test surface small; the internal runtime is now the only dispatch path. The filter for what the internal runtime *adds* is [workflow-fit, not feature creep](#scope-of-the-internal-runtime-workflow-fit-not-feature-creep), a principle that outlived the comparison it was coined for.

### The dispatch-to-PR loop: the defining capability

Owning the runtime turned darkmux from a configuration tool into a **work** tool. The loop:

```
mission launch coder-phase → coder → fresh-context review → fix → frontier/operator sign-off (gate) → PR
```

This is what the [M4 roadmap charter](docs/roadmap/M4.md) hardens, and it's grounded in both research and dogfood: failures we *measured*, then found the literature that explained them.

- **Verification has to be real.** A production dogfood surfaced a *fabricated* sign-off: a coder reported a type-check "passed" when the slim sandbox couldn't actually run the project's toolchain, and a separate run reported the same failure honestly, so the fabrication was **nondeterministic**. You can't trust self-reporting to catch it. The fix: the runtime stamps the dispatch envelope when a verifier didn't run, so a claimed sign-off is mechanically contradicted ([#799](https://github.com/kstrat2001/darkmux/issues/799)). Process-reward-model research confirms step-wise verification catches the *silent errors* outcome-only checks miss ([arXiv 2604.24198](https://arxiv.org/abs/2604.24198)).
- **Self-review is mostly confirmatory.** At one gate a coder's full test suite + linter were *all green on its own broken work*; only a *fresh-context* review caught the regressions. The Self-Verification Dilemma ([arXiv 2602.03485](https://arxiv.org/abs/2602.03485)) measures exactly this: re-checking in your own context entrenches the original answer, while cross-context *re-thinking* corrects it. So the reviewer runs in a fresh context, and escalation is becoming codified loop policy ([#849](https://github.com/kstrat2001/darkmux/issues/849)).
- **A wrong, confident diagnosis is worse than none.** A lab run caught a reviewer verdict that *sounded* authoritative but was wrong; it sent the next coder in circles for 600 seconds, zero net progress, then a watchdog timeout. The fix: detect the no-progress signature and escalate instead of looping ([#453](https://github.com/kstrat2001/darkmux/issues/453)).

darkmux drives this loop on **real production work** (the production services of a private fintech engagement) and on darkmux itself. The recursive case is the strongest evidence: darkmux's own observability features were built *through* `mission launch`, so the data those features visualize is the data the loop produced while building them. One self-building phase ran 106 turns and ~5.2M prompt tokens with **zero compactions** (context peaked near 70K of a 262K window), which retired a standing question by turning it into a measurement: on a window that large the compaction threshold is a *cost* knob, not a correctness one.

### Observability: from a telemetry sketch to a unified stream

The original observability idea was a per-request telemetry hook: useful, but bolted on. It became something better: a **single typed flow stream** every dispatch emits into (tokens, context occupancy, detector firings, runtime events), read by one daemon and one drill-down viewer ([#557](https://github.com/kstrat2001/darkmux/issues/557)). The token view is the payoff, splitting the fleet's usage into **local tokens** (your hardware, no API bill) and **cloud tokens** (the paid endpoint you chose; [#1186](https://github.com/kstrat2001/darkmux/issues/1186)). **Tokens only, never currency, on either tier**: claiming to save or cost another person money is a liability we don't take on; the operator multiplies by their own rate. A dogfood day's data taught its own lesson, that the bulk of a long dispatch's tokens are *re-read* context, not generated output, which is itself a compaction-design input.

### Fleet: many machines become one

The multi-machine substrate (the v0.4 line, current) lets a single operator's couple of Macs over a tailnet function as one development environment, [detailed below](#multi-machine-substrate). The design target is deliberately **heterogeneous**: a high-memory laptop as the inference peer, a smaller always-on machine as the hub. That heterogeneity is the white space. Nearly all distributed-agent research assumes cloud or homogeneous hardware, so a heterogeneous local fleet of Apple-Silicon Macs is darkmux's to define rather than follow (see the [roadmap](ROADMAP.md)).

---

The rest of this document is **reference**: how the current architecture works, section by section. The decisions above are why it's shaped this way.

## Multi-machine substrate

Single-operator multi-machine is the design target. The operator owns a couple of Macs on a tailnet they control; darkmux makes them function as one development environment without becoming team tooling.

**Architecture** (current):

- **Coordination substrate**: Redis Streams via `RedisSink` (opt-in via `DARKMUX_REDIS_URL`). Two stream classes:
  - `darkmux:work`: one global work queue ([#590](https://github.com/kstrat2001/darkmux/issues/590)). Publishers `XADD`; runners `XREADGROUP` + `XACK` via a single shared consumer group. First-claimant-wins is the allocation algorithm; the first available runner claims any job.
  - `darkmux:flow`: fleet-wide event log. Every machine's `TeeSink` includes a `RedisSink` leg; `XADD` per record. Read by the daemon's `/flow/<date>` endpoint for the decentralized topology UI.
- **Audit substrate**: `AuditFileSink` (opt-in via `DARKMUX_AUDIT_DIR`). BLAKE3-chained, `flock(2)`-serialized, per-machine per-day. `darkmux flow integrity-check` walks the chain and exits 2 on a break, so cron/CI can flag tampering. Composes with the casual `LocalFileSink` via `TeeSink`.
- **Provenance fields** (FlowRecord schema 1.14.0): `machine_id`, `orchestrator`, `work_id`, `attempt`, per-turn `telemetry.tokens`. All operator-asserted (env-stamped); no authenticated identity. (The pre-1.4.0 `machine_tier` field was removed when machine-capacity tier stopped routing work; see [#590](https://github.com/kstrat2001/darkmux/issues/590).)
- **Single-stream dispatch routing** ([#590](https://github.com/kstrat2001/darkmux/issues/590)): all dispatches publish onto the one global `darkmux:work` stream and the first available runner claims any job. `darkmux dispatch <role> --machine <id>` is an *advisory* hint: any runner may still claim, and a non-target runner logs a soft warning and proceeds (no NACK/requeue). With no `--machine`, the dispatch runs locally; there is no tier auto-route. The `--machine` path still emits a `dispatch route` flow record (`target_machine` + `decision`) so the topology UI + audit chain capture *why* work went where. Capability-based auto-routing is the planned successor.
- **Per-machine introspection**: `GET /machine/specs` returns version, machine_id, RAM total/free, CPU brand, OS, loaded models from `lms ps`, redacted Redis URL. Consumed by `darkmux machine list --deep` (HTTP fan-out across reachable peers).
- **Daemon resilience**: the SSE Redis tail at `GET /flow/<date>/stream` is bounded. Connect wedges bounded by `REDIS_CONNECT_TIMEOUT`, persistent failures exit cleanly via a synthetic `stream.error` record, and the producer→consumer channel is capped with drop-newest semantics. Concurrent SSE streams are capped and per-route requests are timed out, so a misbehaving viewer tab can't exhaust the daemon. Non-loopback binds require a bearer token (Keychain-stored); loopback stays open ([#881](https://github.com/kstrat2001/darkmux/issues/881)).
- **CORS posture**: default `null` (file://) only. The bundled viewer from disk works; arbitrary localhost dev-server origins are denied. Operator opts in to specific origins via `DARKMUX_DAEMON_CORS_ORIGINS` (exact-match, normalized). Literal `*` is rejected with a stderr hint.

**Out of scope (today; may revisit)**:

- Multi-tenant authn/authz (see [What darkmux is NOT](#what-darkmux-is-not)).
- Cross-machine mission/phase state replication (per-machine FS today; tracked as a future architectural pivot, [#280](https://github.com/kstrat2001/darkmux/issues/280)).
- Mission priority + cross-fleet pause/resume ([#282](https://github.com/kstrat2001/darkmux/issues/282)).
- Elastic-hub failover (any peer promotable to hub), which would close the SPOF of a fixed-hub deployment.

## What darkmux is NOT

- Not a model-swap optimizer (LMStudio handles the actual load; we orchestrate).
- Not an inference framework (vLLM/SGLang have that covered).
- Not an agent framework (LangChain/AutoGen have that covered).
- Not a prompt router across cloud providers (LiteLLM has that covered, and it's cloud-oriented).
- Not *designed* for multi-tenant deployment. **darkmux is single-operator, multi-machine.** A hobbyist or individual engineer's "few Macs joined over a mesh VPN" is the natural deployment shape. The trust boundary is the operator-controlled tailnet, not enforcement in darkmux's code: `DARKMUX_REDIS_URL` carries no auth beyond what the underlying mesh + Redis ACLs already provide; `DARKMUX_MACHINE_ID` is operator-asserted provenance, not authenticated identity; cross-machine state on the shared substrate assumes all participants are the same operator. Fork-friendly if multi-tenant matters to you: the substrate is a reasonable starting point, and the missing pieces (auth, ACLs, fairness across distrusting users) are well-trodden elsewhere.

## History: the openclaw shell-out path (removed in 2.0)

Through the 0.x line, darkmux ran dispatches through either its own internal container-bounded runtime (the default) or an opt-in shell-out to a separately-installed openclaw process (`--runtime openclaw`), with a `crew sync` verb keeping openclaw's `agents.list[]` aligned with darkmux's role manifests. The two paths were deliberately schema-isolated (darkmux never translated its profile fields into openclaw's config shape, and vice versa), so an upstream openclaw schema change had zero impact on darkmux.

The openclaw path was removed on the 2.0 track ([#1405](https://github.com/kstrat2001/darkmux/issues/1405), operator decision on [#1386](https://github.com/kstrat2001/darkmux/issues/1386) theme 5) to keep the build and test surface small: the internal runtime is now the only dispatch path, and the schema-isolation doctrine below continues to apply to it on its own terms.

### Scope of the internal runtime: workflow-fit, not feature creep

When deciding what to add to the internal runtime, the filter is **workflow-fit**: does the feature serve darkmux's own workflow, not "does some other agent runtime have it." darkmux is shaped by three load-bearing decisions:

- **Mission-as-contract.** A phase is a bounded unit of work with explicit inputs (prior phase outputs, scope file), explicit outputs (typed text file persisted to disk), and explicit verify criteria. Cross-phase memory is file-mediated by design, so the frontier orchestrator sees what state moves between phases. Hidden session-state that survives across dispatches breaks this contract.
- **Utility/specialist split.** Utility agents (4B-class: compactor, scribe, estimator, mission-compiler) handle bounded structured work at high throughput. Specialist agents (35B+: coder, code-reviewer, analyst) handle judgment-dependent work at lower throughput. Features that push specialists toward utility work (mid-dispatch planning, todo tracking, autonomous replanning) collapse the layering that makes the split valuable, turning judgment-bearing work into hidden utility work.
- **Operator sovereignty + frontier-as-strategic-layer.** The frontier orchestrator (Claude Code) holds the strategic context; utility agents structure under that context; specialists execute within it. Features that move strategic choices *down* into utility or specialist dispatches (opaque session state, automated replanning, scoped planning verbs) quietly relocate decision authority into layers that lack the context to make them well.

The filter for any proposed internal-runtime feature: **does this reinforce mission-as-contract, the utility/specialist split, and frontier-as-strategic-layer, or does it blur them?** Features that reinforce land cleanly even when they're small. Features that blur produce "works technically but feels wrong" outcomes that surface as bugs months later.

### Schema isolation: darkmux owns its own config

Every field an operator sees in a darkmux profile maps to a darkmux-typed schema entry the internal runtime consumes: no decorative fields that look tunable but have no effect. The internal-runtime path (`src/crew/dispatch_internal.rs`, `runtime/src/`) reads only darkmux-native typed fields from `profile.runtime.*`; darkmux owns these field names, their semantics, and their evolution. An untyped `extras` map exists for forward-compat parse only (so an older binary tolerates a newer config); nothing in the internal-runtime path reads from it (enforced by explicit "must not auto-populate" tests). This discipline predates and outlived the openclaw path: it started as "don't let openclaw's config shape leak into darkmux's," and now stands on its own as "the profile schema is purely darkmux-typed, full stop."

## Lab reproducibility: fixtures + content hashing

The lab harness only earns the word "measurement" if a run is reproducible. The fixture cluster ([#487](https://github.com/kstrat2001/darkmux/issues/487)) closed the two gaps that made earlier `coding-task` numbers untrustworthy: runs mutating their own inputs, and no way to prove two runs started (or ended) in the same place.

- **Per-run COW isolation.** Each run operates on a copy-on-write clone of the source fixture, never the source. The clone is cheap on COW filesystems (`clonefile` on APFS, `--reflink` on btrfs/xfs/zfs) and falls back to a deep copy elsewhere. The provider trait is unchanged: providers see a sandbox path and don't know it's a clone. This eliminated the cross-run baseline drift observed in earlier lab runs.
- **Content hashing as proof, not policy.** `baseline_hash` (source state at clone time) and `final_hash` (post-dispatch sandbox state) are BLAKE3 over a deterministic walk that excludes derived dirs (`.git`, `node_modules`, `target`, `__pycache__`, `.darkmux-runtime`). Determinism is the point: same content + same layout → same hash, independent of mtimes or inode order. Equal `final_hash` across two runs is the strongest reproducibility signal the lab can emit. Hashing is best-effort: a failure logs and records `null` rather than aborting the dispatch.
- **Registry, not embedded sandboxes.** A fixture is an operator-owned directory with a `.fixture.json` manifest; the registry (`lab-registry.json`) is a name→path lookup plus integrity metadata. `lab fixture register`/`lab fixture unregister` never move or delete the directory (operator sovereignty: `lab fixture unregister` drops the *entry*, full stop). Workloads bind to fixtures abstractly via `requires_fixture: "<name>@<version>"`, resolved against each fixture's `satisfies` declaration. `lab doctor` makes drift detectable offline before a dispatch is wasted on it.

## Compaction: tiers, structured slots, and graceful degradation

Compaction is the harness lever with the largest measured wall-clock impact (Articles 1–2), so it gets the most defensive engineering. Two strategies coexist behind one config knob (`profile.runtime.compaction.strategy`):

- **Narrative** (default): prose summary, replaces the middle of the conversation with a synthetic `user`-role message. The Article-2-era shape.
- **Structured-slot** (tier-2, [#352](https://github.com/kstrat2001/darkmux/issues/352)): the compactor is called in JSON mode and emits a typed `StructuredCompactionOutput` (objective, current-truth, completed-decisions, errors-to-preserve, next-actions, verify-criteria), rendered as labeled markdown into a synthetic `system`-role message. Per-slot character caps bound each field. The default compactor prompt (the empirically-won "reality-discipline" prompt) frames every slot as *show, don't tell* to suppress the hallucination-class regressions earlier prompt versions produced.

The design bet behind structured-slot is that **a small model fills labeled slots more reliably than it writes good prose**, and that typed output degrades more gracefully. Three degradation layers make that real, in order:

1. **Lexical JSON repair** ([#401](https://github.com/kstrat2001/darkmux/issues/401) layer 1): a truncated compactor response (runaway escapes, an unterminated string, unbalanced brackets) is walked byte-by-byte and closed off, producing a parseable (if lossy) value rather than a dispatch bail.
2. **Schema patch** (#401 layer 2): if required fields are still missing after parse, safe defaults are inserted and `compaction_metadata.truncation_patched` is set so downstream analysis can flag the run.
3. **Escalation bound** ([#377](https://github.com/kstrat2001/darkmux/issues/377)): `reserve.bail_after_compactions` caps how many times one dispatch may compact; past the bound the runtime emits an `EscalationTriggered` terminal for frontier handoff rather than looping forever.

Two model-shape accommodations round it out: thinking-mode models route JSON to `reasoning_content`, so `extract_compactor_content()` falls back there when `content` is empty; and the JSON-mode request uses LMStudio's `json_schema` response format (decode-time shape enforcement), not OpenAI's looser `json_object`. The dispatch budget (turns/tokens used vs caps) is folded into the structured output's metadata so the model sees its remaining runway, framed as a *floor, not a ceiling*. Every field is darkmux-typed; `custom_instructions` is a typed field appended to the base prompt, not an `extras` passthrough.

## Runtime resilience: struggle detection + feedback injection

A local model in an agent loop fails in characteristic ways: re-reading the same file, re-reasoning the same dead end, hammering a tool that keeps erroring, emitting reasoning until it hits the token cap with nothing to show. The internal runtime carries a family of cheap, edge-triggered detectors for these, plus the recovery and budget machinery to act on them. Three design commitments shape the family:

- **Observability before intervention.** Each detector (cycle, reasoning-loop, tool-failure cascade, cadence-drift) writes a trajectory event and, by default, nothing else changes: the MVP is *visible struggle*, not auto-bail. `MAX_TURNS` and the inactivity deadline catch genuinely-stuck dispatches *late*; the detectors exist to surface the struggle *early*, for the operator and (via feedback injection) for the model.
- **Recover, don't discard.** When a turn hits the per-call token cap but emitted well-formed tool calls, those calls are salvaged rather than treated as a failed turn. A `finish_reason=length` turn with no content and no tool calls (pure runaway reasoning) is dropped, nudged, and retried within a small budget before escalating. Tool calls the model wrote as plain text (bracket, harmony, or darkmux's XML extension) are promoted back to structured calls instead of being lost ([#406](https://github.com/kstrat2001/darkmux/issues/406)). Each recovery is itself a trajectory event so bail/recovery rates stay visible.
- **Feedback injection is the model-facing half.** Detectors and recovery paths queue synthetic `[darkmux-runtime]`-prefixed `system` messages drained into the next turn's prompt: telemetry the model can act on, not just telemetry the operator reads after the fact. The bracketed prefix is the term-provenance contract (see the model-facing-prompt doctrine in [`CLAUDE.md`](CLAUDE.md)); per-signal wording is overridable per role via the manifest's `feedback_templates`, and the whole channel is disable-able with `DARKMUX_FEEDBACK_INJECTION=0`. The deadline and budget caps (`--max-turns` / `--max-tokens`, opt-in; `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` with a 75% soft warning before the host's 100% hard kill) are the coarse backstops underneath the fine-grained detectors.

The unifying principle is operator-sovereignty applied to the runtime: every detector is observable in the trajectory, every nudge is attributable to a named signal, every bound is operator-tunable, and nothing silently changes the dispatch without leaving a record of why.

## Execution: one substrate, four identities

> **Status: this section describes the TARGET, and the tree is partway to it.** Each claim
> below is marked *(holds)* where the code already works this way or *(target)* where it does
> not yet. That distinction is load-bearing: an architecture document that reads as present
> tense while describing an intention is how a stated rule becomes something every consumer
> quietly implements differently. Tracked as [#1979](https://github.com/kstrat2001/darkmux/issues/1979).

Every piece of work darkmux performs — a mission with forty steps, a one-shot `dispatch`, a
lab run — is the same substrate at a different scale. The reason that has not always been
visible is that four different identities were doing overlapping jobs with nothing saying
which answered which question.

### The ladder

**mission › phase › task › step › role execution.** A *run* is the umbrella, never a grain: one
top-level unit of work the operator started, in exactly three kinds (mission, dispatch, lab).
A *step* is a graph node; a *role execution* is what the node did. A `procedural.shell` step
has none; `dispatch.internal` has one; `dispatch.map` has one per collection item, and the
review pipeline's probe and judge steps have seats × draws. That 0..N cardinality is what
makes the step layer real rather than a wrapper.

The inner unit is named for the **role**, not the model, because the model is derived rather
than declared: `select_model(role, profile)` resolves it at dispatch entry, `DispatchOpts`
takes `role_id` as required and `profile_name` as an optional override, and an
endpoint-staffed seat has no local model at all. Role is the stable identity across local and
remote. `dispatch` is then unambiguously TOP-LEVEL — the verb, and the `RunKind` meaning *a
run consisting of exactly one role execution* — which is what stops one word naming both ends
of the ladder. The full definition, and its consequences for
attribution and escalation, is contract 8 in [`CLAUDE.md`](CLAUDE.md)'s cross-system contract
registry. *(holds — the vocabulary is stated and the hash routes are named for their run kind)*

### What each field means

The flow-record schema already carries enough to answer *whose work is this?* The problem was
never missing data; it was that nothing stated which field answers which question, so every
consumer picked its own subset.

- **`payload.step_id` — the canonical record-to-step attribution.** Stamped by the producer,
  not reconstructed by a reader. *(partly holds — `dispatch_internal`'s `stamp_step_id`
  does this for graph-step dispatches; other step-executing paths are the target)*
- **`session_id` — a producer-chosen GROUPING key, deliberately not an identity.** Its grain
  is per kind, and the variation is correct rather than accidental: step-scoped for a solo
  dispatch, task-scoped for fan-out siblings that must share a join key so a seat's tokens
  can be tied to its endpoint. It is opaque to consumers, never an operator-facing noun, and
  never the address of a page. *(holds as behavior; the "not an identity" part is the target
  — one consumer still addresses a page by it)*
- **`mission_id` — the authoritative outer scope, filtered first, always.** This is not
  optional tidiness. A task-scoped `session_id` hashes only the task id, which comes straight
  out of a mission config, so two concurrent runs of the same config produce colliding
  session ids and only the `mission_id` backfill tells them apart. *(holds)*
- **`handle` — a read-side fallback, permanently.** Archives are append-only and never
  rewritten, so every key that has ever been correct stays supported for reading. *(holds)*

### One resolver

Mapping a record to its step is a three-key chain — `payload.step_id`, then a step-scoped
`session_id`, then `handle` — with `mission_id` authoritative above all three. That chain is
correct and already written twice, once per language. What is wrong is that it is not
universally *used*: a second, ad-hoc resolver matches on step-kind strings with a silent
catch-all, and the step detail lens scopes by raw session id instead of by step.

The target is one resolver per language, bound by a shared fixture both test suites consume
so the two implementations cannot drift, and **no kind-keyed registry in any consumer** — a
consumer that switches on `step.kind` to infer record shape is the snowflake being deleted,
not a fix for it. Attribution must be inferable from records alone. *(target)*

### Dispatch names the top of the ladder, not the bottom

`dispatch` names the top of the ladder only: the verb, and the `RunKind` it produces. Naming
the INNER unit instead — the role execution — is what resolves the overload, and it is the
cheaper direction by a wide margin: the flow-record bookends `dispatch start` / `dispatch
complete` keep their historical spelling at both grains, so the four consumers that key on
them at the session grain (`runs.rs`'s `terminal_status_for_action`, the runs board's
representative-session pick, `cards.ts`'s fleet activity, `metaLine.ts`'s last-dispatch) are
untouched. Archives are append-only and grain is already recoverable from `session_id` and
`source`, so renaming the wire would buy a consumer migration for no behavioral gain. What
changes is the word used in code, docs and UI. *(holds for the vocabulary; the review
pipeline still wraps a whole multi-model crew in one pair, which is a separate defect)*

Utility work inside a role execution — compaction above all — is a **sub-execution**:
itself a role execution, of a utility role, attributed to its own role and model rather than
blended into the specialist's. Naming the unit for the role is what makes this compose
instead of needing a special case — a sub-execution is the same kind of thing as its parent,
one level in. Getting this wrong is not
cosmetic: filing the compactor's residency under the specialist is what makes a healthy run
report a model swap that never happened. *(target)*

### Where the substrate lives, and why that is the whole lesson

The substrate a serious run needs — host telemetry sampling, per-step records, budget
accounting, liveness bookends, the resolved-knob snapshot — belongs in the **shared
control-flow path every run already crosses**: the launcher and the scheduler. Not in an
importable type that each mission may or may not adopt.

This is not a preference; it is the measured outcome of trying it both ways in the same arc.
When the telemetry sampler moved into the launcher and per-step records moved into the
scheduler, every mission gained them with *zero changes in its own module* — a mission
author cannot forget what they never had to remember. When the same arc left a piece as an
importable type plus a paragraph of doctrine, it acquired exactly one consumer: the module it
was extracted from, importing it back under its old name.

**Moving a type one crate over and importing it back is a relocation, not a layering.** The
test of whether a capability is really shared is not where it is defined; it is whether a
mission that never mentions it still gets it. *(holds for telemetry, per-step records,
budgets and outcome; the staffing snapshot is the remaining piece that is still caller
choice)*

### The vocabulary, and why each word sits where it does

darkmux names a lot of layers, and the names were not arrived at freely — most of the obvious
ones were already spoken for. This is the map.

**The containment ladder.** `mission › phase › task › step › role execution`.

| Term | What it is |
|---|---|
| **run** | The UMBRELLA: one top-level unit of work the operator started. Never a grain. Three kinds — `mission`, `dispatch`, `lab`. |
| **mission** | A whole task graph, launched from a config. |
| **phase** | A grouping of tasks within a mission (`Mission.phase_ids` → `Phase.task_ids`). |
| **task** | A group of steps — and where resource ASSIGNMENT lives (role, profile, workdir, image), which is why a step inherits staffing rather than declaring it. |
| **step** | One graph node. Contains **0..N** role executions. |
| **role execution** | The inner unit: one role, running many turns until it stops. |
| **dispatch** | TOP-LEVEL ONLY — the verb `darkmux dispatch <role>`, and the `RunKind` meaning *a run consisting of exactly one role execution*. |
| **crew** / **crew member** / **position** | Who staffs a mission, and where each member sits. |
| **role** | A stance, a tool palette, and a system prompt. The declared identity of a role execution. |
| **profile** | The staffing registry entry: which model at what context, local or hosted endpoint. |
| **seat** | A staffed model position within a run (`MemberRecord`), with a `draws` count. |
| **draw** | One invocation of a seat. |
| **item** | One element of a `dispatch.map` collection. Reserved shape: an object with exactly the keys `system` (a string) and `item` is a per-item persona override, and the `item` field is the payload; every other value is the item itself (#2310 P1). |
| **session** | An INTERNAL join key tying a family of flow records together. Never operator-facing. |
| **workload** / **fixture** | Lab-only: the thing being run, and the pinned sandbox it runs against. |

**Why the inner unit is named for the ROLE and not the model.** The model is derived, not
declared: `select_model(role, profile)` resolves it at dispatch entry, `DispatchOpts` takes
`role_id` as required and `profile_name` as an optional override, and an endpoint-staffed seat
has no local model at all. Role is the stable identity across local and remote; the model is a
consequence of the profile. Naming it for the role also makes sub-executions compose — a
compactor's work inside a specialist's is simply another role execution, one level in, rather
than a special case needing its own rule.

**The mission metaphor is deliberate, and it has to close.**

darkmux's operator-facing vocabulary commits to the **NASA mission metaphor**. `Mission` and
`Crew` were canonical from the start; the rest is named to *complete* the metaphor rather than
to borrow from a software subculture.

Locked terms (decided 2026-06-22):

| Term | What it names |
|---|---|
| **Mission** / **Crew** | The work, and who staffs it. |
| **Debrief** | The post-mission review ceremony (`Stage::Debrief`). |
| **Lessons** | The durable engagement-context store — previously "knowledge". |
| **Cautions** | The auto-detected loop pathologies. Already on-theme: spacecraft carry a *Caution & Warning System*. |

And the metaphor closes, which is the point of it: **a mission's runs raise cautions → the
debrief distills them into lessons → lessons brief the next crew.**

**Why a metaphor rather than accurate jargon.** Metaphors endure because they are *coherent and
relatable*, not because they are literal. Xerox PARC and early Apple gave us the **Desktop**,
the **Trash**, **Files** — none of which are literally inside a computer. They lasted because
the metaphor was complete and drawn from a world people already knew.

Software-tribal terms fracture it, and each carries baggage the metaphor does not: a
*retrospective* imports Scrum, which not everyone practices and some actively dislike, and which
means nothing outside engineering; a *post-mortem* imports death. A whole metaphor is something a
person can hold. Half a metaphor is just inconsistency. We are not sending rockets to the moon,
but software ships with a rocket emoji, because the metaphor lands.

**How to apply it.** When naming any new operator-facing surface — a verb, a stage, a concept, a
file — prefer the term that completes the mission metaphor, and check that it *completes* rather
than merely coexists. Where a real NASA term exists, lean on it: "Lessons Learned" (NASA's LLIS)
is the authentic version of what dev culture gestures at with "retro notes".

**Reject "it is already there" as a naming argument.** Consistency with an unconsidered
placeholder is not consistency. `Stage::Retrospect` was renamed to `Stage::Debrief` on exactly
that basis — it existed, and existing was the only thing it had going for it.

**The boundary.** This governs the OPERATOR-facing surface only. Model-facing text — role
prompts, skill descriptions, the autonomous-dispatch preamble, feedback-injection templates —
defaults to AI-convention terminology instead ("the user", "system message", "tool calls"),
because a local model under clean dispatch context has no darkmux history to ground a metaphor
against. See the model-facing prompt doctrine in `CLAUDE.md`. The two rules do not compete; they
apply to different readers.

One consequence worth stating, since it is what put this section here: this doctrine was decided
and then lived nowhere in the repository for two months. A fresh agent session — the recursive
success criterion darkmux sets for itself — would have named new surfaces from dev jargon with
nothing to say it was wrong. A naming rule that exists only in someone's memory is not a rule
the project has.

**Names that are taken, and by what.** Every humanized word that reads naturally for "one crew
member's bounded piece of work" turned out to already name a *different* layer of this same
system — which is itself the finding, because those layers were named with the same instinct:

- **task** — the grouping layer above steps.
- **job** — the FLEET work queue (`fleet::WorkJob`, `ClaimedJob`, `claim_job`), where a job is
  a PHASE published to Redis and claimed by a peer machine. `WORK_JOB_SCHEMA_VERSION` makes it
  a wire contract, not just a word.
- **deployment** — the Azure hosted-model endpoint (`/openai/deployments/<name>`), surfaced in
  `darkmux doctor`'s own remedy text. Reusing it for the execution would re-fuse the exact
  thing choosing *role* over *model* was meant to keep apart.
- **activity** — the viewer's activity lanes.
- **assignment** — a Task's resource assignment.
- **turn** — one iteration of the agent loop; a role execution has many.
- **pass** — the review pipeline's probe/judge/verify passes.

`shift` and `stint` are genuinely unused and were weighed as more humanized alternatives;
`execution` won on precision and on composing cleanly for sub-executions.

**"Rule" already sits at three grains, and none of them touch.** The word slipped past the
discipline above — three subsystems own it, each with its own schema, and one of them even has
a `match` field like another's:

| Which "rule" | Where it lives | What it decides |
|---|---|---|
| **hook rule** | `config.hooks.rules[]` (`HookRule`: a `match` predicate + a target URL) | which FLOW RECORDS leave the machine, and to which receiver. Matched mechanically (`hook_match`); identified by position (`rule_index` on `hook.fired`). |
| **crawl pattern** | a crawl rule file (e.g. `swallowed-error`: `match`/`no_match` prose + `evidence`/`why_hint`) | what COUNTS AS A FINDING. Given to the model verbatim as `<pattern name="…">`; named by id in the manifest, the envelope, and a receiver's `rule` column. |
| **eureka rule** | `darkmux-eureka`'s `RuleDef`s (`RULES_SCHEMA_VERSION`) | what the detection engine flags, surfaced by `darkmux doctor`. |

The collision is survivable because the keys never meet — a hook rule's `match` is a record
predicate, a crawl pattern's `match` is instructions for a model — but prose that says "the
rule fired" is ambiguous in exactly the way this section exists to prevent. Naming discipline,
not renames: say **hook rule**, **crawl pattern** (the model-facing tag already says
`<pattern>`), and **eureka rule**. A fourth "rules" surface must pick a different word.

**The rule this leaves behind:** before naming a new layer, check whether the word already
names a different grain in this system. A word at two grains is the defect that produced this
whole section — `dispatch` meant both a top-level run kind and the innermost unit, and nothing
said so, so every consumer picked a meaning and they disagreed.

### Lab stays separate, deliberately

Lab runs write per-run-local artifacts and stay off the fleet flow stream. That boundary is a
measurement-integrity decision, not an inconsistency to be tidied away: the point of a lab
run is that it is reproducible in isolation, and a bench that quietly enriched the shared
stream would make its own numbers a function of what else the fleet was doing. Lab's one
genuine defect is a route naming its drill-in `run=` while `run` is the umbrella everywhere
else. *(holds)*

## Composability

darkmux is designed to live BELOW agent frameworks and ABOVE inference engines:

```
[ agent framework / frontier orchestrator: Claude Code, OpenClaw, Aider, Cline, … ]
                    |
                    v
               [ darkmux ]   (swap · dispatch · observe)
                    |
                    v
[ inference engine: LMStudio, Ollama, llama.cpp ]
```

darkmux is **not** a proxy that sits in the request path (an OpenAI-compatible router was the v0.2 plan and was deliberately *not* built; see the evolution above). It operates the layer instead of intercepting it: it swaps the resident stack, dispatches work through a runtime it owns, and emits the observability stream. No changes to the inference engine; the frontier orchestrator drives darkmux rather than routing through it.

## Configuration: visible defaults, gated features, secret carve-outs

darkmux's settings live in one file, `~/.darkmux/config.json`, resolved with a single precedence everywhere: **env var > `config.json` > built-in default**. The env layer survives as a live override (CI, tests, a one-off shell); `config.json` is the durable surface; the built-in default is the floor. The whole precedence lives in one module so a reader never has to wonder where a value came from, the same *operator sovereignty* principle the rest of darkmux is built on: every default overridable, every value's source explainable.

Three choices shape it:

**Visible defaults, not hidden code-defaults.** `darkmux init` writes the common knobs *into the file* with their default values, rather than leaving them implicit in the binary. The cost is that a default written today doesn't silently change on upgrade, but that's the point: the operator can *see* what's configurable without reading source, and *change* a default with a file edit instead of a recompile. A config meant to replace env-var sprawl has to be discoverable, or it isn't a config at all.

**Off-by-default features are `enabled`-gated blocks, not presence-gated.** Redis coordination and the audit log are written as complete blocks with `"enabled": false` and every connection knob populated. The block's *presence* doesn't turn the feature on; the `enabled` flag does. So the whole surface is discoverable (you see exactly what Redis would need) and one edit from on, without darkmux guessing intent from whether a `host` happens to be set.

**Secrets are carved out, never plaintext config.** A `config.json` is a file an operator writes, edits, and might share or commit. So the one thing it never holds is a password: the Redis password and the serve-daemon bearer token live in the macOS Keychain, read at runtime and wrapped so they can only ever reach a log redacted. `config.redis` holds the non-secret connection bits; the Keychain holds the secret. (One other carve-out, for a different reason: `DARKMUX_HOME`, the pointer that *locates* the config root, stays an env var because it can't live inside the file it's there to find.)

The schema is lenient on read (every field optional, unknown keys preserved), so a newer config never bricks an older binary and a hand-edited file never panics the CLI. Loud validation is `darkmux doctor`'s job, not the hot load path. Additive schema changes are a minor version bump; the operator's file keeps working across them.

## The command gate: darkmux runs your shell-outs, not its own

Some mission configs exist to run a command that changes something outside darkmux — approve a pull request, merge it, apply a deployment. They are ordinary `procedural.shell` graphs an operator wrote, shelling out to a tool the operator already has installed and signed in, exactly like the `lms` and `zed` shell-outs elsewhere in the binary.

**darkmux holds no credentials of its own.** That is the whole security posture, and it is what makes the gate necessary rather than paranoid: darkmux is borrowing the operator's authenticated tool. A config that can run `gh pr merge` on your behalf is a config that can merge a pull request with your identity, and nothing in darkmux authenticated to earn that.

So a config may declare a `cmd` — a name — and darkmux refuses to run it until that exact name appears in the operator's own allowlist:

```json
{ "cmd": { "enabled": true, "allowed": ["pr-approve", "pr-merge"] } }
```

It **fails closed on both counts**: `enabled: false` blocks every declaring config regardless of the list, and a name absent from the list is blocked even when the gate is on. `darkmux init` writes the block visible, disabled, with an empty list — darkmux ships no opinion about which commands exist.

**The gate knows nothing about what it is gating.** It compares one string against a list. It has no model of pull requests, no knowledge of any tool's subcommands, no notion of what "merge" means. That is deliberate: the operator's configs name their own commands, and the operator opts each one in.

This is why the field is `cmd` and not `gh_verb`, which is what it was called until schema 3.0. The mechanism was always neutral, but the *name* was not, and a name is what people build on: a GitLab user was allowlisting `mr-merge` under `gh.allowed`, and a config gating `terraform apply` — which wants this gate exactly as much — had to declare a GitHub-shaped field to get a check that has nothing to do with GitHub. Renaming it cost one schema major and zero migrations, because no document had declared it yet. Waiting would have cost both.

**One asymmetry is worth stating plainly, because it drove the migration's design.** The gate fails *open* for configs that declare nothing: a config with no `cmd` is never blocked, which is correct — most configs dispatch models and touch nothing outside darkmux, and requiring every one of them to declare a name would make the gate noise. But it means a config that *loses* its declaration silently loses its gate. So a document still carrying the old `gh_verb` key is a loud validation **Error**, never a quiet overflow into the forward-compat bag where unknown keys normally land. An unrecognized field is usually harmless; this one would specifically un-protect the thing it was added to protect.

## Hooks: how records leave the machine, and who is allowed to hold a credential

A hook is not a feature bolted onto the crawler or the review pipeline. It is a **`FlowSink` like every other** — the fourth child of the same tee that already fans a record out to the local day file, the audit chain, and Redis. Its `write` matches the record against operator-configured rules and appends matches to a per-rule on-disk outbox; a drainer thread POSTs them and advances a cursor only after a success.

Two consequences fall out of that placement, and both are the reason it was placed there. Every record kind is hookable with **zero producer-side awareness** — thermal transitions, tool calls, and mission bookends all became deliverable without one line of change at the site that emits them. And delivery is **at-least-once, durable across restarts**: the queue is a file, the cursor moves after the 2xx, and a receiver that is down is an outage to wait out rather than data lost.

**The receiver cannot always be adapted, which decides where transforms live.** When darkmux owns the receiver — the local crawl tracker — the honest shape is a thin adapter in the receiver: darkmux ships one wire contract (the flow record verbatim, schema-versioned, lenient on read) and the receiver projects it into whatever it stores. That stops being available the moment the destination is somebody else's SaaS. You get an API; you cannot put code inside Jira. So for anything not your own, the transform has to live on the **sending** side.

**Two orthogonal axes, and only one of them ever holds authority.**

| Axis | Job | Authority |
|---|---|---|
| **transform** (a `.jq` adapter) | the *shape*: record → request body | **none, by construction** |
| **transport** (`http` · `file` · `cmd` — `cmd` planned, not yet built) | the *destination and its credentials* | http: one Keychain header value · cmd: the operator's own CLI |

The split is the whole design. A transform is a **pure function** — it needs no filesystem, no socket, no subprocess — so it is given none of those. jq is the language because it *is* JSON-to-JSON with no I/O in its grammar: there is nothing to sandbox. Three properties follow. A crawl finding's `evidence` is a source line copied verbatim out of a repo under audit; through a shell-spawning adapter that is an injection target, and through jq it is a string, so the hostile-data class disappears. The transform receives only the record, so it **cannot** read a credential — the delivery path resolves those separately and the two never meet. And because the evaluation is in-process, the outbox's guarantee extends all the way to the real destination rather than stopping at a hop.

**What was rejected, and why, since each looks reasonable from a distance.** A *declarative template* with `{dotted.path}` substitution is safe and becomes a bad programming language the first time someone needs a conditional or a nested document — Jira's ADF `description` alone is enough to break it. *Executing an operator script as the transform* hands a pure function full operator authority — filesystem, network, spawn, Keychain — to do a job that requires none of it, and it walks around the command gate that already exists for exactly this class of thing. *Shipping named adapters for Jira, Slack, and friends* is the safest option of all and re-creates precisely the coupling this whole design refuses: the sender would own N destination schemas and every new API would be darkmux's maintenance. A *sidecar adapter service* on loopback works today with no new code, and quietly breaks the delivery guarantee — darkmux's `hook.fired` would mean "handed to a process that may have dropped it," so the retries and quarantine records would describe the wrong hop. It stays documented as the escape hatch for integrations that need their own state or batching, with the honest note that the operator owns delivery from that point on.

**The transport set is closed at three, and `cmd` is what makes closing it possible.** Two of the three ship today — `http` and `file`; **`cmd` is designed but not yet implemented**, a separately-gated packet, and the paragraph below describes the intended shape rather than current behavior. A rule naming only `cmd` is refused at load today, the same as any rule with no destination. `http` covers the ninety percent with a static credential — Jira, Slack, Telegram, PagerDuty, a webhook, an Azure Function key. `file` writes the delivery to disk instead of sending it, which is the no-network tier for testing an adapter end to end. `cmd` pipes the transformed body to an allowlisted program on stdin and **treats its exit code as the delivery ack**: zero advances the cursor, non-zero enters the existing retry and quarantine policy. That is what keeps at-least-once intact through an arbitrary destination, and it means the protocol library is the operator's own CLI rather than darkmux's source tree. SMTP is a script piping to `mail`; SQS and anything else SigV4-signed is `aws`, which already implements SigV4; gRPC is `grpcurl`; Postgres is `psql`; an Azure AD-protected endpoint is `az account get-access-token` and a curl. darkmux will not learn SigV4, OAuth refresh, SMTP, or gRPC natively — each would be the same coupling wearing a different hat. The one extension planned in advance is `auth: { cmd, ttl_seconds }`, an allowlisted command that prints a header value and is cached for its TTL, which covers every token-refresh case without darkmux implementing OAuth.

**Credentials follow the posture the command gate already states: darkmux holds none of its own.** A rule names a Keychain item; the item holds the **complete header value** — `Basic <base64(email:token)>`, `Bearer …`, whatever the destination wants — not a raw token to be assembled. darkmux therefore has no credential-formatting logic to get wrong and stays scheme-agnostic, and the value is redacted in every record, log line, doctor row, and dry-run dump. The exec transport inherits the same gate as any other shell-out: a **name**, not a path, refused until that exact name appears in the operator's own allowlist, spawned directly rather than through a shell, with the record on stdin only so that nothing derived from a crawled repository can reach a shell parser.

**One limit stated plainly rather than discovered later.** At-least-once is not idempotent, and a create-issue API has no idempotency key, so a lost response can produce a duplicate. The delivery id is stable across retries and an adapter can write it into a searchable field, but a genuine check-then-create needs two requests — which is the `cmd` transport's job, not the transform's.

## Crawl as a mission: the shapes and how data flows between them

Ratified with the operator in #2297 and landing in pieces (#2298 plan step, #2299 `enabled`, #2300 grow seam, #2301 launcher retirement, #2302 create-mods steps — **delivered**, #2303 admission). This section is the map: every record the crawl touches, its shape, who writes it, who reads it. Where a piece is not built yet, it says so, so a reader can tell design from delivery.

### The two documents: config is the shape, plan is the instance

**Mission config** (`templates/builtin/mission-configs/crawl.json`, schema 3.2) is the shape of the work and is the same file for every crawl of every repo. It declares `inputs` (the workspace spec path, the rule ids, sizing knobs), and phases holding tasks holding steps. The `plan` phase holds **one task per rule**, explicitly, each with a single `crawl.plan` step:

```json
{ "id": "plan-unnamed-predicate", "enabled": true,
  "steps": [{ "id": "plan-unnamed-predicate-step", "kind": "crawl.plan",
              "config": { "rule": "unnamed-predicate", "workspace": "{{workspace}}" } }] }
```

A task with `"enabled": false` is pruned when the run is minted and never exists in the run (see "Mission configs: a disabled step never exists in the run"). Ten rules are ten tasks; a nightly that wants six disables four; the run shows six. The config is the only place a run's shape comes from: no CLI override, edit the JSON and run, the snapshot records it.

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

### The `crawl.plan` step kind, and why there is exactly one

Control flow: load the workspace spec, materialize it (bare mirror plus checked-out tree, per source, at a recorded sha), run the rule's prefilter over the files its globs admit, cut a window around each hit, pack sites into units under the sizing knobs, write the plan, hand the plan's path downstream as the step's `output`. That flow is new, so by #1352's test it is a kind. It is Tier 3, co-located with the crawl module (`crates/darkmux-lab/src/crawl/plan_step.rs`), because no second mission plans.

Rules vary in **what a site is and who finds it**, not in how planning runs, so there is never a kind per rule. The site producer is a mux keyed by the rule's declared `prefilter` shape:

| shape | today | example |
|---|---|---|
| `["<regex>", …]` | implemented; the planner compiles and runs it | `unnamed-predicate`, `swallowed-error` |
| `{"command": "…"}` | **reserved, refused at rule load by name (#2297)** | semgrep, ast-grep, a linter emitting SARIF/JSON |
| none | whole files, sized by tokens | `read`-kind rules |

Semantic rules with no cheap prefilter (`doc-contradicts-code`) are the part a linter cannot do and the model is for. When the command shape lands, it is a `procedural.shell` step ahead of the plan step whose output is a site list; the plan step consumes sites in one shape regardless of who produced them.

### Typed step outputs

**Every value one step kind hands to another is a typed serde struct with a `schema_version`** — required fields plain, optional fields `#[serde(default)]` — never a free-form JSON blob and never a string protocol. The consumer deserializes through that struct, and **the read IS the check**: a producer that drifted fails at the read, by field name, instead of being silently summarized as zeros. Required-versus-optional is expressed in the body struct itself, so there is one place to look. Comparing two schemas at CONFIG time is only worth building when composition can wire two different families' outputs together; until then the read is enough.

The body rides in a thin envelope (`darkmux_crew::step_output::Output<T>`), so a consumer knows what it is holding before it looks:

```json
{ "schema_version": "1.0", "kind": "crawl.unit-outcome",
  "producer": { "mission": "crawl-…", "task": "unit-…", "step": "unit-…-step", "machine_id": "laptop" },
  "produced_at": "2026-09-04T…Z",
  "hash": "9f2c…",
  "body": { "schema_version": "1.0", "unit": "u-0001", "result": "stop", "findings": 2, … } }
```

`kind` is a CONTENT id the reader checks against the value it expects **before** deserializing `body`; a mismatch is a refusal naming both, which turns a mis-wired graph into one clear error instead of a confusing field error deep inside a body struct. A **data port's label is the same string as the `kind` its output carries** — `crawl.plan` provides `crawl.plan`, `crawl.unit` requires `crawl.plan` and provides `crawl.unit-outcome`, `crawl.summary` requires `crawl.unit-outcome` and provides `crawl.summary` — so a graph validator (#2312) can compare a producer's `provides` against a consumer's `requires` directly, with no rename table in between. The two concepts stay distinct (a port says where a value flows, `kind` says what is in it); they just agree on their spelling.

`hash` is blake3 over the body written in a canonical form — every object's keys emitted in sorted order, all the way down, arrays left in their own (meaningful) order — so field order can never change the digest, and `Output::read` recomputes it and refuses a mismatch. The sort is explicit rather than inherited from `serde_json::Map`'s default `BTreeMap`, because serde_json's `preserve_order` feature makes that map insertion-ordered and cargo unifies features across a workspace: in darkmux's own tree `agent-client-protocol` enables it, which made the digest stable under `cargo test -p darkmux-crew` and unstable under `cargo test --workspace` until the canonicalizer sorted keys itself. A hash whose value depends on who else is being compiled is not a hash. The reason is not tampering: a consumer must be able to tell a **complete** file from a partial one, and a **stale** copy from the current one, whatever moved it there. A length check cannot and a timestamp lies. Bodies (and plan files) are written once via tmp + rename and never rewritten, so a body whose hash disagrees is a truncated write or a copy that is not the one this run produced. A synced or shared filesystem (iCloud, a network share) is **never** the transport for a `ref` — those deliver partial files as ordinary reads, which is exactly the case this check names. When `body` lives in a file, the hash is of that file's body bytes.

A step's `output` is a string, so `Output::read` accepts an inline envelope, a `{"ref": {"path": "…"}}` pointer, or a bare path (the shape `crawl.plan` used before the wrapper, still read for the transition). The grow seam's `items_from_artifact` looks inside `body` when it finds an envelope and at the top level when it does not, so a pre-wrapper producer keeps working.

**Crawl is the pilot**, in that order: crawl (`crawl.plan` → `Plan`, `crawl.unit` → `UnitOutcome`, `crawl.summary` → `CrawlSummary`), then the coder phase, then review. Fleet transport comes after the wrapper exists everywhere: once every producer wraps, a `ref` can name a MACHINE as well as a path and be fetched from the producing machine's daemon, with the hash as the completeness check on arrival. No body struct changes when that lands.

**The drift guard is the exported types**, not a second hand-written schema. These structs derive `ts_rs::TS` behind each crate's `ts-export` feature and export into `ui/src/types/generated/` — the same generated file the viewer already consumes and CI already diffs (`bun run types:check`). No `schemars`, no hand-written zod: one definition in Rust, one generated TypeScript file, one `git diff --exit-code`.

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
      (metadata + emitted, destination owned by the hook)   <dispatch>/<seq>/finding.json         (brief_refs: [{finding, key}])
                                                                             │
                                                        dispatch --finding / --mod, or a mission step with brief_refs
                                                                             ▼
                                                        create_mod / mod create ──▶ ~/.darkmux/mods/<key>/ (kit + attachments)
```

Every arrow above the finding row is delivered. The grow arrow is #2300 (a task may declare `grow`; the generic launcher expands it at the phase boundary — see "Mission configs: a step's output grows the graph" below), and #2301 finished the picture: **the literal-routed crawl launcher is retired.** `src/crawl_launch.rs` is deleted, the `config_id == "crawl"` branch in `mission_launch::launch` is gone, and `crawl.json` declares the whole crawl — a `crawl.plan` task per rule, a `crawl.unit` GROW template per rule, and one `crawl.summary`. `darkmux mission launch crawl --param workspace=<spec.json>` is an ordinary generic launch. Its inputs are `workspace` (required), `rules` (which rule tracks to mint), `max_sites_per_unit`, `max_est_tokens_per_unit`, `no_fetch` and the generic `dry_run`; the launcher's own `source`/`rule` one-shot pair is gone (a one-shot crawl is a one-source spec file), and so are `plan` (a plan is always written under the run), `plan_out`, `units`, `limit` and `resume` (per-unit reuse becomes the scheduler's step-output reuse, #2303). `--param rules=a,b` prunes the tracks it does not name at mint, with reason `not_selected` in `graph-report.json` — the same mechanism `enabled: false` uses, so a run's graph is always exactly what will execute. Grown unit task ids are unprefixed (`unit-<rule>-<unit-id>`) while declared ids carry the mission-id prefix; that is the shape the phase record's `task_ids` holds. #2302 closed the last arrow: `crawl.json` declares a fourth phase, `create-mods`, whose one task is a grow template over the summary's `finding_refs` — one `coder` `dispatch.internal` step per finding, each carrying `brief_refs: [{"kind": "finding", "key": "<dispatch>/<seq>"}]` and the materialized tree the finding was observed in as its `workdir`, and each asked to record its change with `create_mod` naming that same key in `for`. **It ships OFF** (`"enabled": false` on the task, which prunes the task and then the emptied phase at mint), because the hook → tracker path is still the default exit. To turn it on, copy `templates/builtin/mission-configs/crawl.json` to `~/.darkmux/mission-configs/crawl.json` and set `"enabled": true` on that one task; `darkmux mission config show crawl` reports the gate per task either way. Two consequences are worth stating before an operator flips it. First, the enabled create-mods becomes the LAST phase, and the close-payload rule below promotes that phase's last step output only when it is a JSON OBJECT. A coder `dispatch.internal` step's output is the model's final text, not an object, so an enabled create-mods leaves `mission close` with `payload: null` and the `CrawlSummary` stops reaching it — the summary is still its own step's output on disk either way, no reader keys on `payload.findings` today, and a copy that wants the payload back ends the create-mods phase with its own summarizing task. Second, a `brief_refs` key that addresses no stored record REFUSES the step, loudly and before any container work, so a create-mods step that outran the finding tailer fails naming the key rather than dispatching a coder that never saw the finding. Tracks run in parallel under machine-aware admission (#2303), fail independently, and resume alone; a minted step records the plan it came from and, later, the admission decision that scheduled it, so the operator never wonders where a step came from.

### What each record is for

| record | written by | read by | key |
|---|---|---|---|
| `config-snapshot.json` | mint | provenance; `mission status` | mission id |
| `graph-report.json` | mint (prune) | `mission status`; the `mission start` record carries the same object | mission id |
| `plan/<rule>.json` | `crawl.plan` step | the grow seam (`grow.from`, #2300); `crawl.unit` (via `config.plan`) | mission id + rule |
| `Output<T>` envelope | every typed producer (#2301) | every typed consumer, through `Output::read` | `kind` + `hash` |
| `UnitOutcome` (`crawl.unit-outcome`) | `crawl.unit` step | `crawl.summary`; the run-detail lens (step output) | unit id |
| `CrawlSummary` (`crawl.summary`) | `crawl.summary` step | the `mission close` payload; the viewer's crawl surfaces | mission id |
| `FindingRef` (on `UnitOutcome.finding_refs` / `CrawlSummary.finding_refs`) | `crawl.unit`, from the same one read of the dispatch's `findings.jsonl` that counts them | the create-mods grow template (`items: "finding_refs"`); `brief_refs` resolution, by `key` | `<dispatch>/<seq>` (and `id`, the same key with `/` → `-`, because a task id is one segment) |

The close payload is a **generic** rule, not a crawl one: the launcher promotes the LAST phase's last step `output` to the `mission close` payload whenever that output is a JSON object (unwrapping an `Output` envelope's `body` when it is one). Before #2301 the generic path always closed with a `null` payload, so any config whose final step emits a JSON object now has one — read the config id, never infer the crawl shape from a payload's presence.
| `grown_from` on a grown step's `config` | the grow seam | the on-disk step record; `graph-report.json` carries the same triple per copy | `{task, item, index}` |
| `graph-report.json`'s `grown[]` | the grow seam (appended post-mint) | `mission status`; the `mission.grow` flow record carries the same facts | mission id |
| `dispatch.tool` record with `emitted` | the runtime, via `create_finding` | hooks (external trackers); `finding sync` | dispatch session + `emit_seq` |
| `findings/<dispatch>/<seq>/finding.json` | `finding sync` (the tailer) | `finding list/show`; `dispatch --finding`; brief refs | `<dispatch>/<seq>` |
| `mods/<key>/mod.json` + `attachments/` | `mod create`; `create_mod` | `mod list/show`; `dispatch --mod`; the integration mission | minted `mod-<secs>-<hex>` |

## Findings and mods: what was observed, and how it could change

Settled with the operator on 2026-09-03, after the first crawl findings reached a tracker and the first PR was made from them by an agent that knew nothing about crawls.

### darkmux is the worker

Imagine the tracker is GitHub. The orchestration layer above darkmux knows its job is to post a PR with every change that came out of crawl X. darkmux does not: it is the worker. It is asked to observe (a crawl, a review, a one-off dispatch with the right tool granted) and it is asked to make a change for a given observation, and it records both. It never reads a tracker, never decides which observations deserve a change, never opens a PR. Those are the orchestrator's, whether the orchestrator is a frontier session, a person, or a scheduled darkmux mission later on. The reason is modularity: every step of the loop has to be staffable independently, so that a local model can do the generation and thinking and a frontier model does only the packaging, or the reverse when a step turns out to need it.

### Two records, both opaque

A **finding** is what was observed. It is an event: it happened at a moment, from a dispatch, and it is never rewritten. Its key is `<dispatch>/<seq>` — the dispatch that produced it and the ordinal of the acceptance within that dispatch — which every finding has, crawl or not. A crawl adds context (mission, unit, rule, source, sha) when it launches the dispatch; nothing about a finding requires a crawl. The runtime tool that produces one is `create_finding` (renamed from `report_finding` on 2026-09-03: the tool *creates* a record; a hook is what *reports* it). darkmux does not interpret the emission: the record is metadata plus the model's arguments verbatim (`emitted`, see the flow schema's 1.33.0 entry), and a hook's transform composes whatever a destination needs from that. A finding's location is domain-specific — a line for text, a page for a PDF, a rect for an image — so no field for it exists on darkmux's side.

A **mod** is how something could change. It is a *kit*: instructions plus data, in whatever form the proposer chose — a diff, a sentence, pixel data, a config value — enough for an AI to make the change correctly later, given the mod's own context. darkmux never types a kit and never opens it. A mod has its own minted key and its own store; it may carry provenance, `for`: zero or more finding references. That is the only stored link between the two records, it lives on the thing created later, and it is a list, because one change can address three observations and one observation can attract three competing changes. The view from a finding to its mods is derived by scanning mods, never stored on the finding.

Two producers write the same mod record, and both exist: the CLI, for a change made outside darkmux (`darkmux mod create --by <actor> [--for <finding>]... --kit ... [--attach ...]`), and the runtime tool `create_mod`, for a change made inside a dispatch (its emission rides the same `dispatch.tool` record a finding's does, and the host materializes the record from it — attachments included, since no host path reaches the container's copy of the file). Whoever made it, the record names the proposer and the time. A mod is written even when part of it could not be kept — an attachment that did not decode, a `for` key that addresses no finding — with the reason recorded on the mod itself, because the kit is the product of the work and a malformed sibling field is not a reason to lose it.

### Why the key is minted per mod

Two agents review the same finding at different times. One proposes the code change; the other recommends a comment. Both are valid; they may overlap, conflict, or compose. The record keeps both, judges neither, and leaves the question to whatever integrates them. A key derived from the finding would have made the second overwrite the first.

### Verbs, and what is deliberately absent

`finding list` / `finding show` read the store (the flow stream stays the audit trail; the directory is the queryable copy — JSON on disk is the truth, as with roles). `finding sync` is that store's second producer: it replays the flow stream into the store for anything the live dispatch tailer missed — an older binary, a killed process — and is idempotent because the store is write-once, so the two producers can race without ever disagreeing. `mod create` / `mod list` likewise, and `mod show <key>` prints one mod whole with its kit raw. **Verbatim means byte-exact**: a kit is stored as the text that was written and is never parsed on write, because parsing and re-serializing a JSON-looking kit silently collapses duplicate keys and rounds large integers — a kit is not darkmux's data to normalize. A `for` key is canonicalized to `<dispatch>/<seq>` on create, so one finding has one address; a key that can address no finding is refused rather than stored as a link nothing can follow. `dispatch <role> --finding <key>` appends the finding's stored record to the brief, verbatim, so a role has the *what*; its palette decides whether it may `create_mod`.

Both record kinds reach a brief the same way, and the mechanism is a **step config field, not a verb**. A `dispatch.internal` step carries `brief_refs: [{"kind": "finding"|"mod", "key": "..."}]`, and the step kind that runs it is where each ref is resolved against its store, rendered as a block the model can ground, and appended verbatim after the user's own message, in the order given. That placement is the whole point: the step kind is the single place every producer converges on, so a mission graph that sets the field gets exactly the brief the `dispatch` verb does. The verb's `--finding` / `--mod` flags only write the field (and check the keys early, so a typo refuses before the acknowledgment gate rather than one layer down); the rendering happens once, in one place. A key that addresses no stored record fails the step before any container work, so a dispatch never runs on a silently missing block. A mod ref also bind-mounts that mod's `attachments/` read-only at `/darkmux-mods/<key>/attachments` — the path its block names, from the same constant — after the key is re-validated and the host path is proven to resolve inside the mod store. The refs are **darkmux record kinds only**, never an arbitrary file: the workspace mount is the file channel, and a ref is provenance-bearing by construction. One gap is named rather than papered over: the fleet work queue's job shape carries no refs, so a `--machine` dispatch that names one is refused instead of routed without its blocks.

There is no `integrate` verb. If darkmux integrates mods, that is a mission: a shared workspace, one step per mod applying its kit onto the accumulating change and handing the workspace to the next, a failed step failing itself and not the mission. It composes from existing pieces — a worktree step, shell or coder steps — so the concept lives in a mission config, not in the CLI. Each step names its seat, which is what lets the operator decide, per integration, whether a local model or a frontier model does it.

### What this makes measurable

Tokens on the local seat versus the frontier seat per crawl PR. The frontier-only baseline (a Sonnet agent doing both creation and integration for seven findings, PR #2285) is the number every local-seat experiment is compared against.


### Mission configs: a disabled step never exists in the run

A phase, task or step in a mission config may carry `enabled: false`. It is pruned when the run is minted, before anything is interpreted or persisted, so the run's graph is exactly what will execute. There is no gray state: a nightly crawl config with ten plan tasks and six enabled shows six. A task whose steps were all disabled goes with them, a phase whose tasks all went goes too, and a task whose every dependency was pruned is pruned in turn, while one live dependency keeps it and it simply sees fewer inputs. Provenance is the resolved-config snapshot the run already keeps, which carries the flags verbatim, plus a `graph-report.json` beside it naming what was declared, what was minted, and each pruned item's reason; `mission status` prints the one-line count and the `mission start` record carries the same report. There is deliberately no CLI override. The config is the only place a run's shape comes from: edit the JSON, run, and the snapshot records it.

### Mission configs: a step's output grows the graph

A task may declare `grow` instead of being a task:

```json
"grow": { "from": "plan-task", "items": "units", "id": "{{item.id}}",
          "config": { "unit": "{{item.id}}", "rule": "{{item.rule}}" } }
```

The task is then a **template** and is never minted. After the phase containing `from` completes, the launcher reads that task's last step `output` as a path to a JSON file — the contract every producing step honors — loads it, takes the array at `items`, and mints one copy of the template, with all its steps, per item. `{{item.<field>}}` renders from the item's own top-level scalar fields, into the copy's id suffix and into every step's config; a whole-string placeholder keeps the field's JSON type, so a number stays a number. Zero items mints zero copies, and the phase is explicitly started and completed with `grew_nothing` recorded rather than failing — a phase with no steps is invisible to the step-driven phase open/close, so without that it would sit `Planned` all run and be swept to `Abandoned` by the finalize backstop, recording a failure where the plan simply planned nothing. Every other way this can go wrong — the producer never ran, produced no output, named a path that isn't there, or wrote a shape the template didn't ask for — is a loud error naming the task and the path, never a quiet zero. That is deliberate: the retired `expand` primitive (schema 1.1–1.4) shipped for two schema versions silently expanding to nothing, and the whole point of `grow` is that its input is produced by the run rather than handed in at launch.

**Growth happens at a phase boundary, and that is a real trade.** `run_step_graph` takes its task map by shared reference, so the graph cannot grow mid-run. The generic launcher therefore runs one graph call per phase, in config order, and expands a phase's templates just before minting it. The cost: two phases with no edge between them no longer overlap. That is acceptable because phases are already sequential by design here (`phase_order` and the lazy phase-close logic both assume a strictly linear order), and parallelism lives *inside* a phase, where the wave scheduler still runs every independent task concurrently.

Provenance is on the record, not in the operator's head: every grown step's `config` carries `grown_from: {task, item, index}`, and the item's `rule` lands on `config.rule` through the template. Neither is *rendered* yet — the graph lens builds its step rows without `config`, and `finding list` reads the unit and rule out of a finding's own context — so surfacing them in the viewer, which is what would let an operator filter a run by track, is follow-up. What is readable today: the run's `graph-report.json` gains a `grown` entry per growth event naming the template, the producer, the artifact path, the item count and the real task ids minted; one `mission.grow` flow record carries the same facts live; the phase record's `task_ids` lists the grown tasks alongside the phase's declared ones (the generic launcher writes that field now, matching `crawl_launch.rs`); and `mission status` prints "grew N task(s) from `<from>`".

## ACP: darkmux inside the editor

`darkmux acp` speaks the [Agent Client Protocol](https://github.com/agentclientprotocol/agent-client-protocol) over stdio, so an editor like Zed can drive darkmux from its own agent panel — you type `/review` in the editor and a local crew works the PR, with progress rendering in the panel rather than a terminal you have to go find.

**It is still labeled a spike in its own module docs, and that label is honest.** What works, works in Zed today; what is spike-grade is named in `src/acp.rs` rather than papered over. The most fragile piece: review-stage progress is recognized by pattern-matching known substrings out of the review subprocess's stderr, so a wording change in the pipeline's liveness markers silently stops advancing the on-screen plan. The honest reason it survives is that no structured progress channel exists to read instead — teaching the review pipeline one is the real fix, and it is a feature, not a spike's job.

**Commands come from the mission-config registry, not from code.** `session/new` enumerates the same merged registry `darkmux mission launch` uses (built-ins plus `~/.darkmux/mission-configs/`) and advertises every config that declares a `panel` block. Adding a slash command to your editor is writing a JSON file — no rebuild, no registration call, no darkmux release. The panel block's *presence* is the signal; a config without one stays launch-only.

How an invoked command runs is decided **structurally, never by matching an id**:

- a config whose graph uses review-pipeline step kinds takes the bespoke review path
- a config whose graph dispatches **no models at all** runs as an ephemeral in-process graph — no mission record, no run artifacts, because a command that shells out and prints a result is not a mission and recording it as one would pollute the mission board
- anything else is a real `mission launch`

The test is the same one `mission launch` itself uses, so the two entry points can't drift into disagreeing about what a config is.

**A long-lived agent process needs a way to stop.** ACP gives an agent no disconnect notification, so a naive implementation leaks a session per editor thread forever. `session/close` is advertised and handled: it aborts anything in flight for that session and drops its state. Because a client may never send it, there is also a process-level backstop — the whole process exits once nothing has been in flight for `runtime.acp_idle_exit_minutes`. `session/cancel` shares the same abort-handle registry, so a cancelled command is genuinely aborted rather than left running with its output discarded.

**`darkmux radio` is the terminal twin.** It routes free text onto exactly one advertised command through a bounded local classification dispatch, then executes it — one routing call, one execution, no loop. It prints the route it chose (`radio: routing to /<id> — from your text`) *before* executing, so the choice is never silent, and text that doesn't map cleanly onto one command **refuses and lists the options** rather than guessing. A router that guesses wrong on a command that mutates external state is exactly the failure the command gate above exists to prevent, so it declines instead.

## How we decide

darkmux's design decisions are **grounded in data and in published research where it exists**: we'd rather cite a measurement or a paper than assert from intuition. The framing is *convergence, not priority*: independent research and this project keep arriving at the same architecture (fresh-context review, verifiable-check termination, structured compaction), and the citations explain *why* it works. See the roadmap's [*How we decide*](ROADMAP.md#how-we-decide) for the citation-verification discipline (every cited source re-fetched and confirmed; a confident citation under a correctly-recalled label is exactly where fabrication hides).

The data comes from three places, and the lab notebook captures the *evidence* behind each call so the reasoning survives even when the underlying work is private:

- **Lab runs**: reproducible workloads against registered fixtures, with content-hash proof that two runs started and ended in the same state. This is where harness hypotheses get tested one variable at a time (baseline → single change → re-measure → compare → record).
- **Bake-offs**: documented per-hardware-tier model comparisons with criteria fixed before the runs.
- **Dogfood**: darkmux run against real work, including darkmux building itself through `mission launch` and a private fintech engagement's production services. The failure modes those runs surface (a fabricated sign-off, a confidently-wrong review, a doom loop) are the specs for the next hardening pass. The *data* is what's load-bearing; the sensitive work behind it never has to appear here.

When a decision can't point to a measurement, a citation, or a dogfood observation, that's a flag, not a reason to ship it on intuition.
