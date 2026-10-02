/**
 * Pure logic for the session drill-in ("run detail" for a `#dispatch=<id>`
 * route): `runRegions()`, the derivation behind the run page's regions.
 * This is the "whole separate render surface"
 * `SessionReplay.tsx`'s pre-drill-in doc named as out of scope; this packet
 * is the one that builds it.
 *
 * Validated against the ONE real recorded golden this repo already has for
 * legacy's own render (`tests/parity/goldens/session-task-list.txt`'s
 * `=== stage ===` section, captured from `#dispatch=task-list` against the
 * real corpus fixture `tests/parity/corpus/flow-dispatch-task-list.json`) —
 * `sessionRun.test.ts` asserts this module's output matches that golden
 * BYTE-FOR-BYTE against the real fixture data, not a hand-rolled
 * approximation. That corpus happens to carry zero telemetry records
 * (every one of its 48 records is a scheduler `step start`/`step complete`
 * pair, category `work`) — which is exactly why the golden shows no CPU/RAM
 * host-load track and no context-window chart: legacy's own conditionals
 * (`procs.length&&loadRows`, `cx.length&&nctx`) render NOTHING for this
 * fixture either. Two consequences:
 *
 * 1. Every other region this module DOES emit (header, brief kv rows,
 *    metrics tiles, model track, detections) is genuinely golden-verified,
 *    not just "read from source and hoped right".
 * 2. The two SVG visualizations (the retired viewer's `loadRow`/`ctxChart` —
 *    per-sample CPU/RAM/GPU bars and a context-window step chart) are
 *    DELIBERATELY NOT ported here. Both are PURE re-renderings of numbers
 *    this module already surfaces as text (the CPU/RAM/GPU % samples have
 *    no OTHER textual summary in legacy either — the chart IS the only
 *    place that data appears — so this is a genuine, named scope cut, not
 *    a redundant one; the context-window chart's headline numbers (now/
 *    peak/window) DO have a textual home already, the CONTEXT metric tile
 *    below). Ledgered as a follow-up rather than silently narrowed — see
 *    the drill-in packet's report for the full reasoning.
 *
 * `nowMs` is the reference "now" every open/close/wall-clock computation
 * measures against — NOT `Date.now()`. This port has no scrubber for the
 * session route (`isLiveRoute` treats `session` as a historical-slice
 * fetch, not a live tail — see `route.ts`'s own doc), matching legacy's own
 * `state.t=tMax` set once at boot for a `#dispatch=`/`#mission=` catalog
 * query and never advanced (no `startLiveTail` runs for it either) — so
 * `nowMs` here is the MAX ts across the fetched records, not wall-clock.
 * Verified against the golden: the session's own `frozen_clock_ms` capture
 * timestamp is HOURS after the session's last record — using it instead of
 * `tMax` would NOT reproduce "17:51:54 so far".
 *
 * (U3-7/U5-2) That golden line READ "1071:54 so far" until this packet —
 * legacy's `fmt()` had no hour rollover, and this corpus's run is 17h51m.
 * The golden was rebaselined by hand (`tests/parity/README.md`: a golden
 * change is a spec change, made on purpose in a reviewed diff) because the
 * legacy rendering is the defect: 1071 minutes is not a duration a reader
 * can hold. Sub-hour values are byte-identical to legacy, so no other
 * golden moves — asserted directly in `lib/format.test.ts`.
 */

import { computeTMax, type RunState } from "../../lib/flow";
import { runStatusWord, type RunBadgeStatus } from "../../lib/runStatusWord";
import { DEFAULT_POLICY, NO_PRESENCE, endMs, isRunning, lifecycleAt, recordedActiveMs, recordedWallMs, toRunState, type Close, type CloseEdge, type Lifecycle, type LifecyclePolicy, type Presence } from "../../lib/lifecycle";
import { runIndex, sessionRun, type RunGroup, type RunRecords } from "../../lib/runRef";
import { fmtElapsed, clk, clkAt, fmtC } from "../../lib/format";
import { aggregateHostSamples, roundPct } from "../../lib/hostStats";
import { aggregateLiveState, aggregateTokenRate, averageGenerationRate, lastHeartbeatMs, liveStateWhileConnected } from "../../lib/tokenRate";
import type { LiveState, LiveStateReading } from "../../lib/tokenRate";
import { mergeLive, type LiveOverlay } from "../../lib/liveChannel";
import { isSingleShotWorkUsage, sumUsage } from "../../lib/usageRecords";

import { toolOutcome } from "../../lib/recordDetail";
import type { DispatchStartPayload } from "../../types/generated/DispatchStartPayload";
import { ACTION, CATEGORY, SOURCE, byTime, endPayloadOf, payloadOf, isBookendTerminal, isDispatchTerminal, latestByTime, recordsAsOf, type NormRecord, type NormSource } from "../../lib/ingest";
import { maxOf } from "../../lib/numbers";
import { sameMachine, sameUid } from "../../lib/machineIdentity";
import { NOT_REPORTING_STATUS } from "../../lib/machineAvailability";

/** SYSTEM's WALL CLOCK hover text, for a unit with no model section (the MODEL
 *  section's ACTIVE TIME has its own, below: it shows wall minus rest). */
const WALL_HINT_TITLE =
  "run time: the runtime's own measure of this execution, INCLUDING any rest. A mission step's badge covers a WIDER span (setup and gate included) and reads longer.";

/** The MODEL section's ACTIVE TIME hover text: wall clock minus every rest, the
 *  figure `darkmux run stats` prints as active. The brief's timing line beside it
 *  keeps the wall clock. */
const ACTIVE_HINT_TITLE =
  "run time, active: the run's wall clock minus every rest it took (the figure darkmux run stats reports as active); the timing line in the brief shows the wall clock. A mission step's badge covers a WIDER span (setup and gate included) and reads longer. While the run is live this is the time elapsed so far; rests settle in the final figure.";

/** (#2863) The detectors a clean run passed, in the order the old sentence
 * named them (`cycle, tool-failure, reasoning-loop, edit-drift`). One list,
 * read by the signals card and by the text mirror its tests compare against
 * the parity golden, so the two cannot drift apart.
 *
 * (#2887) `repetition` covers TWO producers under one name: the in-stream
 * degeneracy gate (`dispatch.gate.observation`/`dispatch.gate.abort`,
 * forwarded as `telemetry.detector` records with `kind:"repetition"`) and
 * the reasoning check-in's own judgment (`dispatch.checkpoint` with
 * `would_conclude:true`, read directly below — it is not a detector
 * telemetry record, so it cannot ride the `kind` field the same way, but it
 * is pushed into `finds` under the SAME `"repetition"` kind). Before this,
 * neither producer had ANY entry here, which is the defect the issue names:
 * a run the gate flagged 14 times still read CLEAN. */
export const CLEAN_DETECTORS = ["cycle", "tool failure", "reasoning loop", "edit drift", "repetition"] as const;

interface SessionHeader {
  /** Pre-uppercased (`.sub h2{text-transform:uppercase}` in legacy CSS —
   * this port uppercases the string directly, per `lib/format.ts`'s
   * "uppercase the STRING directly" discipline, rather than depending on a
   * stylesheet rule this port is free to change). */
  pillLabel: string;
  /** (#2813) The canonical run status; the pill's look reads it. `not_reporting` (5.0
   *  R3): the run reads running but the machine it ran on is not reporting. */
  status: RunBadgeStatus;
  /** Pre-uppercased, same reason. */
  role: string;
  sid: string;
  /** `escN(state.machine)` in legacy. Left "" here — NOT because
   * `state.machine` is never set on a real path (an earlier version of
   * this comment claimed that; it's false — `drillMachine`/`goMachine`
   * both set `state.machine=m`, `drillSession` never clears it, so legacy's
   * PRIMARY path into a session drill-in — the machine page's own run-row
   * "open →" link, `data-act="session"` → `drillSession(sid)` — carries
   * that machine context forward and DOES render the machine link there).
   * The golden this module is checked against (`session-task-list.txt`)
   * was captured via the OTHER real entry point — a bare `#dispatch=<id>`
   * catalog deep-link, which never touches `state.machine` at all — so its
   * "(task-list on )" (nothing after "on ") is genuinely empty on THAT
   * path, but not evidence the field is dead everywhere.
   *
   * This port's own output is still correct empty, for an unrelated
   * reason: this port never had a machine-scoped run row with a session
   * drill-in link at all. Pre-#1809 that was `runLines.ts`'s
   * `machineRunLines` — a collapsed `<summary>` with no click-through.
   * #1809 (finishing #1508 step 4) removed that list entirely; the machine
   * page now links out to the Runs lens (`#lens=runs&machine=<uid>`)
   * instead of rendering its own rows. `RunsBoard`'s rows carry their OWN
   * drill-ins now (`/mission/<id>/graph` for a tracked mission/dispatch, the
   * in-page lab-run detail for a lab run — see `RunsBoard.tsx`'s
   * `activateRun`), but neither is a `#dispatch=` drill either. So the real
   * residual gap is unchanged in shape, just relocated: an operator still
   * cannot reach a bare session-subsystem view (this file's own render
   * target) FROM a machine-scoped list, by any path this port builds today.
   * Not built here — ledgered as a follow-up, not a silent narrowing.
   * Deriving a machine name from the session's OWN records instead (rather
   * than building an "open →" link) would be adding information legacy
   * itself doesn't show on this path, not a port. */
  machineName: string;
}

export interface SessionRunView {
  header: SessionHeader;
  /** Flattened brief lines: `["run", <label>, <value>, ...]`, or `[]` when
   * there is nothing to show (no route/runtime/image/model/workspace/
   * mission/timing/prompt data at all — doesn't happen in practice since
   * `route`+`timing` are always present, kept for completeness). */
  briefLines: BriefEntry[];
  /** (#1973) Payloads the brief SUMMARIZES and previously threw away — the
   * prompt above all, which `briefLines` renders as `prompt · 1430 chars`
   * while holding the string itself.
   *
   * That is the THIRD instance of one shape in this codebase (tool-call
   * arguments and session records were the first two, both #1960): a
   * renderer takes `.length` of a payload it is holding and discards the
   * payload. So this field is deliberately the full text, and the golden
   * test asserts it is reachable AFTER expanding — an assertion on the
   * summary line alone would pass against the very bug it is meant to
   * catch. */
  disclosures: Disclosure[];
  /** (U3-6) `hint` is a SHORT label rendered by one CSS rule off
   * `data-hint` (so it never enters `textContent` and
   * `goldens/session-task-list.txt` stays byte-identical); `hintTitle` is the
   * long form on hover. Present only where a tile's number is ambiguous
   * against a number the operator can see on another surface.
   *
   * `sub` (#2xxx — the "CTX PEAK 19K / 262.144K" quirk) is a QUIET third
   * line for a fact that qualifies the value without restating it — CTX's
   * window ceiling, today. A label may never repeat the value it sits next
   * to (that was the defect: `CTX PEAK 19K / 262.144K WINDOW` printed the
   * headline number a second time, inside its own label); a fact that
   * belongs beside the value but isn't the value itself goes in `sub`
   * instead. Every tile renders its `sub` slot, empty or not, so the grid
   * doesn't go ragged the moment one tile has more to say than its
   * neighbors — see `.session-run .msub` in `styles.css`. */
  metrics: Array<{
    value: string;
    label: string;
    hint?: string;
    hintTitle?: string;
    sub?: string;
    unit?: string;
    /** (#2890) The context cell's thin bar: the context in use now and its
     *  peak, each as a percentage (0..100) of the window. */
    bar?: { nowPct: number; peakPct: number };
  }>;
  /** (#1973) Which metrics describe the MODEL's work and which describe the
   * HARNESS around it. `metrics` stays the flat, ordered list every existing
   * consumer reads; this is the grouping laid over it, by index.
   *
   * The split follows the ACTOR, not the effect: compaction lands in HARNESS
   * because the harness decides and performs it through a utility role, even
   * though what it acts on is the model's context.
   *
   * The split is not cosmetic. It is what lets a step that ran no model
   * render without holes: a `procedural.shell` step has harness metrics and
   * NO model metrics, so the model group is absent rather than showing
   * `0 turns · 0 tokens`, which would be a lie shaped like data. It also
   * answers the operator question that prompted this — "what does
   * `model (lms)` mean, and are these numbers about the model or about
   * darkmux?" */
  metricScope: { model: number[]; system: number[] };
  /** (#2877) The live token-rate scope's data for a run STILL IN PROGRESS.
   * `null` whenever there is nothing to show it for: no model work at all
   * (same gate as `metricScope.model`), OR the run already finished — a
   * finished run's TOK/S tile is a plain `push()`'d metric instead (see
   * the "TOK/S" push below), matching the issue's "when the run finishes,
   * the scope goes and the tile shows the final measured tok/s". */
  liveTokScope:
    | {
        tokensPerSec: number | null;
        /** (#2885) `true` when `tokensPerSec` is carried forward from an
         *  earlier turn rather than freshly measured from the current
         *  turn's own two most recent heartbeats — the caller renders the
         *  number dimmed. See `lib/tokenRate.ts::AggregatedTokenRate`. */
        carried: boolean;
        stalled: boolean;
        /** (#2877 pass 2) The legible between-heartbeats state — see
         *  `lib/tokenRate.ts::deriveLiveState`'s own doc. `stalled` above is
         *  now DERIVED from this (`state === "stalled"`), so the two can
         *  never disagree. */
        state: LiveState | null;
        /** Present only when `state === "rest"` — whole seconds left in the
         *  reported rest window. */
        restSecondsLeft?: number;
        /** (#2961) Present only when `state === "rest"`: when the rest ends,
         *  on the clock below. See `LiveStateReading.restEndMs`. */
        restEndMs?: number;
        /** (#2961) The page clock this reading was derived at (the playhead
         *  in playback, the ticking wall clock live, the newest record's time
         *  when frozen). The scope extrapolates from it between renders to
         *  phase REST's seconds hand. */
        clockMs?: number;
        /** (#2950) Present only when `state === "rest"` and the rest's own
         *  record says why. See `LiveStateReading.restReason`. */
        restReason?: string;
        /** (#2886 pass 4, finding 7) `true` exactly when `state === null`
         *  because the connection was lost/half-open, NOT because there is
         *  genuinely no live execution to have a state for (a mission
         *  between model steps). The caller uses this to render a DISTINCT
         *  "no signal" word rather than reusing the ambiguous "no model
         *  working" wording both cases would otherwise share. */
        noSignal: boolean;
        /** (#2890) Present only when `state === "tools"`: the tool the scope's
         *  center draws as an icon. See `LiveStateReading.toolName`. */
        toolName?: string;
        /** (#2963) The file of the call running now, when known. See
         *  `LiveStateReading.toolPath`. */
        toolPath?: string;
        /** (#2889) Present only while the model WRITES a tool call — see
         *  `LiveStateReading.writing` / `writingSeconds`. */
        writing?: true;
        writingSeconds?: number;
        /** (#2890) Present only while generating and the model is reasoning
         *  rather than writing visible text. See `LiveStateReading.thinking`. */
        thinking?: true;
        /** (#2915) Present only while PROMPT because the execution is
         *  compacting. See `LiveStateReading.compacting`. */
        compacting?: true;
        compactingSeconds?: number;
      }
    | null;
  /** (#2890) A FINISHED run's average generation rate, shown in the MODEL
   *  hero scope's center ("avg tok/s") rather than as a TOK/S tile. `sub` is
   *  the qualifier `averageGenerationRate`'s labeling rules produce when the
   *  average is partial or a fallback ("avg · 1 of 2 turns", "avg · wall
   *  clock", "avg · unbilled"), `null` for the ordinary average. `null`
   *  while the run is live or when it did no model work. */
  finishedTokRate: { average: string; sub: string | null } | null;
  /** (#2863) Whether the MODEL section shows its model card. False for an
   * endpoint-served run: the card could only repeat the model name the
   * brief's `model` row already shows. */
  showModelCard: boolean;
  modelTrackLabel: string;
  modelTrackLines: string[];
  /** (#2863) The same loaded models as structure, for the card: the model
   * that ran first. Present only when this session loaded models itself;
   * the endpoint and no-telemetry cases have only their `modelTrackLines`. */
  modelEntries?: Array<{ name: string; gb: number | null; ran: boolean | null }>;
  /** (#1972) Is this run still going? Drives whether the page subscribes to
   *  the shared clock at all — a finished run's elapsed time is a fixed fact,
   *  and re-rendering it once a second is pure waste. */
  /** (#1973) Whether this unit did MODEL work. False for a `procedural.*`
   *  step, whose model pane and loaded-models track are ABSENT rather than
   *  rendered full of em-dashes and a `0 COMPACTIONS` that asserts something
   *  impossible. */
  hasModelWork: boolean;
  live: boolean;
  /** Whether the run recorded an ending (`lib/lifecycle.ts`'s `close`: a
   *  closed run, or a session that recorded its end with nothing opened):
   *  the fact the status reads, so the pulse says "finished" exactly when
   *  the pill states a verdict, never "may be abandoned" beside one. */
  ended: boolean;
  /** (#1972) When the most recent proof-of-life record landed, or `null` if
   *  none has. The pulse pauses when this goes quiet — see `LivenessPulse`. */
  lastBeatMs: number | null;
  /** (#1973) Renamed from `detections`. See the signals block in
   *  `runRegions` for why grouping, severity and run-relative times replaced
   *  a flat list of grey strings. */
  signalsLabel: string;
  signalGroups: SignalGroup[];
  /** (#2887 F2) The run-level `detection_degeneracy_policy.value` the
   * dispatch actually ran under (`payload.bounds.detection_degeneracy_
   * policy` on the `dispatch.start` record — the SAME resolved value the
   * host stamps for the container, distinct from any per-record `policy`
   * field an individual gate/checkpoint record may or may not carry).
   * `true` only when that value is literally `"off"` — the gate never ran
   * at all, so the SIGNALS card must not claim "repetition: clean" (which
   * asserts the detector looked and found nothing); it renders the
   * checklist cell as "off" instead. `false` covers both "ran, found
   * nothing" (conclude/warn/record with no findings; enforce/observe in runs
   * recorded before 4.0) and "unknown" (no
   * `dispatch.start`, or an older record predating this field) — an
   * unknown run-level policy must NOT render as off, since that would be
   * claiming something the data doesn't say either. */
  repetitionOff: boolean;
  /** (#2887 N2) Whether this run's `dispatch.start` names a `flow_schema`
   * of 1.56.0 or later — the version the degeneracy gate's own findings
   * started reaching the flow stream at. `false` (never `true` by
   * default) for any run recorded before that field existed, or with no
   * `dispatch.start` in the window at all. The CLEAN checklist's
   * "repetition" cell renders a checkmark only when this is `true` AND
   * `repetitionOff` is `false` — otherwise "(not recorded)", so the card
   * never claims a check the record can't support. */
  repetitionRecorded: boolean;
}

/** (#1989) Render a detector's `detail` without destroying it.
 *
 *  `String(value)` collapses every object to `[object Object]`. A detector
 *  payload carrying structured data is exactly the case where an operator
 *  most needs to see it, so a non-string is serialized rather than
 *  stringified. An absent detail says so, instead of printing `undefined`. */
function signalDetail(value: unknown): string {
  if (typeof value === "string") return value;
  if (value == null) return "(no detail)";
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    // Circular or otherwise unserializable — fall back rather than throw,
    // since a malformed signal must never take the whole page down.
    return String(value);
  }
}

/** (#1973) `+m:ss` / `+h:mm:ss` from run start. Sub-minute signals still read
 *  `+0:07` rather than collapsing to `+0`, because "seven seconds in" and
 *  "immediately" are different findings. */
function runOffset(deltaMs: number): string {
  if (!Number.isFinite(deltaMs) || deltaMs < 0) return "";
  const total = Math.floor(deltaMs / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const sec = total % 60;
  const two = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `+${h}:${two(m)}:${two(sec)}` : `+${m}:${two(sec)}`;
}

/** (#1973) A behavioral signal raised during the run. `severity` comes from
 *  the emitter — `dispatch_internal`'s detector payload has always carried it
 *  — and is NOT derived from the record's `level`, which is `Info` for every
 *  detector record and therefore says nothing. */
type SignalSeverity = "warn" | "info";

interface Signal {
  kind: string;
  severity: SignalSeverity;
  detail: string;
  fix?: string;
  /** Absolute time, or `null` for a signal synthesized from other records
   *  rather than emitted (`jit-model-swap`) — which has no moment of its own
   *  and must not borrow one. */
  atMs: number | null;
  /** Run-relative label (`+2:14`), empty when `atMs` is unknown. Relative
   *  rather than wall-clock because the question is "how far into the run did
   *  this start", which an absolute timestamp makes the reader compute. */
  offsetLabel: string;
}

interface SignalGroup {
  kind: string;
  severity: SignalSeverity;
  count: number;
  signals: Signal[];
}

/** (#1973) One payload the brief summarizes, carried in full so the renderer
 *  can expand it in place. `chars` is the AUTHORITATIVE length from the
 *  record (`prompt_chars`) when present, so a truncated payload still reports
 *  its true size rather than the size of what survived. */
interface Disclosure {
  id: string;
  label: string;
  chars: number;
  truncated: boolean;
  text: string;
}

/** A run-brief entry, tagged so the renderer can style a LABEL differently
 *  from a VALUE.
 *
 *  It used to be a flat `string[]` with labels and values as adjacent
 *  elements, which left the renderer no way to tell them apart — so the CSS
 *  styled "everything after the first line" identically and `route` looked
 *  exactly like `LMStudio · local · this machine`. The whole block read as an
 *  undifferentiated list.
 *
 *  The TEXT is deliberately unchanged: each entry still renders as its own
 *  block element, in the same order, so `goldens/session-task-list.txt` — which
 *  pins label and value as separate lines — keeps passing. Only the class
 *  differs. */
export interface BriefEntry {
  kind: "label" | "value" | "note";
  text: string;
  /** When set, the entry renders as an in-app link to this hash.
   *
   *  Exists because both drill-in destinations were DEAD ENDS: neither the
   *  mission lens nor this one had a single outbound navigation, so landing on
   *  either left the back button as the only exit and no way to reach the
   *  other view of the same work. The mission id was already displayed here as
   *  inert text; making it a link costs nothing and is the cheapest half of the
   *  fix. The text is unchanged, so the golden still matches. */
  href?: string;
}

function pushKv(rows: BriefEntry[], label: string, value: string | null | undefined) {
  if (value != null && value !== "") {
    rows.push({ kind: "label", text: label });
    rows.push({ kind: "value", text: value });
  }
}

/** (#2902 step 2a, #3067) An execution's token numbers: the plain sum of its
 *  usage records, ALL of them (the one `sumUsage`, the figure the runs board's
 *  TOKENS cell and the server's `Run.tokens` show), with the utility part
 *  (compaction, radio routing) named beside it as `utility` rather than left
 *  out: a page showing less than its row is two answers for one run. `null`
 *  when nothing was measured (the tile shows "—"). */
function executionTokens(records: readonly NormRecord[]): { prompt: number; completion: number; utility: number } | null {
  const s = sumUsage(records);
  return s.reported > 0 ? { prompt: s.prompt, completion: s.completion, utility: s.utility } : null;
}

/** The three token figures a tile reads, each `null` when nothing was measured. */
function tokenFigures(tok: ReturnType<typeof executionTokens>): { tokIn: number | null; tokOut: number | null; tokUtility: number | null } {
  return { tokIn: tok ? tok.prompt : null, tokOut: tok ? tok.completion : null, tokUtility: tok ? tok.utility : null };
}

interface MissionModelRollup {
  hasEvidence: boolean;
  turns: number | null;
  tokIn: number | null;
  tokOut: number | null;
  tokUtility: number | null;
  ctxPeak: number;
  ctxNow: number;
  nctx: number;
  loadLines: string[];
}

/** An execution's context-window figures from its `context` telemetry: the
 * window (`nctx`, the earliest sample's `max`), the peak use, and the use
 * NOW, which is the LATEST sample's (`latestByTime`: an untimed sample only
 * when no timed one exists, the bad-timestamp policy). */
function contextFigures(tel: readonly NormRecord[]): { samples: number; nctx: number; ctxPeak: number; ctxNow: number } {
  const cx = tel.filter((r) => r.source === SOURCE.Context).sort(byTime);
  const used = (r: NormRecord | undefined) => Number((r?.fields as Record<string, unknown> | undefined)?.used) || 0;
  const max0 = cx.length ? Number((cx[0].fields as Record<string, unknown>)?.max) : NaN;
  return {
    samples: cx.length,
    nctx: cx.length && Number.isFinite(max0) && max0 > 0 ? max0 : 0,
    ctxPeak: maxOf(cx.map(used)) ?? 0,
    ctxNow: cx.length ? used(latestByTime(cx)) : 0,
  };
}

/** (#2759) A run's OWN top-level session — the run-grain trio of
 *  `dispatch start`/`dispatch complete`/`mission.grow`, the shape a crawl
 *  mission's own top-level bookend mints — carries no MODEL telemetry at
 *  all: every turn, token and context record lives on the mission's INNER
 *  role executions, each minted under its own `session_id` (contract 8: a
 *  step contains zero or more role executions, and the run's own session is
 *  not one of them). Reading only `sid`'s own records for the MODEL pane is
 *  only ever correct for the degenerate `RunKind::Dispatch` shape — a
 *  crew-of-one graph where the run IS the one execution — and silently
 *  empty for any mission with real work inside it.
 *
 *  This walks every OTHER run of this run's mission (`runRef.ts`'s
 *  `groupsOfMission`), and sums the MODEL-scoped numbers off each one that
 *  did real model work — skipping a utility role's sub-execution so its
 *  tokens never fold into a specialist's total.
 *
 *  KNOWN NARROWING, named rather than hidden: this walks by `session_id`,
 *  not by role EXECUTION. a task session is task-scoped, so a
 *  `dispatch.map` fan-out mints ONE session_id shared by every sibling
 *  seat — this reads them as a single execution and sums their records
 *  together, the same simplification `runRegions`'s own single-session path
 *  already makes for a session carrying more than one `dispatch.start`
 *  (only the LATEST is treated as "the" attempt). Separating siblings would
 *  need the `index`/`remote` keys #2690 put on those records; not attempted
 *  here — this fix targets the reported defect (a run session with zero
 *  telemetry finding real numbers on its inner sessions), not per-seat
 *  breakdown. */
function rollUpMissionModelWork(siblings: readonly RunGroup[]): MissionModelRollup {
  const acc: MissionModelRollup = { hasEvidence: false, turns: null, tokIn: null, tokOut: null, tokUtility: null, ctxPeak: 0, ctxNow: 0, nctx: 0, loadLines: [] };
  for (const g of siblings) {
    const fig = executionFigures(g.records);
    if (fig) addFigures(acc, fig);
  }
  return acc;
}

/** One inner execution's MODEL numbers, or `null` when it did no model
 *  work. (#3067) Its tokens, utility named beside them. */
function executionFigures(own: readonly NormRecord[]): (ModelFigures & { loads: NormRecord[] }) | null {
  const tel = own.filter((r) => r.category === CATEGORY.Telemetry);
  const tok = executionTokens(own);
  const cx = contextFigures(tel);
  const loads = bySource(tel, SOURCE.Lms).filter(isLoad);
  const turns = turnCount(own);
  const fig = { turns, ...tokenFigures(tok), ctxPeak: cx.ctxPeak, ctxNow: cx.ctxNow, nctx: cx.nctx, loads };
  return loads.length > 0 || turns != null || tok != null || cx.samples > 0 ? fig : null;
}

/** THE turn count of a set of records, for every dispatch path: the SUM of the
 *  `total_turns` of the executions' terminal records (one type,
 *  `DispatchEndPayload`, for `dispatch.complete` and `dispatch.error`; every
 *  producer states it). It sums over the same records `executionTokens` sums
 *  usage over, so an execution retried within the set, or several sharing one
 *  session, count once each in both figures. With no terminal that states it
 *  (a run still in progress, or an archive from before every producer did) it
 *  falls back to the turns so far: the `runtime` telemetry record, which
 *  `flowToRenderModel` derives from the `dispatch.turn` records. `null` when
 *  neither exists. */
function turnCount(records: readonly NormRecord[]): number | null {
  const stated = records.filter((r) => isDispatchTerminal(r.action)).flatMap((r) => endPayloadOf(r)?.total_turns ?? []);
  if (stated.length > 0) return stated.reduce((a, b) => a + b, 0);
  const runtime = bySource(records, SOURCE.Runtime).slice(-1)[0];
  const sofar = Number((runtime?.fields as Record<string, unknown> | undefined)?.turns);
  if (runtime && Number.isFinite(sofar)) return sofar;
  // (5.0 R3) A single-shot run has no turn records: until its terminal states
  // the count, each work call made so far is one turn.
  const calls = records.filter(isSingleShotWorkUsage).length;
  return calls > 0 ? calls : null;
}

const addOpt = (a: number | null, b: number | null): number | null => (b == null ? a : (a ?? 0) + b);

function addFigures(acc: MissionModelRollup, f: ModelFigures & { loads: NormRecord[] }): void {
  acc.hasEvidence = true;
  acc.turns = addOpt(acc.turns, f.turns);
  acc.tokIn = addOpt(acc.tokIn, f.tokIn);
  acc.tokOut = addOpt(acc.tokOut, f.tokOut);
  acc.tokUtility = addOpt(acc.tokUtility, f.tokUtility);
  acc.ctxPeak = Math.max(acc.ctxPeak, f.ctxPeak);
  acc.ctxNow = Math.max(acc.ctxNow, f.ctxNow);
  acc.nctx = Math.max(acc.nctx, f.nctx);
  // Every inner execution records each model resident when it started, so
  // one model appears once per execution; list it once.
  for (const l of f.loads) {
    const line = `${loadFields(l).model} · ${loadFields(l).gb ?? "?"}GB`;
    if (!acc.loadLines.includes(line)) acc.loadLines.push(line);
  }
}

/** (#2887 N2) `version >= min`, comparing dotted numeric components
 * (`"1.56.0"` vs `"1.9.0"` — a plain string compare would read `"1.56.0" <
 * "1.9.0"` since `'5' < '9'` lexicographically, which is wrong). `null`
 * (no `flow_schema` on the record at all — every run before this field
 * itself) reads as `false`, never as "assume current". */
function flowSchemaAtLeast(version: string | null, min: string): boolean {
  if (!version) return false;
  const va = version.split(".").map((n) => parseInt(n, 10) || 0);
  const vb = min.split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(va.length, vb.length); i++) {
    const a = va[i] ?? 0;
    const b = vb[i] ?? 0;
    if (a !== b) return a > b;
  }
  return true;
}

/** Where the page's run stands as of `nowMs`: every lifecycle fact
 *  `runRegions` reads, from `lifecycle.ts` and nowhere else. */
interface RunContext {
  run: RunRecords | null;
  l: Lifecycle | null;
  /** The attempt's `dispatch.start`: the brief's payload. */
  d: NormRecord | null;
  /** The run's first record: where its name falls back to. */
  firstSessRec: NormRecord | null;
  /** FINITE, or it poisons every downstream comparison: the attempt's
   *  start, else the run's first record, else `now`. */
  startTs: number;
  /** Whether a record belongs to this attempt. A record with an unparsable
   *  `ts` belongs to its run's latest attempt: visible and wrong-looking,
   *  never silently dropped. */
  inAttempt: (r: NormRecord) => boolean;
  closeTs: number | null;
  /** Where the run's time ends: its close, or its last sign of life when
   *  it stopped with no ending recorded; `null` while it runs. */
  endTs: number | null;
  /** The dispatch terminal the outcome was read from (`wall_ms`, the
   *  endpoint, the tokens); `null` for any other close. */
  c: NormRecord | null;
  done: boolean;
  /** (#1988) The close came from a terminal timestamped before the run's
   *  own start: honored, and said so. */
  skewedClose: boolean;
  state: RunState;
}

function runContext(data: NormRecord[], sid: string, nowMs: number, policy: LifecyclePolicy, presence: Presence): RunContext {
  const run = sessionRun(data, sid, nowMs);
  return run ? contextOf(run, lifecycleAt(run, nowMs, policy, presence), nowMs) : noRunContext(nowMs);
}

/** A session with no records in the window: nothing opened, nothing to
 *  read. */
function noRunContext(nowMs: number): RunContext {
  return {
    run: null,
    l: null,
    d: null,
    firstSessRec: null,
    startTs: nowMs,
    inAttempt: () => false,
    closeTs: null,
    endTs: null,
    c: null,
    done: false,
    skewedClose: false,
    state: { status: "planned" },
  };
}

function contextOf(run: RunRecords, l: Lifecycle, nowMs: number): RunContext {
  const members = new Set<NormRecord>(run.attempt ? run.attempt.records : run.group.records);
  const firstSessRec = run.group.records[0] ?? null;
  const done = !isRunning(l);
  return {
    run,
    l,
    d: run.attempt ? run.attempt.start : null,
    firstSessRec,
    startTs: l.startMs ?? firstSessRec?.tMs ?? nowMs,
    inAttempt: (r) => members.has(r),
    endTs: done ? endMs(l, nowMs) : null,
    done,
    state: toRunState(l),
    ...closeFacts(l.close),
  };
}

/** What the page reads off a run's close: when, whether its clock was
 *  skewed, and the bookend terminal (a run's or an execution's) its payload
 *  comes from. */
function closeFacts(close: Close | null): Pick<RunContext, "closeTs" | "skewedClose" | "c"> {
  if (!close) return { closeTs: null, skewedClose: false, c: null };
  return { closeTs: close.atMs, skewedClose: close.skewed, c: isBookendTerminal(close.record.action) ? close.record : null };
}

/** The attempt's telemetry, by source. */
interface AttemptTelemetry {
  tel: NormRecord[];
  lms: NormRecord[];
  /** Host cpu/ram/gpu samples: the retired per-session `telemetry.process`
   *  and the machine's own `machine.telemetry` over the run's window. */
  procs: NormRecord[];
  dets: NormRecord[];
  loads: NormRecord[];
  /** The models loaded, first-seen order. */
  distinct: string[];
  comps: NormRecord[];
}

const bySource = (recs: readonly NormRecord[], source: NormSource): NormRecord[] => recs.filter((r) => r.source === source);

const isLoad = (r: NormRecord): boolean => (r.fields as Record<string, unknown> | undefined)?.event === "load";

function attemptTelemetry(visible: readonly NormRecord[], ctx: RunContext): AttemptTelemetry {
  const tel = visible.filter((r) => ctx.inAttempt(r) && r.category === CATEGORY.Telemetry);
  const lms = bySource(tel, SOURCE.Lms);
  const loads = lms.filter(isLoad);
  return {
    tel,
    lms,
    procs: [...bySource(tel, SOURCE.Host), ...hostSamplesOf(visible, ctx)],
    dets: bySource(tel, SOURCE.Detector),
    loads,
    distinct: [...new Set(loads.map((r) => (r.fields as Record<string, unknown>).model as string))],
    comps: bySource(tel, SOURCE.Compaction),
  };
}

/** (#2413 M4) The machine's host samples over this run's window. The
 * retired per-dispatch `telemetry.process` record rode this session's own
 * `session_id` (and still matches through `attemptTelemetry`'s `process`
 * source, for historical runs); its replacement, `machine.telemetry`, is
 * machine-scoped (`category: "machinery"`, no `session_id`), so it cannot
 * belong to an attempt. The server joins the samples covering the run's
 * window into the same record set (darkmux-serve's
 * `join_host_samples_into_session_records`, keyed on machine_uid and the
 * dispatch window), so here it is a plain time-window filter.
 *
 * (#2413 round 3 CONSIDER 3) Gated on the run's own machine (its start
 * record's, else its first record's) too: a multi-machine playback fixture
 * would otherwise render every machine's samples on every run's pane. */
function hostSamplesOf(visible: readonly NormRecord[], ctx: RunContext): NormRecord[] {
  // (5.0 R2) One identity rule: by uid when both sides carry one, else by a
  // name spelling. A run that names no machine at all matches no sample;
  // it never accepts every machine's.
  const runMachine = ranOn(ctx.d, ctx.firstSessRec);
  const inWindow = (t: number) => t >= ctx.startTs && (ctx.closeTs == null || t <= ctx.closeTs);
  return visible.filter(
    (r) => r.action === ACTION.MachineTelemetry && sameMachine({ uid: r.machine_uid, name: r.machine_id }, runMachine) && (r.tMs === null || inWindow(r.tMs)),
  );
}

/** (#2011) The run's duration and the run-time tile's figures. `activeElapsed`
 * is the ACTIVE TIME figure: a finished run's `recordedActiveMs` (wall minus
 * rest), else what `wallElapsed` reads (a live run's elapsed so far, a close
 * with no recorded wall). A finished
 * run's duration is its dispatch terminal's own `wall_ms` (the runtime's
 * measure, `recordedWallMs`), not recomputed from two timestamps: a page
 * whose records go stale keeps counting, and taking the number from the
 * record that ENDS the run means the worst a stale page can do is show a
 * stale label, never invent a duration. It also avoids two arithmetic
 * hazards: a terminal timestamped before its own start (the skewed close)
 * subtracting to a negative, and an unparsable `ts` to `NaN`. A close with
 * no payload (a `session.end`, an archived record) falls back to its end
 * minus its start. (U3-7/U5-2) `fmtElapsed` says hours past an hour.
 * (#2860) How it ended rides the tile's `sub` line, never the figure, which
 * is contracted to one short `nowrap` value. */
function wallClock(ctx: RunContext, nowMs: number): { runWallMs: number; activeElapsed: string; wallBase: string; wallSub: string | undefined } {
  const recorded = recordedWallMs(ctx.l?.close ?? null);
  const runWallMs = recorded ?? (ctx.endTs !== null ? ctx.endTs - ctx.startTs : NaN);
  const wallElapsed = ctx.done ? fmtElapsed(runWallMs) : fmtElapsed(nowMs - ctx.startTs);
  const activeMs = recordedActiveMs(ctx.l?.close ?? null);
  return {
    runWallMs,
    activeElapsed: ctx.done && activeMs !== null ? fmtElapsed(activeMs) : wallElapsed,
    wallBase: ctx.done ? wallElapsed : `${wallElapsed} so far`,
    wallSub: errorOutcome(ctx.l?.close?.edge),
  };
}

/** The machine a run executed on, from its own records: the `dispatch.start`'s
 *  (else the session's first record's) machine name and hardware uid. The one
 *  derivation the header's "on <machine>" and the route line both read. */
function ranOn(d: NormRecord | null, first: NormRecord | null | undefined): { name: string; uid: string } {
  return { name: String(d?.machine_id || first?.machine_id || ""), uid: String(d?.machine_uid || first?.machine_uid || "") };
}

/** The status and word the page's pill shows (5.0 R3): the run's own, except
 *  that a run reading running on a machine that is not reporting reads "not reporting".
 *  `open` is what the brief's timing line says of a run with no end: the same
 *  word, so the page states its status in one voice. */
function pillOf(
  state: RunState,
  notReporting: boolean | undefined,
): { status: SessionHeader["status"]; label: string; open: string } {
  if (state.status === "running" && notReporting) {
    const word = runStatusWord(NOT_REPORTING_STATUS);
    return { status: NOT_REPORTING_STATUS, label: word, open: word };
  }
  return { status: state.status, label: runStatusWord(state.status, state.abandonReason), open: "running" };
}

/** Whether the run executed on the machine showing it. `viewerUid` is the page's
 *  own identity (`localMachineUid`: a hardware uid, or its name when no uid is
 *  known); `null` when it is not known yet, which claims nothing. */
function ranHere(on: { name: string; uid: string }, viewerUid: string | null): boolean {
  // One identity rule (`lib/machineIdentity`): uids compare case-normalized.
  // A record with no uid falls back to its name, as the viewer's own identity
  // may be a name when the daemon has no uid.
  return viewerUid !== null && ((on.uid !== "" && sameUid(on.uid, viewerUid)) || (on.name !== "" && sameUid(on.name, viewerUid)));
}

/** (#2834) The route line: the dialect and address the dispatch record
 *  names, read as facts. `openai:` names the request FORMAT, not a vendor,
 *  so a local server speaking it is labelled by its address, never as
 *  having left the machine. A run on local LM Studio names the machine it
 *  ran on: "this machine" only when that machine IS the viewer's own, else
 *  its name (a relayed run ran on the peer), and nothing when the records
 *  name none. */
function routeLabel(ep: string | undefined, on: { name: string; uid: string }, viewerUid: string | null): string {
  if (!ep) {
    if (ranHere(on, viewerUid)) return "LMStudio · local · this machine";
    return on.name !== "" ? `LMStudio · local · ${on.name}` : "LMStudio · local";
  }
  const i = ep.indexOf(":");
  const kind = i >= 0 ? ep.slice(0, i) : "";
  const rest = i >= 0 ? ep.slice(i + 1) : ep;
  const label = kind === "azure" ? "Azure OpenAI" : kind === "openai" ? "OpenAI" : kind || "endpoint";
  return `${label} · ${rest}`;
}

/** (5.0 R3) The route row: stated by a dispatch start or a terminal naming the
 *  endpoint. Without either (a partial peer feed, a mission-level session that
 *  makes no model call) there is nothing to assert. */
function briefRoute(d: NormRecord | null, ep: string | undefined, first: NormRecord | null | undefined, viewerUid: string | null): string | null {
  return d?.action === ACTION.DispatchStart || ep ? routeLabel(ep, ranOn(d, first), viewerUid) : null;
}

function briefRowsOf(sp: DispatchStartPayload, model: string | null, d: NormRecord | null, route: string | null, timing: string): BriefEntry[] {
  const rows: BriefEntry[] = [];
  pushKv(rows, "route", route);
  pushKv(rows, "image", sp.image);
  pushKv(rows, "model", model);
  pushKv(rows, "workspace", sp.workspace);
  if (d?.mission_id) {
    rows.push({ kind: "label", text: "mission" });
    rows.push({
      kind: "value",
      text: `${d.mission_id}${d.phase_id ? ` · phase ${d.phase_id}` : ""}`,
      href: `#mission=${encodeURIComponent(d.mission_id)}`,
    });
  }
  pushKv(rows, "timing", timing);
  return rows;
}

/** (#1973) The prompt, as a disclosure holding the text itself (never only
 *  its length), whose summary reads `prompt · <n> chars`. A record that
 *  reports a length but carries no text says so in the brief instead,
 *  rather than offering an expander onto nothing. */
function promptOf(sp: DispatchStartPayload): { promptLines: BriefEntry[]; disclosures: Disclosure[] } {
  if (sp.prompt) {
    const chars = sp.prompt_chars ?? sp.prompt.length;
    const truncated = sp.prompt_chars != null && sp.prompt.length < sp.prompt_chars;
    return { promptLines: [], disclosures: [{ id: "prompt", label: "prompt", chars, truncated, text: sp.prompt }] };
  }
  if (sp.prompt_chars == null) return { promptLines: [], disclosures: [] };
  return { promptLines: [{ kind: "label", text: "prompt" }, { kind: "value", text: `${sp.prompt_chars} chars` }], disclosures: [] };
}

/** The MODEL pane's numbers. */
interface ModelFigures {
  turns: number | null;
  tokIn: number | null;
  tokOut: number | null;
  /** The utility part of `tokIn + tokOut` (compaction, radio routing), named
   *  rather than left out; `null` when nothing was measured. */
  tokUtility: number | null;
  ctxPeak: number;
  ctxNow: number;
  nctx: number;
}

/** (#3067) The hover text naming the utility part of the token tiles. */
function utilityHintOf(tokUtility: number | null): string | undefined {
  return tokUtility ? `includes ${fmtC(tokUtility)} tokens of utility calls (compaction, radio routing)` : undefined;
}

/** (#2759) The MODEL pane's numbers: this run's own when it has telemetry,
 *  else its mission's rolled-up ones. Only these roll up (contract 8's own
 *  scope for the fix): WALL CLOCK and COMPACTIONS stay this run's own, as
 *  HARNESS metrics about running this bookend pair. */
function effectiveFigures(own: ModelFigures, ownEvidence: boolean, rollup: MissionModelRollup | null): ModelFigures {
  if (ownEvidence || !rollup) return own;
  const ctx = rollup.hasEvidence ? rollup : own;
  return {
    turns: rollup.turns ?? own.turns,
    tokIn: rollup.tokIn ?? own.tokIn,
    tokOut: rollup.tokOut ?? own.tokOut,
    tokUtility: rollup.tokUtility ?? own.tokUtility,
    ctxPeak: ctx.ctxPeak,
    ctxNow: ctx.ctxNow,
    nctx: ctx.nctx,
  };
}

/** (operator, 2026-09-05) The context tile's three slots: `label` names the
 *  number and never restates it, `headline` IS the number (the peak once
 *  done, the use now while live), and `sub` carries the window ceiling (and,
 *  live, the peak so far), in `fmtC`'s compact form (`262k`). */
function ctxTile(f: ModelFigures, done: boolean): { headline: number; label: string; sub: string | undefined } {
  if (!f.nctx) return { headline: done ? f.ctxPeak : f.ctxNow, label: "CONTEXT", sub: undefined };
  if (done) return { headline: f.ctxPeak, label: "CTX PEAK", sub: `of ${fmtC(f.nctx)}` };
  return { headline: f.ctxNow, label: "CTX NOW", sub: `peak ${fmtC(f.ctxPeak)} · of ${fmtC(f.nctx)}` };
}

interface ScopeInputs {
  policy: LifecyclePolicy;
  presence: Presence;
  connected: boolean;
  lastContactMs: number | null;
}

/** (#2877 pass 2) The live scope's readings: ONE state derivation
 *  (`lib/tokenRate.ts`'s `aggregateLiveState`, the most informative reading
 *  among the run's executions wins) and the rate. `stalled` is DERIVED from
 *  the state, so the two cannot disagree.
 *
 *  (#2886 pass 3/4) A stall is downgraded to no reading when the page has
 *  lost the daemon (`liveStateWhileConnected`): no record could have arrived
 *  either way, and a false STALL is worse than saying nothing. `lastContactMs`
 *  closes the half-open gap. `noSignal` is true ONLY when that downgrade is
 *  what produced the empty reading: a genuinely idle run (a mission between
 *  model steps) must not read "no signal". */
function scopeReadings(sets: NormRecord[][], nowMs: number, inp: ScopeInputs) {
  const raw = aggregateLiveState(sets, nowMs, inp.policy, inp.presence);
  const halfOpen = inp.lastContactMs != null ? { lastContactMs: inp.lastContactMs, lastHeartbeatMs: lastHeartbeatMs(sets) } : undefined;
  const state = liveStateWhileConnected(raw, inp.connected, halfOpen);
  return {
    tokRateLiveState: state,
    tokRateStalled: state?.state === "stalled",
    tokRateNoSignal: raw?.state === "stalled" && state === null,
    liveTokRate: aggregateTokenRate(sets, nowMs, inp.policy, inp.presence),
  };
}

const finiteOrUndefined = (v: unknown): number | undefined => {
  const n = Number(v);
  return Number.isFinite(n) ? n : undefined;
};

/** (#1973, #2107) Host CPU / RAM / GPU over the run, average and peak,
 *  through the ONE aggregation the machine drawer also uses
 *  (`lib/hostStats.ts`'s `aggregateHostSamples`), so the two surfaces never
 *  report different numbers for overlapping samples. (#2413 M4) The retired
 *  `telemetry.process` payload names bare `cpu`/`mem`/`gpu`, the
 *  machine-scoped `machine.telemetry` names `cpu_pct`/`mem_pct`/`gpu_pct`;
 *  both read. */
function hostAggregate(procs: readonly NormRecord[]) {
  return aggregateHostSamples(
    procs.map((r) => {
      const f = r.fields as Record<string, unknown> | undefined;
      return {
        cpu: finiteOrUndefined(f?.cpu ?? f?.cpu_pct),
        mem: finiteOrUndefined(f?.mem ?? f?.mem_pct),
        gpu: finiteOrUndefined(f?.gpu ?? f?.gpu_pct),
      };
    }),
  );
}

/** One metric tile. See `SessionRunView.metrics`. */
type Tile = SessionRunView["metrics"][number];

/** (#2008) A run's tool calls, and how many failed by `toolOutcome`'s rule
 *  (the event log row's): a command that ran and exited non-zero is the
 *  tool working, not a failure. */
function toolCounts(records: readonly NormRecord[]): { calls: number; failed: number } {
  let calls = 0;
  let failed = 0;
  for (const r of records) {
    if (r.action !== ACTION.DispatchTool) continue;
    calls += 1;
    const call = payloadOf(r, ACTION.DispatchTool);
    if (call && toolOutcome(call) === "failed") failed += 1;
  }
  return { calls, failed };
}

/** (#2890) ACTIVE TIME's sub line: a live run's "so far" (the narrow cell
 *  holds the bare time), how it ended, and the thermal rest the figure
 *  INCLUDES, shown when the governor was armed or a rest occurred. */
function activeTimeSub(done: boolean, outcome: string | undefined, thermal: RestKind | undefined, thermalArmed: boolean): string | undefined {
  const rest = thermalArmed || thermal ? `${fmtElapsed(thermal?.totalMs ?? 0)} thermal rest` : undefined;
  return [done ? undefined : "so far", outcome, rest].filter(Boolean).join(" · ") || undefined;
}

/** The context tile, with (#2890) a thin bar under the figure: now and
 *  peak against the window, as percentages of it. */
function ctxTileOf(f: ModelFigures, t: { headline: number; label: string; sub: string | undefined }): Tile {
  const tile: Tile = { value: f.nctx ? fmtC(t.headline) : "—", label: t.label, sub: t.sub };
  if (f.nctx > 0) tile.bar = { nowPct: pctOf(f.ctxNow, f.nctx), peakPct: pctOf(f.ctxPeak, f.nctx) };
  return tile;
}

const pctOf = (part: number, whole: number): number => Math.min(100, Math.max(0, (part / whole) * 100));

/** (#2886) A finished run's average generation rate: billed tokens over
 *  generation time (`averageGenerationRate`), an exact average. Three
 *  outcomes: every paired turn billed reads the plain average; some
 *  excluded (a checkpointed turn) reads "avg · M of N turns"; all excluded
 *  reads "—" ("avg · unbilled"), never the wall-clock fallback, which is only
 *  for a runtime that predates `generation_ms` ("avg · wall clock"). The
 *  sub line appears only when it says more than "avg". */
function finishedRate(sets: NormRecord[][], tokOut: number | null, runWallMs: number): { average: string; sub: string | null } {
  const genRate = averageGenerationRate(sets);
  if (genRate == null) {
    const wallRate = tokOut != null && runWallMs > 0 ? tokOut / (runWallMs / 1000) : null;
    return { average: wallRate != null ? String(Math.round(wallRate)) : "—", sub: "avg · wall clock" };
  }
  if (genRate.tokensPerSec == null) return { average: "—", sub: "avg · unbilled" };
  const partial = genRate.billedTurns !== genRate.totalTurns;
  return { average: String(Math.round(genRate.tokensPerSec)), sub: partial ? `avg · ${genRate.billedTurns} of ${genRate.totalTurns} turns` : null };
}

/** How many rests of one kind a run took, and for how long. */
interface RestKind {
  label: string;
  count: number;
  totalMs: number;
}

/** (rest-reason cards) Which kind of rest a `dispatch.rest`'s reason names. */
function restKindOf(reason: unknown): { key: string; label: string } {
  const r = String(reason ?? "").trim();
  if (r.startsWith("thermal")) return { key: "thermal", label: "THERMAL REST" };
  if (r === "turn_delay") return { key: "turn_delay", label: "TURN DELAY" };
  if (r.startsWith("battery")) return { key: "battery", label: "BATTERY PAUSE" };
  if (r.startsWith("operator")) return { key: "operator_hold", label: "OPERATOR HOLD" };
  const key = r || "rest";
  return { key, label: `${key.toUpperCase()} REST` };
}

/** The attempt's rests by kind. A record with `delay_ms` and no `ms` is the
 *  governor changing its PACING, not a rest (`dispatch_internal.rs`'s
 *  `emit_rest`): only a real `ms` is one rest. */
function restsByKind(records: readonly NormRecord[]): Map<string, RestKind> {
  const out = new Map<string, RestKind>();
  for (const r of records) {
    const f = payloadOf(r, ACTION.DispatchRest);
    if (!f || typeof f.ms !== "number" || !Number.isFinite(f.ms) || f.ms <= 0) continue;
    const { key, label } = restKindOf(f.reason);
    const cur = out.get(key) ?? { label, count: 0, totalMs: 0 };
    cur.count += 1;
    cur.totalMs += f.ms;
    out.set(key, cur);
  }
  return out;
}

/** Which rest protections were ARMED for this dispatch (`dispatch.start`'s
 *  `bounds`), so a card shows at 0 rests when configured: "configured and
 *  never fired" differs from "not configured". No recorded bounds (before
 *  #2165) reads as unknown, never off. Operator hold has no knob to arm. */
function restArmed(bounds: DispatchStartPayload["bounds"]): Record<string, boolean> {
  const delay = bounds?.turn_delay_ms?.value;
  return {
    thermal: bounds?.thermal_pacing_enabled?.value === true,
    turn_delay: typeof delay === "number" && delay > 0,
    battery: bounds?.battery_pause_enabled?.value === true,
  };
}

const STATIC_REST_LABELS: Record<string, string> = {
  thermal: "THERMAL REST",
  turn_delay: "TURN DELAY",
  battery: "BATTERY PAUSE",
  operator_hold: "OPERATOR HOLD",
};
const REST_KIND_ORDER = ["thermal", "turn_delay", "battery", "operator_hold"];

/** (rest-reason cards) One SYSTEM tile per rest KIND, in a fixed order
 *  (then any kind this file does not name, longest first), for a kind that
 *  was armed or occurred. `thermalInModel` skips thermal rest when ACTIVE
 *  TIME already names it. */
function restTiles(rests: Map<string, RestKind>, armed: Record<string, boolean>, thermalInModel: boolean): Tile[] {
  const extra = [...rests.keys()].filter((k) => !REST_KIND_ORDER.includes(k)).sort((a, b) => (rests.get(b)?.totalMs ?? 0) - (rests.get(a)?.totalMs ?? 0));
  const tiles: Tile[] = [];
  for (const key of [...REST_KIND_ORDER, ...extra]) {
    const occurred = rests.get(key);
    if ((key === "thermal" && thermalInModel) || (!armed[key] && !occurred)) continue;
    tiles.push(restTile(key, occurred));
  }
  return tiles;
}

function restTile(key: string, occurred: RestKind | undefined): Tile {
  const count = occurred?.count ?? 0;
  const totalMs = occurred?.totalMs ?? 0;
  return { value: fmtElapsed(totalMs), label: STATIC_REST_LABELS[key] ?? occurred?.label ?? key.toUpperCase(), sub: restSub(key, count, totalMs) };
}

/** "N rests", and for turn delays the length of each. */
function restSub(key: string, count: number, totalMs: number): string {
  const rests = `${count} rest${count === 1 ? "" : "s"}`;
  return key === "turn_delay" && count > 0 ? `${rests} · ${Math.round(totalMs / count / 1000)} s each` : rests;
}

/** (operator, 2026-09-05) A host figure's tile: the AVERAGE is the value
 *  (one figure on one line), the peak its `sub`, and (#2863) "avg" a small
 *  `unit` beside the value. */
function avgHighTile(label: string, m: { avg: number | null; high: number | null }): Tile {
  return { value: `${roundPct(m.avg)}%`, label, sub: `${roundPct(m.high)}% high`, unit: "avg" };
}

/** The CPU / RAM / GPU tiles for whichever figures were sampled. (#2413 M4)
 *  A model-work run whose host join came up empty says so ("no host
 *  samples", the machine drawer's words) rather than silently dropping the
 *  tiles; a run with no model work has nothing to sample. The run's own
 *  records are on screen, so its machine streams here: `machine.telemetry`
 *  rides the same flow stream, and a missing sample means the sampler did
 *  not cover the run. */
function hostTiles(agg: ReturnType<typeof hostAggregate>, hasModelWork: boolean): Tile[] {
  const tiles: Tile[] = [];
  if (agg.cpu.high != null) tiles.push(avgHighTile("CPU", agg.cpu));
  if (agg.mem.high != null) tiles.push(avgHighTile("RAM", agg.mem));
  if (agg.gpu.high != null) tiles.push(avgHighTile("GPU", agg.gpu));
  if (hasModelWork && tiles.length === 0) tiles.push({ value: "—", label: "HOST", sub: "no host samples for this run" });
  return tiles;
}

/** (#2863) Model names compared WITHOUT darkmux's namespace: since #2240 a
 *  local dispatch names the model `darkmux:<key>` on the wire, while LM
 *  Studio's load telemetry reports the bare key. */
const bareModel = (m: unknown): string => String(m ?? "").replace(/^darkmux:/, "");

const loadFields = (r: NormRecord): Record<string, unknown> => r.fields as Record<string, unknown>;

type ModelEntry = { name: string; gb: number | null; ran: boolean | null };

/** The loaded-models track, the model that ran first. `endpointModel` is
 *  the model an endpoint-served run names (it has no loads to list). A run
 *  that loaded nothing itself falls back, in order, to: the model it ran on
 *  (`residentModel`: the model its `dispatch.start` names, which every path
 *  writes, given to it only once the run has model evidence; it was already
 *  resident, so nothing here loaded it), its mission's loads (`rollupLines`,
 *  unlabeled: primary/also-loaded compare this run's own fields), then
 *  "no telemetry yet". */
function modelTrackOf(loads: readonly NormRecord[], primaryModel: string | null, endpointModel: string | null, residentModel: string | null, rollupLines: string[]): { modelEntries?: ModelEntry[]; modelTrackLines: string[] } {
  if (endpointModel !== null) return { modelTrackLines: [endpointModel] };
  const isRan = (r: NormRecord) => primaryModel != null && bareModel(loadFields(r).model) === bareModel(primaryModel);
  const ordered = [...loads].sort((a, b) => Number(isRan(b)) - Number(isRan(a)));
  if (!ordered.length) return { modelTrackLines: fallbackTrackLines(residentModel, rollupLines) };
  return {
    modelEntries: ordered.map((r) => modelEntryOf(loadFields(r), primaryModel == null ? null : isRan(r))),
    modelTrackLines: ordered.map((r) => modelLineOf(loadFields(r), primaryModel == null ? null : isRan(r))),
  };
}

function fallbackTrackLines(residentModel: string | null, rollupLines: string[]): string[] {
  if (residentModel !== null) return [`${bareModel(residentModel)} · already resident`];
  return rollupLines.length ? rollupLines : ["no telemetry yet"];
}

function modelEntryOf(f: Record<string, unknown>, ran: boolean | null): ModelEntry {
  return { name: String(f.model ?? "?"), gb: typeof f.gb === "number" ? f.gb : null, ran };
}

function modelLineOf(f: Record<string, unknown>, ran: boolean | null): string {
  const tag = ran === null ? "" : ran ? " · primary" : " · also loaded";
  return `${f.model} · ${f.gb ?? "?"}GB${tag}`;
}

// ── signals ────────────────────────────────────────────────────────────

interface SignalInputs {
  skewedClose: boolean;
  lms: NormRecord[];
  loads: NormRecord[];
  distinct: string[];
  d: NormRecord | null;
  dets: NormRecord[];
  /** `dispatch.checkpoint` is NOT a detector telemetry record (`category=
   *  work`, no `source`), so it never reaches `dets`: read straight off the
   *  attempt's records. */
  checkpoints: NormRecord[];
}

/** (#1973) The SIGNALS card. Was "detections", one flat list of grey
 *  strings with no times: the emitter has ALWAYS sent a severity (`warn` for
 *  cycle / reasoning-loop / tool-failure, `info` for `intra-turn-stall`, a
 *  RECOVERY), and a cycle in the first ten seconds looked exactly like one
 *  an hour in. Signals now carry their severity and their offset into the
 *  run, grouped by kind. */
function runSignals(inp: SignalInputs): { signalGroups: SignalGroup[]; signalsLabel: string; repetitionOff: boolean; repetitionRecorded: boolean } {
  const facts = runPolicyFacts(inp.d);
  const finds: Signal[] = [
    ...(inp.skewedClose ? [SKEW_SIGNAL] : []),
    ...modelSwapSignals(inp.lms, inp.loads, inp.distinct),
    ...detectorSignals(inp.dets, facts.runStartMs),
    ...(facts.repetitionOff ? [] : repetitionSignals(turnFlags(inp.dets, inp.checkpoints), facts)),
  ];
  return {
    signalGroups: groupSignals(finds),
    signalsLabel: finds.length ? `signals (${finds.length})` : "signals",
    repetitionOff: facts.repetitionOff,
    repetitionRecorded: facts.repetitionRecorded,
  };
}

/** (#1988) The page reconstructed this run's outcome from a terminal record
 *  that precedes its own start. Honored rather than hidden (a finished
 *  dispatch must not read RUNNING forever), but NOT presented as if the
 *  timeline were sound: clock skew is itself worth an operator's attention
 *  on a fleet. */
const SKEW_SIGNAL: Signal = {
  kind: "clock-skew",
  severity: "warn",
  detail:
    "this run's terminal record is timestamped BEFORE its own start: the outcome is read from it anyway, but elapsed time and signal offsets on this page are unreliable.",
  fix: "check the clocks on the machines that produced these records.",
  atMs: null,
  offsetLabel: "",
};

const isUtilitySeat = (role: unknown): boolean => role === "compactor" || role === "utility";

/** (#1934) A mid-run model swap. Two model ids loaded in one run is not a
 * swap: a correct `deep`/`balanced` profile loads a primary and a compactor
 * BY CONSTRUCTION, and a resident leftover from an earlier session is
 * ambient. The producer tags each `telemetry.lms` load/unload with `role`
 * (`primary` / `compactor` / `utility` / `resident`, `telemetry_sampler.rs`'s
 * `role_for_load`) and marks the sampler's first tick `baseline` (the
 * starting lineup, never an event). A genuine swap is what happens to a
 * SPECIALIST seat after that: a new specialist model going resident, or one
 * being unloaded at all.
 *
 * UNTAGGED RECORD SETS ARE NOT JUDGED. Records from before the tags (and
 * hand-authored fixtures, and the committed demo corpus) carry neither
 * field; judging them admitted every load and said "X was unloaded mid-run"
 * for sets with no unload at all, a false factual claim. So the seat reading
 * applies only when EVERY lms record carries a `role`; otherwise the
 * detector declines and says so (`untaggedTrackSignal`). */
function modelSwapSignals(lms: NormRecord[], loads: NormRecord[], distinct: string[]): Signal[] {
  if (lms.length === 0) return [];
  const untagged = lms.some((r) => typeof ((r.fields ?? {}) as Record<string, unknown>).role !== "string");
  const signal = untagged ? untaggedTrackSignal(distinct) : taggedSwapSignal(lms, loads);
  return signal ? [signal] : [];
}

/** `info`, not `warn`: nothing is known to have gone wrong. Emitted exactly
 *  where the old count-based rule would have raised a `warn`, so a
 *  historical run is neither silently blind nor crying wolf. */
function untaggedTrackSignal(distinct: string[]): Signal | null {
  if (distinct.length <= 1) return null;
  return {
    kind: "model-track-unclassified",
    severity: "info",
    detail: `${distinct.length} models loaded in one run (${distinct.join(" → ")}), but these records carry no seat tag: a real mid-run swap and a correct primary+compactor staffing look identical here, so this run is not judged either way.`,
    fix: "runs recorded at flow schema 1.45.0 or later tag each load with its seat; the swap reading returns for those.",
    atMs: null,
    offsetLabel: "",
  };
}

/** Whether a load/unload is a specialist-seat event after the baseline. */
function isSwapEvent(r: NormRecord): boolean {
  const f = r.fields as Record<string, unknown> | undefined;
  if (!f || isUtilitySeat(f.role)) return false;
  if (f.event === "unload") return true;
  return f.event === "load" && !(f.role != null && f.baseline === true);
}

const modelsOf = (recs: readonly NormRecord[]): string[] => [...new Set(recs.map((r) => loadFields(r).model as string))];

/** The seat reading. TWO KNOWN NARROWINGS, both deliberate: a compactor or
 *  utility model EVICTED mid-run is invisible here (right for a utility
 *  LOAD, wrong for an UNLOAD; compactor thrash wants its own signal,
 *  #2565); and a `resident` model going resident mid-run fires, including
 *  one the operator loaded for unrelated use (it cannot be excluded: a
 *  genuine second specialist also tags `resident`). The GATE is "did
 *  anything happen to a specialist seat after the starting point"; the
 *  model counts only shape the WORDING, each branch keyed on the thing it
 *  claims. Synthesized from the load track, so it has no record of its own
 *  and no timestamp: `null` says so. */
function taggedSwapSignal(lms: NormRecord[], loads: NormRecord[]): Signal | null {
  const swaps = lms.filter(isSwapEvent);
  if (swaps.length === 0) return null;
  const specialistModels = modelsOf(loads.filter((r) => !isUtilitySeat(loadFields(r).role)));
  const unloaded = modelsOf(swaps.filter((r) => loadFields(r).event === "unload"));
  const loadedMidRun = modelsOf(swaps.filter((r) => loadFields(r).event === "load"));
  const detail = swapDetail(specialistModels, unloaded, loadedMidRun);
  if (!detail) return null;
  return { kind: "jit-model-swap", severity: "warn", detail, fix: "pin one model for the run, or pre-warm the swap target.", atMs: null, offsetLabel: "" };
}

function swapDetail(specialistModels: string[], unloaded: string[], loadedMidRun: string[]): string | null {
  if (specialistModels.length > 1) {
    return `${specialistModels.length} models loaded in one run (${specialistModels.join(" → ")}): mid-run swap stalls the dispatch while the new model loads.`;
  }
  if (unloaded.length > 0) return `${unloaded.join(", ")} was unloaded mid-run: the seat's reload stalls the dispatch while the model loads.`;
  if (loadedMidRun.length > 0) return `${loadedMidRun.join(", ")} loaded mid-run rather than before it: the dispatch stalls while the model loads.`;
  return null;
}

/** What the run's `dispatch.start` says about its detectors. */
interface RunPolicyFacts {
  runStartMs: number | null;
  /** (#2887 F2) The run-level policy the dispatch ran under
   *  (`payload.bounds.detection_degeneracy_policy.value`, the value the host
   *  stamps for the container): the one "what was this run configured to
   *  do" answer, whatever an individual record carries. `null` when unknown,
   *  and unknown is never read as "off". */
  runDegeneracyPolicy: string | null;
  repetitionOff: boolean;
  /** (#2887 N2) Whether the run's `flow_schema` is 1.56.0 or later, when
   *  the degeneracy gate's findings started reaching the flow stream. An
   *  older (or unstamped) run cannot tell "never flagged" from "no forwarder
   *  yet", and must render as unmeasured, never as a clean tick. */
  repetitionRecorded: boolean;
}

function runPolicyFacts(d: NormRecord | null): RunPolicyFacts {
  const sf = d?.fields as Record<string, unknown> | undefined;
  const block = (sf?.bounds as Record<string, unknown> | undefined)?.detection_degeneracy_policy as Record<string, unknown> | undefined;
  const runDegeneracyPolicy = typeof block?.value === "string" ? block.value : null;
  return {
    runStartMs: d ? d.tMs : null,
    runDegeneracyPolicy,
    repetitionOff: runDegeneracyPolicy === "off",
    repetitionRecorded: flowSchemaAtLeast(typeof sf?.flow_schema === "string" ? sf.flow_schema : null, "1.56.0"),
  };
}

const offsetLabelOf = (atMs: number | null, runStartMs: number | null): string =>
  atMs != null && runStartMs != null ? runOffset(atMs - runStartMs) : "";

/** One signal per detector record, except `repetition` (#2887 F4: grouped
 *  by turn in `repetitionSignals`, since one cut writes an observation, an
 *  abort and a checkpoint). (#1989) A missing `kind` is named
 *  `unknown-signal`, never the string "undefined"; an unknown severity
 *  degrades to `warn`, never `info` (quietly downgrading is how a new
 *  detector ships invisible); a non-string `detail` is serialized, never
 *  `[object Object]`. */
function detectorSignals(dets: NormRecord[], runStartMs: number | null): Signal[] {
  const out: Signal[] = [];
  for (const r of dets) {
    const f = r.fields as Record<string, unknown>;
    if (f.kind === "repetition") continue;
    out.push({
      kind: typeof f.kind === "string" && f.kind ? f.kind : "unknown-signal",
      severity: f.severity === "info" ? "info" : "warn",
      detail: signalDetail(f.detail),
      atMs: r.tMs,
      offsetLabel: offsetLabelOf(r.tMs, runStartMs),
    });
  }
  return out;
}

/** (#2887 F4) One flagged TURN, not one raw record. */
type TurnFlag = {
  turnSeq: number | string;
  acted: boolean;
  /** (#2887 N4) How many DISTINCT calls the gate itself ended for this turn,
   *  counted off `dispatch.gate.abort` records alone (never the degenerate
   *  observation naming the same cut). */
  gateAbortCount: number;
  /** Whether a gate-sourced record contributed at all, vs. the flag coming
   *  only from the checkpoint's post-hoc judge (#2836 the in-stream gate,
   *  #1221 the reasoning check-in: independent detectors). */
  sawGate: boolean;
  policy: string | null;
  atMs: number | null;
  ratio: string | null;
};

/** One record's contribution to a turn's flag. */
type FlagPart = Omit<TurnFlag, "turnSeq" | "gateAbortCount" | "sawGate"> & { turnSeqRaw: unknown; seatKey: string; isGateAbort: boolean; isGateSourced: boolean };

/** (#2887 N3) `turn_seq` alone is not a safe key: a dispatch session id is
 *  TASK-scoped, so sibling seats fanned out in one task share it and can
 *  each be on their own "turn 2". `handle` (the role) and `payload.step_id`
 *  attribute a record even then. */
const seatKeyFor = (r: NormRecord, f: Record<string, unknown>): string => `${r.handle ?? ""}::${typeof f.step_id === "string" ? f.step_id : ""}`;

const ratioOf = (f: Record<string, unknown>): string | null => (typeof f.tail_ratio === "number" ? f.tail_ratio.toFixed(3) : null);
const policyOf = (f: Record<string, unknown>): string | null => (typeof f.policy === "string" ? f.policy : null);

/** A repetition detector record's part. Only `dispatch.gate.abort` writes
 *  `generated_chars`, and an abort's own existence IS the acted outcome
 *  (#2887 F2: an older host forwards `acted` as an explicit `null`). */
function gatePart(r: NormRecord): FlagPart | null {
  const f = r.fields as Record<string, unknown>;
  if (f.kind !== "repetition") return null;
  const isGateAbort = f.generated_chars != null;
  return { turnSeqRaw: f.turn_seq, seatKey: seatKeyFor(r, f), acted: f.acted === true || isGateAbort, isGateAbort, isGateSourced: true, policy: policyOf(f), atMs: r.tMs, ratio: ratioOf(f) };
}

/** A checkpoint's part, when it flags: `would_conclude` (the judge found
 *  the turn repetitive) or a `conclude` verdict (F1: a checkpoint from
 *  before #2846 carries only `verdict`). */
function checkpointPart(r: NormRecord): FlagPart | null {
  const f = r.fields as Record<string, unknown>;
  const acted = f.verdict === "conclude";
  if (f.would_conclude !== true && !acted) return null;
  return { turnSeqRaw: f.turn_seq, seatKey: seatKeyFor(r, f), acted, isGateAbort: false, isGateSourced: false, policy: policyOf(f), atMs: r.tMs, ratio: ratioOf(f) };
}

/** The repetition flags by turn. (#2887 N3) A record with no numeric
 *  `turn_seq` never collapses with another such record just because both
 *  read "?": each keeps a group of its own. */
function turnFlags(dets: NormRecord[], checkpoints: NormRecord[]): TurnFlag[] {
  const byTurn = new Map<string, TurnFlag>();
  const parts = [...dets.map(gatePart), ...checkpoints.map(checkpointPart)];
  for (const p of parts) {
    if (!p) continue;
    const turnSeq = typeof p.turnSeqRaw === "number" ? p.turnSeqRaw : "?";
    const key = turnSeq === "?" ? `${p.seatKey}::?::${byTurn.size}` : `${p.seatKey}::${turnSeq}`;
    const existing = byTurn.get(key);
    if (existing) mergeFlag(existing, p);
    else byTurn.set(key, { turnSeq, acted: p.acted, gateAbortCount: p.isGateAbort ? 1 : 0, sawGate: p.isGateSourced, policy: p.policy, atMs: p.atMs, ratio: p.ratio });
  }
  return [...byTurn.values()];
}

/** The earlier of two times, a missing one never winning. */
const earliestOf = (a: number | null, b: number | null): number | null => (a == null ? b : b == null ? a : Math.min(a, b));

function mergeFlag(acc: TurnFlag, p: FlagPart): void {
  if (p.acted) acc.acted = true;
  if (p.isGateAbort) acc.gateAbortCount += 1;
  if (p.isGateSourced) acc.sawGate = true;
  if (p.policy && !acc.policy) acc.policy = p.policy;
  acc.atMs = earliestOf(acc.atMs, p.atMs);
  if (p.ratio && !acc.ratio) acc.ratio = p.ratio;
}

/** One `repetition` signal per flagged turn. (#2887 F2) `off` means the gate
 *  never ran, so its caller asks nothing then: a stray repetition-shaped
 *  record must not manufacture a finding for a detector that was not
 *  measuring. */
function repetitionSignals(flags: TurnFlag[], facts: RunPolicyFacts): Signal[] {
  return flags.map((acc) => ({
    kind: "repetition",
    severity: "warn" as const,
    detail: repetitionDetail(acc, facts.runDegeneracyPolicy ?? acc.policy),
    atMs: acc.atMs,
    offsetLabel: offsetLabelOf(acc.atMs, facts.runStartMs),
  }));
}

/** The turn's line, in the words of the policy the RUN ran under (#2887
 *  F2), citing the detector that produced it (#2887 N4: #2836 whenever the
 *  gate contributed, #1221 for a checkpoint-only flag). (#2947) Policy
 *  values name the action: `record` (silent measure; `observe` is its
 *  pre-4.0 spelling, still read) and `warn` (surfaced, not concluded). */
function repetitionDetail(acc: TurnFlag, policy: string | null): string {
  const ratio = acc.ratio ? ` (tail_ratio=${acc.ratio})` : "";
  const citation = acc.sawGate ? "#2836" : "#1221";
  if (acc.acted) return `turn ${acc.turnSeq}: judged repeating${ratio} and ended it${acc.gateAbortCount > 1 ? ` ${acc.gateAbortCount}×` : ""} (${citation})`;
  if (policy === "record" || policy === "observe") return `turn ${acc.turnSeq}: judged repeating${ratio}: recorded, not concluded (#2846)`;
  if (policy === "warn") return `turn ${acc.turnSeq}: judged repeating${ratio}: warned, not concluded (#2947)`;
  return `turn ${acc.turnSeq}: judged repeating${ratio} (${citation})`;
}

const SEV_RANK: Record<SignalSeverity, number> = { warn: 0, info: 1 };

/** Severity first, then most recent first inside each (a recovery never
 *  outranks a struggle), grouped by kind so eleven cycle detections are one
 *  row that says 11. A group takes the highest severity it holds. */
function groupSignals(finds: Signal[]): SignalGroup[] {
  const sorted = [...finds].sort((a, b) => SEV_RANK[a.severity] - SEV_RANK[b.severity] || (b.atMs ?? 0) - (a.atMs ?? 0));
  const byKind = new Map<string, Signal[]>();
  for (const f of sorted) byKind.set(f.kind, [...(byKind.get(f.kind) ?? []), f]);
  return [...byKind].map(([kind, signals]) => ({
    kind,
    severity: signals.some((x) => x.severity === "warn") ? ("warn" as const) : ("info" as const),
    count: signals.length,
    signals,
  }));
}

/** The header's role: the run's start record's handle (else its first
 *  record's), without darkmux's role namespace. */
function roleOf(r: NormRecord | null): string {
  const handle = r ? r.handle : "unknown";
  return String(handle || "").replace(/^darkmux\//, "").toUpperCase();
}

/** The model the brief names: the start record's, else the endpoint's
 *  deployment, else the first model loaded. */
function modelOf(d: NormRecord | null, endpoint: string | undefined, distinct: string[]): string | null {
  if (d?.model) return d.model;
  if (endpoint) return endpoint.slice(endpoint.lastIndexOf("/") + 1);
  return distinct[0] ?? null;
}

/** (#2759) EVIDENCE of model work in a run's own telemetry: actual numbers,
 *  not a start record (a run-grain session has a start and nothing else, and
 *  is exactly the case the mission rollup exists for). */
function hasTelemetryEvidence(f: { loads: NormRecord[]; turnsValue: number | null; tokIn: number | null; tokOut: number | null; ctxSamples: number; comps: NormRecord[] }): boolean {
  return f.loads.length > 0 || f.turnsValue != null || f.tokIn != null || f.tokOut != null || f.ctxSamples > 0 || f.comps.length > 0;
}

type LiveTokScope = NonNullable<SessionRunView["liveTokScope"]>;

/** (#2877) The live scope's data for a run still in progress. */
function liveTokScopeOf(
  reading: LiveStateReading | null,
  rate: { tokensPerSec: number; carried: boolean } | null,
  stalled: boolean,
  noSignal: boolean,
  nowMs: number,
): LiveTokScope {
  return {
    tokensPerSec: rate?.tokensPerSec ?? null,
    carried: rate?.carried ?? false,
    stalled,
    // null: no live execution right now (a mission between model steps), or
    // downgraded for a lost connection. Every lamp is off.
    state: reading?.state ?? null,
    restSecondsLeft: reading?.restSecondsLeft,
    clockMs: nowMs,
    noSignal,
    toolName: undefined,
    ...stateExtras(reading),
  };
}

/** The fields only one state carries (see `LiveStateReading`). */
function stateExtras(r: LiveStateReading | null): Partial<LiveTokScope> {
  switch (r?.state) {
    case "rest":
      return restExtras(r);
    case "tools":
      return toolExtras(r);
    case "generating":
      return r.thinking ? { thinking: true } : {};
    case "prompt":
      return r.compacting ? { compacting: true, compactingSeconds: r.compactingSeconds } : {};
    case "stalled":
    case undefined:
      return {};
  }
}

function restExtras(r: LiveStateReading): Partial<LiveTokScope> {
  return { ...(r.restEndMs !== undefined ? { restEndMs: r.restEndMs } : {}), ...(r.restReason !== undefined ? { restReason: r.restReason } : {}) };
}

function toolExtras(r: LiveStateReading): Partial<LiveTokScope> {
  return {
    toolName: r.toolName,
    ...(r.toolPath !== undefined ? { toolPath: r.toolPath } : {}),
    ...(r.writing ? { writing: true as const, writingSeconds: r.writingSeconds } : {}),
  };
}

/** How an errored run ended, for the run-time tile's sub line. */
function errorOutcome(edge: CloseEdge | undefined): string | undefined {
  if (edge?.kind !== "error") return undefined;
  if (edge.killed) return "killed (timeout)";
  return `errored${edge.exitCode != null ? ` (exit ${edge.exitCode})` : ""}`;
}

/** `runRegions()`, minus the two SVG chart regions
 * (see this module's own top doc). `data` should already be scoped to ONE
 * session (the `/flow-dispatch/<id>` response, through `flowToRenderModel`
 * — see that function's own doc) — `sid` further scopes every derivation
 * to it, matching legacy's `state.session`. */
/**
 * @param nowOverride (#1972) The wall-clock "now" for a LIVE run.
 *
 * Without it this derives `now` from `computeTMax(data)` — the newest
 * record's timestamp — which is correct for a finished run or playback, and
 * exactly wrong for a live one: the elapsed counter then only advances when a
 * record ARRIVES, so a dispatch that goes quiet shows a frozen clock. Which
 * is precisely when the operator most wants to know how long it has been
 * quiet. The reading was not merely stale; it was structurally incapable of
 * moving during a stall.
 *
 * Passing a real clock here fixes that. It is never allowed to run BACKWARDS
 * of the records, though: `max(nowOverride, tMax)` keeps a machine whose
 * clock lags a peer's from rendering a negative elapsed time.
 *
 * @param connected (#2886 pass 3, "STALL while disconnected") Whether the
 * PAGE currently has a working connection to the daemon — read by the
 * caller from the same liveness source the header renders
 * (`hooks/useLiveTail.ts`'s `LiveTailStatus`). Defaults to `true` (assume
 * connected) so every existing caller/test that doesn't pass it keeps
 * behaving exactly as before; `SessionReplay.tsx` is the one caller that
 * passes the real value. See `lib/tokenRate.ts::liveStateWhileConnected`'s
 * own doc for why only a `"stalled"` reading is affected.
 *
 * @param lastContactMs (#2886 pass 4, do-it — fresh-reviewer finding 5,
 * "half-open connection race") The last moment the page confirmed contact
 * with the daemon — `App.tsx`'s `lastContactRef.current`, sourced from
 * `useLiveTail`'s `onContact`. `null` (the default) skips the half-open
 * check inside `liveStateWhileConnected` and falls back to the plain
 * `connected` boolean, same as omitting it there.
 */
export function runRegions(
  data: NormRecord[],
  sid: string,
  nowOverride?: number,
  connected = true,
  lastContactMs: number | null = null,
  /** (#2928) The live channel's overlay, at the live edge only (see
   *  `SessionReplay.tsx`); merged into the scope's per-execution record sets
   *  alone, never into turns, tokens or the event rows. `null` derives from
   *  durable records exactly as before. */
  live: LiveOverlay | null = null,
  /** Session presence (`hooks/useSessionLiveness.ts`): holds a silent run
   *  open, never a closed one. */
  presence: Presence = NO_PRESENCE,
  /** The daemon's lifecycle policy (`/runs.policy`). */
  policy: LifecyclePolicy = DEFAULT_POLICY,
  /** The viewing page's own machine identity (`localMachineUid`), so the route
   *  line says "this machine" only for a run that ran on it. `null` when unknown. */
  viewerUid: string | null = null,
  /** (5.0 R3) Whether the machine a run executed on is not reporting. A run
   *  that reads running there has no live evidence, so its status is unknown. */
  notReporting?: boolean,
): SessionRunView {
  const tMax = computeTMax(data);
  const nowMs = nowOverride != null ? Math.max(nowOverride, tMax) : tMax;

  // The run this page shows, and where it stands, from the one lifecycle
  // (`lib/lifecycle.ts`) every surface reads: its attempt as of `nowMs`
  // (the latest start, #1988's skewed close honored and flagged), its close
  // edge, and whether it is still in flight.
  const ctx = runContext(data, sid, nowMs, policy, presence);
  const { run, l, d, firstSessRec, startTs, inAttempt, endTs, c, done, skewedClose, state } = ctx;
  const visible = recordsAsOf(data, nowMs);
  const { tel, lms, procs, dets, loads, distinct, comps } = attemptTelemetry(visible, ctx);
  const attemptRecs = visible.filter(inAttempt);
  const turnsValue = turnCount(attemptRecs);

  const { samples: ctxSamples, nctx, ctxPeak, ctxNow } = contextFigures(tel);

  // (#1972) Proof of life: the newest record belonging to THIS attempt. Not
  // heartbeats alone — a run emitting turns and tool results is demonstrably
  // alive whether or not a heartbeat happens to have landed recently, and
  // keying only on heartbeats would make a busy run look dead.
  const lastBeatMs = latestByTime(attemptRecs)?.tMs ?? null;

  const { runWallMs, activeElapsed, wallBase, wallSub } = wallClock(ctx, nowMs);

  const role = roleOf(d ?? firstSessRec);
  const on = ranOn(d, firstSessRec);
  const pill = pillOf(state, notReporting);

  const sp: DispatchStartPayload = payloadOf(d, ACTION.DispatchStart) ?? {};
  const remoteEp = sp.endpoint || endPayloadOf(c)?.endpoint;
  const model = modelOf(d, remoteEp, distinct);

  // (#2902 step 2a, #3067) The plain sum of this attempt's usage records, all
  // purposes, the utility part named.
  const { tokIn, tokOut, tokUtility } = tokenFigures(executionTokens(c ? [...tel, c] : tel));

  // ── brief ──────────────────────────────────────────────────────────
  // (#2011) Same `runWallMs` the WALL CLOCK tile shows. The two lines report
  // one quantity, so they read from one source — deriving it twice is how
  // they end up disagreeing by a second at a rounding boundary. The two
  // CLOCK stamps stay record-derived: they are timestamps, not a duration.
  const briefTiming = `${clk(startTs)}${done ? ` → ${clkAt(endTs)} (${fmtElapsed(runWallMs)})` : ` · ${pill.open}`}`;
  const ep = remoteEp;
  const briefRows = briefRowsOf(sp, model, d, briefRoute(d, ep, firstSessRec, viewerUid), briefTiming);
  const { promptLines, disclosures } = promptOf(sp);

  // No "run" heading inside the block: the region's own `<h2>` directly above
  // already reads `RUN · <ROLE> (<session> on <machine>)`, so a second bare
  // "run" was the same word twice, six pixels apart. Legacy printed it and the
  // golden pinned it; the golden is a spec for catching UNINTENDED drift, not a
  // veto on removing something redundant, so this is a deliberate hand-edit
  // there rather than a regression.
  const briefLines: BriefEntry[] =
    briefRows.length || promptLines.length ? [...briefRows, ...promptLines] : [];

  // (#2759) EVIDENCE of model work on THIS session alone — the same
  // predicate `hasModelWork` below uses, minus its `d != null` clause (a
  // session with a `dispatch.start` and nothing else is "started, no
  // telemetry yet" for a live dispatch, but it is ALSO exactly the run-grain
  // session's shape: `d` exists, every other field is empty). Gating the
  // mission-wide rollup on `d != null` would never fire for the one case it
  // exists to fix, so this checks for actual numbers instead.
  const ownHasTelemetryEvidence = hasTelemetryEvidence({ loads, turnsValue, tokIn, tokOut, ctxSamples, comps });
  const missionIdForRollup = run?.group.missionId ?? null;
  const missionRuns = missionIdForRollup ? runIndex(data).groupsOfMission(missionIdForRollup) : [];
  const rollup =
    !ownHasTelemetryEvidence && missionIdForRollup ? rollUpMissionModelWork(missionRuns.filter((g) => g !== run?.group)) : null;
  const eff = effectiveFigures({ turns: turnsValue, tokIn, tokOut, tokUtility, ctxPeak, ctxNow, nctx }, ownHasTelemetryEvidence, rollup);
  const { turns: effTurnsValue, tokIn: effTokIn, tokOut: effTokOut, tokUtility: effTokUtility } = eff;
  const ctxTileFigures = ctxTile(eff, done);

  // (#1973) Did this unit do MODEL work at all?
  //
  // The pane split created the ability to omit the model half; this is what
  // uses it. A `procedural.shell` step compiles, moves files or runs a
  // command — it will never have turns, tokens, a context window or a
  // compaction. Rendering those as `— TURNS` and, worse, `0 COMPACTIONS` is a
  // lie shaped like data: a zero asserts "this happened, none occurred", when
  // the truth is "this cannot happen here".
  //
  // The discriminator is EVIDENCE of model work, not the absence of numbers.
  // A dispatch that has started but reported nothing yet is model work with
  // no telemetry, and must keep its pane — otherwise a live run would render
  // no model metrics until its first turn landed, and then grow a pane.
  // A run of execution grain (`runRef.ts`: a dispatch, or a hosted call
  // held by its budget before its first bookend, whose pane, where REST
  // reads "budget · <endpoint>", must not grow in when the call is sent).
  const executionGrain = run !== null && run.group.grain !== "lifecycle";
  const hasModelWork = executionGrain || ownHasTelemetryEvidence;
  // (#2759) The MODEL pane's own gate. Own-session evidence keeps the
  // existing behavior byte-for-byte (including the `d != null` "started, no
  // telemetry yet" case); otherwise a rolled-up execution elsewhere in the
  // mission turns the pane on. Kept SEPARATE from `hasModelWork` above,
  // which still gates COMPACTIONS/HOST — those are this session's own
  // harness measurements and must not flip on just because a sibling
  // session did model work.
  const effHasModelWork = hasModelWork || !!rollup?.hasEvidence;

  // (#2877) Live token-rate scope. A mission's own top-level session never
  // carries heartbeats — its INNER role executions do (same fact
  // `rollUpMissionModelWork`'s doc above names) — so when this session has
  // no telemetry of its own but rolled up a mission's, the heartbeats live
  // on the same mission runs `rollUpMissionModelWork` walked.
  const ownRuns = run ? [run.group] : [];
  const tokRateRuns = ownHasTelemetryEvidence || missionRuns.length === 0 ? ownRuns : missionRuns;
  const tokRateRecordSets = tokRateRuns.map((g) => mergeLive(g.records as NormRecord[], live?.bySession.get(g.sessionId)));
  const { tokRateLiveState, tokRateStalled, tokRateNoSignal, liveTokRate } = scopeReadings(tokRateRecordSets, nowMs, { policy, presence, connected, lastContactMs });

  const hostAgg = hostAggregate(procs);
  // Built as a list with its scope recorded AS EACH TILE IS ADDED, rather
  // than as a fixed array plus hardcoded indices: the indices are
  // conditional (host tiles only exist when host telemetry does), and this
  // makes the list and its grouping unable to drift because there is only one.
  const metrics: SessionRunView["metrics"] = [];
  const modelIdx: number[] = [];
  const systemIdx: number[] = [];
  const push = (into: number[], t: Tile) => {
    into.push(metrics.length);
    metrics.push(t);
  };
  // (#2890) Run time is the MODEL section's ACTIVE TIME cell for any unit
  // with a model section, SYSTEM's WALL CLOCK for a unit with none (a
  // `procedural.shell` step).
  const activeInModel = effHasModelWork;
  const rests = restsByKind(attemptRecs);
  const armed = restArmed(sp.bounds);
  // (#2890) The MODEL section's cells, in the order the operator reads them:
  // turns, tool calls, active time, tokens in, tokens out, context.
  push(modelIdx, { value: effTurnsValue != null ? String(effTurnsValue) : "—", label: "TURNS" });
  if (activeInModel) {
    // The tool calls of the same executions the turn and token counts
    // describe (this run's attempt, or its mission's executions when those
    // numbers rolled up).
    const tools = toolCounts(tokRateRuns === ownRuns ? attemptRecs : recordsAsOf(tokRateRuns.flatMap((g) => g.records), nowMs));
    push(modelIdx, { value: String(tools.calls), label: "TOOL CALLS", sub: `${tools.failed} failed` });
    push(modelIdx, { value: activeElapsed, label: "ACTIVE TIME", hintTitle: ACTIVE_HINT_TITLE, sub: activeTimeSub(done, wallSub, rests.get("thermal"), armed.thermal === true) });
  }
  // (#3067) The tiles count every usage record of the run, the figure the runs
  // board shows; the part that is darkmux's own utility calls is named in the
  // tiles' hover text (no layout of its own).
  const utilityHint = utilityHintOf(effTokUtility);
  push(modelIdx, { value: effTokIn != null ? fmtC(effTokIn) : "—", label: "TOKENS IN", hintTitle: utilityHint });
  push(modelIdx, { value: effTokOut != null ? fmtC(effTokOut) : "—", label: "TOKENS OUT", hintTitle: utilityHint });
  // A single-shot call records no `telemetry.context` sample and neither
  // bookend names the model's window, so its prompt has nothing to be a share
  // of: the tile reads a dash rather than a guessed window.
  push(modelIdx, ctxTileOf(eff, ctxTileFigures));
  // (#2877, #2890) A finished run's average generation rate: the MODEL
  // hero scope's center once the run is done. A run still in progress shows
  // the live scope instead (`liveTokScope`).
  const finishedTokRate = done && effHasModelWork ? finishedRate(tokRateRecordSets, effTokOut, runWallMs) : null;
  // (U3-6) The mission graph's per-step badge shows the STEP SPAN (setup, the
  // model's work, the gate) while this tile is the dispatch's own `wall_ms`,
  // the execution alone; the flow record carries no step span, so this tile
  // names what it measures. (#2890) With a model section the same figure is
  // ACTIVE TIME above; SYSTEM keeps WALL CLOCK only for a unit with none.
  if (!activeInModel) push(systemIdx, { value: wallBase, label: "WALL CLOCK", hint: "run time", hintTitle: WALL_HINT_TITLE, sub: wallSub });
  // (#1973) COMPACTIONS is a HARNESS metric: the harness decides to compact
  // and performs it through a utility role's sub-execution. Gated on model
  // work: a `procedural.shell` step has no context to compact, and
  // `0 COMPACTIONS` would assert something impossible.
  if (hasModelWork) push(systemIdx, { value: String(comps.length), label: "COMPACTIONS" });
  // (#2890) Thermal rest rides under ACTIVE TIME when that cell exists.
  for (const t of restTiles(rests, armed, activeInModel)) push(systemIdx, t);
  for (const t of hostTiles(hostAgg, hasModelWork)) push(systemIdx, t);

  // (#1973) Indices into `metrics`, not a second copy — one ordered list, one
  // grouping over it, so the two cannot drift apart. TURNS/TOKENS/CTX/
  // COMPACTIONS are the model's work; WALL CLOCK is the harness's. Compaction
  // is a UTILITY role's sub-execution rather than the specialist's own work,
  // but it is counted here because what the operator is reading is "what
  // happened to this model's context", which is exactly what a compaction did.

  const metricScope = { model: effHasModelWork ? modelIdx : [], system: systemIdx };

  // ── model track ────────────────────────────────────────────────────
  // (#1973) Was `model (lms)`, which named the SUBSYSTEM rather than the
  // content and left an operator asking what it meant — the question that
  // started this redesign. It is a list of every model LMStudio held during
  // the run, so it says that.
  //
  // Marking the primary needs no new wire field: the `dispatch start` record
  // carries the resolved model (`NormRecord.model`), which is ground truth
  // for what this role actually ran on. Note it is read from `d?.model`
  // SPECIFICALLY, not from the `model` binding above — that one falls back to
  // `distinct[0]`, the first-loaded model, which is a heuristic. Marking a
  // guess as authoritative is precisely the mistake #1934 is about, so when
  // the record does not name a model, nothing is marked primary.
  //
  // The other entries are labelled `also loaded` and NOT "compactor": what a
  // secondary model was FOR is not knowable until `telemetry.lms` carries the
  // profile's declared role (#1973 slice 4). Saying "also loaded" is true;
  // guessing by size or load order would not be.
  const primaryModel = d?.model ?? null;
  // (#2834) "endpoint model", not "remote model": the record says a model
  // was reached over HTTP, which is true of a server on this machine too.
  const modelTrackLabel = ep ? "endpoint model" : "loaded models";
  const showModelCard = hasModelWork && !ep;
  // (#2759) When THIS session loaded nothing itself, fall back to whatever
  // the mission-wide rollup found on its inner executions — the same data
  // that just turned TURNS/TOKENS/CONTEXT on above. Unlabeled (no primary/
  // also-loaded tag): those tags read `d?.model` against `f.model`, both of
  // which are THIS session's own fields and mean nothing for a load that
  // happened on a different session entirely.
  // (#2863) Compared WITHOUT darkmux's namespace. Since #2240 a local
  // dispatch names the model `darkmux:<key>` on the wire, while LM Studio's
  // load telemetry reports the bare key; compared as-is they never matched,
  // and every model on a real run, including the one that ran, read "also
  // loaded". The model that ran is listed first.
  const { modelEntries, modelTrackLines } = modelTrackOf(loads, primaryModel, ep ? (model || "unknown") : null, ownHasTelemetryEvidence ? primaryModel : null, rollup?.loadLines ?? []);

  const { signalGroups, signalsLabel, repetitionOff, repetitionRecorded } = runSignals({
    skewedClose,
    lms,
    loads,
    distinct,
    d,
    dets,
    checkpoints: visible.filter((r) => inAttempt(r) && r.action === ACTION.DispatchCheckpoint),
  });



  return {
    // (#1221) The machine name was a hardcoded `""`, so the run card header
    // read `(<sid> on )` with a dangling "on" — while every record in the
    // stream carried the right `machine_id` and the events list below rendered
    // it correctly. A stub, not a data gap.
    //
    // Prefer the dispatch.start record's machine (the machine that OWNS the
    // run) and fall back to any record in the session, so a run whose start
    // record has scrolled out of the window still names its machine.
    header: {
      pillLabel: pill.label.toUpperCase(),
      status: pill.status,
      role,
      sid,
      machineName: on.name,
    },
    briefLines,
    disclosures,
    metrics,
    metricScope,
    liveTokScope: effHasModelWork && !done ? liveTokScopeOf(tokRateLiveState, liveTokRate, tokRateStalled, tokRateNoSignal, nowMs) : null,
    finishedTokRate,
    showModelCard,
    modelTrackLabel,
    modelTrackLines,
    modelEntries,
    // (#2759) Gates the "loaded models" track's own visibility
    // (`SessionReplay.tsx`'s `view.hasModelWork &&` render guard) — a rolled-
    // up mission execution must open that track the same as an own-session
    // one would, or `modelTrackLines`'s rollup fallback above is computed
    // and never shown.
    hasModelWork: effHasModelWork,
    live: !done,
    ended: l?.close != null,
    lastBeatMs,
    signalsLabel,
    signalGroups,
    repetitionOff,
    repetitionRecorded,
  };
}
