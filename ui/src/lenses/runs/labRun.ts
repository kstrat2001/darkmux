/**
 * Pure logic for the lab-run detail view ("run detail" for `kind=lab` rows)
 * — the pipeline fold (`computeLabPipeline`), its stage lines, and the
 * event-feed lines and badge (see `LabRunDetail.tsx`'s own doc for the
 * surface they feed).
 *
 * Output shape follows the same convention `lenses/machine/memoryLedgerLines.ts`
 * established: flat line arrays, one visible text
 * unit per array element, because legacy's own `.labstage`/`.labfeedrow`
 * are `display:flex` — each flex child becomes its own
 * `innerText` line, so a literal array element per child reproduces that
 * without depending on a stylesheet this port is free to change.
 */

import { RUNNING_WORD } from "../../components/WorkStatus";
import { shortModel } from "../../lib/format";
import type { LabReviewSummary } from "../../types/generated/LabReviewSummary";
import { ACTION, CATEGORY, SOURCE, type NormRecord } from "../../lib/ingest";

/** `computeLabPipeline()`: folds the event feed
 * into per-`step_id` completion payloads (in first-seen order) plus a
 * provisional judge/verify ruling tally, read only when no terminal
 * envelope (`env`) has landed yet. */
export interface LabPipeline {
  steps: Record<string, Record<string, unknown>>;
  order: string[];
  rulingTally: { 1: Record<string, number>; 2: Record<string, number> };
}

export function computeLabPipeline(events: NormRecord[]): LabPipeline {
  const steps: Record<string, Record<string, unknown>> = {};
  const order: string[] = [];
  const rulingTally: { 1: Record<string, number>; 2: Record<string, number> } = { 1: {}, 2: {} };

  for (const r of events) {
    if (r.action !== ACTION.StepResult || !r.payload) continue;
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

/** One stage's meta line. */
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

/** `renderLabPipeline()`, reduced to lines: two per
 * stage (name, meta), in ARRIVAL order (the step_ids come straight from the
 * review graph / sequential driver, so this needs no hardcoded stage list),
 * plus a trailing synthesis stage. */
export function labPipelineLines(pipe: LabPipeline, env: LabReviewSummary | null): string[] {
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

/** A run id shortened for display. */
export function labShortId(id: unknown): string {
  const s = String(id ?? "");
  return s.length > 18 ? `${s.slice(0, 18)}…` : s;
}

/** A feed timestamp, without the ISO `T` and `Z`. */
export function labFeedTs(ts: unknown): string {
  return String(ts ?? "").replace("T", " ").replace("Z", "");
}

/** `labFeedRow()`, reduced to its three visible
 * lines (ts, tag, text — `.labfeedrow` is `display:flex`, each span is its
 * own `innerText` line, same convention as `labPipelineLines` above). */
export function labFeedRowLines(r: NormRecord): string[] {
  const tt = labFeedTs(r.ts);
  const f = r.payload || {};

  if (r.category === CATEGORY.Telemetry && r.source === SOURCE.Host) {
    const cpu = f.cpu ?? "–";
    const mem = f.mem ?? "–";
    const gpu = f.gpu ?? "–";
    return [tt, "host", `cpu ${cpu}% · mem ${mem}% · gpu ${gpu}%`];
  }

  if (r.action === ACTION.StepResult) {
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
 * `LAB_FEED_CAP`. */
export const LAB_FEED_CAP = 500;

/** `renderLabFeed()`: newest at top (issue #1247's
 * "watch it think" narrative); flattens `labFeedRowLines` per surviving
 * event into one array (matching the div-per-line convention this module
 * uses throughout). */
export function labFeedLines(events: NormRecord[]): string[] {
  if (!events.length) return [];
  return events
    .slice()
    .reverse()
    .slice(0, LAB_FEED_CAP)
    .flatMap((r) => labFeedRowLines(r));
}

/** The event-feed header's count disclosure (#1640):
 * `labFeedLines` above only ever returns the newest `LAB_FEED_CAP` events,
 * but the header must print the RAW total truncation happened against — a
 * 900-event run says "newest 500 of 900", not "900 records" above a list
 * holding 500. A truncation presented as a bare total is the one thing
 * CLAUDE.md's no-silent-caps rule forbids; every other cap in this codebase
 * (RUNS_CAP, CATALOG_MISSION_CAP, the unfiltered-log 50)
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

/** `labBadge()`, text-only. `unreachable` is the (events-poll consecutive-failure signal, see
 * `LabRunDetail.tsx`'s own doc) — defaulted so every existing caller/test
 * that only ever passed `finished` keeps its exact prior behavior. Ignored
 * once `finished`, same reasoning as `labFeedStatusSuffix` above. */
export function labBadgeText(finished: boolean, unreachable: boolean = false): string {
  if (!finished && unreachable) return "⚠ daemon unreachable — retrying";
  // A live lab run is a running chip, and a running chip has ONE word (see
  // `RUNNING_WORD`) — the legacy `● live` was the lab series' private spelling.
  return finished ? "finished" : RUNNING_WORD;
}
