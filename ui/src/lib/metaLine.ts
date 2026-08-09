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

import { T} from "./flow";
import { relAgoFrom } from "./format";
import type { FlowRecord, PresenceBeat } from "../types/handwritten";

/** `idleStatus()` — viewer.html:1258-1276. */
function idleHeadline(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number): string {
  const n = liveMachines.size;
  if (!n) return "○ waiting for a machine";
  const starts = data.filter((r) => r.action === "dispatch.start");
  const last = starts.length ? Math.max(...starts.map((r) => T(r.ts))) : null;
  const known = last != null && nowMs - last >= 0;
  const ago = known ? relAgoFrom(nowMs, last as number) : "";
  // Trailing space after `n` and the leading space on the `ago` suffix are
  // BOTH literal — legacy's template concatenates `${n} ${ICON}` (icon
  // renders no text, leaving the space) with `${ago?' · last run '+ago:''}`,
  // producing a double space before "· last run" when ago is present. See
  // `format.ts`'s module doc for why this is baked into the string rather
  // than reproduced via CSS/DOM structure.
  return `${n} ` + (ago ? ` · last dispatch ${ago}` : "");
}

/** The ready headline as PARTS, so the caller can render legacy's real
 *  elements — `<span class="rdot ok">` (green) and `<span class="mco">` with
 *  the machine icon — instead of a flat string. Flattening them lost the
 *  dot's colour AND the icon while keeping the text identical, which is
 *  exactly why the goldens never noticed. */
export interface ReadyParts { kind: "ready"; n: number; ago: string }
export function readyParts(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number): ReadyParts | null {
  const n = liveMachines.size;
  if (!n) return null;
  // LAST DISPATCH, measured at its START.
  //
  // Three iterations to get here, each wrong for a different reason.
  // "last run" used dispatch COMPLETION — so a dispatch still running was
  // invisible, and the line aged while work was actively happening.
  // "last event" used the newest record of any kind — useless, because
  // heartbeats stream continuously and it would read "just now" forever.
  // A dispatch START is the honest activity signal: it says when work last
  // BEGAN, counts in-flight work, and cannot be kept warm by telemetry.
  const starts = data.filter((r) => r.action === "dispatch.start");
  const last = starts.length ? Math.max(...starts.map((r) => T(r.ts))) : null;
  const known = last != null && nowMs - last >= 0;
  return { kind: "ready", n, ago: known ? relAgoFrom(nowMs, last as number) : "" };
}

/** The two `#meta` lines (joined by `<br>` in legacy — two lines here). */
export function computeMetaLines(data: FlowRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number): string[] {
  // (operator) One line. The record count lives in the event pane now, next
  // to the records — stating it here too cost the status bar a second line
  // for something the pane already says. See EventLogColumn's counter chip.
  return [idleHeadline(data, liveMachines, nowMs)];
}
