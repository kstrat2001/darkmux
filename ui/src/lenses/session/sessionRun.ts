/**
 * Pure logic for the session drill-in ("run detail" for a `#session=<id>`
 * route) — a TypeScript port of `viewer.html`'s `runRegions()`
 * (viewer.html:2064-2285), the derivation behind `renderSubsystem()`
 * (viewer.html:2292-2309). This is the "whole separate render surface"
 * `SessionReplay.tsx`'s pre-drill-in doc named as out of scope; this packet
 * is the one that builds it.
 *
 * Validated against the ONE real recorded golden this repo already has for
 * legacy's own render (`tests/parity/goldens/session-task-list.txt`'s
 * `=== stage ===` section, captured from `#session=task-list` against the
 * real corpus fixture `tests/parity/corpus/flow-session-task-list.json`) —
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
 * 2. The two SVG visualizations (viewer.html's `loadRow`/`ctxChart` —
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
 * `state.t=tMax` set once at boot for a `#session=`/`#mission=` catalog
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

import { T, dispatchErrored, dispatchKilled, statusLabel, runStateFrom, computeTMax } from "../../lib/flow";
import { fmtElapsed, clk, fmtC } from "../../lib/format";
import { aggregateHostSamples, roundPct } from "../../lib/hostStats";
import { aggregateLiveState, aggregateTokenRate, averageGenerationRate } from "../../lib/tokenRate";
import type { LiveState } from "../../lib/tokenRate";
import type { FlowRecord, DispatchStartPayload, DispatchCompletePayload } from "../../types/handwritten";

export type PillCls = "run" | "err" | "done" | "canceled";

/** `statusVisual()`'s `cls` half (viewer.html:1151-1155) — `statusLabel`
 * (already ported in `lib/flow.ts`) gives the SAME vocabulary's `lbl` half;
 * this maps back to the class the two vocabularies share. */
function pillClsFor(label: string): PillCls {
  if (label === "running") return "run";
  if (label === "errored" || label === "killed") return "err";
  if (label === "complete") return "done";
  return "canceled";
}

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

export interface SessionHeader {
  /** Pre-uppercased (`.sub h2{text-transform:uppercase}` in legacy CSS —
   * this port uppercases the string directly, per `lib/format.ts`'s
   * "uppercase the STRING directly" discipline, rather than depending on a
   * stylesheet rule this port is free to change). */
  pillLabel: string;
  pillCls: PillCls;
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
   * was captured via the OTHER real entry point — a bare `#session=<id>`
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
   * `activateRun`), but neither is a `#session=` drill either. So the real
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
  metrics: Array<{ value: string; label: string; hint?: string; hintTitle?: string; sub?: string; unit?: string }>;
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
        stalled: boolean;
        /** (#2877 pass 2) The legible between-heartbeats state — see
         *  `lib/tokenRate.ts::deriveLiveState`'s own doc. `stalled` above is
         *  now DERIVED from this (`state === "stalled"`), so the two can
         *  never disagree. */
        state: LiveState | null;
        /** Present only when `state === "rest"` — whole seconds left in the
         *  reported rest window. */
        restSecondsLeft?: number;
      }
    | null;
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
   * nothing" (enforce/observe with no findings) and "unknown" (no
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
export type SignalSeverity = "warn" | "info";

export interface Signal {
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

export interface SignalGroup {
  kind: string;
  severity: SignalSeverity;
  count: number;
  signals: Signal[];
}

/** (#1973) One payload the brief summarizes, carried in full so the renderer
 *  can expand it in place. `chars` is the AUTHORITATIVE length from the
 *  record (`prompt_chars`) when present, so a truncated payload still reports
 *  its true size rather than the size of what survived. */
export interface Disclosure {
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

/** (#2759) The utility/sub-execution role family CLAUDE.md's "Role families"
 *  section names — compactor / scribe / estimator / mission-compiler — plus
 *  the generic `utility` tag `telemetry.lms` records already use
 *  (`isUtilitySeat` above, for the SAME family at the load-track level).
 *  Never rolled into a run's MODEL total: contract 8 requires a sub-
 *  execution keep its own role/model attribution, and blending a 4B
 *  compactor's tokens into a specialist's total is exactly the violation
 *  that rule exists to stop. */
function isUtilityRoleHandle(handle: string | null | undefined): boolean {
  if (!handle) return false;
  const bare = String(handle).replace(/^darkmux\//, "").toLowerCase();
  return bare === "compactor" || bare === "scribe" || bare === "estimator" || bare === "mission-compiler" || bare === "utility";
}

interface MissionModelRollup {
  hasEvidence: boolean;
  turns: number | null;
  tokIn: number | null;
  tokOut: number | null;
  ctxPeak: number;
  ctxNow: number;
  nctx: number;
  loadLines: string[];
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
 *  This walks every OTHER `session_id` present in `data` that shares this
 *  run's `mission_id`, and sums the MODEL-scoped numbers off each one that
 *  did real model work — skipping a utility role's sub-execution so its
 *  tokens never fold into a specialist's total.
 *
 *  KNOWN NARROWING, named rather than hidden: this walks by `session_id`,
 *  not by role EXECUTION. `session_id::task` is task-scoped, so a
 *  `dispatch.map` fan-out mints ONE session_id shared by every sibling
 *  seat — this reads them as a single execution and sums their records
 *  together, the same simplification `runRegions`'s own single-session path
 *  already makes for a session carrying more than one `dispatch.start`
 *  (only the LATEST is treated as "the" attempt). Separating siblings would
 *  need the `index`/`remote` keys #2690 put on those records; not attempted
 *  here — this fix targets the reported defect (a run session with zero
 *  telemetry finding real numbers on its inner sessions), not per-seat
 *  breakdown. */
function rollUpMissionModelWork(data: FlowRecord[], missionId: string, excludeSid: string): MissionModelRollup {
  const candidateSids = new Set<string>();
  for (const r of data) {
    if (r.mission_id === missionId && r.session_id && r.session_id !== excludeSid) candidateSids.add(r.session_id);
  }
  let turns: number | null = null;
  let tokIn: number | null = null;
  let tokOut: number | null = null;
  let ctxPeak = 0;
  let ctxNow = 0;
  let nctx = 0;
  const loadLines: string[] = [];
  let hasEvidence = false;
  for (const csid of candidateSids) {
    const own = data.filter((r) => r.session_id === csid);
    const cStart = own.find((r) => r.action === "dispatch.start") ?? null;
    if (isUtilityRoleHandle(cStart?.handle)) continue; // sub-execution — never blended in
    const tel = own.filter((r) => r.category === "telemetry");
    const rt = tel.filter((r) => r.source === "runtime").slice(-1)[0] ?? null;
    const toks = tel.filter((r) => r.source === "tokens");
    const cx = tel
      .filter((r) => r.source === "context")
      .slice()
      .sort((a, b) => T(a.ts) - T(b.ts));
    const loads = tel.filter(
      (r) => r.source === "lms" && (r.fields as Record<string, unknown> | undefined)?.event === "load",
    );
    const cTurns = rt ? Number((rt.fields as Record<string, unknown>).turns) : null;
    const cTokIn = toks.length
      ? toks.reduce((a, r) => a + (Number((r.fields as Record<string, unknown>)?.prompt_tokens) || 0), 0)
      : null;
    const cTokOut = toks.length
      ? toks.reduce((a, r) => a + (Number((r.fields as Record<string, unknown>)?.completion_tokens) || 0), 0)
      : null;
    const cCx0Max = cx.length ? Number((cx[0].fields as Record<string, unknown>)?.max) : NaN;
    const cNctx = cx.length && Number.isFinite(cCx0Max) && cCx0Max > 0 ? cCx0Max : 0;
    const cCtxPeak = cx.length ? Math.max(...cx.map((r) => Number((r.fields as Record<string, unknown>)?.used) || 0)) : 0;
    const cCtxNow = cx.length ? Number((cx[cx.length - 1].fields as Record<string, unknown>)?.used) || 0 : 0;
    const csHasEvidence = loads.length > 0 || cTurns != null || cTokIn != null || cTokOut != null || cx.length > 0;
    if (!csHasEvidence) continue;
    hasEvidence = true;
    if (cTurns != null) turns = (turns ?? 0) + cTurns;
    if (cTokIn != null) tokIn = (tokIn ?? 0) + cTokIn;
    if (cTokOut != null) tokOut = (tokOut ?? 0) + cTokOut;
    ctxPeak = Math.max(ctxPeak, cCtxPeak);
    ctxNow = Math.max(ctxNow, cCtxNow);
    nctx = Math.max(nctx, cNctx);
    // Every inner execution records each model resident when it started, so
    // one model appears once per execution; list it once.
    for (const l of loads) {
      const f = l.fields as Record<string, unknown>;
      const line = `${f.model} · ${f.gb ?? "?"}GB`;
      if (!loadLines.includes(line)) loadLines.push(line);
    }
  }
  return { hasEvidence, turns, tokIn, tokOut, ctxPeak, ctxNow, nctx, loadLines };
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

/** `runRegions()` — viewer.html:2064-2285, minus the two SVG chart regions
 * (see this module's own top doc). `data` should already be scoped to ONE
 * session (the `/flow-session/<id>` response, through `flowToRenderModel`
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
 */
export function runRegions(data: FlowRecord[], sid: string, nowOverride?: number): SessionRunView {
  const tMax = computeTMax(data);
  const nowMs = nowOverride != null ? Math.max(nowOverride, tMax) : tMax;

  // (#1988) `T(ts)` is `NaN` for an unparsable timestamp, and EVERY
  // comparison against `NaN` is false — including `NaN <= nowMs`. So a single
  // malformed `ts` on the start record used to drop it from `sidStarts`
  // entirely, leave `startTs` as `NaN`, and make `inAttempt` false for every
  // record in the session including a perfectly good `dispatch.complete`.
  // The run then read RUNNING forever AND lost its whole brief — prompt,
  // runtime, image, workspace, model — because `d` was null.
  //
  // Two separate repairs, because the record serves two purposes: it is the
  // PAYLOAD source (the brief) and the CLOCK source (the attempt window).
  // A bad clock must not cost the payload.
  const finiteTs = (r: FlowRecord): number | null => {
    const t = T(r.ts);
    return Number.isFinite(t) ? t : null;
  };
  const allSidStarts = data.filter((r) => r.session_id === sid && r.action === "dispatch.start");
  const sidStarts = allSidStarts
    .filter((r) => {
      const t = finiteTs(r);
      return t != null && t <= nowMs;
    })
    .sort((a, b) => T(a.ts) - T(b.ts));
  // Prefer a start with a usable clock; fall back to ANY start so the brief
  // survives a malformed timestamp rather than vanishing with it.
  const d = sidStarts.length ? sidStarts[sidStarts.length - 1] : (allSidStarts[allSidStarts.length - 1] ?? null);
  const firstSessRec = data.find((r) => r.session_id === sid) ?? null;
  // `startTs` must be FINITE or it poisons every downstream comparison. Walk
  // outward for a usable clock: the start record, then the session's first
  // record, then its earliest parsable one, then `now`.
  const sessionTimes = data.filter((r) => r.session_id === sid).map(finiteTs).filter((t): t is number => t != null);
  const startTs =
    (d ? finiteTs(d) : null) ??
    (firstSessRec ? finiteTs(firstSessRec) : null) ??
    (sessionTimes.length ? Math.min(...sessionTimes) : null) ??
    nowMs;
  // A record whose own `ts` is unparsable is INCLUDED, not silently dropped.
  // Excluding it is what hid a legitimate terminal; a malformed record should
  // be visible and wrong-looking, never invisible.
  const inAttempt = (r: FlowRecord) => {
    if (r.session_id !== sid) return false;
    const t = finiteTs(r);
    return t == null || t >= startTs;
  };

  // (#1988) The close edge is selected WITHOUT requiring `ts >= startTs`.
  //
  // Requiring it meant a terminal record timestamped before its own start —
  // ordinary cross-machine clock skew, which this function's own `nowMs`
  // clamp above already anticipates — was filtered out, so a finished
  // dispatch reported as perpetually in flight. Guarding the elapsed-time
  // arithmetic against skew while leaving the terminal SELECTION exposed to
  // it was the inconsistency.
  const isTerminal = (r: FlowRecord) =>
    r.action === "dispatch.complete" || r.action === "dispatch.error" || r.action === "session.end";
  const sessionTerminals = data.filter((r) => r.session_id === sid && isTerminal(r)).sort((a, b) => T(a.ts) - T(b.ts));
  const inAttemptCloses = sessionTerminals.filter(inAttempt);
  // Prefer terminals inside the attempt window; fall back to any terminal on
  // the session, so a skewed one is honored rather than hidden. `skewedClose`
  // records that the fallback fired, so the page can SAY so instead of
  // quietly presenting a reconstructed timeline as fact.
  const skewedClose = inAttemptCloses.length === 0 && sessionTerminals.length > 0;
  const attemptCloses = inAttemptCloses.length ? inAttemptCloses : sessionTerminals;
  const close = attemptCloses[0] ?? null;
  const c = attemptCloses.find((r) => r.action !== "session.end") ?? null;
  // A close with an unparsable `ts` still terminates the run — it is a
  // terminal record, and `NaN <= nowMs` being false must not resurrect it.
  const closeTs = close ? finiteTs(close) : null;
  const done = !!close && (closeTs == null || closeTs <= nowMs);

  const visible = data.filter((r) => T(r.ts) <= nowMs);
  const tel = visible.filter((r) => inAttempt(r) && r.category === "telemetry");
  const lms = tel.filter((r) => r.source === "lms");
  // (#2413 M4) Host cpu/ram/gpu samples used to ride the per-dispatch
  // `telemetry.process` record — `category: "telemetry"`, `source:
  // "process"`, this session's own `session_id` — so `tel`'s filters
  // above caught it for free. M3 retired that producer; the replacement,
  // `machine.telemetry`, is machine-scoped: `category: "machinery"`
  // (NOT "telemetry"), `source: "host"`, and no `session_id` at all — so
  // `inAttempt` (which requires a session_id match) silently excludes it
  // and this pane's CPU/RAM/GPU tiles would vanish. The server already
  // joins the machine-scoped samples covering this run's window into the
  // SAME record set this session's own records arrive in (darkmux-serve's
  // `join_host_samples_into_session_records`, keyed on machine_uid + the
  // dispatch.start..terminal window) — so here it's a plain time-window
  // filter instead of `inAttempt`'s session match. Historical (pre-#2413)
  // `telemetry.process` records with this session's own `session_id`
  // still match via the `tel`/`source==="process"` half below —
  // lenient-on-read, both curves render.
  // (#2413 round 3 CONSIDER 3) `d`'s own `machine_uid` (the dispatch.start
  // record — falls back to the session's first record for the same reason
  // `startTs` does above) gates the join client-side too: without it, a
  // multi-machine playback fixture (records from more than one machine's
  // day file, e.g. a fleet view) would render every machine's samples
  // on every run's SYSTEM pane, not just the run's own machine's.
  const runMachineUid = d?.machine_uid ?? firstSessRec?.machine_uid ?? null;
  const hostSamples = visible.filter(
    (r) =>
      r.action === "machine.telemetry" &&
      (runMachineUid == null || r.machine_uid === runMachineUid) &&
      T(r.ts) >= startTs &&
      (closeTs == null || T(r.ts) <= closeTs),
  );
  const procs = [...tel.filter((r) => r.source === "process"), ...hostSamples];
  const rt = tel.filter((r) => r.source === "runtime").slice(-1)[0] ?? null;
  const dets = tel.filter((r) => r.source === "detector");
  const loads = lms.filter((r) => (r.fields as Record<string, unknown> | undefined)?.event === "load");
  const distinct = [...new Set(loads.map((r) => (r.fields as Record<string, unknown>).model as string))];

  const handle = d ? d.handle : firstSessRec ? firstSessRec.handle : "unknown";
  const turnsValue = rt ? Number((rt.fields as Record<string, unknown>).turns) : null;

  const cx = tel
    .filter((r) => r.source === "context")
    .slice()
    .sort((a, b) => T(a.ts) - T(b.ts));
  const comps = tel.filter((r) => r.source === "compaction");
  const cx0Max = cx.length ? Number((cx[0].fields as Record<string, unknown>)?.max) : NaN;
  const nctx = cx.length && Number.isFinite(cx0Max) && cx0Max > 0 ? cx0Max : 0;
  const ctxPeak = cx.length ? Math.max(...cx.map((r) => Number((r.fields as Record<string, unknown>)?.used) || 0)) : 0;
  const ctxNow = cx.length ? Number((cx[cx.length - 1].fields as Record<string, unknown>)?.used) || 0 : 0;

  // (#1972) Proof of life: the newest record belonging to THIS attempt. Not
  // heartbeats alone — a run emitting turns and tool results is demonstrably
  // alive whether or not a heartbeat happens to have landed recently, and
  // keying only on heartbeats would make a busy run look dead.
  const attemptRecs = visible.filter(inAttempt);
  const lastBeatMs = attemptRecs.length ? Math.max(...attemptRecs.map((r) => T(r.ts))) : null;

  // (#2011) The finished run's DURATION is read from the terminal record's
  // own `wall_ms` — the runtime's measure, taken between its start and
  // terminal record writes (`dispatch_internal.rs`'s
  // `dispatch_complete_payload`) — instead of being recomputed here from two
  // timestamps. Same shape as #1960/#1973/#2007: a payload in hand, and a
  // renderer deriving its own answer beside it.
  //
  // Why it matters beyond tidiness. The `so far` branch below is driven by
  // the shared 1s clock (#1972), so a page whose records go STALE keeps
  // counting: a run left open overnight rendered ~10 hours of elapsed time
  // for a ten-minute dispatch, with nothing on screen saying it was wrong.
  // Taking the number from the record that ENDS the run means the worst a
  // stale page can do is show a stale LABEL — it can no longer invent a
  // duration. (The staleness itself is fixed separately, in
  // `hooks/useSessionLiveness.ts`; this is the half that makes the failure
  // survivable when a fetch is missed anyway.)
  //
  // It also removes two arithmetic hazards that are already reachable in
  // this function: a terminal timestamped BEFORE its own start (the
  // `skewedClose` case above) subtracts to a negative, and an unparsable
  // `ts` subtracts to `NaN`.
  //
  // Read off `c`, not `close`: a `session.end` close-edge carries no payload
  // at all (`presence_reconciler.rs`'s `build_session_end_record` sets
  // `payload: None`), and archived records predate the field — so the
  // subtraction stays as the fallback rather than being deleted.
  const recordedWallMs = (c?.payload as DispatchCompletePayload | undefined)?.wall_ms;
  const runWallMs =
    typeof recordedWallMs === "number" && Number.isFinite(recordedWallMs)
      ? recordedWallMs
      : close
        ? T(close.ts) - startTs
        : NaN;

  // (U3-7/U5-2) `fmtElapsed`, not the retired `fmtDuration`: a dispatch
  // that runs past an hour used to read "75:23" here.
  const wallBase = done ? fmtElapsed(runWallMs) : `${fmtElapsed(nowMs - startTs)} so far`;
  const exitCode = (c?.payload as DispatchCompletePayload | undefined)?.exit_code;
  // (#2860) How the run ended goes on the tile's `sub` line, not appended to
  // the figure: the value is `nowrap` because it is contracted to be one
  // short figure (`styles.css`, `.session-run .mv`), and "3:38 · errored
  // (exit 1)" ran through the neighbouring tile on a phone.
  const wallOutcome =
    done && c && dispatchErrored(c)
      ? dispatchKilled(c)
        ? "killed (timeout)"
        : `errored${exitCode != null ? ` (exit ${exitCode})` : ""}`
      : undefined;
  // (rest-reason cards) WALL CLOCK used to append a "incl. N rest" breakdown
  // here (#2863). That breakdown now lives as its own per-kind SYSTEM tiles
  // (THERMAL REST / TURN DELAY / BATTERY PAUSE / OPERATOR HOLD, built below
  // via `restKindTiles`) — a card showing a count AND whether the
  // protection was even armed, rather than one crowded sub-line. WALL CLOCK
  // goes back to naming only what it always named: run time + outcome.
  const wallSub = wallOutcome;

  const role = String(handle || "").replace(/^darkmux\//, "").toUpperCase();
  const svLabel = statusLabel(
    runStateFrom({
      open: !done,
      errored: !!c && dispatchErrored(c),
      killed: !!c && dispatchKilled(c),
      clean: done && !!c && !dispatchErrored(c),
    }),
  );

  const sp = (d?.payload ?? {}) as DispatchStartPayload;
  const dp = (c?.payload ?? {}) as DispatchCompletePayload;
  const remoteEp = sp.endpoint || dp.endpoint;
  const model = d?.model ? d.model : remoteEp ? remoteEp.slice(remoteEp.lastIndexOf("/") + 1) : (distinct[0] as string | undefined) ?? null;

  const toks = tel.filter((r) => r.source === "tokens");
  const tokIn = toks.length
    ? toks.reduce((a, r) => a + (Number((r.fields as Record<string, unknown>)?.prompt_tokens) || 0), 0)
    : (dp.prompt_tokens ?? null);
  const tokOut = toks.length
    ? toks.reduce((a, r) => a + (Number((r.fields as Record<string, unknown>)?.completion_tokens) || 0), 0)
    : (dp.completion_tokens ?? null);

  // ── brief ──────────────────────────────────────────────────────────
  // (#2011) Same `runWallMs` the WALL CLOCK tile shows. The two lines report
  // one quantity, so they read from one source — deriving it twice is how
  // they end up disagreeing by a second at a rounding boundary. The two
  // CLOCK stamps stay record-derived: they are timestamps, not a duration.
  const briefTiming = `${clk(startTs)}${done ? ` → ${clk(T(close!.ts))} (${fmtElapsed(runWallMs)})` : " · running"}`;
  const RUNTIME_LABEL: Record<string, string> = {
    internal: "internal container",
    direct: "direct client (hosted · no container)",
    openclaw: "openclaw shell-out",
  };
  const ep = remoteEp;
  const route = ep
    ? (() => {
        const i = ep.indexOf(":");
        const kind = i >= 0 ? ep.slice(0, i) : "";
        const rest = i >= 0 ? ep.slice(i + 1) : ep;
        // (#2834) The dialect and the address are FACTS darkmux read off
        // the dispatch record. "off-fleet" was an inference on top of them,
        // and a wrong one: `openai:` names the request FORMAT, not a
        // vendor, so a local inference server on 127.0.0.1 speaking the
        // OpenAI-compatible protocol was labelled as having left the
        // machine. The address is right there for the operator to read;
        // darkmux does not need to editorialize about where it points.
        const label = kind === "azure" ? "Azure OpenAI" : kind === "openai" ? "OpenAI" : kind || "endpoint";
        return `${label} · ${rest}`;
      })()
    : "LMStudio · local · this machine";

  const briefRows: BriefEntry[] = [];
  pushKv(briefRows, "route", route);
  pushKv(briefRows, "runtime", sp.runtime ? RUNTIME_LABEL[sp.runtime] ?? sp.runtime : "");
  pushKv(briefRows, "image", sp.image);
  pushKv(briefRows, "model", model);
  pushKv(briefRows, "workspace", sp.workspace);
  if (d?.mission_id) {
    briefRows.push({ kind: "label", text: "mission" });
    briefRows.push({
      kind: "value",
      text: `${d.mission_id}${d.phase_id ? ` · phase ${d.phase_id}` : ""}`,
      href: `#mission=${encodeURIComponent(d.mission_id)}`,
    });
  }
  pushKv(briefRows, "timing", briefTiming);

  const promptLines: BriefEntry[] = [];
  const disclosures: Disclosure[] = [];
  if (sp.prompt) {
    const chars = sp.prompt_chars ?? sp.prompt.length;
    const isTrunc = sp.prompt_chars != null && sp.prompt.length < sp.prompt_chars;
    // (#1973) The text itself — which this function used to read the length of
    // and then drop on the floor.
    //
    // NO brief note here. The disclosure's own summary already reads
    // `prompt · <n> chars`, so pushing one would print the same sentence twice,
    // a few pixels apart — the same duplication the run brief's bare "run"
    // heading was removed for (see `briefLines` below). The summary IS the
    // one-liner now, and it is the one that expands.
    disclosures.push({ id: "prompt", label: "prompt", chars, truncated: isTrunc, text: sp.prompt });
  } else if (sp.prompt_chars != null) {
    // A record that reports a length but carries no text: say so in the brief,
    // rather than offering an expander onto nothing. This is the ONLY case
    // that still produces a brief prompt line.
    promptLines.push({ kind: "label", text: "prompt" });
    promptLines.push({ kind: "value", text: `${sp.prompt_chars} chars` });
  }

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
  const ownHasTelemetryEvidence =
    loads.length > 0 || turnsValue != null || tokIn != null || tokOut != null || cx.length > 0 || comps.length > 0;
  const missionIdForRollup = d?.mission_id ?? firstSessRec?.mission_id ?? null;
  const rollup =
    !ownHasTelemetryEvidence && missionIdForRollup ? rollUpMissionModelWork(data, missionIdForRollup, sid) : null;
  // Only the four MODEL-pane numbers roll up (contract 8's own scope for
  // this fix — see the run-detail issue's "Direction"). WALL CLOCK and
  // COMPACTIONS stay scoped to this session's own attempt window below,
  // deliberately: they are HARNESS metrics about running THIS bookend pair,
  // not about the model's work inside it.
  const effTurnsValue = ownHasTelemetryEvidence ? turnsValue : (rollup?.turns ?? turnsValue);
  const effTokIn = ownHasTelemetryEvidence ? tokIn : (rollup?.tokIn ?? tokIn);
  const effTokOut = ownHasTelemetryEvidence ? tokOut : (rollup?.tokOut ?? tokOut);
  const effCtxPeak = ownHasTelemetryEvidence || !rollup?.hasEvidence ? ctxPeak : rollup.ctxPeak;
  const effCtxNow = ownHasTelemetryEvidence || !rollup?.hasEvidence ? ctxNow : rollup.ctxNow;
  const effNctx = ownHasTelemetryEvidence || !rollup?.hasEvidence ? nctx : rollup.nctx;

  // ── metrics ────────────────────────────────────────────────────────
  // (operator, 2026-09-05) This used to be ONE string that did the whole
  // tile's talking — `CTX PEAK 19K / 262.144K WINDOW` — printed as the
  // tile's LABEL while the tile's VALUE printed the same "19K" a second
  // time right above it. Two defects rode together: the label restated the
  // value, and `nctx / 1000` (a bare division, no formatter) produced
  // `262.144K` for a 262144-token window instead of a compact `262k`.
  //
  // The fix splits the one string into the three slots every metric tile
  // now has: `label` names WHAT the number is (never repeats it), `value`
  // IS the number, and `sub` carries the qualifying fact — the window
  // ceiling, and, while live, the peak-so-far — that used to be crammed
  // into the label. `fmtC` (the same compact formatter TOKENS IN/OUT
  // already use) replaces the hand-rolled `/1000 + toFixed + "K"` division,
  // which both fixes the format and gets the casing that formatter uses
  // (`262k`, not `262.144K`) — matching TOKENS IN/OUT rather than the
  // uppercase `K` this tile used to invent on its own.
  const ctxHeadline = done ? effCtxPeak : effCtxNow;
  const ctxLabel = !effNctx ? "CONTEXT" : done ? "CTX PEAK" : "CTX NOW";
  const ctxSub = !effNctx ? undefined : done ? `of ${fmtC(effNctx)}` : `peak ${fmtC(effCtxPeak)} · of ${fmtC(effNctx)}`;

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
  const hasModelWork =
    d != null || loads.length > 0 || turnsValue != null || tokIn != null || tokOut != null || cx.length > 0 || comps.length > 0;
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
  // on those same candidate sibling sessions `rollUpMissionModelWork`
  // walked. Re-deriving that candidate set here (rather than threading it
  // out of that function) keeps this additive and keeps the rollup
  // function's contract — MissionModelRollup's four numbers — unchanged.
  const tokRateSids: string[] =
    ownHasTelemetryEvidence || !missionIdForRollup
      ? [sid]
      : (() => {
          const set = new Set<string>();
          for (const r of data) {
            if (r.mission_id === missionIdForRollup && r.session_id) set.add(r.session_id);
          }
          return set.size ? [...set] : [sid];
        })();
  const tokRateRecordSets = tokRateSids.map((s) => data.filter((r) => r.session_id === s));
  // (#2877 pass 2) ONE state derivation, `lib/tokenRate.ts::deriveLiveState`,
  // aggregated across the same sibling-session candidates the tok/s reading
  // already sums (`aggregateLiveState`'s own doc: the best/most-informative
  // reading wins). `tokRateStalled` is now DERIVED from it rather than a
  // second, separately-computed "every candidate stale" check — the two
  // used to be able to disagree (a marker explaining the gap on every
  // candidate would still read "stalled" under the old rule); they can't
  // any more, because there is only one rule now.
  const tokRateLiveState = aggregateLiveState(tokRateRecordSets, nowMs);
  const tokRateStalled = tokRateLiveState?.state === "stalled";

  // (#1973) Host telemetry — CPU / RAM / GPU — was FETCHED and thrown away:
  // `const procs = ...` followed by `void procs` to silence the unused
  // warning, with a comment parking it for "a future packet". That is the
  // fifth instance of one shape in this lens (tool arguments, session
  // records, the prompt, and now this): the data is in hand and the renderer
  // drops it.
  //
  // (#2107) PEAK alone answered "did this saturate the machine" but not how
  // hard it was driven ON AVERAGE — the same gap the host-side reduction
  // (`dispatch_internal.rs`'s `HostStats`) closed for the envelope. `avg`
  // now rides beside `high` in the SAME tile ("add the avg to the card with
  // the high" — operator), through the ONE aggregation
  // (`lib/hostStats.ts::aggregateHostSamples`) the global machine drawer
  // also uses, so the two surfaces can't report different numbers for
  // overlapping samples.
  const toNum = (v: unknown): number | undefined => {
    const n = Number(v);
    return Number.isFinite(n) ? n : undefined;
  };
  const hostAgg = aggregateHostSamples(
    procs.map((r) => {
      const f = r.fields as Record<string, unknown> | undefined;
      // (#2413 M4) The retired `telemetry.process` payload used bare
      // `cpu`/`mem`/`gpu`; the machine-scoped `machine.telemetry`
      // replacement uses `cpu_pct`/`mem_pct`/`gpu_pct` (see
      // `host_probe::sample_full_json`) — read either so both curves
      // aggregate through the same code.
      return {
        cpu: toNum(f?.cpu ?? f?.cpu_pct),
        mem: toNum(f?.mem ?? f?.mem_pct),
        gpu: toNum(f?.gpu ?? f?.gpu_pct),
      };
    }),
  );
  const cpuPeak = hostAgg.cpu.high;
  const ramPeak = hostAgg.mem.high;
  const gpuPeak = hostAgg.gpu.high;
  // (operator, 2026-09-05, second pass) Used to be ONE string —
  // `41% avg · 94% high` — crammed into the value slot. The value slot
  // holds exactly one figure on one line, always (same rule as every other
  // tile now): the AVERAGE is the primary figure an operator reads at a
  // glance, so it becomes `value`; the peak qualifies it, so it becomes
  // `sub` — the same value/label/sub split CTX got, applied here because
  // this was the OTHER place a tile's value was a multi-stat phrase rather
  // than a figure.
  // (#2863) "avg" names the big figure, so it rides beside it as a `unit`
  // (rendered small, same line) rather than leading the sub line, where it
  // read as a label for the peak beneath it.
  const avgHighSplit = (m: { avg: number | null; high: number | null }): { value: string; sub: string; unit: string } => ({
    value: `${roundPct(m.avg)}%`,
    sub: `${roundPct(m.high)}% high`,
    unit: "avg",
  });

  // Built as a list with its scope recorded AS EACH TILE IS ADDED, rather
  // than as a fixed array plus hardcoded indices. The indices are now
  // conditional (host tiles only exist when host telemetry does), and an
  // audit already flagged the hardcoded form as a positional contract nothing
  // enforced — this makes the two unable to drift because there is only one.
  const metrics: Array<{ value: string; label: string; hint?: string; hintTitle?: string; sub?: string; unit?: string }> = [];
  const modelIdx: number[] = [];
  const systemIdx: number[] = [];
  const push = (into: number[], value: string, label: string, hint?: string, hintTitle?: string, sub?: string, unit?: string) => {
    into.push(metrics.length);
    metrics.push({ value, label, hint, hintTitle, sub, unit });
  };
  push(modelIdx, effTurnsValue != null ? String(effTurnsValue) : "—", "TURNS");
  push(modelIdx, effTokIn != null ? fmtC(effTokIn) : "—", "TOKENS IN");
  push(modelIdx, effTokOut != null ? fmtC(effTokOut) : "—", "TOKENS OUT");
  push(modelIdx, effNctx ? fmtC(ctxHeadline) : "—", ctxLabel, undefined, undefined, ctxSub);
  // (#2877) The fifth MODEL tile, TOK/S. A FINISHED run gets a plain text
  // tile like its four neighbors here — "the scope goes... the tile shows
  // the final measured tok/s" (issue text). A run still in progress does
  // NOT push here at all; `SessionReplay.tsx` renders `liveTokScope` (the
  // live canvas + centered number) as the fifth tile instead, since a
  // pushed string tile has no way to host a component. Final rate: total
  // billed output tokens over the run's own wall clock — the same two
  // numbers TOKENS OUT and WALL CLOCK already show, so this tile's number
  // is reconcilable against its neighbors rather than a third, opaque
  // measurement.
  if (done) {
    // The model's generation rate: billed tokens over generation time, an
    // exact average, not an estimate. Wall clock is only the fallback for a
    // runtime that predates `generation_ms`, and the label says so.
    const genRate = averageGenerationRate(tokRateRecordSets);
    const wallRate = effTokOut != null && runWallMs > 0 ? effTokOut / (runWallMs / 1000) : null;
    const finalTokPerSec = genRate ?? wallRate;
    push(
      modelIdx,
      finalTokPerSec != null ? String(Math.round(finalTokPerSec)) : "—",
      "TOK/S",
      undefined,
      undefined,
      genRate != null ? "avg" : "avg · wall clock",
    );
  }
  // (U3-6) The mission graph's per-step badge shows the STEP SPAN — setup,
  // the model's work, and the gate — while this tile is the dispatch's own
  // `wall_ms`, the runtime's measure of the execution alone. On a real
  // mission the same step read 10:36 there and 10:07 here with nothing on
  // either screen saying why. The flow record carries no step span (see
  // `DispatchCompletePayload`: no step start/end field exists), so this side
  // cannot show BOTH numbers — it can only stop being anonymous, which is
  // what the label does. `StepRow.tsx` carries the matching half.
  push(
    systemIdx,
    wallBase,
    "WALL CLOCK",
    "run time",
    "run time — the runtime's own measure of this execution, INCLUDING any thermal rest. A mission step's badge covers a WIDER span (setup and gate included) and reads longer.",
    wallSub,
  );
  // (#1973) COMPACTIONS is a HARNESS metric, not a model one — operator call,
  // and it is the reading contract 8 supports: the harness DECIDES to compact
  // and performs it through a UTILITY role's sub-execution. The specialist
  // neither chooses it nor does it; it only experiences the result.
  // An earlier comment here argued the opposite — that an operator reads it
  // as "what happened to this model's context" — which describes the EFFECT
  // rather than the actor, and is exactly the blending the sub-execution rule
  // exists to stop.
  // Gated on model work for the same reason the model pane is: a
  // `procedural.shell` step has no context to compact, so `0 COMPACTIONS`
  // would assert "the harness compacted nothing" where the truth is "there
  // was nothing here that could be compacted".
  if (hasModelWork) push(systemIdx, String(comps.length), "COMPACTIONS");
  // (rest-reason cards) One SYSTEM tile per rest KIND — THERMAL REST / TURN
  // DELAY / BATTERY PAUSE / OPERATOR HOLD, plus a generic label for a
  // reason string this file doesn't recognize — replacing the single
  // blended "incl. N rest" WALL CLOCK sub-line stripped out above. A card
  // shows even at 0 rests when the protection was ARMED for this dispatch
  // (`sp.bounds`), so an operator can tell "configured and never fired"
  // from "not configured at all"; a run with no recorded `bounds` (older
  // runs, before #2165) shows a card only for a kind that actually
  // occurred — this mirrors `config_access`'s own `env > config > built-in`
  // leniency posture: absent data reads as "unknown", never "off".
  const restKindOf = (reason: unknown): { key: string; label: string } => {
    const r = String(reason ?? "").trim();
    if (r.startsWith("thermal")) return { key: "thermal", label: "THERMAL REST" };
    if (r === "turn_delay") return { key: "turn_delay", label: "TURN DELAY" };
    if (r.startsWith("battery")) return { key: "battery", label: "BATTERY PAUSE" };
    if (r.startsWith("operator")) return { key: "operator_hold", label: "OPERATOR HOLD" };
    const key = r || "rest";
    return { key, label: `${key.toUpperCase()} REST` };
  };
  const restByKind = new Map<string, { label: string; count: number; totalMs: number }>();
  for (const r of attemptRecs) {
    if (r.action !== "dispatch.rest") continue;
    const f = (r.fields || r.payload || {}) as Record<string, unknown>;
    // A record carrying `delay_ms` with no `ms` is the governor changing
    // its PACING, not a rest (`emit_rest`/`emit_rest_with_extra`,
    // `dispatch_internal.rs`) — only a record with a real `ms` is one rest.
    if (typeof f.ms !== "number" || !Number.isFinite(f.ms) || f.ms <= 0) continue;
    const { key, label } = restKindOf(f.reason);
    const cur = restByKind.get(key) ?? { label, count: 0, totalMs: 0 };
    cur.count += 1;
    cur.totalMs += f.ms;
    restByKind.set(key, cur);
  }
  const restBounds = sp.bounds;
  const restConfiguredByKind: Record<string, boolean> = {
    thermal: restBounds?.thermal_pacing_enabled?.value === true,
    turn_delay: typeof restBounds?.turn_delay_ms?.value === "number" && restBounds.turn_delay_ms.value > 0,
    battery: restBounds?.battery_pause_enabled?.value === true,
    // operator_hold has no config knob to arm — always falls through to
    // "shown only if it actually occurred", per operator design.
  };
  const STATIC_REST_LABELS: Record<string, string> = {
    thermal: "THERMAL REST",
    turn_delay: "TURN DELAY",
    battery: "BATTERY PAUSE",
    operator_hold: "OPERATOR HOLD",
  };
  const restKindOrder = ["thermal", "turn_delay", "battery", "operator_hold"];
  const extraKinds = [...restByKind.keys()]
    .filter((k) => !restKindOrder.includes(k))
    .sort((a, b) => (restByKind.get(b)?.totalMs ?? 0) - (restByKind.get(a)?.totalMs ?? 0));
  for (const key of [...restKindOrder, ...extraKinds]) {
    const occurred = restByKind.get(key);
    const configured = restConfiguredByKind[key] === true;
    if (!configured && !occurred) continue;
    const label = STATIC_REST_LABELS[key] ?? occurred?.label ?? key.toUpperCase();
    const count = occurred?.count ?? 0;
    const totalMs = occurred?.totalMs ?? 0;
    const sub =
      key === "turn_delay" && count > 0
        ? `${count} rest${count === 1 ? "" : "s"} · ${Math.round(totalMs / count / 1000)} s each`
        : `${count} rest${count === 1 ? "" : "s"}`;
    push(systemIdx, fmtElapsed(totalMs), label, undefined, undefined, sub);
  }
  if (cpuPeak != null) {
    const s = avgHighSplit(hostAgg.cpu);
    push(systemIdx, s.value, "CPU", undefined, undefined, s.sub, s.unit);
  }
  if (ramPeak != null) {
    const s = avgHighSplit(hostAgg.mem);
    push(systemIdx, s.value, "RAM", undefined, undefined, s.sub, s.unit);
  }
  if (gpuPeak != null) {
    const s = avgHighSplit(hostAgg.gpu);
    push(systemIdx, s.value, "GPU", undefined, undefined, s.sub, s.unit);
  }
  // (#2413 M4) CPU/RAM/GPU used to silently vanish here whenever the
  // machine-scoped join below found nothing for this run's window — no
  // tile, no explanation, indistinguishable from "the pane doesn't cover
  // host stats". `hasModelWork` gates it the same as COMPACTIONS above: a
  // Tier-1-only run genuinely has nothing to sample, so no explicit tile
  // there either — this is specifically for a model-work run whose join
  // came up empty (historical pre-#2413 data with no machine_uid, or a
  // machine-scoped sampler that simply never ran during this window).
  // Wording matches the machine drawer's own "no host samples" tile
  // (`machineStatsContent.tsx`) rather than inventing a second phrase for
  // the same fact.
  if (hasModelWork && cpuPeak == null && ramPeak == null && gpuPeak == null) {
    push(systemIdx, "—", "HOST", undefined, undefined, "no host samples for this run");
  }

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
  // carries the resolved model (`FlowRecord.model`), which is ground truth
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
  const bare = (m: unknown) => String(m ?? "").replace(/^darkmux:/, "");
  const isRan = (r: FlowRecord) =>
    primaryModel != null && bare((r.fields as Record<string, unknown>).model) === bare(primaryModel);
  const orderedLoads = [...loads].sort((a, b) => Number(isRan(b)) - Number(isRan(a)));
  const modelEntries =
    !ep && orderedLoads.length
      ? orderedLoads.map((r) => {
          const f = r.fields as Record<string, unknown>;
          return {
            name: String(f.model ?? "?"),
            gb: typeof f.gb === "number" ? f.gb : null,
            ran: primaryModel == null ? null : isRan(r),
          };
        })
      : undefined;
  const modelTrackLines = ep
    ? [model || "unknown"]
    : orderedLoads.length
      ? orderedLoads.map((r) => {
          const f = r.fields as Record<string, unknown>;
          const tag = primaryModel == null ? "" : isRan(r) ? " · primary" : " · also loaded";
          return `${f.model} · ${f.gb ?? "?"}GB${tag}`;
        })
      : rollup && rollup.loadLines.length
        ? rollup.loadLines
        : ["no telemetry yet"];

  // ── signals ────────────────────────────────────────────────────────
  //
  // (#1973) Was "detections", rendered as one flat list of grey strings with
  // a `⚠` in front of every entry and no times at all.
  //
  // Two things were wrong with that beyond the styling. First, the emitter
  // has ALWAYS sent a severity — `dispatch_internal`'s detector payload is
  // `{kind, severity, detail}` with `warn` for cycle / reasoning-loop /
  // tool-failure and `info` for `intra-turn-stall`, which is a RECOVERY, not
  // a problem. The viewer read `kind` and `detail` and dropped `severity`, so
  // a successful recovery rendered identically to a doom loop. Second, with
  // no timestamps a cycle detected in the first ten seconds looked exactly
  // like one detected an hour in, which is most of what tells you whether a
  // run was struggling from the start or drifted late.
  const finds: Signal[] = [];
  if (skewedClose) {
    // (#1988) The page reconstructed this run's outcome from a terminal
    // record that precedes its own start. That is honored rather than hidden
    // — a finished dispatch must not read RUNNING forever — but it is NOT
    // presented as if the timeline were sound. Saying so is the difference
    // between a repaired reading and a quietly wrong one, and clock skew is
    // itself worth an operator's attention on a fleet.
    finds.push({
      kind: "clock-skew",
      severity: "warn",
      detail:
        "this run's terminal record is timestamped BEFORE its own start — the outcome is read from it anyway, but elapsed time and signal offsets on this page are unreliable.",
      fix: "check the clocks on the machines that produced these records.",
      atMs: null,
      offsetLabel: "",
    });
  }
  // (#1934) `distinct.length > 1` used to fire on ANY two model ids seen
  // loaded in one run — which a correct `deep`/`balanced` profile trips BY
  // CONSTRUCTION (primary + compactor, sometimes + the internal utility
  // model too), and which also counted a merely-resident leftover from an
  // earlier session that this run never touched at all. Both are staffing
  // or ambient noise, not a swap.
  //
  // The producer now tags each `telemetry.lms` load/unload with `role`
  // (`"primary"` / `"compactor"` / `"utility"` / `"resident"` — see
  // `role_for_load` in `telemetry_sampler.rs`) and marks a load `baseline`
  // when it is the sampler's FIRST tick emitting whatever was already
  // resident before this attempt did anything (never itself a swap — it is
  // the starting lineup, not an event). A genuine swap only exists in what
  // happens to a SPECIALIST seat (never `compactor`/`utility` — those are
  // declared staffing, doing exactly their job) AFTER that starting point:
  // a new specialist model going resident mid-run, or any specialist model
  // being unloaded at all (an eviction the run must reload from, whether or
  // not a replacement load has landed yet).
  //
  // UNTAGGED RECORD SETS ARE NOT JUDGED. Records written before this fix
  // shipped (and hand-authored fixtures, and the committed demo corpus)
  // carry neither field. The first revision of this fix claimed those "fall
  // back to the pre-#1934 reading"; they did not. `isUtilitySeat(undefined)`
  // is false and `isBaselineLoad` requires a `role`, so on untagged data
  // EVERY load and unload was admitted and the gate went from a count
  // (`n > 1`) to a presence (`n > 0`) — measured on this repo's own public
  // demo corpus, firings went from 8 of 11 sessions to 11 of 11, and three
  // of the new ones rendered "X was unloaded mid-run" for a record set
  // containing no unload at all. That is a false factual claim in the
  // signals pane, not noise.
  //
  // So: the seat reading applies only when EVERY lms record in the set
  // carries a `role`. Otherwise the detector declines, and says so — an
  // `info` naming what it cannot tell apart, emitted exactly where the old
  // count-based rule would have raised a `warn`, so a historical run is
  // neither silently blind nor crying wolf.
  const lmsFields = lms.map((r) => (r.fields ?? {}) as Record<string, unknown>);
  const anyUntaggedLms = lmsFields.some((f) => typeof f.role !== "string");
  const isUtilitySeat = (role: unknown) => role === "compactor" || role === "utility";
  const isBaselineLoad = (f: Record<string, unknown>) => f.event === "load" && f.role != null && f.baseline === true;
  if (lms.length > 0 && anyUntaggedLms) {
    if (distinct.length > 1) {
      finds.push({
        kind: "model-track-unclassified",
        // `info`, not `warn`: nothing here is known to have gone wrong. The
        // run may have swapped a model mid-flight or may have staffed a
        // compactor exactly as its profile declares, and these records
        // cannot tell those apart.
        severity: "info",
        detail: `${distinct.length} models loaded in one run (${distinct.join(" → ")}), but these records carry no seat tag — a real mid-run swap and a correct primary+compactor staffing look identical here, so this run is not judged either way.`,
        fix: "runs recorded at flow schema 1.45.0 or later tag each load with its seat; the swap reading returns for those.",
        atMs: null,
        offsetLabel: "",
      });
    }
  } else if (lms.length > 0) {
    // The seat reading, per the rule stated above. TWO KNOWN NARROWINGS in
    // it, both deliberate, neither hidden:
    //
    // 1. The `isUtilitySeat` exclusion returns before the unload branch, so
    //    a COMPACTOR OR UTILITY MODEL BEING EVICTED MID-RUN IS INVISIBLE
    //    HERE. That is right for a utility LOAD (staffing doing its job) and
    //    wrong for a utility UNLOAD — compactor thrash under memory pressure
    //    is real, measurable, and exactly the residency churn darkmux exists
    //    to surface. It is not surfaced anywhere today. Deliberately not
    //    folded into `jit-model-swap`, which is a claim about the
    //    SPECIALIST seat and would be mislabeled carrying this; it wants its
    //    own signal. Tracked as #2565.
    // 2. A `"resident"` model going resident MID-RUN fires. That includes a
    //    model the operator loaded from their own unrelated LMStudio use,
    //    which per #1274 is user state this run never touched. It cannot be
    //    excluded: a genuine swap-in of a second specialist ALSO tags
    //    `"resident"` (the dispatch declared no seat for it), so excluding
    //    the class would blind the detector to the only case it exists for.
    //    The leftover-resident half of #1934 is fixed at the SEED tick,
    //    where `baseline` distinguishes them; after it, the record carries
    //    no information that separates the two.
    const specialistLoads = loads.filter((r) => !isUtilitySeat((r.fields as Record<string, unknown>).role));
    const specialistModels = [...new Set(specialistLoads.map((r) => (r.fields as Record<string, unknown>).model as string))];
    const swapEvents = lms.filter((r) => {
      const f = r.fields as Record<string, unknown> | undefined;
      if (!f || isUtilitySeat(f.role)) return false;
      if (f.event === "unload") return true;
      return f.event === "load" && !isBaselineLoad(f);
    });
    // The detail line must describe what the RECORDS say happened. The
    // first revision had two branches keyed on the model COUNT, so a set
    // with a single specialist model and no unload at all still rendered
    // "X was unloaded mid-run" — stating an event that never happened.
    // Three branches, each keyed on the thing it claims:
    const unloadedModels = [
      ...new Set(
        swapEvents
          .filter((r) => (r.fields as Record<string, unknown>).event === "unload")
          .map((r) => (r.fields as Record<string, unknown>).model as string),
      ),
    ];
    const midRunLoadedModels = [
      ...new Set(
        swapEvents
          .filter((r) => (r.fields as Record<string, unknown>).event === "load")
          .map((r) => (r.fields as Record<string, unknown>).model as string),
      ),
    ];
    // The GATE is still "did anything happen to a specialist seat after the
    // starting point" — `swapEvents`. `specialistModels` only shapes the
    // WORDING; on its own it counts the baseline lineup, which is precisely
    // the staffing this fix exists to stop firing on.
    let detail: string | null = null;
    if (swapEvents.length === 0) {
      detail = null;
    } else if (specialistModels.length > 1) {
      detail = `${specialistModels.length} models loaded in one run (${specialistModels.join(" → ")}) — mid-run swap stalls the dispatch while the new model loads.`;
    } else if (unloadedModels.length > 0) {
      detail = `${unloadedModels.join(", ")} was unloaded mid-run — the seat's reload stalls the dispatch while the model loads.`;
    } else if (midRunLoadedModels.length > 0) {
      detail = `${midRunLoadedModels.join(", ")} loaded mid-run rather than before it — the dispatch stalls while the model loads.`;
    }
    if (detail) {
      finds.push({
        kind: "jit-model-swap",
        // Synthesized from the load track rather than emitted by a detector, so
        // it has no record of its own and therefore no timestamp — `null` says
        // so, instead of borrowing one and implying a moment it did not have.
        severity: "warn",
        detail,
        fix: "pin one model for the run, or pre-warm the swap target.",
        atMs: null,
        offsetLabel: "",
      });
    }
  }
  const runStartMs = d?.ts ? T(d.ts) : null;
  // (#2887 F2) The run-level policy the dispatch actually ran under, read
  // from `dispatch.start`'s own `payload.bounds.detection_degeneracy_
  // policy.value` — the SAME resolved value the host stamps into the
  // container's env for this run, so it is the one true "what was this run
  // configured to do" answer, independent of whether any individual
  // gate/checkpoint record happens to carry its own `policy` field (an
  // older runtime image may not). `null` when unknown (no dispatch.start in
  // the window, or a record predating this field) — unknown is never
  // treated as "off".
  const runDegeneracyPolicy = (() => {
    const sf = d?.fields as Record<string, unknown> | undefined;
    const bounds = sf?.bounds as Record<string, unknown> | undefined;
    const block = bounds?.detection_degeneracy_policy as Record<string, unknown> | undefined;
    return typeof block?.value === "string" ? block.value : null;
  })();
  const repetitionOff = runDegeneracyPolicy === "off";
  // (#2887 N2) A run recorded before FLOW_SCHEMA_VERSION 1.56.0 has no way
  // to tell "the gate genuinely never flagged anything" from "the forwarder
  // that reports flags didn't exist yet for this run" — the degeneracy
  // gate's own findings only started reaching the flow stream at 1.56.0
  // (see `schema.rs`'s own history entry). `dispatch.start`'s
  // `payload.flow_schema` (the SAME `FLOW_SCHEMA_VERSION` constant the host
  // stamped this run's records against) is the one place that can say
  // which case applies. Absent entirely on any run older than this field
  // itself, which reads the same as "too old" — both must render as
  // unmeasured, never as a checked-and-clean tick.
  const runFlowSchema = (() => {
    const sf = d?.fields as Record<string, unknown> | undefined;
    return typeof sf?.flow_schema === "string" ? sf.flow_schema : null;
  })();
  const repetitionRecorded = flowSchemaAtLeast(runFlowSchema, "1.56.0");

  for (const r of dets) {
    const f = r.fields as Record<string, unknown>;
    // (#2887 F4) `repetition`-kind detector records are handled below,
    // grouped by turn — NOT pushed one-per-record here. Under enforce a
    // single cut produces a degenerate observation AND an abort AND
    // (usually) a concluding checkpoint for the SAME turn; pushed through
    // this generic per-record loop that reads as three-to-six findings for
    // one operator-visible event.
    if (f.kind === "repetition") continue;
    const atMs = r.ts ? T(r.ts) : null;
    finds.push({
      // (#1989) `String(f.kind)` turned a missing field into the literal
      // string `undefined`, rendered verbatim as a group heading — an
      // operator scanning SIGNALS reads that as a finding named "undefined".
      // `unknown-signal` says what actually happened instead, and matches the
      // discipline the severity line below already had: a malformed payload
      // must stay VISIBLE and be named honestly, never silently mangled.
      kind: typeof f.kind === "string" && f.kind ? f.kind : "unknown-signal",
      // Unknown severities degrade to `warn`, never to `info`: a signal this
      // build does not recognize is more likely to matter than not, and
      // quietly downgrading it is how a new detector ships invisible.
      severity: f.severity === "info" ? "info" : "warn",
      // (#1989) A non-string `detail` used to stringify to `[object Object]`,
      // destroying real diagnostic content rather than formatting it oddly.
      // Serializing keeps the data where a human can read it — the operator
      // can act on a JSON blob and cannot act on `[object Object]`.
      detail: signalDetail(f.detail),
      atMs,
      offsetLabel: atMs != null && runStartMs != null ? runOffset(atMs - runStartMs) : "",
    });
  }

  // (#2887 F4) One flagged TURN, not one raw record. Under enforce the
  // runtime writes a degenerate `dispatch.gate.observation`, a
  // `dispatch.gate.abort` for the SAME moment, and a concluding
  // `dispatch.checkpoint` for the same turn — three (or, across a turn's
  // several continuations, more) records for what the operator experiences
  // as ONE cut. Every one of those record kinds names the turn it belongs
  // to (`turn_seq`, forwarded from the runtime's own `seq`), so they
  // collapse here into a single Signal per distinct turn.
  //
  // `dispatch.checkpoint` is NOT a detector telemetry record (`category=
  // work`, `source` unset — it rides `self.emit`, not `self.emit_
  // telemetry`), so it never reached `dets`/`tel` above; read it straight
  // off `visible` instead, scoped to this session's attempt window the same
  // way every other region here is.
  const checkpoints = visible.filter((r) => inAttempt(r) && r.action === "dispatch.checkpoint");

  // (#2887 N3) `turn_seq` alone is not a safe key. A dispatch session id is
  // TASK-scoped (`darkmux_types::session_id::task` — see this project's own
  // "task-scoped session id trap" note): sibling seats fanned out within one
  // task can share ONE session_id, so two concurrent seats can each be on
  // their own "turn 2" at the same time. What DOES individually attribute a
  // record even when its session id is a shared grouping key is the pair
  // `dispatch.internal`'s own doc names for exactly this reason:
  // `payload.step_id` (present only inside a mission graph step — absent
  // for a standalone `darkmux dispatch`) and `handle` (the role). Combined
  // with `turn_seq` this is the merge key below.
  const seatKeyFor = (r: FlowRecord, f: Record<string, unknown>): string =>
    `${r.handle ?? ""}::${typeof f.step_id === "string" ? f.step_id : ""}`;

  type TurnFlag = {
    turnSeq: number | string;
    acted: boolean;
    // (#2887 N4) How many DISTINCT calls the gate itself ended for this
    // turn — counted off `dispatch.gate.abort`-sourced records specifically
    // (identified by `generated_chars`, a field only an abort ever
    // populates — see the loop below), never off the DEGENERATE
    // OBSERVATION that names the SAME cut, which would double the count.
    gateAbortCount: number;
    // Whether a gate-sourced record (observation or abort) contributed at
    // all, vs. the flag coming ONLY from the checkpoint's own post-hoc
    // judge — the two are independent detectors (#2836 the in-stream gate,
    // #1221 the reasoning check-in) that usually but not always co-occur:
    // the checkpoint's judge can conclude a turn the stream gate's
    // per-observation-boundary sampling never crossed.
    sawGate: boolean;
    policy: string | null;
    atMs: number | null;
    ratio: string | null;
  };
  const byTurn = new Map<string, TurnFlag>();
  const mergeTurn = (
    turnSeqRaw: unknown,
    seatKey: string,
    acted: boolean,
    isGateAbort: boolean,
    isGateSourced: boolean,
    policy: string | null,
    atMs: number | null,
    ratio: string | null,
  ) => {
    const turnSeq = typeof turnSeqRaw === "number" ? turnSeqRaw : "?";
    // (#2887 N3) A record with no numeric `turn_seq` must never collapse
    // with ANOTHER such record just because both read "?" — a fresh
    // per-record suffix keeps every unknown-turn record its own group
    // rather than silently merging unrelated findings.
    const key =
      turnSeq === "?" ? `${seatKey}::?::${byTurn.size}` : `${seatKey}::${turnSeq}`;
    const existing = byTurn.get(key);
    if (!existing) {
      byTurn.set(key, {
        turnSeq,
        acted,
        gateAbortCount: isGateAbort && acted ? 1 : 0,
        sawGate: isGateSourced,
        policy,
        atMs,
        ratio,
      });
      return;
    }
    if (acted) existing.acted = true;
    if (isGateAbort && acted) existing.gateAbortCount += 1;
    if (isGateSourced) existing.sawGate = true;
    if (policy && !existing.policy) existing.policy = policy;
    if (atMs != null && (existing.atMs == null || atMs < existing.atMs)) existing.atMs = atMs;
    if (ratio && !existing.ratio) existing.ratio = ratio;
  };

  // (#2887 F2) `off` means the gate never ran — there is nothing to flag,
  // and any stray repetition-shaped record in the window (a policy change
  // mid-investigation, a malformed fixture) must not manufacture a finding
  // for a detector this run's own bounds say was not measuring anything.
  if (!repetitionOff) {
    for (const r of dets) {
      const f = r.fields as Record<string, unknown>;
      if (f.kind !== "repetition") continue;
      const atMs = r.ts ? T(r.ts) : null;
      const ratio = typeof f.tail_ratio === "number" ? f.tail_ratio.toFixed(3) : null;
      // Only `dispatch.gate.abort` ever populates `generated_chars` (the
      // runtime's own trajectory shape — `append_gate_observation` never
      // writes it); a degenerate OBSERVATION for the SAME cut carries
      // `acted:true` too, and must not be double-counted as a second abort.
      const isGateAbort = f.generated_chars != null;
      mergeTurn(
        f.turn_seq,
        seatKeyFor(r, f),
        f.acted === true,
        isGateAbort,
        true,
        typeof f.policy === "string" ? f.policy : null,
        atMs,
        ratio,
      );
    }
    // A checkpoint counts as a flag when `would_conclude` is `true` (the
    // judge found the turn repetitive under a runtime that measures) OR
    // `verdict === "conclude"` (F1: a HISTORICAL checkpoint from before
    // #2846 shipped `would_conclude` at all carries only `verdict` — a
    // conclude with no `would_conclude` key must still flag, or every
    // pre-#2846 enforced conclusion on record reads CLEAN).
    for (const r of checkpoints) {
      const f = r.fields as Record<string, unknown>;
      const acted = f.verdict === "conclude";
      const flagged = f.would_conclude === true || acted;
      if (!flagged) continue;
      const atMs = r.ts ? T(r.ts) : null;
      const ratio = typeof f.tail_ratio === "number" ? f.tail_ratio.toFixed(3) : null;
      mergeTurn(
        f.turn_seq,
        seatKeyFor(r, f),
        acted,
        false,
        false,
        typeof f.policy === "string" ? f.policy : null,
        atMs,
        ratio,
      );
    }
  }

  for (const acc of byTurn.values()) {
    // (#2887 F2) Prefer the RUN-LEVEL policy for the observed/enforced
    // wording — it is the one resolved value every record in this run
    // shares, where an individual record's own `policy` field may be
    // absent (an older runtime image) or, in principle, stale.
    const effectivePolicy = runDegeneracyPolicy ?? acc.policy;
    const ratioClause = acc.ratio ? ` (tail_ratio=${acc.ratio})` : "";
    // (#2887 N4) Cite the detector that actually produced this finding:
    // #2836 (the in-stream degeneracy gate) whenever a gate-sourced record
    // contributed, #1221 (the reasoning check-in) for a checkpoint-only
    // flag the stream gate never saw.
    const citation = acc.sawGate ? "#2836" : "#1221";
    const timesClause = acc.gateAbortCount > 1 ? ` ${acc.gateAbortCount}×` : "";
    const detail = acc.acted
      ? `turn ${acc.turnSeq}: judged repeating${ratioClause} and ended it${timesClause} (${citation})`
      : effectivePolicy === "observe"
        ? `turn ${acc.turnSeq}: judged repeating${ratioClause} — flagged (observed), not enforced (#2846)`
        : `turn ${acc.turnSeq}: judged repeating${ratioClause} (${citation})`;
    finds.push({
      kind: "repetition",
      severity: "warn",
      detail,
      atMs: acc.atMs,
      offsetLabel: acc.atMs != null && runStartMs != null ? runOffset(acc.atMs - runStartMs) : "",
    });
  }

  // Severity first, then most recent first inside each severity. A run with
  // twenty signals is read top-down for "what went wrong", and a recovery
  // never outranks a struggle.
  const SEV_RANK: Record<SignalSeverity, number> = { warn: 0, info: 1 };
  finds.sort((a, b) => SEV_RANK[a.severity] - SEV_RANK[b.severity] || (b.atMs ?? 0) - (a.atMs ?? 0));

  // Grouped by kind so eleven cycle detections are one row that says 11,
  // not eleven rows that bury everything else.
  const groupOrder: string[] = [];
  const byKind = new Map<string, Signal[]>();
  for (const f of finds) {
    if (!byKind.has(f.kind)) {
      byKind.set(f.kind, []);
      groupOrder.push(f.kind);
    }
    byKind.get(f.kind)!.push(f);
  }
  const signalGroups: SignalGroup[] = groupOrder.map((kind) => {
    const signals = byKind.get(kind)!;
    return {
      kind,
      // A group takes the highest severity it contains — a kind that fired
      // once as a recovery and once as a struggle is a struggle.
      severity: signals.some((x) => x.severity === "warn") ? "warn" : "info",
      count: signals.length,
      signals,
    };
  });
  const signalsLabel = finds.length ? `signals (${finds.length})` : "signals";



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
      pillLabel: svLabel.toUpperCase(),
      pillCls: pillClsFor(svLabel),
      role,
      sid,
      machineName: String(d?.machine_id || firstSessRec?.machine_id || ""),
    },
    briefLines,
    disclosures,
    metrics,
    metricScope,
    liveTokScope:
      effHasModelWork && !done
        ? {
            tokensPerSec: aggregateTokenRate(tokRateRecordSets, nowMs),
            stalled: tokRateStalled,
            // null: no live execution right now (a mission between model
            // steps). Every lamp is off; nothing claims a state.
            state: tokRateLiveState?.state ?? null,
            restSecondsLeft: tokRateLiveState?.restSecondsLeft,
          }
        : null,
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
    lastBeatMs,
    signalsLabel,
    signalGroups,
    repetitionOff,
    repetitionRecorded,
  };
}
