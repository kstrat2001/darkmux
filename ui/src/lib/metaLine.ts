/**
 * `renderMeta()`, live-mode branch only — `/next` is
 * always daemon-served (no playback/static-context path exists in this
 * scaffold; see `ui/README.md`), so the `else` branch (`DATA_SOURCE`,
 * playback date range) is out of scope. Also folds in `idleStatus()`
 * — the "● ready · N · last run Xh ago" headline —
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

import { relAgoFrom } from "./format";
import type { PresenceBeat } from "../types/generated/PresenceBeat";
import { canonUid } from "./machineIdentity";
import { ACTION, latestByTime, type NormRecord } from "./ingest";

/** How long ago the newest dispatch STARTED, as of `nowMs` ("" when none
 *  has, or the newest lies after `nowMs`). The idle headline and the ready
 *  parts both read it, so the two can never date "last dispatch" differently.
 */
function lastDispatchAgo(data: NormRecord[], nowMs: number): string {
  const last = latestByTime(data.filter((r) => r.action === ACTION.DispatchStart))?.tMs ?? null;
  const known = last != null && nowMs - last >= 0;
  return known ? relAgoFrom(nowMs, last as number) : "";
}

/** How many machines are up: those presence names, plus this one. SELF IS
 *  NEVER UNKNOWN (`lib/machineIdentity.isSelfMachine`): the daemon serving the
 *  page is a machine whether or not presence (off with Redis, empty on a
 *  fresh install) lists it, so an empty map is not "waiting for a machine".
 *  `selfUid` is `null` until the daemon has named itself. */
function machineCount(liveMachines: Map<string, PresenceBeat>, selfUid: string | null): number {
  const uids = new Set([...liveMachines.keys()].map(canonUid));
  if (selfUid) uids.add(canonUid(selfUid));
  return uids.size;
}

/** `idleStatus()`. */
function idleHeadline(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number, selfUid: string | null): string {
  const n = machineCount(liveMachines, selfUid);
  if (!n) return "○ waiting for a machine";
  const ago = lastDispatchAgo(data, nowMs);
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
export function readyParts(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number, selfUid: string | null = null): ReadyParts | null {
  const n = machineCount(liveMachines, selfUid);
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
  return { kind: "ready", n, ago: lastDispatchAgo(data, nowMs) };
}

/** The two `#meta` lines (joined by `<br>` in legacy — two lines here). */
export function computeMetaLines(data: NormRecord[], liveMachines: Map<string, PresenceBeat>, nowMs: number, selfUid: string | null = null): string[] {
  // (operator) One line. The record count lives in the event pane now, next
  // to the records — stating it here too cost the status bar a second line
  // for something the pane already says. See EventLogColumn's counter chip.
  return [idleHeadline(data, liveMachines, nowMs, selfUid)];
}
