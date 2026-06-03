# darkmux concepts — the source of truth

**Status:** living reference · **Last verified against code:** 2026-06-01 (`main`)

This is the canonical model of *what darkmux is made of* — role families, the
mission/sprint lifecycle, the internal runtime, telemetry, and the flow stream.
Guides, the README, and agent doctrine should defer to this doc when they
describe a concept; if another doc contradicts it, this doc (and the code it
cites) wins, and the other doc is the bug.

### How to read this doc

Every concrete claim cites the code it comes from (`path:line`), so the doc is
self-verifying — if a citation no longer matches, the doc has drifted and that's
a bug to fix, not a footnote. The one discipline that matters most here:

> **Shipped vs. planned is load-bearing.** A capability described in the present
> tense is in the current binary. Anything not yet built lives in
> [§8 Planned](#8-planned--not-yet-shipped), clearly fenced and issue-linked.
> This doc never describes unbuilt work as if it ships today — that's the
> failure mode it exists to prevent.

When in doubt, the code is the source of truth (per `CLAUDE.md`). This doc is the
*map*; the crates are the *territory*.

---

## 1. What darkmux is

darkmux is a pre-1.0 Rust CLI for operators running local LLMs on Apple Silicon.
It does three things:

1. **Profile multiplexer** — `darkmux swap <profile>` switches the loaded model
   stack (model + context length + compaction settings) to a named profile.
2. **Lab harness** — `darkmux lab run <workload>` dispatches a workload and
   records timing + trajectory + verify outcome, so empirical claims are
   reproducible.
3. **AI-first local-AI orchestrator** — darkmux dispatches local *utility* and
   *specialist* agents internally (compaction, mission proposal, sprint
   estimation, code review) and records every dispatch to an auditable flow
   stream.

The strategic reasoning loop lives in your **frontier orchestrator** (Claude
Code today). darkmux operates the local tier as a self-contained capability the
frontier dispatches to. The rest of this doc is the vocabulary that makes that
division of labor concrete.

---

## 2. Role families — specialist vs utility

darkmux defines exactly **two** crew role families. The defining axis is
**scope**: a *specialist* works the **mission/sprints** (the deliverable); a
*utility* role supports the **runtime**, *outside* mission scope (compaction,
mission-compiling, estimation). The family also governs **how a role is
dispatched** (the table below). Bias toward utility for as much outside-mission
work as possible, so the runtime isn't loading extra models for support
tasks.

| Family | What it is | Dispatch shape |
|---|---|---|
| **specialist** | Works the mission/sprints — judgment-dependent, multi-turn agent-loop roles (coder, code-reviewer, analyst). Free-form output. | Runs the full agent loop; the autonomous-dispatch preamble (`templates/builtin/AUTONOMOUS_DISPATCH_PREAMBLE.md`) is prepended to the system prompt. |
| **utility** | Supports the runtime, outside mission scope — `mission-compiler` today; `compactor`/`estimator`/`scribe` joining (#590). Bounded I/O, structured output, no agent loop. | A single bounded transform; no preamble. |

- The family is carried by the optional `role_family` field on a role manifest
  (`crates/darkmux-crew/src/types.rs:161-174`).
- `is_specialist()` returns `true` for any role whose `role_family` is **not**
  explicitly `"utility"` — an absent field defaults to specialist
  (`crates/darkmux-crew/src/types.rs:213-214`).
- **mission-compiler is the single canonical utility role today** — it is the
  only built-in with `role_family: "utility"`; all other built-ins omit the
  field and default to specialist
  (`templates/builtin/roles/mission-compiler.json`;
  `crates/darkmux-crew/src/loader.rs:1230-1242`).

### "utility" is a role *family*, not "the compaction role"

A utility role is defined by its **scope** — supporting the runtime *outside*
mission work — and typically carries a bounded work shape (structured I/O, low
per-call failure cost). The mission-compiler turns unstructured intent into a
structured Mission + Sprint proposal; the same family is home to estimation,
scribe, and (in transition, #590) compaction. The util tier is a baked-in
runtime **affordance** — the runtime can always summon a util model for these
built-in tasks; *which* model is config, *whether* it's resident is
resource-dependent. Compaction is one of N such tasks, not the definition of
"utility."

### The compactor: runtime function today, utility role in transition (#590)

Compaction runs *inside* the agent loop (§5), and **today** there is no
`compactor` crew role — the fourteen built-in roles (`coder`, `scribe`,
`code-reviewer`, `analyst`, `voice-editor`, `design-reviewer`, `test-designer`,
`lab-runner`, `mission-compiler`, `trip-researcher`, `logistics-coordinator`,
`health-research`, `fitness-coach`, `legal-research`) include none named
`compactor` (`crates/darkmux-crew/src/loader.rs`), and the compactor model is
selected via the `ModelRole::Compactor` slot in the profile
(`crates/darkmux-types/src/lib.rs`).

> **Direction (#590):** that coupling is being undone. The compactor becomes a
> standalone **utility role** (alongside `mission-compiler`/`scribe`); its model
> is registered via the `[internal] utility` binding (#450) instead of a
> `ModelRole::Compactor` profile slot; and `ModelRole` is removed — so a
> **profile becomes a wrapper for its models' capabilities**, not a
> `Primary`+`Compactor` stack. The runtime keeps a built-in *affordance* to
> summon the util model for compaction (and estimation, planning, …).

### Legacy `admin` is rejected, with no compat alias

The role family was renamed from `admin` to `utility`. The loader **rejects**
`role_family: "admin"` at load time via `validate_role_family()`, with distinct
operator-actionable error messages for user-authored vs. built-in manifests
(`crates/darkmux-crew/src/loader.rs:379-398`). There is no compatibility alias —
consistent with the pre-1.0 no-compat-baggage posture.

---

## 3. Mission → Crew → Sprint

The structured-work model. **Note the actual field shapes** — this is where
informal summaries tend to drift.

### Mission

A mission groups a body of work. Its fields
(`crates/darkmux-crew/src/types.rs:304-325`):

- `id`, `description`
- `status` — `Active` / `Paused` / `Closed` (default `Active`)
- `sprint_ids: Vec<String>`
- timestamps: `created_ts`, `started_ts`, `paused_ts`, `closed_ts`

There is **no `scope` field and no `goal` field** — a mission carries a
`description`, not a separate goal/scope pair. "Scope" appears in the codebase
only in comments, referring to the filesystem working directory (`--workdir`),
never as a mission attribute (`src/mission_propose.rs:196-197`). This is
deliberate: per `CLAUDE.md`, engagement context (the *why*, local-vs-fleet
framing, nuance) lives in the frontier orchestrator and the input prose — it is
**by design not a CLI field**. The mission carries structure the local tier can
act on; the engagement-level intent stays with the frontier.

### Crew

A crew is a **static manifest** — `id`, `description`, and
`members: Vec<CrewMember>` where each member is `{ role_id, position }`
(`Lead`/`Support`) (`crates/darkmux-crew/src/types.rs:284-290`). Crews are
indexed and queryable.

**Crews are not dynamically composed per mission today.** A mission has no
`crew_id` field; `darkmux mission dispatch` takes an explicit `role` and fans the
mission's ready sprints out onto the single global work stream, where the first
available worker claims each one (#590). The operator names the role per
dispatch; dynamic per-mission crew assembly is
[planned](#8-planned--not-yet-shipped), not shipped.

### Sprint

A sprint is one unit of work within a mission. Its fields
(`crates/darkmux-crew/src/types.rs:339-366`):

- `id`, `mission_id`, `description`
- `status` — `Planned` / `Running` / `Complete` / `Abandoned` (default `Planned`)
- `depends_on: Vec<String>` — sprint ids this one waits on
- timestamps: `created_ts`, `started_ts`, `completed_ts`, `abandoned_ts`

There is **no `estimate`, `assignee`, or `burn-down` field** on a sprint.
Estimation is a *pre-dispatch verb* (`darkmux sprint estimate`, §4), not a stored
sprint attribute; burn-down/remaining-work tracking is
[planned](#8-planned--not-yet-shipped).

### Lifecycle state machines

Both lifecycles are implemented (`src/main.rs:1321-1392` — sprint verbs at
`1321-1349`, mission verbs at `1355-1392`):

- **Mission:** `Active ⇄ Paused → Closed` (terminal). Verbs: `start`, `pause`,
  `resume`, `close` — each persisted with operator reasoning.
- **Sprint:** `Planned → Running → Complete | Abandoned`. Verbs: `start`,
  `complete`, `abandon`.

`darkmux mission add-sprint` inserts a sprint with cross-reference validation
(mission exists, `depends_on` ids resolve, no id collision)
(`src/main.rs:584-611`).

---

## 4. Crew dispatch verbs

The AI-built-in verbs compose CLI primitives with utility/specialist dispatches.
All of the following are **shipped**:

| Verb | What it does | Where |
|---|---|---|
| `darkmux mission propose` | Reads unstructured intent → dispatches the **mission-compiler** utility role → renders a Mission + Sprints proposal → **mandatory operator approve/edit/reject/regenerate gate** → persists only on approval (atomic rollback on partial write). | `src/mission_propose.rs`; `src/main.rs:542-577` |
| `darkmux sprint estimate` | Reads a `WorkloadSpec` → deterministic per-turn token math → recommends the smallest adequate profile → optional 4B narration. | `src/sprint_cli.rs:233-310, 500-514`; `src/main.rs:459-471` |
| `darkmux sprint review` | Auto-detects base branch → computes the git diff → dispatches the **code-reviewer** role → parses the `QA-REVIEW-SIGNOFF` block (`BLOCK`/`FLAG`/`NIT`) → emits a verdict (`clean` / `flags-only` / `blockers`, or `indeterminate` when the reviewer output doesn't match the expected format). | `src/sprint_cli.rs:683-970`; `src/main.rs:472-485` |
| `darkmux mission dispatch` | Loads a mission, validates status, confirms the role exists, fans out its ready sprints (`depends_on == []`) as work jobs onto the single global fleet work queue (`darkmux:work`); waits or returns session ids. | `src/main.rs:642-662`, `1453-1721` |
| `darkmux crew dispatch <role>` | Single-turn dispatch to a named role through the internal runtime (default) or `--runtime openclaw`. | see `CLAUDE.md` → operator-facing commands |

The **operator-approval gate on `mission propose`** is the sovereignty contract
in action: the utility agent proposes structure; the operator approves before
anything is written.

---

## 5. The internal runtime + compaction

`darkmux crew dispatch` and `darkmux lab run` default to the **internal runtime**
— a per-dispatch `darkmux-runtime` Docker container running an in-house Rust agent
loop (`runtime/src/loop_runner.rs`). `--runtime openclaw` opts into the openclaw
shell-out path instead.

### Compaction is a runtime primitive, not a crew role

Compaction runs *inside* the agent loop, invoked synchronously after each
tool-call turn (`runtime/src/loop_runner.rs:826-849`) — it is not a separately
dispatched role. Two strategies (`runtime/src/compaction.rs:396, 498`):

- **Narrative** — a prose summary from a companion compactor model, inserted as a
  synthetic user message.
- **StructuredSlot** — a JSON-mode typed fact schema rendered to labeled markdown
  as a synthetic system message; the raw JSON is persisted to disk for replay and
  methodology research.

Both **replace the middle conversation slice**, preserving `PRESERVE_HEAD = 2`
leading and `PRESERVE_TAIL = 4` trailing messages
(`runtime/src/compaction.rs:65-70`).

Supporting mechanisms, all shipped:

- **Companion compactor model** — a 4B-class utility model by default, selected
  via `ModelRole::Compactor` in the profile. Compaction config is passed as
  explicit CLI args (`--compact-threshold-tokens`, `--compact-strategy`,
  `--compactor-model`, `--compactor-custom-instructions`) sourced from
  `profile.runtime.compaction.*` — **not** env vars
  (`runtime/src/compaction.rs:28-30, 165-269`).
  *Direction (#590): the compactor model moves out of the `ModelRole::Compactor`
  profile slot into the `[internal] utility` binding (#450); compaction becomes
  one of several built-in runtime util affordances that summon the
  config-registered util model.*
- **Two trigger modes** (whichever fires first): an absolute token threshold, and
  a ratio trigger (`latest_prompt_tokens >= context_window * threshold_ratio`)
  (`runtime/src/compaction.rs:367-381`).
- **JSON repair (#401)** for truncated compactor output in two layers — a
  balance-only lexical layer that closes unterminated strings and balances
  brackets (`runtime/src/json_repair.rs`), then a schema-level patch that inserts
  safe defaults for missing required fields (`patch_missing_required_fields` at
  `runtime/src/compaction.rs:850`, applied in `call_and_parse` around `789-820`).
- **Escalation bound** — when the compaction count reaches the operator-configured
  `bail_after_compactions`, the loop returns
  `EscalationTriggered(CompactionLimitReached)` rather than continuing
  (`runtime/src/loop_runner.rs:930-948`).

---

## 6. Telemetry + behavioral detectors

### What is measured per dispatch

The runtime records per-dispatch metrics — a top-line summary in `metrics.json`
(the `Metrics` struct) plus per-event detail (including token `usage`) in
`trajectory.jsonl` (`runtime/src/trajectory.rs:32-58`;
`runtime/src/loop_runner.rs:181-194`):

- **turns** (model-completion count)
- **`total_prompt_tokens`** and **`total_completion_tokens`** — **absolute token
  counts**, not a percentage of the context window
- **compaction count**

> Context-usage *as a percentage of the loaded model's window* is **not** a
> stored metric — only absolute counts are recorded. A "% of window" view is
> something a viewer derives from absolute tokens + the loaded model's `n_ctx`;
> treat any such chart as a derived visualization, not a captured signal.

There is **no operator-facing "token budget."** The one cumulative-token control
is an opt-in **internal safety boundary** (`#423`): set
`DARKMUX_RUNTIME_MAX_TOKENS` and a dispatch that exceeds it **escalates to the
frontier tier** (`EscalationTriggered(CumulativeTokensExceeded)`) rather than
continuing. Default is `None` (unlimited); it is never surfaced to the model as a
budget metric (`runtime/src/loop_runner.rs:80-88, 414-423`). (The pre-`#457`
hardcoded `250_000` constant was removed in favor of this opt-in.)

### Behavioral detectors (observability-only)

Three struggle detectors run during a dispatch. **All three are
observability-only in the current MVP** — they emit trajectory events and queue
model-facing feedback nudges, but do **not** bail, escalate, or otherwise change
dispatch behavior on their own (`runtime/src/loop_runner.rs:245-259`):

| Detector | Fires when | Default threshold | Where |
|---|---|---|---|
| **Cycle (#418)** | the same `tool + canonical_args` hash recurs | K=3 within a window of N=10 recent tool calls | `runtime/src/cycle_detector.rs:37-130` |
| **Tool-failure cascade (#419)** | one tool + signature fails consecutively | N=3 in a row | `runtime/src/failure_rate.rs:38-136` |
| **Reasoning-loop (#461)** | normalized (lowercased, whitespace-collapsed) reasoning content hashes identically | N=3 within a window of 10 turns | `runtime/src/reasoning_loop.rs:66-206` |

Separately, a host-side **inactivity watchdog** hard-kills a container that emits
no proof-of-work signal within `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` (default 600),
with a soft model-facing warning at 75% (see `CLAUDE.md` → environment variables).

---

## 7. Flow records + the serve daemon

### Machine (≡ node)

A **machine** is one operator-named host in your fleet (e.g. `studio`, `laptop`):
a single unified compute pool, reached at one Tailnet address, serving one local
LMStudio endpoint. On Apple Silicon that mapping is exact — one machine = one
integrated GPU + one shared-memory RAM budget + one inference endpoint
(`crates/darkmux-hardware/src/lib.rs` models one `total_ram_gb` +
`has_unified_memory`, no per-GPU fields). Scaling means *adding machines*, not
splitting GPUs within one.

**"node" is a synonym for "machine."** darkmux has no separate node concept — the
only fleet unit is `MachineEntry` (a logical `id` + a Tailnet `address`; the
machine-capacity tier field was dropped in #590). If "node" shows up in
discussion, read it as "machine." The distinction
that *would* earn its own word is a single machine hosting **more than one
inference endpoint** (a CUDA eGPU tier, or several LMStudio instances on
different ports) — out of scope today; darkmux models a machine as one endpoint,
the LMStudio default. That multi-endpoint case is tracked in #597, where the
right noun is **`endpoint`** (not `node`): #590's capability layer already
handles the *routing* (a model offers `vision`, a role requests it), so only
per-model endpoint *addressing* is deferred.

### The flow stream

Every dispatch, decision, and review is recorded as a `FlowRecord` — the audit
and coordination substrate. The schema version is **`1.9.0`**
(`crates/darkmux-flow/src/schema.rs:14`). The fields group as:

- **Core (always present):** `ts`, `level`, `category`, `tier`, `stage`,
  `action`, `handle`.
- **Correlation:** `sprint_id`, `session_id`, `mission_id`, `source`.
- **Provenance (env-stamped at write time):** `model`, `machine_id` (from
  `DARKMUX_MACHINE_ID`), `orchestrator` (from `DARKMUX_ORCHESTRATOR`). *(The
  `machine_tier` provenance field was removed in schema 1.9.0, #587 — the
  {inference/hub/client} machine-capacity tier is retired; see #590.)*
- **Audit chain (`AuditFileSink` only):** `prev_hash`, `hash` — a BLAKE3
  chain-of-custody verified by `darkmux flow integrity-check` (#163).
- **Parallel-dispatch (#246, schema 1.8):** `work_id`, `attempt`.
- **Extension point:** **`payload`** — an optional `serde_json::Value` map (added
  in schema 1.6.0, #204) that gives new event types
  (`dispatch.turn`/`dispatch.tool`/`dispatch.compaction`/`dispatch.reasoning`/
  `mission.compile.*`) a place to carry event-specific fields without growing the
  struct. Older readers ignore it (`crates/darkmux-flow/src/schema.rs:157-175`).

> The extension field is named **`payload`** in the struct. Prose that calls it a
> "`fields` map" is describing the same mechanism by its 1.6.0 changelog wording —
> the actual member is `payload`.

Schema changes follow semver on the **data shape**: additive event types and
optional fields are minor bumps that older viewers safely ignore (the
[versioning rules](../../CLAUDE.md) live in `CLAUDE.md`).

### The serve daemon

`darkmux serve` exposes a **JSON-only** HTTP API — exactly **8 GET routes**
(`crates/darkmux-serve/src/lib.rs:44-57`):

```
/health   /flow/:date   /flow/:date/stream   /flow-status
/model/status   /machine/specs   /missions   /sprints
```

- `/flow/:date` aggregates fleet-wide records from Redis (`XRANGE`) with a
  local-file fallback when Redis is unreachable; `/flow/:date/stream` is an SSE
  tail (Redis `XREAD`) (`crates/darkmux-serve/src/lib.rs:630-692, 707-732, 740-841`).
- The daemon **does not serve HTML** — there is no `ServeDir`, `fallback`, or
  `nest`, and unmapped paths `404`. The single-page observability viewer is
  [planned](#8-planned--not-yet-shipped) (#554), not shipped.

### Lab telemetry transport (today)

Cross-layer telemetry is always-on (#557) — the internal runtime and crew dispatch
emit it as `category=telemetry` flow records on the flow stream (sources: `lms`,
`process`, `detector`, `runtime`, `context`, `compaction`). There is no sidecar
file and no flag: the standalone `instruments.jsonl` sidecar and the `--instrument`
flag on `darkmux lab run` were retired as part of the
[observability-unification](#8-planned--not-yet-shipped) work (#557).

---

## 8. Planned — not yet shipped

Everything below is **issue-tracked and not in the current binary** — the
darkmux.com demo excepted, which is a website playback fixture, not a binary
feature (the daemon serves no `/demo` route). Do not document these as current
behavior. The observability items are the
[#556](https://github.com/kstrat2001/darkmux/issues/556) epic, designed in
[`docs/architecture/observability-unification-plan.md`](./observability-unification-plan.md).

| Planned | Status | Tracking |
|---|---|---|
| **Stream unification** — fold lab telemetry into the flow stream as an event family; make instrumentation always-on; retire `instruments.jsonl` + `--instrument`. | in progress | [#557](https://github.com/kstrat2001/darkmux/issues/557) |
| **Unified drill-down viewer** — one app (fleet → machine → subsystem), replacing the `topology`/`flow`/`lab` pages. | website demo only | [#558](https://github.com/kstrat2001/darkmux/issues/558) |
| **Daemon hosts the viewer** at its own origin (`GET /`), making CORS/mixed-content impossible by construction. | not started | [#554](https://github.com/kstrat2001/darkmux/issues/554) |
| **darkmux.com demo** as an explicit badged playback fixture (live at `darkmux.com/demo`). | live on the website | [#559](https://github.com/kstrat2001/darkmux/issues/559) |
| **`select_model` capability scoring** — per-role-optimal model routing. Today a Phase-1 stub returns the Primary model (`crates/darkmux-crew/src/select.rs:60-74`). | stub | — |
| **Dynamic per-mission crew composition** — assemble a crew for a dispatch. Today the operator names one role explicitly. | not started | — |
| **Mission-level scope / sprint burn-down / per-sprint estimate tracking** — stored attributes for the above (intentionally absent today). | not started | — |
| **Context-usage as % of window** as a captured telemetry signal (today: derived from absolute counts). | not started | — |
| **Detector-driven behavior change** — bail/escalate on cycle, etc. (today: observability-only). | not started | — |
| **Tier-1 access-pattern eviction + operator-tunable per-slot compaction caps.** | schema only | [#352](https://github.com/kstrat2001/darkmux/issues/352) |

---

## 9. Cross-cutting principles

Two principles thread through every concept above; both are spelled out in full in
[`CLAUDE.md`](../../CLAUDE.md):

- **Operator sovereignty** — defaults are overridable, automatic actions are
  auditable, suggestions are explainable. The operator never has to wonder where a
  decision came from. (`mission propose`'s approval gate, the flow stream's
  provenance fields, and "read + propose, never write user state silently" are all
  instances.) Tracked as [#44](https://github.com/kstrat2001/darkmux/issues/44).
- **Namespacing in shared state** — darkmux-owned entries in systems others also
  use are namespaced (`darkmux:<model-id>` in LMStudio, `darkmux/<role>` in
  openclaw) so darkmux's state-mutating operations touch only the namespaced
  subset; user state is off-limits by construction.

---

### See also

- [`README.md`](../../README.md) — the user-facing pitch.
- [`DESIGN.md`](../../DESIGN.md) — implementation reasoning and version history.
- [`CLAUDE.md`](../../CLAUDE.md) — agent doctrine, environment variables, the
  authoritative "Where things live" module map, and the schema-versioning rules.
- [`observability-unification-plan.md`](./observability-unification-plan.md) — the
  *why* behind the #556 observability arc.
