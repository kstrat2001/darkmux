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
   * The split is not cosmetic. It is what lets a step that ran no model
   * render without holes: a `procedural.shell` step has harness metrics and
   * NO model metrics, so the model group is absent rather than showing
   * `0 turns · 0 tokens`, which would be a lie shaped like data. It also
   * answers the operator question that prompted this — "what does
   * `model (lms)` mean, and are these numbers about the model or about
   * darkmux?" */
  metricScope: { model: number[]; harness: number[] };
  modelTrackLabel: string;
  modelTrackLines: string[];
  detectionsLabel: string;
  detectionLines: string[];
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
export function runRegions(data: FlowRecord[], sid: string): SessionRunView {
  const nowMs = computeTMax(data);

  const sidStarts = data
    .filter((r) => r.session_id === sid && r.action === "dispatch.start" && T(r.ts) <= nowMs)
    .sort((a, b) => T(a.ts) - T(b.ts));
  const d = sidStarts.length ? sidStarts[sidStarts.length - 1] : null;
  const firstSessRec = data.find((r) => r.session_id === sid) ?? null;
  const startTs = d ? T(d.ts) : firstSessRec ? T(firstSessRec.ts) : nowMs;
  const inAttempt = (r: FlowRecord) => r.session_id === sid && T(r.ts) >= startTs;

  const attemptCloses = data
    .filter((r) => inAttempt(r) && (r.action === "dispatch.complete" || r.action === "dispatch.error" || r.action === "session.end"))
    .sort((a, b) => T(a.ts) - T(b.ts));
  const close = attemptCloses[0] ?? null;
  const c = attemptCloses.find((r) => r.action !== "session.end") ?? null;
  const done = !!close && T(close.ts) <= nowMs;

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

  const metrics = [
    { value: turnsValue != null ? String(turnsValue) : "—", label: "TURNS" },
    { value: tokIn != null ? fmtC(tokIn) : "—", label: "TOKENS IN" },
    { value: tokOut != null ? fmtC(tokOut) : "—", label: "TOKENS OUT" },
    { value: nctx ? `${(ctxHeadline / 1000).toFixed(0)}K` : "—", label: ctxLabel },
    { value: String(comps.length), label: "COMPACTIONS" },
    { value: wall, label: "WALL CLOCK" },
  ];

  // (#1973) Indices into `metrics`, not a second copy — one ordered list, one
  // grouping over it, so the two cannot drift apart. TURNS/TOKENS/CTX/
  // COMPACTIONS are the model's work; WALL CLOCK is the harness's. Compaction
  // is a UTILITY role's sub-execution rather than the specialist's own work,
  // but it is counted here because what the operator is reading is "what
  // happened to this model's context", which is exactly what a compaction did.
  const metricScope = { model: [0, 1, 2, 3, 4], harness: [5] };

  // ── model track ────────────────────────────────────────────────────
  const modelTrackLabel = ep ? "model (remote)" : "model (lms)";
  const modelTrackLines = ep
    ? [`${model || "unknown"} · served off-fleet — no local model (see route above)`]
    : loads.length
      ? loads.map((r) => {
          const f = r.fields as Record<string, unknown>;
          return `${f.model} · ${f.gb ?? "?"}GB`;
        })
      : ["no telemetry yet"];

  // ── detections ─────────────────────────────────────────────────────
  interface Finding {
    k: string;
    d: string;
    f?: string;
  }
  const finds: Finding[] = [];
  if (distinct.length > 1) {
    finds.push({
      k: "jit-model-swap",
      d: `${distinct.length} models loaded in one run (${distinct.join(" → ")}) — mid-run swap stalls the dispatch while the new model loads.`,
      f: "pin one model for the run, or pre-warm the swap target.",
    });
  }
  for (const r of dets) {
    const f = r.fields as Record<string, unknown>;
    finds.push({ k: String(f.kind), d: String(f.detail) });
  }
  const detectionsLabel = finds.length ? `detections (${finds.length})` : "detections";
  const detectionLines = finds.length
    ? finds.flatMap((f) => [`⚠ ${f.k}`, f.d, ...(f.f ? [`fix: ${f.f}`] : [])])
    : ["✓ clean", "no behavioral flags (cycle, tool-failure, reasoning-loop, edit-drift)"];

  // procs (host CPU/RAM/GPU) contributes only to the two skipped SVG
  // charts — referenced here so the variable isn't flagged unused, and so
  // a future packet porting those charts has an obvious hook.
  void procs;

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
    detectionsLabel,
    detectionLines,
  };
}
