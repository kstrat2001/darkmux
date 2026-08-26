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
 * `tMax` would NOT reproduce "1071:54 so far".
 */

import { T, dispatchErrored, dispatchKilled, statusLabel, computeTMax } from "../../lib/flow";
import { fmtDuration, clk, fmtC } from "../../lib/format";
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
  metrics: Array<{ value: string; label: string }>;
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
  modelTrackLabel: string;
  modelTrackLines: string[];
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
  const procs = tel.filter((r) => r.source === "process");
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

  const wallBase = done ? fmtDuration(T(close!.ts) - startTs) : `${fmtDuration(nowMs - startTs)} so far`;
  const exitCode = (c?.payload as DispatchCompletePayload | undefined)?.exit_code;
  const wallLoud =
    done && c && dispatchErrored(c)
      ? ` · ${dispatchKilled(c) ? "killed (timeout)" : `errored${exitCode != null ? ` (exit ${exitCode})` : ""}`}`
      : "";
  const wall = wallBase + wallLoud;

  const role = String(handle || "").replace(/^darkmux\//, "").toUpperCase();
  const svLabel = statusLabel({
    open: !done,
    errored: !!c && dispatchErrored(c),
    killed: !!c && dispatchKilled(c),
    clean: done && !!c && !dispatchErrored(c),
  });

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
  const briefTiming = `${clk(startTs)}${done ? ` → ${clk(T(close!.ts))} (${fmtDuration(T(close!.ts) - startTs)})` : " · running"}`;
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
        const label = kind === "azure" ? "Azure OpenAI" : kind === "openai" ? "OpenAI" : kind || "remote";
        return `${label} · ${rest} · off-fleet`;
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

  // ── metrics ────────────────────────────────────────────────────────
  const ctxHeadline = done ? ctxPeak : ctxNow;
  const ctxLabel = !nctx
    ? "CONTEXT"
    : done
      ? `CTX PEAK ${(ctxPeak / 1000).toFixed(0)}K / ${nctx / 1000}K WINDOW`
      : `CTX NOW · PEAK ${(ctxPeak / 1000).toFixed(0)}K / ${nctx / 1000}K WINDOW`;

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

  // (#1973) Host telemetry — CPU / RAM / GPU — was FETCHED and thrown away:
  // `const procs = ...` followed by `void procs` to silence the unused
  // warning, with a comment parking it for "a future packet". That is the
  // fifth instance of one shape in this lens (tool arguments, session
  // records, the prompt, and now this): the data is in hand and the renderer
  // drops it.
  //
  // PEAK rather than latest, because the question a local-AI operator asks of
  // a finished run is "did this saturate the machine" — a last-sample reading
  // taken after the model stopped answers nothing. `mean` is deliberately not
  // shown: a run that pegged the GPU for ten seconds inside two minutes is a
  // very different fact from one that sat at the mean throughout, and only
  // the peak makes the first visible.
  const peakOf = (key: string): number | null => {
    const vals = procs
      .map((r) => Number((r.fields as Record<string, unknown> | undefined)?.[key]))
      .filter((n) => Number.isFinite(n));
    return vals.length ? Math.max(...vals) : null;
  };
  const cpuPeak = peakOf("cpu");
  const ramPeak = peakOf("mem");
  const gpuPeak = peakOf("gpu");

  // Built as a list with its scope recorded AS EACH TILE IS ADDED, rather
  // than as a fixed array plus hardcoded indices. The indices are now
  // conditional (host tiles only exist when host telemetry does), and an
  // audit already flagged the hardcoded form as a positional contract nothing
  // enforced — this makes the two unable to drift because there is only one.
  const metrics: Array<{ value: string; label: string }> = [];
  const modelIdx: number[] = [];
  const systemIdx: number[] = [];
  const push = (into: number[], value: string, label: string) => {
    into.push(metrics.length);
    metrics.push({ value, label });
  };
  push(modelIdx, turnsValue != null ? String(turnsValue) : "—", "TURNS");
  push(modelIdx, tokIn != null ? fmtC(tokIn) : "—", "TOKENS IN");
  push(modelIdx, tokOut != null ? fmtC(tokOut) : "—", "TOKENS OUT");
  push(modelIdx, nctx ? `${(ctxHeadline / 1000).toFixed(0)}K` : "—", ctxLabel);
  push(systemIdx, wall, "WALL CLOCK");
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
  if (cpuPeak != null) push(systemIdx, `${Math.round(cpuPeak)}%`, "CPU PEAK");
  if (ramPeak != null) push(systemIdx, `${Math.round(ramPeak)}%`, "RAM PEAK");
  if (gpuPeak != null) push(systemIdx, `${Math.round(gpuPeak)}%`, "GPU PEAK");

  // (#1973) Indices into `metrics`, not a second copy — one ordered list, one
  // grouping over it, so the two cannot drift apart. TURNS/TOKENS/CTX/
  // COMPACTIONS are the model's work; WALL CLOCK is the harness's. Compaction
  // is a UTILITY role's sub-execution rather than the specialist's own work,
  // but it is counted here because what the operator is reading is "what
  // happened to this model's context", which is exactly what a compaction did.

  const metricScope = { model: hasModelWork ? modelIdx : [], system: systemIdx };

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
  const modelTrackLabel = ep ? "remote model" : "loaded models";
  const modelTrackLines = ep
    ? [`${model || "unknown"} · served off-fleet — no local model (see route above)`]
    : loads.length
      ? loads.map((r) => {
          const f = r.fields as Record<string, unknown>;
          const isPrimary = primaryModel != null && f.model === primaryModel;
          const tag = primaryModel == null ? "" : isPrimary ? " · primary" : " · also loaded";
          return `${f.model} · ${f.gb ?? "?"}GB${tag}`;
        })
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
  if (distinct.length > 1) {
    finds.push({
      kind: "jit-model-swap",
      // Synthesized from the load track rather than emitted by a detector, so
      // it has no record of its own and therefore no timestamp — `null` says
      // so, instead of borrowing one and implying a moment it did not have.
      severity: "warn",
      detail: `${distinct.length} models loaded in one run (${distinct.join(" → ")}) — mid-run swap stalls the dispatch while the new model loads.`,
      fix: "pin one model for the run, or pre-warm the swap target.",
      atMs: null,
      offsetLabel: "",
    });
  }
  const runStartMs = d?.ts ? T(d.ts) : null;
  for (const r of dets) {
    const f = r.fields as Record<string, unknown>;
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
    modelTrackLabel,
    modelTrackLines,
    hasModelWork,
    live: !done,
    lastBeatMs,
    signalsLabel,
    signalGroups,
  };
}
