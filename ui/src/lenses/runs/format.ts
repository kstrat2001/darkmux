/**
 * Pure port of the runs-lens formatting/grouping logic in `viewer.html`
 * (the `── the runs lens ──` and `── Lab observer lens ──` sections). Kept
 * as standalone functions — not component-local — so they're independently
 * unit-testable against the same inputs/outputs the legacy `<script>` block
 * produces, and so `RunsBoard.tsx` reads as "wire data in, JSX out" rather
 * than re-deriving this logic inline.
 *
 * Every function name and behavior below is a DELIBERATE 1:1 match to its
 * `viewer.html` namesake (see that file's own comments for the "why", not
 * repeated here) — this is a port, not a redesign. `RUNS_KINDS`/`RunsKind`
 * live in `../../lib/route.ts` already (the scaffold's hash-grammar port);
 * imported from there rather than redeclared.
 *
 * `shortModel` lives in `../../lib/format.ts` and is re-exported from here.
 * The "lab-series six" it once belonged to (`labFieldVal`/`labTaskKey`/
 * `groupLabRunsByTask`/`labKnobSummary`/`labKnobDiff`) and this file's own
 * `labCounts` backed the `◧ series` knob-diff sub-view, removed in the
 * #2860 follow-up (see `RunsBoard.tsx`'s own module doc for why);
 * `shortModel` survives because `runSubtitle` below uses it for every run
 * kind, not just lab.
 */

import type { Run } from "../../types/generated/Run";
import { shortModel } from "../../lib/format";
import { NOT_REPORTING_STATUS } from "../../lib/machineAvailability";
import { dispatchHash } from "../../lib/route";
import { canonUid, machineRefKey, matchesMachine, nameKey, type MachineMatch, type MachineRef } from "../../lib/machineIdentity";

export { shortModel };

export const RUNS_CAP = 25; // viewer.html: `const RUNS_CAP=25`

/** viewer.html: `function runActivity(r)`. STRICTLY newest-activity-first
 * ordering — see that function's own comment for why `running` rows are
 * never hoisted above their actual last-activity time. */
export function runActivity(r: Run): number {
  return r.updated_ts || r.completed_ts || r.started_ts || 0;
}

/** viewer.html: `function runsAgo(r)`, minus the `runsIsPlayback()` branch —
 * this port has no playback/`daemon-mode-play` concept (the scaffold's
 * `/next` route is always "live"), so only the relative-time branch applies.
 * `now` defaults to `Date.now()` but is threaded as a parameter so a test
 * (or a future frozen-clock caller) doesn't have to mock the global clock. */
export function runsAgo(r: Run, now: number = Date.now()): string {
  const ts = runActivity(r);
  if (!ts) return "";
  const secs = Math.max(0, Math.floor(now / 1000) - ts);
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/**
 * (#1907) The status BADGE's display text. `RunStatus::Abandoned` alone
 * covers two genuinely different situations — a human ran `mission abort`,
 * or nothing ever wrote an ending (killed/crashed/no terminal record) —
 * and reading the same word for both is what prompted "i'm not sure what
 * abandoned means?" on the console's own activity list (this function's
 * origin issue). `Run.abandoned_reason` is set ONLY alongside `"abandoned"`
 * (`crates/darkmux-serve/src/runs.rs::Run::abandoned_reason`'s own doc), so
 * this reads it directly rather than re-deriving the distinction client-
 * side. Every other status keeps its own plain name, unchanged — this is
 * the ONE place `RunRow`'s badge text can diverge from `run.status` itself
 * (the CSS class backing the badge's COLOR stays keyed on the status from
 * `runBadgeStatus`, which is `run.status` verbatim except for a running run
 * whose machine is not reporting, so `.labbadge.abandoned` styling is
 * untouched by this).
 */
export function runStatusLabel(r: Run, notReporting = false): string {
  const status = runBadgeStatus(r, notReporting);
  if (status !== "abandoned") return status;
  return r.abandoned_reason === "aborted" ? "aborted" : "no ending recorded";
}

/** (5.0 R3) The status a row's badge keys its color and pulse on. It is the
 * run's own, except that a run recorded as running on a machine that is not
 * reporting (held as down by the fleet view, with no live session beat, so no
 * terminal record can arrive)
 * is `unknown` (`NOT_REPORTING_STATUS`). */
export function runBadgeStatus(r: Run, notReporting: boolean): string {
  return notReporting && r.status === "running" ? NOT_REPORTING_STATUS : r.status;
}

/** A lab row's verify outcome, in three states (#2494): what the workload's
 * own tests said is a different fact from how the dispatch ended (`status`).
 * `none` is a lab run naming a workload with no verify result recorded. */
type VerifyOutcome = "pass" | "fail" | "none";

/** The word each outcome reads as. The twin of `src/run_list.rs::verify_label`. */
const VERIFY_WORD: Record<VerifyOutcome, string> = { pass: "pass", fail: "FAIL", none: "\u2014" };

/** One `·`-separated piece of a row's subtitle. A verify piece carries its
 * outcome, so the board can color the word without re-reading the text. */
export interface SubtitlePart {
  text: string;
  verify?: { outcome: VerifyOutcome; word: string };
}

/** viewer.html: `function runSubtitle(r, showMachine)`, as parts. `machine` is
 * the machine's display label (`runMachineLabels`), or `null` to leave it out. */
export function runSubtitleParts(r: Run, machine: string | null): SubtitlePart[] {
  const parts: SubtitlePart[] = [];
  if (r.workload) parts.push({ text: r.workload });
  const outcome = verifyOutcome(r);
  if (outcome) parts.push({ text: `verify ${VERIFY_WORD[outcome]}`, verify: { outcome, word: VERIFY_WORD[outcome] } });
  if (r.role) parts.push({ text: r.role });
  if (r.model) parts.push({ text: shortModel(r.model) });
  if (r.route) parts.push({ text: `via ${r.route}` });
  if (machine) parts.push({ text: machine });
  // (#3016) The machine above is the one that RAN it; a relayed run also says
  // where it was asked. The same words from any serving machine: both names
  // come from the row, never from who is looking.
  if (r.relay) parts.push({ text: `from ${r.relay.asked_on_machine}` });
  return parts;
}

/** The subtitle as one line of text: the parts, `·`-separated. */
export function runSubtitle(r: Run, machine: string | null): string {
  return runSubtitleParts(r, machine)
    .map((p) => p.text)
    .join(" · ");
}

/** (#2925) The verify word a lab run reads as (`pass`, `FAIL`, or the dash for
 * no result recorded); `null` for a row that is not a lab run. */
export function runVerifyWord(r: Run): string | null {
  if (r.kind !== "lab") return null;
  if (r.verify_passed === true) return VERIFY_WORD.pass;
  return r.verify_passed === false ? VERIFY_WORD.fail : VERIFY_WORD.none;
}

/** A lab run with no manifest yet names no workload, and says nothing about
 * verify; a non-lab row never does. */
function verifyOutcome(r: Run): VerifyOutcome | null {
  if (r.kind !== "lab") return null;
  if (r.verify_passed === true) return "pass";
  if (r.verify_passed === false) return "fail";
  return r.workload ? "none" : null;
}

/** The machine a run names: its uid when the row carries one, else its name. */
function runMachineRef(r: Run): MachineRef {
  return { uid: r.machine_uid, name: r.machine };
}

/** The machine key of each run in `runs`: its uid when the row carries one; a
 * row with only a name takes the uid that one name belongs to among these rows
 * (a name two uids share resolves to neither), so a machine seen under a uid
 * on one row and a bare spelling on another is still one machine. */
export function machineKeyOfRuns(runs: Run[]): (r: Run) => string | null {
  const uidsByName = new Map<string, Set<string>>();
  for (const r of runs) {
    if (!r.machine_uid || !r.machine) continue;
    const k = nameKey(r.machine);
    uidsByName.set(k, (uidsByName.get(k) ?? new Set()).add(canonUid(r.machine_uid)));
  }
  return (r) => {
    const own = machineRefKey(runMachineRef(r));
    if (r.machine_uid || !r.machine) return own;
    const uids = uidsByName.get(nameKey(r.machine));
    return uids?.size === 1 ? `uid:${[...uids][0]}` : own;
  };
}

/** viewer.html: `function runsMultiMachine()`, generalized to take the runs
 * array as a parameter (legacy reads the module-global `RUNS`). Counts
 * MACHINES, not spellings: one machine under two names is one, two machines
 * under one name are two. */
export function runsMultiMachine(runs: Run[]): boolean {
  const keyOf = machineKeyOfRuns(runs);
  return new Set(runs.map(keyOf).filter((k): k is string => k !== null)).size > 1;
}

/** The label to print for each run's machine, by run id: the name on the
 * machine's most recently active run, with machines that would read alike
 * told apart by an ordinal ("Mac", "Mac 2"). */
export function runMachineLabels(runs: Run[]): Map<string, string> {
  const keyOf = machineKeyOfRuns(runs);
  const newestFirst = [...runs].sort((a, b) => runActivity(b) - runActivity(a));
  const named = new Map<string, string>();
  for (const r of newestFirst) {
    const key = keyOf(r);
    if (key !== null && r.machine && !named.has(key)) named.set(key, r.machine);
  }
  const seen = new Map<string, number>();
  const labelOfKey = new Map<string, string>();
  for (const [key, name] of named) {
    const n = (seen.get(nameKey(name)) ?? 0) + 1;
    seen.set(nameKey(name), n);
    labelOfKey.set(key, n === 1 ? name : `${name} ${n}`);
  }
  const labels = new Map<string, string>();
  for (const r of runs) {
    const key = keyOf(r);
    const label = key === null ? undefined : labelOfKey.get(key);
    if (label) labels.set(r.id, label);
  }
  return labels;
}

/** viewer.html: `function runsFiltered()`, parameterized over `runs`/`kind`
 * rather than reading `state.runsKind`/`RUNS` off module globals. */
export function runsFiltered(runs: Run[], kind: string): Run[] {
  const rows = kind === "all" ? runs.slice() : runs.filter((r) => r.kind === kind);
  rows.sort((a, b) => runActivity(b) - runActivity(a));
  return rows;
}


/**
 * (#1809, #1508 step 4) Filter a runs list down to ONE pinned machine — the
 * runs-lens half of the machine dimension legacy never had. Unlike every
 * other export in this file (see the module doc's opening paragraph), this
 * one has no `viewer.html` namesake; it is new.
 *
 * A run belongs to a machine by UID (`Run.machine_uid`, stamped from the
 * records that produced the row), compared case-normalized: one machine under
 * a renamed or `.local` spelling stays one machine, and two machines that
 * share a display name stay two (`lib/machineIdentity.ts::matchesMachine`).
 * Only a run whose record carried no uid falls back to its name, and then
 * only to a name no other machine also answers to.
 *
 * A run with NO `machine` and no uid at all is excluded from every pin. That
 * set is missions and dispatches only — every lab run carries a machine,
 * because `lab_summary_to_run` takes the daemon's own `machine_id` directly
 * instead of deriving it — and every one of them is `tracked: true`, so this
 * is not ghost noise. The mechanism, rather than a count that rots: `/runs`
 * resolves a mission's machine from the WINDOWED flow session index
 * (`RUNS_FLOW_SCAN_WINDOW_DAYS`, 14 days), and the durable `mission.json`
 * has no machine field to fall back on, so any tracked run older than that
 * window loses its attribution even though the flow records are still on
 * disk. Filed as #1810.
 *
 * Excluding them is the honest call — claiming an unattributed row as "this
 * machine" would be the worse lie.
 */
export function runsForMachine(runs: Run[], machine: MachineMatch): Run[] {
  return runs.filter((r) => matchesMachine(runMachineRef(r), machine));
}

/**
 * (#1904 QA fix) A run's click destination, extracted from `RunsBoard.tsx`'s
 * `activateRun` so the four-branch decision lives in exactly one place. It
 * used to also serve `ActivityPanel.tsx`'s own `activityRunActivate` — a
 * SECOND hand-rolled copy of the same decision, whose independent drift
 * (dropping the `unreachable` branch silently: a tracked mission/dispatch
 * row rendered clickable via `RunRow`'s own `interactive` gate, but
 * clicking it when `missionGraphReachable()` is false did nothing at all
 * — the exact #1900 failure class) is why this got pulled out at all.
 * `ActivityPanel.tsx` is deleted (#1905 step 3 — `run-list`, a real CLI
 * panel over the same `/runs` union, supersedes it), leaving `RunsBoard`
 * as this function's one remaining caller; the shared shape stays because
 * the decision itself — and the drift risk a second caller could someday
 * reintroduce — is unchanged by having only one caller today.
 *
 * The LAB branch is deliberately NOT resolved to a navigation here —
 * `RunsBoard` swaps to an in-page detail pane for a lab run (`openLabRun`,
 * a `labRunDir` state change plus a hash write) rather than navigating
 * anywhere. Returning the run's `id` (the lab run's directory) lets the
 * caller build its own destination from it, rather than this function
 * picking a navigation mechanism the caller doesn't use. */
export type RunDestination =
  | { kind: "lab"; dir: string }
  | { kind: "hash"; hash: string }
  /** A tracked mission (its own mission graph exists), but this page has
   * no live daemon behind it to fetch `/mission/<id>/graph.json` from
   * (the daemon-less static demo build). The row is still INTERACTIVE —
   * clicking it is a real, expected action — it just can't navigate
   * anywhere useful; callers show `MISSION_GRAPH_UNREACHABLE_NOTICE`
   * instead of silently doing nothing. */
  | { kind: "unreachable" }
  /** An untracked row with no representative session to drill into either
   * — genuinely nothing to do here. Callers render the row
   * non-interactive, matching `RunRow`'s own `interactive` gate ("has a
   * destination" — see that component's own doc), which already excludes
   * exactly this case. Rare in practice (every untracked row this build
   * has actually produced carries a `dispatch_id`, ghost or mission
   * alike), but not impossible — a mission this daemon knows about
   * ONLY through a terminal record, with no dispatch session ever
   * joined to it, is the honest shape that reaches here. */
  | { kind: "none" };

/**
 * (#1915) Untracked no longer means inert. #1900/#1902 widened this ONLY
 * for `kind === "dispatch"`, because a dispatch row's `id` happens to BE
 * its own session id — but `tracked` was never actually the right test;
 * "does this row carry a session it can be drilled into" is. The server
 * now carries that pick explicitly (`Run.dispatch_id`:
 * `crates/darkmux-serve/src/runs.rs`'s `mission_to_run`/
 * `flow_mission_to_run`/`ghost_runs`, all resolving it the SAME
 * representative-session rule already used for role/model/route), so ANY
 * untracked row with one, a ghost dispatch (whose `dispatch_id` equals
 * its own `id`) or an untracked mission (a peer's, #1705, or a local
 * ephemeral with no durable record) — drills the same way. On the
 * reported machine this was 40 of 104 mission rows, the entire newest
 * page a person actually sees (the board sorts newest-first).
 *
 * A tracked mission cannot use this shortcut even when it also carries a
 * `dispatch_id` (`mission_to_run` populates it uniformly, see that
 * field's own doc): `/mission/<id>/graph.json` is served from THIS
 * machine's own durable state, which a tracked row by definition has, so
 * the richer mission GRAPH is the right destination, not a session. An
 * UNTRACKED mission structurally cannot make that same claim — there is
 * no local `Mission`/`Phase`/`Task`/`Step` record for it, on this machine
 * or (for a peer's mission) on any machine this daemon can query — so its
 * session is the best this view can ever offer, not a fallback pending a
 * richer one.
 */
/** The mission a row's run belongs to, for its detail link: a mission row
 *  (tracked, or a remote mission) is keyed by its mission id; an untracked
 *  dispatch row is keyed by its session and names no mission. */
function missionOfRow(run: Run): string | null {
  return run.tracked || run.kind === "mission" ? run.id : null;
}

export function runDestination(run: Run, graphReachable: boolean): RunDestination {
  // (#2860) A lab row with a session opens the SAME session detail view as
  // every other run, running or finished. It used to switch, once finished,
  // to `LabRunDetail`: a page kept from the funnel-eval era with its own
  // idea of "finished" (funnel artifacts a `lab run <workload>` never
  // writes) and its own event source. Every finished workload run therefore
  // read RUNNING there, with an empty pipeline and no events, while the run
  // list said complete. One run, one detail view, one status source.
  //
  // `LabRunDetail` remains only for a row with no session to open: archived
  // runs from before lab rows carried one (#2511).
  if (run.kind === "lab") {
    if (run.dispatch_id) {
      return { kind: "hash", hash: dispatchHash(run.dispatch_id, null) };
    }
    return { kind: "lab", dir: run.id };
  }
  // (#1973) A DISPATCH-kind run drills to the DETAIL view, tracked or not.
  //
  // Previously only UNTRACKED rows came here; a tracked `darkmux dispatch`
  // mints a crew-of-one mission, so it fell through to `#mission=` and opened
  // the graph. That graph is a single node with no click handler — it showed
  // strictly less than the detail view and then dead-ended, so the path
  // "Runs -> Dispatch -> detail" did not exist for the rows most likely to be
  // clicked. The only way here was a hand-typed URL or a fleet activity bar.
  //
  // Gated on `dispatch_id` because that is the key this route addresses (a
  // dispatch-kind run is named for its content but keyed by its session — see
  // `CLAUDE.md` contract 8). A run whose flow records have aged out of the
  // window carries none, and falls through to the graph rather than offering
  // a link to nothing.
  if (run.kind === "dispatch" && run.dispatch_id) {
    return { kind: "hash", hash: dispatchHash(run.dispatch_id, missionOfRow(run)) };
  }
  if (!run.tracked) {
    // No `graphReachable` gate here — `/flow-dispatch/<id>` is a plain
    // daemon fetch (`SessionReplay`'s own fetch, same as the ungated
    // `#dispatch=<sid>` bars `FleetLens.tsx`'s activity timeline already
    // navigates to), not the mission-graph lens's endpoint.
    if (run.dispatch_id) return { kind: "hash", hash: dispatchHash(run.dispatch_id, missionOfRow(run)) };
    return { kind: "none" };
  }
  if (!graphReachable) return { kind: "unreachable" };
  return { kind: "hash", hash: `mission=${encodeURIComponent(run.id)}` };
}

/** The notice used to point at "the classic viewer at /" — `viewer.html`
 * was deleted in #1865 and `/` now serves THIS SAME app, so a daemon-less
 * visitor was being told to go to the page they were already on. There is
 * genuinely nowhere else to send them (a static build has no daemon to
 * reach, full stop), so the fix names the missing capability instead of a
 * bogus destination — matching `RunsBoard.tsx`'s own `onLabRunUnresolvable`
 * `"run detail needs a running daemon — …"` phrasing. Shared (not
 * redeclared per caller) so the two surfaces that can hit `runDestination`'s
 * `unreachable` branch say the same thing. */
export const MISSION_GRAPH_UNREACHABLE_NOTICE =
  "mission graph needs a running daemon behind this page — this static build has no mission graph data to show.";
