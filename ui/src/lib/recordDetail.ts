import type { FlowRecord } from "../types/handwritten";

/**
 * The event-log row's trailing preview (`renderLog()`'s `detail`,
 * viewer.html:2487-2503) — the part that says WHAT a record did, not merely
 * what kind of record it is.
 *
 * The port rendered `time · activity · machine · session` and stopped there,
 * so every tool call in the log read "tool call" with no name, no arguments
 * and no result size. The stream was never the problem: a `dispatch.tool`
 * record carries `tool_name`, the full `args`, `result_chars` and `ok`. For a
 * `report_finding` call those args ARE the finding — file, line, evidence and
 * reasoning — which is why a crawl's findings were invisible in a viewer that
 * was already receiving all of them.
 *
 * Kept as plain string-building rather than JSX so it can be unit-tested
 * directly. Escaping is React's job here; legacy had to `esc()` by hand
 * because it built HTML.
 */

const MAX_VALUE = 60;
const MAX_RAW = 80;

function clip(s: string, max: number): string {
  return s.length > max ? s.slice(0, max) + "…" : s;
}

/**
 * `prettyArgs()` (viewer.html:2454-2467) — flatten a tool call's JSON
 * arguments to `k=v k=v`, each value clipped, so a row shows the search
 * pattern / path / command itself.
 *
 * Non-JSON or non-object args fall back to the clipped raw string, which is
 * what keeps a truncated tool call (the per-call cap can cut one mid
 * arguments, producing `args_chars: 0`) legible rather than blank.
 */
export function prettyArgs(args: unknown): string {
  if (typeof args !== "string") return "";
  let parsed: unknown;
  try {
    parsed = JSON.parse(args);
  } catch {
    return clip(args, MAX_RAW);
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    return Object.entries(parsed as Record<string, unknown>)
      .map(([k, v]) => `${k}=${clip(typeof v === "string" ? v : JSON.stringify(v) ?? "", MAX_VALUE)}`)
      .join(" ");
  }
  return clip(args, MAX_RAW);
}

function firstLine(s: string, max: number): string {
  return clip(s.slice(0, max).replace(/\n/g, " "), max);
}

/** The row's trailing preview, or `""` when this record kind has none.
 * Branch-for-branch the legacy set. */
export function recordDetail(r: FlowRecord): string {
  const f = (r.fields || r.payload) as Record<string, unknown> | undefined;
  const a = r.action || "";

  if (a === "dispatch.reasoning" && typeof f?.reasoning_text === "string") {
    return `"${firstLine(f.reasoning_text, 60)}..."`;
  }
  if (a === "dispatch.tool" && f) {
    // `args` absent on pre-1.16 records — fall back to its size, so an old
    // record degrades to what it can say rather than rendering a bare arrow.
    const args = f.args != null ? prettyArgs(f.args) : `${f.args_chars ?? 0}ch`;
    const failed = f.ok === false ? " ❌" : "";
    return `${String(f.tool_name ?? "")} ${args} → ${f.result_chars ?? 0}ch${failed}`;
  }
  if (a === "dispatch.turn" && f) {
    return `turn ${f.turn_seq ?? 0} (${String(f.finish_reason ?? "")})`;
  }
  // `reasoning` is a tier-decision-only top-level field absent from
  // `FlowRecord`'s typed surface, so it is read defensively rather than added
  // to the type for one branch.
  const reasoning = (r as unknown as { reasoning?: unknown }).reasoning;
  if (a === "tier-decision" && typeof reasoning === "string") {
    return `"${firstLine(reasoning, 60)}..."`;
  }
  if (a === "dispatch.start" || a === "dispatch start") {
    return `start (prompt: ${f?.prompt_chars ?? 0}ch)`;
  }
  return "";
}
