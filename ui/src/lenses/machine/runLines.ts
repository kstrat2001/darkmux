/**
 * `recentRow()` (viewer.html:1745-1781) — reduced to just the lines its
 * COLLAPSED `<summary>` exposes to `innerText`. The row's `<details>` boots
 * closed (`state.expanded` starts empty and this lens never opens one in
 * the parity harness's click path), so `.rrdetail`'s expanded fields
 * (workspace, full token breakdown, result text) never reach `innerText` —
 * out of scope for this port for the same reason (see `tests/parity/
 * lib/extract-lens.js`'s module doc: the extractor reads rendered text,
 * closed `<details>` contribute nothing).
 *
 * `<summary>` is `display:flex` in legacy (`.rr>summary`, viewer.html:471) —
 * each flex child (`#ord`, the pill, `rrmain`, `rrmeta`, `rrtime`) becomes
 * its own line under `innerText`, which is why this returns an array of
 * lines rather than one joined string — see `format.ts`'s module doc for
 * why this port reproduces line breaks via literal array elements /
 * separate block elements instead of leaning on CSS flex + `innerText`'s
 * layout-dependent line-break behavior.
 */

import { fmtC, fmtDuration, clk, relAgoFrom } from "../../lib/format";
import type { MachineRunNode } from "../../lib/flow";
import type { DispatchCompletePayload } from "../../types/handwritten";

function metaBitsLine(donePayload: Record<string, unknown> | null): string | null {
  if (!donePayload) return null;
  const dp = donePayload as DispatchCompletePayload;
  const bits: string[] = [];
  if (dp.total_turns != null) bits.push(`${dp.total_turns}t`);
  if (dp.total_tools != null) bits.push(`${dp.total_tools}⚙`);
  if (dp.total_tokens != null) bits.push(`${fmtC(dp.total_tokens)}tok`);
  if (dp.total_compactions != null) bits.push(`${dp.total_compactions}⚡`);
  return bits.length ? bits.join(" ") : null;
}

/** The row's visible lines: `#ord`, the status pill, the main
 * handle/model/mission line, an optional metaBits line, an optional
 * when/duration line. */
export function machineRunLines(n: MachineRunNode, ord: number, tMax: number): string[] {
  const lines: string[] = [`#${ord}`, n.lbl];

  const miss = n.mission ? ` · ${n.mission}` : "";
  lines.push(`${n.handle} · ${n.model}${miss}`);

  const metaBits = metaBitsLine(n.donePayload);
  if (metaBits) lines.push(metaBits);

  if (n.closeTs != null) {
    const rel = relAgoFrom(tMax, n.closeTs);
    const dur = n.startTs != null ? fmtDuration(n.closeTs - n.startTs) : null;
    lines.push(`${clk(n.closeTs)}${rel ? " · " + rel : ""}${dur ? " · " + dur : ""}`);
  }

  return lines;
}
