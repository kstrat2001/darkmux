/**
 * `renderMeta()` (viewer.html:1278-1307), live-mode branch only — `/next` is
 * always daemon-served (no playback/static-context path exists in this
 * scaffold; see `ui/README.md`), so the `else` branch (`DATA_SOURCE`,
 * playback date range) is out of scope. Also folds in `idleStatus()`
 * (viewer.html:1258-1276) — the "● ready · N · last run Xh ago" headline —
 * since this port's corpus never has a LIVE subject (`liveSubject()` needs a
 * running session; the recorded corpus has none — see the packet report),
 * so the `head` branch legacy picks is always `idleStatus()`'s here. The
 * `liveSubject()`-non-null branch (an actively running dispatch) is a real
 * gap, named rather than silently assumed away — see the packet report's
 * deviations section.
 *
 * The `#meta` region is GLOBAL — every lens shows the same badge line
 * (confirmed: `goldens/fleet.txt` and `goldens/machine.txt` carry byte-
 * identical `=== meta ===` sections), so this is called once from `App`,
 * not per-lens.
 */

import { T, LIVE_WINDOW_MS } from "./flow";
import { relAgoFrom } from "./format";
import type { FlowRecord, PresenceBeat } from "../types/handwritten";

/** `idleStatus()` — viewer.html:1258-1276. */
function idleHeadline(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number): string {
  const n = liveMachines.size;
  if (!n) return "○ waiting for a machine";
  const closes = data.filter((r) => r.action === "dispatch.complete" || r.action === "dispatch.error");
  const last = closes.length ? Math.max(...closes.map((r) => T(r.ts))) : null;
  const known = last != null && nowMs - last >= 0;
  const ago = known ? relAgoFrom(nowMs, last as number) : "";
  // Trailing space after `n` and the leading space on the `ago` suffix are
  // BOTH literal — legacy's template concatenates `${n} ${ICON}` (icon
  // renders no text, leaving the space) with `${ago?' · last run '+ago:''}`,
  // producing a double space before "· last run" when ago is present. See
  // `format.ts`'s module doc for why this is baked into the string rather
  // than reproduced via CSS/DOM structure.
  return `● ready · ${n} ` + (ago ? ` · last run ${ago}` : "");
}

/** The two `#meta` lines (joined by `<br>` in legacy — two lines here). */
export function computeMetaLines(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number): string[] {
  const hours = Math.round(LIVE_WINDOW_MS / 3600000);
  return [idleHeadline(data, liveMachines, nowMs), `${data.length} records · last ${hours}h`];
}
