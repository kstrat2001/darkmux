//! Flow record schema + time helpers + provenance resolution.
//!
//! The `FlowRecord` shape, its enum fields (`Level`, `Category`, `Tier`,
//! `Stage`), the per-day file/timestamp helpers, and the env-driven
//! machine-provenance resolver (`resolve_machine_id`).
//! Split out of the crate's sink/record core (#508).

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// The dispatch-lifecycle action vocabulary (#1852).
///
/// `FlowRecord::action` is a bare `String` while its neighbours (`category`,
/// `tier`, `stage`) are enums — so the one field every consumer JOINS on is
/// the only one nothing constrains. Two producer lineages consequently spell
/// the bookends differently: `darkmux-crew` and the CLI emit the SPACED form,
/// `darkmux-lab` and the runtime emit the DOTTED one. Consumers cope through
/// THREE independent defenses: a normalizer in the React viewer's `flow.ts`
/// (the sole surviving normalizer since the legacy viewer's own, in
/// `viewer.html`, retired along with that file, #1806); the
/// [`is_dispatch_start`]/[`is_dispatch_complete`]/[`is_dispatch_error`]
/// helpers right here, which are now the ONE Rust-side hedge — both
/// `serve/lib.rs` call sites and `serve/runs.rs` route through these
/// functions rather than each carrying its own `||` comparison, so a fix
/// here fixes every Rust consumer at once; and the mission-graph lens's
/// own inline `||` hedges (`ui/src/lenses/mission/graph.ts`: `action ===
/// "dispatch complete" || action === "dispatch.complete"`, etc. — folded
/// into the React port #1868, the standalone `mission-graph.html` page
/// this doc used to cite is retired), which stay genuinely independent
/// because nothing routes this module's action matching through
/// `flow.ts`'s shared normalizer either.
///
/// That is not currently a live bug — every consumer that needs to cope, does.
/// It is fragile in the obvious way: it works until the next consumer
/// forgets, and `savings.ts` is already correct only *because* its data
/// passed through `buildFlowWindow` first, a coupling nothing states or
/// tests.
///
/// These constants carry the SPACED value deliberately: it is what is on disk,
/// in Redis, and in every historical record. Changing the emitted string would
/// be a data-shape change requiring a `FLOW_SCHEMA_VERSION` bump and would
/// strand history. The point here is to make the string un-retypeable, not to
/// pick a winner — that is a separate decision, and a migration.
pub const DISPATCH_START: &str = "dispatch start";
/// See [`DISPATCH_START`].
pub const DISPATCH_COMPLETE: &str = "dispatch complete";
/// See [`DISPATCH_START`].
pub const DISPATCH_ERROR: &str = "dispatch error";

/// True for either spelling of a dispatch-start bookend.
///
/// Consumers MUST use these rather than comparing a literal: a record may
/// carry either spelling depending on which lineage emitted it, and which
/// spelling arrives is not a property a call site can reason about locally.
pub fn is_dispatch_start(action: &str) -> bool {
    action == DISPATCH_START || action == "dispatch.start"
}

/// True for either spelling of a dispatch-complete bookend. See
/// [`is_dispatch_start`].
pub fn is_dispatch_complete(action: &str) -> bool {
    action == DISPATCH_COMPLETE || action == "dispatch.complete"
}

/// True for either spelling of a dispatch-error bookend. See
/// [`is_dispatch_start`].
pub fn is_dispatch_error(action: &str) -> bool {
    action == DISPATCH_ERROR || action == "dispatch.error"
}

/// True for any dispatch-lifecycle terminal (complete OR error) — the
/// "did this dispatch stop" question, which several consumers ask.
pub fn is_dispatch_terminal(action: &str) -> bool {
    is_dispatch_complete(action) || is_dispatch_error(action)
}

pub const FLOW_SCHEMA_VERSION: &str = "1.37.0";
// Version history:
//   1.2.0 — added optional `model` (#106)
//   1.3.0 — added optional `reasoning` + `mission_id`; new Stage::TierDecision (#136)
//   1.4.0 — added optional `machine_id` + `orchestrator` (#167; substrate for #162 fleet UI)
//   1.5.0 — added optional `prev_hash` + `hash` (#163; AuditFileSink chain-of-custody fields)
//   1.6.0 — added optional `payload` JSON blob for event-specific fields. New action
//           values for richer dispatch observability: `dispatch.turn`, `dispatch.tool`,
//           `dispatch.compaction`, `dispatch.reasoning`, `mission.compile.start`,
//           `mission.compile.complete`. Existing `dispatch.start/complete` carry
//           runtime metadata in `payload` (runtime_path, prompt_chars, total_turns, etc.).
//           Backward-compatible — older readers ignore the new field + new actions. (#204)
//   1.7.0 — added action `dispatch.turn.heartbeat` emitted by the live trajectory
//           tailer (`crew/dispatch_internal.rs`) from streaming `model.partial` SSE
//           chunks at most once per 2s. Keeps topology edges animated mid-turn and
//           closes the post-exit-only observability gap. Backward-compatible —
//           older readers safely ignore the new action. (#231)
//   1.8.0 — added optional `machine_tier` (the hardware tier of the emitting machine —
//           `"inference"` / `"hub"` / `"client"`), `work_id` (the work-queue claim id
//           for parallel-dispatch jobs), and `attempt` (retry counter; 1 = first try)
//           for the Article 4 parallel-dispatch substrate (#246). `machine_tier` is
//           auto-populated from `DARKMUX_MACHINE_TIER` env at record-write time, same
//           pattern as `machine_id`. `work_id` and `attempt` are populated by the
//           dispatch path when the work flowed through the queue; absent on direct
//           local dispatches. Backward-compatible — older readers ignore the new
//           fields. (#246 PR-A tier substrate)
//   1.9.0 — REMOVED `machine_tier` (the {inference/hub/client} machine-capacity
//           label). It conflated the orchestration `tier` enum with a hardware
//           label that no routing consumed; the capacity concept moves to
//           capability-based model selection driven by the lab-vetted
//           recommendation registry (#321/#322). New records omit the field.
//           Casual LocalFileSink readers are unaffected (unknown keys ignored).
//           Pre-1.9.0 AuditFileSink hash-chains cannot be re-verified after this
//           canonical-form change — rotate to a fresh chain (no-compat-baggage;
//           small known audience). `work_id`/`attempt` are unchanged.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — removed the orphaned
//           `resolve_machine_tier()` resolver. The `machine_tier` FlowRecord
//           field was already removed in 1.9.0; the resolver lingered with no
//           consumers after the fleet single-stream collapse retired tier
//           routing (#590). No on-the-wire shape change. (#590)
//   1.10.0 — added the `Category::Telemetry` variant (#557 slice 1; the
//           observability-unification keystone). Telemetry folds into the one
//           flow stream as a first-class event family (sources: lms / process /
//           detector / runtime / context / compaction), retiring the separate
//           instruments.jsonl sidecar. Minor + additive: older readers ignore
//           the unknown category; new records only, so prior AuditFileSink
//           chains survive without rotation (unlike the 1.9.0 field removal).
//   1.11.0 — added optional `machine_uid` (#640): the stable hardware
//           identity (IOPlatformUUID), auto-populated at write time. The
//           canonical machine identity, distinct from the mutable `machine_id`
//           label. Older records lack it; the viewer treats absence as
//           *unknown identity* (NOT a fallback to the name). Minor + additive
//           — new records only, prior AuditFileSink chains survive.
//   1.12.0 — added the `telemetry.tokens` action + `source=tokens`
//           (#782 token-emission spine): a per-dispatch telemetry record
//           carrying `prompt_tokens` / `completion_tokens` / `total_tokens`
//           in `payload`, read from the runtime's metrics.json at exit. The
//           data the live "tokens off-meter" savings view (#783) aggregates
//           over a window. The same totals also land on the existing
//           `dispatch.complete` Work record's payload. Tokens-only — NO
//           currency by design. Minor + additive: a new action value + new
//           telemetry source, no struct/field change; older readers ignore
//           the unknown action. New records only, prior AuditFileSink chains
//           survive without rotation.
//   1.13.0 — `telemetry.tokens` is now emitted PER TURN by the dispatch
//           tailer (#795) with a new optional `turn_seq` payload field; the
//           per-dispatch at-complete aggregate (1.12.0 / #782) is retired so
//           records sum to the dispatch total without double-counting.
//           Minor + additive: same action + source, new payload field only;
//           consumers that SUM the family are unaffected (old aggregates and
//           new per-turn records sum identically). New records only, chains
//           survive without rotation.
//   1.14.0 — renamed the `Stage::Retrospect` variant → `Stage::Debrief`
//           (serde value `"retrospect"` → `"debrief"`), the NASA-vocabulary
//           rename for the post-mission review stage (#999). The variant was
//           an unemitted placeholder — NO record ever carried the old value —
//           so this changes only the enum's value-set, not any persisted data;
//           AuditFileSink chains survive without rotation (no records change),
//           and the variant stays unemitted until the debrief ceremony (#1000)
//           writes it. Treated as minor since nothing on-the-wire carried the
//           old value; the bump signals the value-set change for that future
//           consumer.
//   1.15.0 — `telemetry.process` (source=process) now samples the HOST system,
//           not the per-dispatch container. Payload gains `mem` + `gpu` (host
//           RAM-used% + GPU-utilization%) alongside `cpu`, and `cpu`'s meaning
//           changes from container-CPU% — which read ~0 because inference runs
//           in LMStudio, off-container (#814/#1064) — to host system-CPU%. Same
//           action + source; additive payload fields (the wire key `cpu` is
//           unchanged, only its source). Older readers ignore `mem`/`gpu`; new
//           records only, so prior AuditFileSink chains survive without rotation.
//           (#1064)
//   1.16.0 — `dispatch.tool` payload gains `args` — the ACTUAL tool arguments
//           (search pattern / file path / shell command), capped at 512 chars
//           in the runtime, so the operator can recall WHAT each tool call did,
//           not just its size. Results stay size-only (large + re-derivable).
//           Additive payload field; older readers ignore `args`; new records
//           only, so prior AuditFileSink chains survive without rotation.
//   1.17.0 — new action values for the review-funnel driver's run
//           observability (#1247 Part 1, `darkmux_lab::lab::funnel`):
//           `funnel.task` (one run's started/finished bookends), `funnel.step`
//           (a step transition — bundle/probe/probe:<seat>/dedup/judge-pass1/
//           judge-pass2 — payload shape `{step_id, kind, items_in, items_out,
//           status, wall_ms}` per #1230's named substrate), and `funnel.ruling`
//           (the per-judge-ruling live ticker). `status` on both task and step
//           is `started` | `finished` | `error` — `error` is the abort-path
//           terminal value the funnel's bookend guard emits on early return /
//           panic (same guarantee #717's DispatchBookendGuard gives
//           `dispatch.start`), so no consumer ever sees an orphaned `started`.
//           No struct/field change — same `payload` blob every other richer
//           action already uses. Additive: older readers ignore the unknown
//           actions. Emitted through TWO sinks depending on caller
//           (lab-vs-fleet scope boundary): `darkmux mission launch review` writes to
//           the real flow stream via this crate; `darkmux lab review-bench
//           --funnel` writes to a per-run-local `funnel-events.jsonl` file
//           instead, never this stream — so existing AuditFileSink chains are
//           unaffected either way.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — a `dispatch complete`
//           record's payload now carries `endpoint` alongside `remote_tokens`
//           whenever the dispatch involved a remote-endpoint seat (#1230
//           Packet 0, the new `crate::bookend::stamp_remote_classification`
//           helper). `endpoint` already exists on `dispatch.start`/
//           `dispatch.complete` for the container/direct paths since #1187;
//           this only fixes `src/pr_review.rs`'s funnel→dispatch bookend
//           bridge (`with_dispatch_bookends`), which stamped `remote_tokens`
//           alone — the viewer's `tokensOffMeter()` reads `payload.endpoint`
//           exclusively to classify a session as cloud vs. local, so the
//           bridge's records previously counted 100% of a remote-seat
//           funnel run as local savings. Purely additive payload key on an
//           existing action; older readers ignore it, and a fully-local
//           dispatch's payload is byte-identical to before (both fields
//           stay absent).
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1349: the review
//           pipeline's module (`darkmux_lab::lab::funnel` -> `::lab::review`)
//           and its `funnel.task`/`funnel.step`/`funnel.ruling` action
//           vocabulary from 1.17.0 above are renamed to the `review.`
//           `{task,step,ruling}` family — "funnel" described a separate,
//           bespoke execution mechanism that #1348 retired (the pipeline is
//           just a mission now, like any other); the name outlived the
//           thing it named. Same payload shapes, same semantics, action
//           STRING only. #1349 also retired the redundant task-level
//           bookend `run_review_graph` (nee `run_funnel_graph`) used to open
//           from INSIDE the pipeline's own top-level call — every
//           production caller already wraps that call in
//           `src/pr_review.rs`'s `with_dispatch_bookends`, which opens the
//           canonical `dispatch start`/`dispatch complete`/`dispatch error`
//           bookend around it (#1230 Packet 0); the inner per-run task
//           bookend now fires ONLY from the still-used sequential
//           `--charges-file` driver (`run_judge_only` — its sibling
//           `run_review` was deleted as dead code in #1357), never from the
//           Task/Step graph driver. Older readers that don't recognize the
//           renamed actions degrade the same way 1.17.0 documented:
//           additive, unknown-action-tolerant.
//   (code-internal, no FLOW_SCHEMA_VERSION bump) — #1434: the review
//           pipeline's bespoke per-run task/step/ruling action vocabulary
//           (the two prior entries above) is RETIRED entirely. The sequential
//           `--charges-file` driver folded onto the SAME generic `step result`
//           companion vocabulary the Task/Step graph driver already emitted,
//           so exactly one review record vocabulary now exists. These records
//           are per-run-local/ephemeral (no versioned consumer), so no bump
//           and no migration — the vocabulary simply stops being written.
//   1.18.0 — live seat-card metrics for AGENTIC seats (#1483 emit half; the
//           render half shipped in #1485/#1488). The live trajectory tailer's
//           per-event records gain optional `payload.step_id` — the
//           mission-graph STEP this dispatch runs as — so the viewer can
//           attribute the live turn/tool/token climb to the seat card even
//           when `session_id` isn't the `step-<id>` default (the coder-phase
//           `mission.coder` seat dispatches under a shared `mission-run-<…>`
//           session, so its live records were previously unattributable and
//           the agentic seat never ticked). Alongside it, `dispatch.turn`
//           gains `turns_so_far` and `dispatch.tool` gains `tool_calls_so_far`
//           — the AUTHORITATIVE monotonic running counts the viewer's seat
//           meter ticks off (so a page opened mid-dispatch reads the true
//           count, not an under-count from the tail-from-now stream). Same
//           actions, same `payload` blob — additive optional fields only;
//           older readers ignore them; new records only, so prior AuditFileSink
//           chains survive without rotation.
//   1.19.0 — REMOVED `orchestrator` (#1758). It was stamped from
//           `config.json` — MACHINE scope — to describe which frontier
//           orchestrator drove the work — INVOCATION scope — so every
//           record on a machine carried the same value regardless of
//           whether it came from an orchestrator session, a hand-typed
//           CLI command, a cron job, or the CI runner. Grepped every
//           consumer: nothing ever READ it (no viewer lens, no CLI verb,
//           no aggregation; `darkmux doctor` only checked "is it
//           declared"). A field that lies is worse than an absent one.
//           Same shape of removal as 1.9.0's `machine_tier`. Casual
//           LocalFileSink readers are unaffected (unknown keys ignored
//           on read; `#[serde(flatten)] extras` on `DarkmuxConfig`
//           absorbs an old `config.json`'s `"orchestrator"` key the same
//           way). Pre-1.19.0 AuditFileSink hash-chains cannot be
//           re-verified after this canonical-form change — rotate to a
//           fresh chain (no-compat-baggage; small known audience). If
//           the want comes back, it comes back differently: a truthful
//           version would stamp per-invocation from process environment
//           (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, …), not from
//           machine-scoped config — build that when a real consumer
//           needs it.
//   1.22.0: additive payload keys `outcome` / `exit_code` / `failure_reason`
//           on `dispatch.tool` (#2008) — AND a CORRECTION to what the
//           existing `ok` key means for `bash`. Read this before comparing
//           records across the boundary.
//
//           `ok` was derived from the exit code: anything non-zero was
//           false. That conflated a command that RAN and reported a result
//           (a failing test in TDD's red phase, a lint finding, `grep` with
//           no matches) with one that NEVER RAN (exit 127/126, a spawn
//           failure, a toolchain that would not load, a timeout). `ok` now
//           means what every place that specifies it always said it meant —
//           "the tool call succeeded" — so a red test is `ok: true`.
//
//           This is a defect correction, not a semantic redefinition: the
//           field's contract is stated as tool-success in `trajectory.rs`'s
//           `append_tool_completed`, in the watchdog comment in
//           `dispatch_internal.rs` ("ONLY a successful tool call resets the
//           deadline"), and in the darkmux-analyze-run skill. The classifier
//           feeding it was wrong; the field's meaning was not. Contract 5's
//           major triggers (rename, retype, new required field) are not
//           tripped — same name, same type, same optionality — so this is a
//           minor bump.
//
//           IT IS STILL A BOUNDARY. Records written before 1.22.0 carry the
//           old meaning and cannot be rewritten; the audit file header
//           stamps its writer's schema version, so the boundary is locatable
//           per file. Any series that aggregates `ok: false` across it — the
//           lab `tool_bench`'s `failed_calls` stat above all — is mixing two
//           definitions and must be read per-side, not summed.
//
//           `outcome` is the three-way discriminator (`"ok"` / `"reported"`
//           / `"failed"`); `exit_code` accompanies `reported`;
//           `failure_reason` accompanies `failed`. Flat keys rather than a
//           nested tagged enum, because every reader here is lenient-on-read
//           and a flat key is the shape they already tolerate.
//   1.21.0: additive payload key `result` on the existing `dispatch.tool`
//           action (#2007). The record already carried `result_chars`; it
//           now carries the result TEXT beside it, so a failed tool call can
//           be diagnosed from the record rather than only counted. Motivated
//           by the 3.0.0 release dogfood, where the tool-failure cascade
//           detector fired on three `bash` failures and the evidence for WHY
//           had already been discarded — while `dispatch.reasoning` had been
//           persisting the model's thinking verbatim the whole time, so the
//           stream kept one side of a two-sided conversation.
//           Bounded by `MAX_TOOL_RESULT_BYTES` (64 KiB) rather than the 4 KiB
//           `MAX_TRAJ_FIELD_BYTES` used for the short fields: a tool result
//           is a test run's output, and 64 KiB clears the largest observed
//           across 455 real calls (52,936 chars) while still refusing a
//           container that wants to write megabytes into the audit chain.
//           Truncation is in-band and self-describing (`cap_str` appends
//           `… [truncated; original N chars / M bytes]`), and `result_chars`
//           remains the TRUE length, so a cut is never silent. Purely
//           additive: older readers ignore the key, and the container-side
//           trajectory keeps the result in full and uncapped.
//   1.20.0: new action `"step timing"` (#1877, this arc's final wiring
//           step): `darkmux-crew::scheduler::apply_step_terminal` now
//           streams one companion flow record per scheduler-produced
//           `StepRecord`, live, at the moment it's pushed onto
//           `SchedulerReport::step_records`. Every mission that runs
//           through `run_step_graph` gets it by construction (coder-phase
//           included, with no change to `src/coder_phase.rs`). Payload is
//           `StepRecord`'s own `serde_json::to_value` (`step_id`/`kind`/
//           `wall_ms`, `items_in`/`items_out` when known). Deliberately its
//           own action, never `"step result"`. See `run_record.rs`'s
//           module doc in `darkmux-crew` for why reusing that action would
//           put two ambiguous records under one name for the same step.
//           Minor + additive: a new action value + new payload shape, no
//           struct/field change; older readers (including `darkmux-serve`'s
//           `mission_graph::fold_step_finals`) ignore the unknown action.
//           New records only, prior AuditFileSink chains survive without
//           rotation.
//   1.23.0: new action values `hook.fired` / `hook.failed` (#2093) —
//           `darkmux_flow::hooks::HookSink`'s own firing/failure records,
//           emitted after each delivery attempt against a configured hook
//           rule. Payload carries `rule_index` / `target_host` /
//           `delivered_action` / `attempt`, plus `delivered_hash` and
//           `error` when present. No struct/enum change — both actions use
//           the existing `Category::Machinery` / `Tier::Local` /
//           `Stage::Ship`. Minor + additive: older readers ignore the two
//           new action values; new records only, prior AuditFileSink
//           chains survive without rotation.
//   1.24.0: new action family for `darkmux mission launch crawl` (#1959
//           packet 2, the crawl LAUNCHER — packet 1 landed the corpus
//           manifest / rules / read-only source worktrees / `darkmux crawl
//           plan` machinery with no flow-record vocabulary of its own).
//           `crawl.mission.started` / `crawl.mission.completed` bookend
//           the whole sequential unit loop (payload: corpus, units_in_
//           plan/units_selected/units_not_run — plan-level: a unit
//           excluded by `--param units=`, cut by `--param limit=`, or
//           never reached because the loop stopped early all land in
//           `units_not_run`, one number for every reason (merge-gate
//           finding 2, renamed from the original `units_planned`);
//           units_completed/errored/skipped, tokens, wall_ms,
//           tokens_per_hour, stopped_by — `done`/`limit` reach a real
//           `phase complete`; a kill file, SIGINT, or an early error/
//           panic reach `phase abandon` instead (finding 3, so the phase
//           record itself, not just this payload, tells a deliberate
//           stop apart from a completion); `model`/`profile`/
//           `timeout_secs`/`limit`/`plan_path`/`units_filter` self-
//           describe the run's own config (finding 8)); `crawl.unit.
//           started` / `crawl.unit.completed` bookend each unit (payload:
//           corpus, unit, source, sha, rule, kind, result — `stop`/
//           `error`/`timeout`/`interrupted` — findings, tokens, model;
//           both records now share the UNIT's own session id rather than
//           `started` carrying the mission id, finding 5); `crawl.finding`
//           carries one recorded finding (payload: corpus, unit, source,
//           sha, rule, plus the finding record's own fields verbatim —
//           `file` rewritten from the container path to a source-relative
//           path, with the original kept as `file_raw`). Every record in
//           the family now stamps `FlowRecord.model` when the unit's
//           envelope reported one (previously hardcoded `None`, finding
//           8). The launcher's own PER-UNIT model dispatch already
//           satisfies the dispatch-liveness contract (registry entry 2)
//           via the ordinary `dispatch start`/`dispatch complete`/
//           `dispatch error` bookends `crew::dispatch::dispatch` always
//           emits — this family is descriptive scaffolding around that,
//           not a replacement for it. Minor + additive: five new action
//           values, same `payload` blob shape every other richer action
//           already uses; older readers ignore the unknown actions and
//           unknown payload fields. New records only, prior AuditFileSink
//           chains survive without rotation. (The field rename/additions
//           above landed via merge-gate review on the same branch that
//           introduced 1.24.0, before this schema version ever shipped —
//           amending this entry in place, not a further version bump.)
//   1.25.0 (#2094): additive payload fields for the global inter-turn
//           rest — `turn_delay_ms` (the resolved knob) on `dispatch.start`,
//           and `rest_ms` / `rests` (sum + count of the rests this
//           dispatch took) on `dispatch.complete`, surfaced beside the
//           existing `wall_ms` (which INCLUDES rest time; a consumer
//           wanting model-only time subtracts `rest_ms`). No struct/field
//           change — same `payload` blob every other richer action
//           already uses. Older readers ignore the new keys; new records
//           only, prior AuditFileSink chains survive without rotation.
//           This branch took 1.25.0 directly (skipping the two RESERVED
//           slots above) so the eventual three-way merge is a one-line
//           reconcile: whichever branch lands last just renumbers its own
//           bump past whatever the other two already claimed.
//           (finding 2, same 1.25.0) Also added the `dispatch.rest` action
//           itself — one per `runtime.rest` trajectory event, live on the
//           flow stream (not just summarized at `dispatch.complete`).
//           Payload: `ms` (this rest's duration), `turn`, and the running
//           `rest_ms` / `rests` totals so far. A new action value under the
//           same additive rule this whole version already documents; older
//           readers that don't recognize `dispatch.rest` ignore it exactly
//           like they ignore any other unfamiliar action.
//           (finding 8, same 1.25.0) Also added `turn_delay_effective_ms`
//           to `dispatch.complete` — the POST-CLAMP cadence the runtime
//           actually applied, distinct from `rest_ms`/`rests` (what
//           happened) and from the operator's raw configured value
//           (`dispatch.start`'s `turn_delay_ms`). `null` when unknowable.
//           Additive payload field, same rule as the rest of this version.
//   1.26.0 (#1959, revised): the 1.24.0 `crawl.*` action family
//           (`crawl.mission.started/completed`, `crawl.unit.started/
//           completed`, `crawl.finding`) is RETIRED — never shipped past
//           this repo's own history, so this is a removal, not a
//           deprecation window. The crawl launcher mints a real Mission/
//           Phase/Task/Step (it always did), so it now uses the GENERIC
//           lifecycle actions every other mission uses instead of its own
//           bespoke vocabulary: `mission start`/`mission close` (payload
//           gains `workspace`, `units_in_plan`, `units_selected`,
//           `est_tokens`, `sources` on start; `units_completed/errored/
//           skipped/not_run`, `findings`, `prompt_tokens`,
//           `completion_tokens`, `wall_ms`, `tokens_per_hour`,
//           `stopped_by`, `model` on close) and `step start`/`step
//           complete`/`step error` (payload gains `workspace`, `unit`,
//           `source`, `sha`, `rule`, `kind`, `est_tokens`, `sites`/`files`
//           on start; `result`, `findings`, `prompt_tokens`,
//           `completion_tokens`, `wall_ms`, `model` on the terminal).
//           `crawl.finding` is retired outright with NO replacement
//           action — a finding is never a special record; the runtime
//           classifies a REJECTED/NOT-RECORDED `report_finding` reply as a
//           FAILED tool call (`payload.ok: false` on the ordinary
//           `dispatch.tool` record an accepted OR rejected call already
//           produces), and a hook rule subscribes to the accepted subset
//           via a new payload-predicate match (`"payload.tool_name":
//           "report_finding", "payload.ok": true` — see `HookMatch::
//           payload_predicates`, config schema unaffected: predicates ride
//           the existing `#[serde(flatten)] extras` map, no new field).
//           `DispatchOpts::record_context` (a caller-supplied JSON object
//           — the crawl launcher's `workspace`/`source`/`sha`/`rule`/
//           `unit`, `None` for every other caller) merges under
//           `payload.context` on EVERY record this dispatch's flow-record
//           surface emits — the bookends (`dispatch start`/`dispatch
//           complete`/`dispatch error`) and every tailer-emitted record
//           (`dispatch.tool`, `dispatch.turn`, `telemetry.*`, …) alike —
//           so a consumer never has to special-case one action to find
//           provenance the runtime itself has no concept of. Minor +
//           additive on the FLOW side (new optional payload keys on
//           existing generic actions; `crawl.*`'s removal is a vocabulary
//           retirement, not a schema-breaking field/struct change — older
//           readers simply stop seeing those five action strings). The
//           ledger written to `<workspace root>/runs/<mission>/
//           ledger.jsonl` at readback is UNCHANGED by any of this — it was
//           never part of the flow-record schema.
//   1.27.0 (#2107): `dispatch.complete`'s `host` block (carried in the
//           `--json` envelope's payload, not the flow record itself — see
//           `dispatch_internal::enrich_envelope_with_summary`) gains a
//           real reduction per metric instead of two bare peaks. `host.cpu`
//           / `host.mem` / `host.gpu` each carry `{peak_pct, mean_pct,
//           p95_pct, above_80_ms}` — a peak alone answers "did this ever
//           spike"; it cannot say how hard the host was driven ON AVERAGE,
//           which `runtime.turn_delay_ms` (#2094) needs. `host.samples` is
//           unchanged; `host.sample_interval_ms` is new (the MEASURED mean
//           gap between ticks, not the nominal constant). Additive and
//           backward compatible: the pre-1.27.0 top-level `peak_cpu_pct` /
//           `peak_mem_pct` fields are KEPT on `host` for one release,
//           mirroring `host.cpu.peak_pct` / `host.mem.peak_pct` exactly, so
//           a reader that never upgrades keeps working. Also fixes the
//           null-host-on-crawl-units gap named in the same issue: the
//           sampler always populated `host` on the raw envelope
//           per-dispatch, but the crawl launcher's own readback
//           (`interpret_dispatch_result` in `src/crawl_launch.rs`) never
//           extracted it, so it never reached the unit's own `step
//           complete`/`step error` payload or the mission's `envelope.json`
//           — a launcher-side readback gap, not a flow-schema change (no
//           new field on the FLOW record itself; `payload.host` on `step
//           complete`/`step error` was always a legal free-form key under
//           the existing `payload` blob).
//   1.28.0 (#2108): `dispatch.complete`'s `host` block (same carrier as
//           1.27.0 — the `--json` envelope's payload) gains `host.power`,
//           `host.thermal` and `host.energy_mwh`, from the in-process host
//           probe that replaced the sampler's `top`/`vm_stat`/`sysctl`/
//           `ioreg` shell-outs. `host.power.{cpu,gpu,total}` each carry
//           `{mean_mw, peak_mw}` (IOReport `Energy Model` counter deltas);
//           `host.thermal` carries `{worst_state, above_nominal_ms,
//           min_cpu_speed_limit_pct}` (`ProcessInfo.thermalState` +
//           `IOPMCopyCPUPowerStatus`); `host.energy_mwh` is the integral of
//           total power over the dispatch. They answer a question the
//           percentages cannot: a dispatch that ran at 40% CPU the whole way
//           while the kernel held the speed cap at 62% was not a comfortable
//           run, and neither the wall clock nor the utilization figure says
//           so. Purely ADDITIVE — every 1.27.0 field is byte-identical and a
//           reader that ignores the new keys is unaffected. Each of the
//           three is present only when the probe actually READ that source
//           on this host (Apple Silicon; IOReport reachable), for the same
//           reason `host` itself is present only when the sampler ran: an
//           absent block says "not measured", a zeroed one would say
//           "measured, and idle". No new field on the FLOW record itself,
//           and no struct change, so prior AuditFileSink chains survive
//           without rotation.
//   1.28.x (#2110/#2109): the thermal governor/breaker's dispatch.rest
//           records now carry a SECOND payload shape alongside the
//           1.25.0 rest-episode one (`ms`/`turn`/`rest_ms`/`rests`):
//           `reason` (`"thermal"` for an ordinary governor pause/resume,
//           `"thermal-critical"` for the breaker), `state` (the OS
//           thermal state name that triggered the write), and `pause`
//           (`true`/`false`) — the thermal governor's own event, not a
//           per-poll-increment rest. Same additive rule as every action
//           value already documented in this history: an unfamiliar key
//           under the existing free-form `payload` blob, ignored by a
//           reader that doesn't recognize it. No struct/field change, no
//           version bump — `dispatch.rest` was already documented as an
//           action whose payload shape varies by writer (1.25.0 above),
//           so this is the second shape under that umbrella, not a new
//           contract.
//   1.28.x (#2110/#2109 review finding 5, N3): a NEW action,
//           `thermal.stop_unresolved` — Level::Warn (operator-actionable,
//           not routine telemetry), `source: "thermal"`. Fires when the
//           breaker trips on what looked like a crawl unit
//           (`record_context` carried the crawl launcher's `unit`
//           marker) but the STOP path could not be derived trustworthily
//           (`workspace` missing or empty) — the breaker never writes a
//           STOP at a guessed path, so this event is the operator's only
//           signal that a crawl may keep dispatching units past a
//           tripped breaker. Payload: `stop_written: false`, `reason`
//           (why derivation failed), `state` (the OS thermal state that
//           tripped the breaker), plus the usual `context` block merged
//           in via `merge_record_context` (unit/source/sha/rule, when
//           present). A brand-new action value under the same additive
//           rule as every other entry in this history — no version bump,
//           no struct change.
//   1.29.0 (#2165): `dispatch start`'s payload gains `bounds` — the
//           resolved runtime knobs WITH provenance (`max_tokens_per_call`,
//           `reasoning_checkpoint_interval_tokens`, `inactivity_timeout_
//           seconds`, `max_turns`, `max_tokens`, `turn_delay_ms`,
//           `feedback_injection`), each `{value, source}` where `source`
//           is `"built-in"` / `"config"` / `"env"`. The finished envelope
//           (the `--json` stdout payload the orchestrator reads) gains the
//           SAME `bounds` block, built by the same producer
//           (`resolved_runtime_bounds_json` in `dispatch_internal.rs`) so
//           the start record and the finished run can't independently
//           drift on what "the resolved knobs" means. Answers the #2165
//           miss directly: a remote reader watching the flow stream no
//           longer has to reconstruct from memory of the design whether a
//           cap-hit stderr line named an operator override or a built-in
//           default — the same information now rides on the dispatch's
//           own start record. No struct/field change — same free-form
//           `payload` blob every other richer action already uses; older
//           readers ignore the new key.
//   1.30.0 (2026-08-30 fleet-observability finding): `dispatch.rest` gains
//           `reason` (always present now — `"turn_delay"` for a routine
//           inter-turn rest, or the pace file's own operator/governor-
//           supplied reason for a paced rest) and `state` (the pace file's
//           OS thermal-state name, present only on a paced rest that
//           carried one). Before this, a manual pace pause and a routine
//           turn-delay rest were indistinguishable on the flow stream
//           except by cadence (2000ms paced-poll increments vs the
//           configured `turn_delay_ms`) — a fragile signal for a remote
//           reader to reverse-engineer. `dispatch.complete`'s payload (and
//           the finished envelope) gain `paced_rest_ms`: of `rest_ms`, the
//           portion attributable to a paced rest, so a reader can separate
//           "cool-down by policy" from "paused by operator/governor"
//           without subtracting `turn_delay_effective_ms * rests`
//           themselves. No struct/field change — same free-form `payload`
//           blob every other richer action already uses; older readers
//           ignore the new keys.
//   1.31.0 (#2111): the "no blind runs" doctrine applied to the #2108
//           probe. Two new action values, plus an additive payload field
//           on the existing dispatch-lifecycle terminal:
//
//           `machine.thermal` (Category::Machinery, source
//           `"host-sampler"`) — a TRANSITION record from `darkmux serve`'s
//           daemon-side host sampler (`darkmux-serve::host_sampler`),
//           edge-detected between consecutive ticks at the daemon
//           sampler's own configured cadence (`runtime.
//           host_sampler_interval_ms`, default 5s — NOT the dispatch
//           sampler's separate 2s tick; the two sampler threads run at
//           independent cadences). Payload `{from, to,
//           cpu_speed_limit_pct, power_mw_total, sampled_at_ms}`.
//           `Level::Warn` when the state RISES into `serious`/`critical`;
//           `Level::Info` otherwise (recovering, or a lateral move). Fires
//           only on a genuine state change — a steady sampler emits none,
//           and the FIRST reading after daemon start (or after any gap,
//           sleep/wake included) seeds the baseline silently rather than
//           firing a transition from "unknown". No mission/session
//           context (the daemon sampler runs independently of any
//           dispatch); `machine_id`/`machine_uid` are the usual write-time
//           auto-stamp.
//
//           `machine.telemetry` (Category::Telemetry, source `"host"`) —
//           a periodic SAMPLE record from the DISPATCH-scoped sampler
//           (`darkmux-crew::dispatch_internal::run_telemetry_sampler`,
//           already running during every local dispatch to drive the
//           thermal governor), emitted every Nth tick
//           (`runtime.telemetry_record_every_samples`, default 5 ≈ 10s at
//           this sampler's own 2s tick; `0` disables it) rather than on
//           every tick, so the periodic curve costs a fraction of the
//           tick cadence on the flow stream. Payload carries the FULL
//           host reading — `thermal{state,cpu_speed_limit_pct}`,
//           `power_mw{cpu,gpu,ane,total}`, `cpu_pct`,
//           `cpu_clusters[]{name,cores,pct,mhz}`, `gpu_pct`, `gpu_mhz`,
//           `gpu_mem_bytes`, `mem_pct`, `sampler_cost_ms`,
//           `sampled_at_ms` (UNIX epoch ms — the same clock every
//           `sampled_at_ms` producer uses, via the shared
//           `host_probe::epoch_ms_now()`, so a strip charting this
//           alongside `machine.thermal` compares like clocks), plus
//           `prev_record_write_ms` when a PRIOR emission exists — the
//           measured wall-clock of the previous `darkmux_flow::record()`
//           call (CLAUDE.md "samplers stamp their own cost" applied to
//           the record-write path, distinct from the probe read
//           `sampler_cost_ms` already covers). Deliberately the PREVIOUS
//           write's cost, not this one's: a record cannot measure its own
//           write before that write has happened, so this stamps the
//           last completed one rather than a build-only proxy mislabeled
//           as "the write". Rides through `merge_record_context` like
//           every other tailer-emitted telemetry record, so
//           `mission_id`/`phase_id`/`payload.context` key it to the
//           dispatch the same way `dispatch.tool` etc. do.
//
//           `dispatch complete`/`dispatch error`'s payload (and the
//           `--json` envelope, alongside the existing nested `host` block
//           from 1.27.0/1.28.0) gains `host_window` — a FLATTER,
//           dispatch-summary shape distinct from `host`'s per-metric
//           breakdown: `{thermal_worst_state, above_nominal_ms,
//           min_cpu_speed_limit_pct, power_mw_total{mean,max,p95},
//           energy_mwh, samples, span_ms}`, built from the exact same
//           `HostStats`/`HostExtras` reduction `host` already uses (no
//           duplicated math) — so a fleet reader watching the FLOW STREAM
//           (not just the CLI's own `--json` stdout, which is all `host`
//           ever reached) can answer "was this dispatch thermally
//           comfortable" from the terminal record alone. Present only
//           when the sampler took at least one sample, same "absent means
//           not measured" convention as `host`. NOTE: `span_ms` is the
//           UNCAPPED wall-clock span of the sample series, while
//           `above_nominal_ms` (like `energy_mwh`) is derived through
//           `reduce_host_extras`'s sleep-gap cap (`MAX_GAP_CADENCE_
//           MULTIPLE`) — so their ratio is NOT "fraction of the dispatch
//           spent above nominal" across a dispatch that suspended and
//           resumed; it undercounts the capped gap on purpose (see
//           `reduce_host_extras`'s own doc for why an uncapped duty figure
//           there was the #2108 bug).
//
//           All three are additive: two new action values under the
//           existing free-form `payload` blob, plus a new payload key on
//           an existing action. Older readers ignore what they don't
//           recognize; no struct/field change on `FlowRecord` itself, so
//           prior AuditFileSink chains survive without rotation.
//   1.32.0 (#2268): `dispatch start`'s payload gains `tools_requested` —
//           the tool names the host asked the runtime to advertise
//           (`--allowed-tools`, derived from the role's palette), or `null`
//           when the role declares no palette and the runtime's full catalog
//           applies. The runtime's own trajectory `dispatch.start` gains the
//           matching `tools` (what was ADVERTISED). Two records, two producers,
//           so a gap between them is visible in the artifact: the crawler's
//           `report_finding` was requested and never advertised for every
//           build from #2182 (2026-08-31) to this one, and nothing recorded
//           either list. Additive payload key on an existing action; older
//           readers ignore it.
//   1.33.0 (#2272): `dispatch.tool`'s payload gains `emitted` and `emit_seq`.
//           `emitted` is an accepted `report_finding` call's arguments
//           VERBATIM — an opaque value darkmux never interprets: it has no
//           idea where the record will end up, so it cannot carry knowledge
//           of any destination; a hook's transform composes that
//           destination's payload from the record's metadata (mission,
//           unit, rule, source, sha, model, ts, `emit_seq`) plus this blob.
//           Bounded on the serialized whole at 64 KiB, and LOUDLY — over
//           it, the value is `{ "truncated": <prefix>, "emitted_truncated":
//           true }`. `emit_seq` is the 1-based ordinal of the acceptance
//           within the dispatch (the runtime's findings-file count, so it
//           survives a resume). Both are `null` on every other tool call, so
//           a non-emitting call never reads as a pre-1.33 record. Why: the
//           record's `args` is (and stays) the runtime's 512-char viewer
//           preview; nine of nine crawl findings on 2026-09-02 reached the
//           tracker as that preview, cut mid-JSON, and were rejected. The
//           crawl's product now rides its own field. Additive payload keys
//           on an existing action; older readers ignore them.
//   1.34.0 (operator, 2026-09-03): the finding tool is renamed —
//           `dispatch.tool` records carry `payload.tool_name: "create_finding"`
//           where they carried `"report_finding"`. No field changed; the
//           VOCABULARY did: the tool CREATES a record (a hook then reports
//           it), and it must read like its sibling `create_mod`. Pre-1.0, so
//           no alias: hook rules matching `payload.tool_name`, the viewer's
//           record detail, and any transform keying on the old name update
//           with this version. Historical entries above keep the old name.
//   1.35.0 (#2265): the MOD channel's two additive keys.
//           `dispatch.tool` records now also carry `payload.tool_name:
//           "create_mod"` — the runtime tool that records a proposed CHANGE,
//           the sibling of `create_finding`'s observation. Its accepted calls
//           ride the SAME `emitted` / `emit_seq` fields 1.33.0 added (the
//           emission is `{for, kit, attach}` as the model sent it, opaque to
//           darkmux and never parsed), so no new field is needed for it and a
//           reader keys on `tool_name` to tell the two channels apart. The
//           host materializes a mod record from that emission the way it
//           materializes a finding, so a hook rule matching `{"payload.
//           tool_name": "create_mod", "payload.ok": true}` sees exactly the
//           accepted ones.
//           `dispatch start`'s payload gains `findings_in_brief` — the finding
//           keys `dispatch --finding <key>` appended to the brief, `[]` on
//           every other dispatch (present-and-empty rather than absent, so an
//           older writer's record stays distinguishable). Why its own field:
//           `prompt` is capped, and a key is the address the finding store
//           answers to, so "which observations was this dispatch briefed on"
//           is answerable from the record without recovering it from prose.
//           `findings_in_brief` rides EVERY dispatch path's start record (the
//           container path and both single-shot hosted/local paths), because
//           `prompt` is capped on all of them. ONE gap, stated rather than
//           papered over: a CROSS-MACHINE (`--machine`) dispatch carries the
//           finding's text — it is inside `message` — but not the keys, because
//           `WorkJob` does not have the field. Adding it there is a real wire
//           break (`deny_unknown_fields` + a coordinated
//           `WORK_JOB_SCHEMA_VERSION` bump that would reject EVERY job from a
//           peer on the old version, for a provenance field), so it is a
//           deliberate follow-up rather than a rider on this change. A runner's
//           record therefore reads `[]` for a `--finding` dispatch that was
//           routed to it; the brief it worked from is unaffected.
//           Both additive; older readers ignore what they do not recognize.
//   1.36.0 (#2295): `dispatch start`'s payload `findings_in_brief` (added one
//           version ago, in 1.35.0) is REPLACED by `brief_refs` — a list of
//           `{"kind": "finding"|"mod", "key": "..."}` rather than a list of
//           bare finding keys. The reason is that a finding was never the only
//           record a brief can carry: `dispatch --mod <key>` appends a stored
//           mod's kit the same way, and two provenance fields for one
//           operation would drift. `[]` on every dispatch that names no
//           record, present-and-empty for the same reason as before.
//           `findings_in_brief` is DROPPED outright rather than kept as an
//           alias: it had ZERO consumers (nothing in the viewer, the serve
//           daemon, or any hook rule read it), it shipped one version ago, and
//           a rename while nothing reads it costs nothing — carrying a second
//           spelling forever would.
//           The 1.35.0 cross-machine carve-out CHANGES SHAPE. Resolution moved
//           from the CLI down to the step kind (the one point every producer
//           of a `brief_refs` step config goes through — before that move a
//           mission graph setting the field got the stamp and no block), so a
//           `--machine` dispatch no longer carries the record's text inside
//           `message`. `WorkJob` still has no field for the refs — adding one
//           is a wire break (`deny_unknown_fields` + a coordinated
//           `WORK_JOB_SCHEMA_VERSION` bump that would reject EVERY job from a
//           peer on the old version) — so rather than route a brief whose
//           blocks are silently missing, the CLI now REFUSES
//           `--finding`/`--mod` together with a remote `--machine`, naming the
//           gap. No runner record can therefore carry these refs at all, and
//           none reads `[]` for a routed ref dispatch that silently lost its
//           blocks.
//   1.37.0 (#2299): `mission start`'s payload gains `graph` on every run
//           minted by `mission launch <config>` (the generic config launcher;
//           NOT the crew-of-one dispatch, the review launcher, or the crawl
//           launcher, which mint their graphs without a document to prune):
//           `{phases_in_config, phases_minted, tasks_in_config, tasks_minted,
//           steps_in_config, steps_minted, pruned: [{id, kind, reason}]}`.
//           A mission config item with `enabled: false` is PRUNED before the
//           run is minted — it never exists in the run, no record, nothing
//           gray — so this object is the only place a reader can learn that
//           the config declared more than the run shows. `reason` is one of
//           `disabled`, `parent_pruned`, `all_steps_pruned`,
//           `all_tasks_pruned`, `all_dependencies_pruned`. The same report
//           is written beside the run's config snapshot as
//           `graph-report.json`; `mission status` reads that file. Additive:
//           older readers ignore the key.

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    /// (#1611) Lenient-on-read catch-all. Without it, a variant this binary does
    /// not know fails serde for the WHOLE record — `category` is a required,
    /// non-`Option` field, so one unrecognized string makes the entire
    /// `FlowRecord` unparseable rather than partially readable.
    ///
    /// The schema's own version history promised this already ("older readers
    /// ignore the unknown category", FLOW_SCHEMA 1.10.0) — true for the additive
    /// `Option` fields and the free-form `action` string, and never true for
    /// these enums until now. Contract 5: consumers are lenient-on-read, loud in
    /// doctor. The typed production reader this makes whole is `darkmux-crew`'s
    /// role index.
    ///
    /// Nothing ever WRITES `Unknown` — no code constructs it — so no record is
    /// emitted carrying one. Reading IS lossy in the sense that a record parsed
    /// to `Unknown` re-serializes as `"unknown"`, not as whatever the writer
    /// wrote — but that no longer matters to the audit chain (#1769): the
    /// hash-chain's content check hashes the RAW BYTES a writer put on disk, and
    /// never round-trips a record through this struct to do it. A record
    /// carrying an unknown spelling here reads fine and verifies fine; the two
    /// concerns (can I read this field? can I trust the chain?) are now fully
    /// decoupled. See `crates/darkmux-flow/src/integrity.rs`'s module doc for
    /// the hash-format details.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Work,
    Machinery,
    Audit,
    Review,
    /// Telemetry as a first-class flow-event family (#557): per-dispatch
    /// instrument samples — context-fill, detector firings, compaction, lms
    /// load/unload, container CPU — emitted into the one stream, always-on.
    /// Replaces the retired instruments.jsonl sidecar.
    Telemetry,
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Operator,
    Frontier,
    Local,
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Scope,
    Estimate,
    Dispatch,
    Review,
    Ship,
    /// Post-mission review stage (#999, NASA vocabulary — Mission · Crew ·
    /// Debrief · Lessons). The mission debrief ceremony (#1000) distills the
    /// mission's cautions + corrections into durable lessons; records of that
    /// review carry this stage. Serialized as `"debrief"`. Renamed from the
    /// unemitted `Retrospect` placeholder in FLOW_SCHEMA 1.14.0.
    Debrief,
    /// Tier-decision record (#136): the frontier orchestrator's reasoning
    /// for routing this piece of work to local vs. holding in frontier.
    /// Emitted via `darkmux flow tier-decision`. Category typically
    /// `audit`; the `reasoning` field carries the operator-visible
    /// rationale. Serialized as `"tier-decision"`.
    TierDecision,
    /// See [`Level::Unknown`] — same lenient-on-read contract.
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlowRecord {
    pub ts: String,
    pub level: Level,
    pub category: Category,
    pub tier: Tier,
    pub stage: Stage,
    pub action: String,
    pub handle: String,
    /// Sprint→Phase rename read-compat: historical flow records on disk
    /// (append-only JSONL, never rewritten) carry this under the pre-
    /// rename wire key `sprint_id`. `alias` lets readers accept either
    /// key so historical records don't silently lose the field; every
    /// newly-written record emits the canonical `phase_id` key.
    #[serde(skip_serializing_if = "Option::is_none", alias = "sprint_id")]
    pub phase_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// LMStudio model id that handled this work, when known. Set on
    /// dispatch records (`tier=local, stage=dispatch`) so the viewer
    /// can render which model ran the work without cross-referencing
    /// the model-status pill's timestamp. Resolved at dispatch entry
    /// from the active profile (`crew::select::select_model`). None for
    /// non-dispatch records (lifecycle transitions, phase review
    /// verdicts) and for dispatches where the model can't be resolved.
    /// Schema 1.2 addition (#106).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Operator-facing reasoning for this record. Used primarily by
    /// tier-decision records (#136) where the frontier orchestrator
    /// explains WHY work was routed to local vs. held in frontier. The
    /// audit substrate's "why" layer. Schema 1.3 addition.
    ///
    /// Non-tier-decision records typically leave this `None`. When set
    /// on any record, it's free-form prose intended for human review
    /// (debrief, compliance audit, post-mortem).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Parent mission id. Optional because some flow records aren't
    /// scoped to a mission (operator-initiated dispatches without an
    /// active mission, machinery events). Schema 1.3 addition (#136).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// Machine that emitted this record. Auto-populated at write time
    /// from `DARKMUX_MACHINE_ID` env (operator-named — e.g. `"studio"`,
    /// `"mini-1"`) or hostname (default). Older records (pre-1.4.0) lack
    /// the field; viewer treats absence as `unknown`. Schema 1.4 addition
    /// (#167; substrate for fleet UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// Stable hardware identity of the machine that emitted this record
    /// (`IOPlatformUUID`, #640) — the canonical machine identity, distinct
    /// from the mutable `machine_id` label above. Auto-populated at write time
    /// from `darkmux_hardware::machine_uid()`. `None` off macOS, or on records
    /// written before 1.11.0; the viewer treats absence as *unknown identity*
    /// and groups such records under one "unknown" machine — never falling
    /// back to the (unprovable) name. Schema 1.11 addition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_uid: Option<String>,
    /// BLAKE3 hash of the previous record in this audit file's chain.
    /// `None` on records written through LocalFileSink (the casual sink);
    /// AuditFileSink (the detection-substrate sibling) populates this
    /// with the prior LINE's hash so tampering with any single record is
    /// detectable via a linear walk. The first record in a file points to
    /// the hash of the schema-header line. Schema 1.5 addition (#163).
    /// This is the ONE hash-chain field that still lives inside the JSON
    /// body under the byte-hash format (#1769) — it's covered by the
    /// content hash the same way every other field is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    /// Legacy field (pre-2.6.0): under the OLD struct-hash audit format
    /// this carried THIS record's own content hash, embedded inside the
    /// JSON body. #1769's byte-hash format moved that hash OUTSIDE the
    /// JSON — it's now a `<hash-hex><SP>` prefix on the line itself (see
    /// `crates/darkmux-flow/src/integrity.rs`'s module doc), so current
    /// AuditFileSink writes never populate this field; it stays `None` on
    /// every newly-written record and exists only so a pre-2.6.0 line
    /// (which DID embed `hash` here) still deserializes. Schema 1.5
    /// addition (#163).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Event-specific structured fields that aren't promoted to first-class
    /// `FlowRecord` members. Schema 1.6 addition (#204) — gives new event
    /// types (`dispatch.turn`, `dispatch.tool`, `dispatch.compaction`,
    /// `dispatch.reasoning`, `mission.compile.start/complete`) a place to
    /// carry their event-specific fields without growing the struct
    /// indefinitely.
    ///
    /// Convention: keys are snake_case strings; values are typed by event
    /// shape (e.g. `dispatch.tool` uses `tool_name: string`, `args_chars:
    /// integer`, `result_chars: integer`, `success: boolean`). See the
    /// emit sites in `dispatch.rs` / `dispatch_internal.rs` /
    /// `mission_propose.rs` for the per-event-type payload shapes.
    ///
    /// Older records (pre-1.6) lack the field; viewer treats absence as
    /// the empty object `{}`. New event types degrade to "action only" on
    /// older viewers — they see the action string and the standard
    /// FlowRecord fields, just not the event-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Work-queue claim id when this record was produced by a job that
    /// flowed through the global `darkmux:work` stream. Absent on direct
    /// local dispatches (the operator ran `darkmux dispatch <role>`
    /// with no `--machine`). Populated by the dispatch path when it claims
    /// work from the queue. Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    /// Retry counter for queued work — 1 on first attempt, 2+ on retries
    /// after lease expiry. Surfaces in `darkmux doctor` as a "recent
    /// retries" rollup. Absent on direct local dispatches (no retry
    /// semantics outside the queue). Schema 1.8 addition (#246).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Resolve the flows directory. Precedence (#661 Slice 3):
/// `env(DARKMUX_FLOWS_DIR) > config.dirs.flows > ~/.darkmux/flows`, with a
/// `/tmp/darkmux/flows` HOME-less (CI / sandbox) fallback. Delegates to the
/// single resolver in `darkmux_types::config_access` so the precedence — now
/// including the config tier — lives in exactly one place.
pub fn flows_dir() -> PathBuf {
    darkmux_types::config_access::flows_dir()
}

/// ISO 8601 UTC date string from current time — `YYYY-MM-DD`. Used for
/// per-day file naming (one JSONL file per UTC day), NOT for record `ts`.
pub fn day_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, m, d) = epoch_to_yyyymmdd(secs);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// ISO 8601 UTC datetime string from current time — `YYYY-MM-DDTHH:MM:SSZ`.
/// Used for `FlowRecord.ts`. Seconds precision is sufficient for the
/// dispatch / phase timing surfaces; finer precision is a future bump.
pub fn ts_utc_now() -> String {
    let secs = current_epoch_secs();
    let (y, mo, d) = epoch_to_yyyymmdd(secs);
    let (h, mi, s) = epoch_to_hhmmss(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

pub(crate) fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Convert unix epoch seconds to (year, month, day) in UTC.
/// Civil calendar algorithm from Howard Hinnant (public-domain).
pub(crate) fn epoch_to_yyyymmdd(epochs: i64) -> (i32, u8, u8) {
    let days = epochs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp as i32 + 3 } else { mp as i32 - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u8, d as u8)
}

/// Convert unix epoch seconds to (hour, minute, second) in UTC.
pub(crate) fn epoch_to_hhmmss(epochs: i64) -> (u8, u8, u8) {
    let secs_of_day = epochs.rem_euclid(86_400);
    let h = (secs_of_day / 3600) as u8;
    let mi = ((secs_of_day % 3600) / 60) as u8;
    let s = (secs_of_day % 60) as u8;
    (h, mi, s)
}

/// Resolve the machine identifier for new flow records.
///
/// Order of precedence:
/// 1. `DARKMUX_MACHINE_ID` env var — operator-named (e.g. `"studio"`,
///    `"mini-1"`). Fleet operators prefer logical names over DNS-style
///    identifiers, so the env override always wins. Re-read on every
///    call so a `set_var` in tests + operator shells takes effect
///    without a process restart.
/// 2. Cached `hostname(1)` output — POSIX-portable; works on macOS,
///    Linux, BSD without adding a dep. Hostname doesn't change during
///    process lifetime, so we cache the subprocess result to keep the
///    per-record write hot-path cheap AND to avoid the thread-yield
///    that would otherwise turn `flow::record()` into a synchronization
///    hazard for tests that mutate env without `#[serial_test::serial]`.
/// 3. `None` — extremely rare (CI in a sandbox without `hostname`).
pub fn resolve_machine_id() -> Option<String> {
    // env(DARKMUX_MACHINE_ID) > config.machine_id (#661 Slice 4). config_access
    // reads the env LIVE per-call, so a `set_var` in tests / operator shells
    // still takes effect without a process restart — the property this hot path
    // (and the serial tests) rely on. The hostname fallback below is unchanged.
    if let Some(id) = darkmux_types::config_access::machine_id() {
        return Some(id);
    }
    static HOSTNAME: OnceLock<Option<String>> = OnceLock::new();
    HOSTNAME
        .get_or_init(|| {
            std::process::Command::new("hostname").output().ok().and_then(|out| {
                if !out.status.success() {
                    return None;
                }
                let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if h.is_empty() { None } else { Some(h) }
            })
        })
        .clone()
}


#[cfg(test)]
mod forward_compat_tests {
    use super::*;

    /// (#1611) A record from a NEWER peer must still parse. Before the
    /// `#[serde(other)]` catch-alls, one unrecognized enum string failed the
    /// whole `FlowRecord`.
    ///
    /// Parsing is the FLOOR. The same record still does not re-serialize to
    /// its original bytes through THIS struct — see
    /// `unknown_variants_are_lossy_on_write_at_the_struct_level` below — but
    /// that lossiness stopped being a security concern once the audit chain
    /// moved to byte-hashing (#1769): the chain never round-trips a record
    /// through `FlowRecord` to verify it.
    #[test]
    fn a_record_from_a_newer_peer_parses_instead_of_failing_the_whole_record() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "catastrophe",
            "category": "quantum",
            "tier": "orbital",
            "stage": "teleport",
            "action": "dispatch start",
            "handle": "seat-1",
            "session_id": "task-t1",
            "model": "gpt-4o"
        }"#;
        let rec: FlowRecord =
            serde_json::from_str(wire).expect("an unknown enum variant must not fail the record");

        // The unknown values degrade to Unknown; everything else survives, which
        // is the whole point — a reader keeps the fields it understands.
        assert!(matches!(rec.level, Level::Unknown));
        assert!(matches!(rec.category, Category::Unknown));
        assert!(matches!(rec.tier, Tier::Unknown));
        assert!(matches!(rec.stage, Stage::Unknown));
        assert_eq!(rec.action, "dispatch start");
        assert_eq!(rec.session_id.as_deref(), Some("task-t1"));
        assert_eq!(rec.model.as_deref(), Some("gpt-4o"));
    }

    /// Known variants are untouched — the catch-all must not swallow a
    /// variant this binary DOES understand.
    #[test]
    fn known_variants_still_round_trip_exactly() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "info",
            "category": "telemetry",
            "tier": "local",
            "stage": "dispatch",
            "action": "telemetry.tokens",
            "handle": "seat-1"
        }"#;
        let rec: FlowRecord = serde_json::from_str(wire).unwrap();
        assert!(matches!(rec.level, Level::Info));
        assert!(matches!(rec.category, Category::Telemetry));
        assert!(matches!(rec.tier, Tier::Local));
        assert!(matches!(rec.stage, Stage::Dispatch));
        // And a re-serialize keeps the wire spelling — true for variants this
        // binary KNOWS. It is emphatically not true for unknown ones; that
        // asymmetry is the subject of the next test.
        let out = serde_json::to_value(&rec).unwrap();
        assert_eq!(out["category"], "telemetry");
        assert_eq!(out["stage"], "dispatch");
    }

    /// Leniency makes a newer record READABLE through this struct; it does
    /// not make re-serializing it through this struct REPRODUCE the
    /// original bytes. That WAS the audit chain's problem (#1768/#1769):
    /// the pre-byte-hash chain hashed a re-serialization, so this
    /// lossiness became a false tamper alert, and the bypass added to
    /// paper over it became a real one.
    ///
    /// It is no longer the chain's problem. #1769's byte-hash format
    /// hashes the literal bytes a writer put on disk and never
    /// round-trips a record through `FlowRecord` to compute or verify a
    /// hash — see `crates/darkmux-flow/src/integrity.rs`'s module doc and
    /// its own `a_newer_peers_record_with_an_unknown_enum_validates_cleanly`
    /// test, which is the sibling of this one at the byte-hash layer. This
    /// test still pins the STRUCT-level lossiness (useful for e.g. `darkmux
    /// flow tail`'s human-readable rendering, which DOES round-trip through
    /// this struct) — it is simply no longer a security-relevant fact.
    #[test]
    fn unknown_variants_are_lossy_on_write_at_the_struct_level() {
        let wire = r#"{
            "ts": "2026-08-03T12:00:00Z",
            "level": "catastrophe",
            "category": "audit",
            "tier": "local",
            "stage": "dispatch",
            "action": "dispatch start",
            "handle": "seat-1"
        }"#;
        let rec: FlowRecord = serde_json::from_str(wire).unwrap();
        let out = serde_json::to_value(&rec).unwrap();

        assert_eq!(
            out["level"], "unknown",
            "the writer's spelling is GONE on re-serialize through this struct — true, and no \
             longer load-bearing for the audit chain (#1769)"
        );
        assert_ne!(out["level"], "catastrophe");
    }

    /// (#1758) A record written by a pre-1.19.0 binary carries an
    /// `orchestrator` key this struct no longer has. It must still parse:
    /// `FlowRecord` has no `deny_unknown_fields`, so serde drops the key.
    ///
    /// The PR removing the field claimed this compatibility; it was true
    /// only BY CONSTRUCTION, with nothing pinning it. The existing
    /// forward-compat tests cover unknown enum VALUES, not unknown KEYS, so
    /// a future `deny_unknown_fields` would silently break every archived
    /// record and no test would notice. This is that pin.
    #[test]
    fn a_pre_1_19_record_carrying_the_removed_orchestrator_key_still_parses() {
        let old = r#"{
            "ts": "2026-08-01T00:00:00Z",
            "level": "info",
            "category": "dispatch",
            "tier": "local",
            "stage": "run",
            "action": "dispatch.start",
            "handle": "h1",
            "orchestrator": "claude-code",
            "machine_id": "studio"
        }"#;
        let rec: FlowRecord = serde_json::from_str(old)
            .expect("a record with the removed `orchestrator` key must still deserialize");
        assert_eq!(rec.handle, "h1");
        assert_eq!(rec.machine_id.as_deref(), Some("studio"));
    }
}
