/**
 * `hybridNote()` — viewer.html:1555-1576 (#803, #807, #1186). The hero
 * card's conclusion: a short, data-driven encouraging line. Priority order
 * (first match wins):
 *
 *  1. The latest `note` record with `source==="orchestrator"` and no
 *     `session_id` (`orchNotes()`, viewer.html:1553) — a frontier session's
 *     own `darkmux flow note --source orchestrator` sign-off. Real,
 *     operator/orchestrator-authored free text.
 *  2. The latest `mission.run*` record's mission — a deterministic
 *     template naming the mission.
 *  3. A deterministic template keyed on the local/cloud run split
 *     (`t.runs`/`t.cloudRuns`/`t.unknownRuns`) — local-only, cloud-only, a
 *     mixed count, or (#2637) an all-unattributed line when neither local
 *     nor cloud has any positive evidence. Unattributed runs are otherwise
 *     left OUT of this template's local/cloud narrative rather than folded
 *     into either side — see the comment on the branching below for why
 *     silence, not a third number, is the honest choice here.
 *  4. An invitation, at zero.
 *
 * Deterministic templates only (no generation); the ONE real free-text
 * source (case 1) is operator/orchestrator-authored, not model-generated.
 *
 * (#1869) Like `savings.ts`'s `tokensOffMeter`, this function carries no
 * playhead of its own — legacy's own `orchNotes()`/`mission.run*` scans are
 * both gated `T(r.ts)<=state.t`, and that gate is restored at the SAME call
 * site: `FleetLens` passes its `scopedData` (already filtered to `ts <=
 * playhead`) as this function's `data` argument, not the raw window. See
 * `savings.ts`'s module doc for the full reasoning; it applies here
 * verbatim.
 */

import { T } from "../../lib/flow";
import { clk } from "../../lib/format";
import type { FlowRecord } from "../../types/handwritten";
import type { TokensOffMeter } from "./savings";

/** `orchNotes()` — viewer.html:1553-1554. Dashboard notes are MISSION-level
 * by definition (`!r.session_id`) — session-scoped notes are adjudication
 * trail, not hero material.
 *
 * Exported (not just used internally by `hybridNote` below) so
 * `NotesDialog.tsx` — the notes-HISTORY modal `openNotes()` builds
 * (viewer.html:1606-1610) — reads the SAME set rather than re-deriving it;
 * `hybridNote`'s own `hasHistory` flag is `orchNotes(data).length > 0`. */
export function orchNotes(data: FlowRecord[]): FlowRecord[] {
  return data
    .filter((r) => r.action === "note" && r.source === "orchestrator" && !r.session_id)
    .sort((a, b) => T(a.ts) - T(b.ts));
}

export interface HybridNote {
  /** Everything after the "Orchestrator note:" prefix — a single line of
   * text (a note's timestamp suffix, when present, is already folded in). */
  text: string;
  /** True when there's a real notes history to link to (`orchNotes().length
   * > 0`) — independent of whether THIS render picked a note as its text
   * (the mission/runs/invite branches can still have history behind them). */
  hasHistory: boolean;
}

export function hybridNote(data: FlowRecord[], t: TokensOffMeter): HybridNote {
  const notes = orchNotes(data);
  const hasHistory = notes.length > 0;

  const last = notes[notes.length - 1];
  if (last) {
    return { text: `${last.handle ?? ""} · ${clk(T(last.ts))}`, hasHistory };
  }

  const mr = data
    .filter((r) => r.action?.startsWith("mission.run") && r.mission_id)
    .sort((a, b) => T(a.ts) - T(b.ts))
    .pop();
  if (mr) {
    return {
      text: `${mr.mission_id} ran through the local loop — the fleet lifted, the frontier judged. that's hybrid. keep going.`,
      hasHistory,
    };
  }

  if (t.runs) {
    // (#2637) `lr` is runs with POSITIVE local evidence — total minus both
    // cloud AND unattributed, never a residual that silently absorbs
    // whatever isn't cloud. `t.unknownRuns` never appears as a number in
    // this template: the fleet total chip (`t.runs`) already records it
    // exhaustively, so this note's job is only to describe what's actually
    // known about the local/cloud split — mentioning "N unattributed" here
    // would either read as a third bucket nobody asked about, or tempt a
    // reader into assuming it means "neither, so it must be free" (or
    // "cloud", or "local") — exactly what darkmux must not imply. When
    // there is nothing POSITIVE to say about either side, the dedicated
    // all-unattributed branch below says so plainly instead of guessing.
    const lr = t.runs - t.cloudRuns - t.unknownRuns;
    const d = (n: number) => `dispatch${n === 1 ? "" : "es"}`;
    if (lr && t.cloudRuns) {
      return {
        text: `${lr} ${d(lr)} local + ${t.cloudRuns} via cloud. The hybrid loop is humming, keep it up.`,
        hasHistory,
      };
    }
    if (lr) {
      return { text: `${lr} local ${d(lr)}. The hybrid loop is humming, keep it up.`, hasHistory };
    }
    if (t.cloudRuns) {
      return { text: `${t.cloudRuns} ${d(t.cloudRuns)} via cloud. The right brain for the job, keep it up.`, hasHistory };
    }
    // Neither side has any positive evidence — every run this window is
    // unattributed. Say so plainly rather than defaulting to either the
    // local-only or cloud-only template with a zero-dressed-up count.
    return {
      text: `${t.runs} ${d(t.runs)} with no attribution darkmux could confirm. keep going.`,
      hasHistory,
    };
  }

  return { text: "going hybrid takes nerve. the fleet is ready when you are.", hasHistory };
}
