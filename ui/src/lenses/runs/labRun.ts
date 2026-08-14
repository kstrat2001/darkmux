/**
 * Pure logic for the lab-run detail view ("run detail" for `kind=lab` rows)
 * — a TypeScript port of `viewer.html`'s `computeLabPipeline`/
 * `labStageMeta`/`labStageRow`/`renderLabPipeline`/`labShortId`/
 * `labFeedTs`/`labFeedRow`/`renderLabFeed`/`labCliHint`/`labBadge`
 * (viewer.html:4210-4847). This is a NEW render surface (`RunsBoard`'s row
 * click for `kind==="lab"` previously showed a `NOT_PORTED_NOTICE` — see
 * `LabRunDetail.tsx`'s own doc), so there is no existing parity golden to
 * hit byte-for-byte; these functions still track legacy's real derivation
 * logic line-for-line (not a redesign) per the drill-in packet's brief.
 *
 * Output shape follows the same convention `lenses/machine/memoryLedgerLines.ts`
 * established: flat line arrays, one visible text
 * unit per array element, because legacy's own `.labstage`/`.labfeedrow`/
 * `.labcli` are all `display:flex` — each flex child becomes its own
 * `innerText` line, so a literal array element per child reproduces that
 * without depending on a stylesheet this port is free to change.
 */

import { shortModel } from "../lab/labSeries";
import type { LabFunnelEnvelope, LabRunEvent, LabScoresDoc } from "../../types/handwritten";

/** `computeLabPipeline()` — viewer.html:4756-4774. Folds the event feed
 * into per-`step_id` completion payloads (in first-seen order) plus a
 * provisional judge/verify ruling tally, read only when no terminal
 * envelope (`env`) has landed yet. */
export interface LabPipeline {
  steps: Record<string, Record<string, unknown>>;
  order: string[];
  rulingTally: { 1: Record<string, number>; 2: Record<string, number> };
}

export function computeLabPipeline(events: LabRunEvent[]): LabPipeline {
  const steps: Record<string, Record<string, unknown>> = {};
  const order: string[] = [];
  const rulingTally: { 1: Record<string, number>; 2: Record<string, number> } = { 1: {}, 2: {} };

  for (const r of events) {
    if (r.action !== "step result" || !r.payload) continue;
    const p = r.payload;
    if (p.step_id === "review-ruling") {
      const pass = p.pass as 1 | 2 | undefined;
      const rul = p.ruling as string | undefined;
      if ((pass === 1 || pass === 2) && rul) {
        rulingTally[pass][rul] = (rulingTally[pass][rul] || 0) + 1;
      }
      continue;
    }
    const stepId = p.step_id as string | undefined;
    if (stepId) {
      if (!(stepId in steps)) order.push(stepId);
      steps[stepId] = p;
    }
  }

  return { steps, order, rulingTally };
}

/** `labStageMeta()` — viewer.html:4775-4783. */
export function labStageMeta(payload: Record<string, unknown> | null | undefined): string {
  if (!payload) return "not started";
  const bits: string[] = [];
  const drawsTotal = payload.draws_total;
  const itemsIn = payload.items_in;
  const itemsOut = payload.items_out;
  const model = payload.model as string | undefined;
  if (drawsTotal != null) {
    const drawsDone = (payload.draws_done as number) || 0;
    bits.push(`${drawsDone}/${drawsTotal} draws${model ? ` · ${shortModel(model)}` : ""}`);
  } else if (itemsIn != null || itemsOut != null) {
    bits.push(`${itemsIn ?? "—"} → ${itemsOut ?? "—"}`);
  } else if (model) {
    bits.push(shortModel(model));
  }
  if (payload.wall_ms != null) bits.push(`${payload.wall_ms}ms`);
  return bits.length ? bits.join(" · ") : "done";
}

function tallyStr(t: Record<string, number>): string {
  const entries = Object.entries(t).map(([k, v]) => `${k}:${v}`);
  return entries.length ? entries.join(" ") : "—";
}

/** `renderLabPipeline()` — viewer.html:4791-4804, reduced to lines: two per
 * stage (name, meta), in ARRIVAL order (the step_ids come straight from the
 * review graph / sequential driver, so this needs no hardcoded stage list),
 * plus a trailing synthesis stage. */
export function labPipelineLines(pipe: LabPipeline, env: LabFunnelEnvelope | null): string[] {
  const lines: string[] = [];
  const order = pipe.order.length ? pipe.order : ["pipeline"];
  for (const id of order) {
    const payload = pipe.order.length ? pipe.steps[id] : null;
    lines.push(id, labStageMeta(payload));
  }
  const synthMeta = env
    ? `confirmed ${env.confirmed} · needs_check ${env.needs_check} · archived ${env.archived}`
    : `(provisional, from rulings so far) pass1 ${tallyStr(pipe.rulingTally[1])} · pass2 ${tallyStr(pipe.rulingTally[2])}`;
  lines.push("synthesis", synthMeta);
  return lines;
}

/** `labShortId()` — viewer.html:4805. */
export function labShortId(id: unknown): string {
  const s = String(id ?? "");
  return s.length > 18 ? `${s.slice(0, 18)}…` : s;
}

/** `labFeedTs()` — viewer.html:4806. */
export function labFeedTs(ts: unknown): string {
  return String(ts ?? "").replace("T", " ").replace("Z", "");
}

/** `labFeedRow()` — viewer.html:4807-4825, reduced to its three visible
 * lines (ts, tag, text — `.labfeedrow` is `display:flex`, each span is its
 * own `innerText` line, same convention as `labPipelineLines` above). */
export function labFeedRowLines(r: LabRunEvent): string[] {
  const tt = labFeedTs(r.ts);
  const f = r.payload || {};

  if (r.category === "telemetry" && r.source === "process") {
    const cpu = f.cpu ?? "–";
    const mem = f.mem ?? "–";
    const gpu = f.gpu ?? "–";
    return [tt, "host", `cpu ${cpu}% · mem ${mem}% · gpu ${gpu}%`];
  }

  if (r.action === "step result") {
    if (f.step_id === "review-ruling") {
      const stage = f.stage ? String(f.stage) : "ruling";
      const passLbl = f.pass != null ? ` pass${f.pass}` : "";
      const seconds = f.seconds != null ? ` (${Number(f.seconds).toFixed(1)}s)` : "";
      return [tt, stage, `${labShortId(f.bundle_id)}${passLbl} → ${f.ruling}${seconds}`];
    }
    return [tt, String(f.step_id || "step"), labStageMeta(f)];
  }

  return [tt, "", String(r.action || "")];
}

/** The cap is NAMED so the header can disclose it — `LAB_FEED_CAP`,
 * viewer.html:4833. */
export const LAB_FEED_CAP = 500;

/** `renderLabFeed()` — viewer.html:4834-4838. Newest at top (issue #1247's
 * "watch it think" narrative); flattens `labFeedRowLines` per surviving
 * event into one array (matching the div-per-line convention this module
 * uses throughout). */
export function labFeedLines(events: LabRunEvent[]): string[] {
  if (!events.length) return [];
  return events
    .slice()
    .reverse()
    .slice(0, LAB_FEED_CAP)
    .flatMap((r) => labFeedRowLines(r));
}

/** The event-feed header's count disclosure (#1640, viewer.html:4866):
 * `labFeedLines` above only ever returns the newest `LAB_FEED_CAP` events,
 * but the header must print the RAW total truncation happened against — a
 * 900-event run says "newest 500 of 900", not "900 records" above a list
 * holding 500. A truncation presented as a bare total is the one thing
 * CLAUDE.md's no-silent-caps rule forbids; every other cap in this codebase
 * (RUNS_CAP, RECENT_CAP, CATALOG_MISSION_CAP, the unfiltered-log 50)
 * already discloses this way. `totalEvents` is the FULL accumulated event
 * count (`events.length` in the caller), not the capped feed-line count. */
export function labFeedCountText(totalEvents: number): string {
  if (totalEvents > LAB_FEED_CAP) return `newest ${LAB_FEED_CAP} of ${totalEvents}`;
  return `${totalEvents} record${totalEvents === 1 ? "" : "s"}`;
}

/** The event-feed header's live/playback/unreachable suffix. `unreachable`
 * (the events-poll's consecutive-failure signal — see `LabRunDetail.tsx`'s
 * own doc + `LAB_POLL_FAILURE_THRESHOLD`) only overrides the live case:
 * once `isFinished`, the poll has stopped for a legitimate reason (a
 * drained playback), so there is nothing left to name as "retrying". */
export function labFeedStatusSuffix(isFinished: boolean, unreachable: boolean): string {
  if (isFinished) return " (playback)";
  if (unreachable) return " — daemon unreachable, retrying";
  return " — live, polling";
}

/** `labCliHint()` — viewer.html:4843-4847. `--workdirs`/the cases dir
 * aren't recorded in the artifact (a known gap the legacy comment names),
 * so those two stay explicit placeholders rather than a guess dressed up
 * as fact. */
export function labCliHint(env: LabFunnelEnvelope | null, scores: LabScoresDoc | null): string {
  const crew = env?.crew || scores?.crew || "<crew>";
  const mode = env?.mode || scores?.exec_mode || "auto";
  return `darkmux lab eval --funnel --roster-profile ${crew} --exec-mode ${mode} --workdirs <workdirs-root> <cases-dir>`;
}

/** `labBadge()` — viewer.html:4210-4214, text-only. `unreachable` is new
 * relative to legacy (the events-poll consecutive-failure signal, see
 * `LabRunDetail.tsx`'s own doc) — defaulted so every existing caller/test
 * that only ever passed `finished` keeps its exact prior behavior. Ignored
 * once `finished`, same reasoning as `labFeedStatusSuffix` above. */
export function labBadgeText(finished: boolean, unreachable: boolean = false): string {
  if (!finished && unreachable) return "⚠ daemon unreachable — retrying";
  return finished ? "finished" : "● live";
}
