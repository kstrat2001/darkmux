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

**mission › task › step › role execution.** A *run* is the umbrella, never a grain: one
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

### Dispatch means one specialist execution

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

## How we decide

darkmux's design decisions are **grounded in data and in published research where it exists**: we'd rather cite a measurement or a paper than assert from intuition. The framing is *convergence, not priority*: independent research and this project keep arriving at the same architecture (fresh-context review, verifiable-check termination, structured compaction), and the citations explain *why* it works. See the roadmap's [*How we decide*](ROADMAP.md#how-we-decide) for the citation-verification discipline (every cited source re-fetched and confirmed; a confident citation under a correctly-recalled label is exactly where fabrication hides).

The data comes from three places, and the lab notebook captures the *evidence* behind each call so the reasoning survives even when the underlying work is private:

- **Lab runs**: reproducible workloads against registered fixtures, with content-hash proof that two runs started and ended in the same state. This is where harness hypotheses get tested one variable at a time (baseline → single change → re-measure → compare → record).
- **Bake-offs**: documented per-hardware-tier model comparisons with criteria fixed before the runs.
- **Dogfood**: darkmux run against real work, including darkmux building itself through `mission launch` and a private fintech engagement's production services. The failure modes those runs surface (a fabricated sign-off, a confidently-wrong review, a doom loop) are the specs for the next hardening pass. The *data* is what's load-bearing; the sensitive work behind it never has to appear here.

When a decision can't point to a measurement, a citation, or a dogfood observation, that's a flag, not a reason to ship it on intuition.
