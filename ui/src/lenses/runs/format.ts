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
 */

import type { Run } from "../../types/generated/Run";
import type { LabRun } from "../../types/handwritten";

export const RUNS_CAP = 25; // viewer.html: `const RUNS_CAP=25`

/** viewer.html: `function shortModel(m)`. Strips the darkmux LMStudio
 * load-namespace prefix (see the root CLAUDE.md's "Namespace convention")
 * so the operator never sees the internal `darkmux:` marker in a run row. */
export function shortModel(m: string | null | undefined): string {
  return String(m || "").replace(/^darkmux:/, "");
}

/** viewer.html: `function labFieldVal(v)`. */
export function labFieldVal(v: unknown): string {
  return v == null ? "—" : typeof v === "string" && v.startsWith("darkmux:") ? shortModel(v) : String(v);
}

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

/** viewer.html: `function runSubtitle(r, showMachine)`. */
export function runSubtitle(r: Run, showMachine: boolean): string {
  const bits: string[] = [];
  if (r.role) bits.push(r.role);
  if (r.model) bits.push(shortModel(r.model));
  if (r.route) bits.push(`via ${r.route}`);
  if (showMachine && r.machine) bits.push(r.machine);
  return bits.join(" · ");
}

/** viewer.html: `function runsMultiMachine()`, generalized to take the runs
 * array as a parameter (legacy reads the module-global `RUNS`). */
export function runsMultiMachine(runs: Run[]): boolean {
  return new Set(runs.map((r) => r.machine).filter((m): m is string => Boolean(m))).size > 1;
}

/** viewer.html: `function runsFiltered()`, parameterized over `runs`/`kind`
 * rather than reading `state.runsKind`/`RUNS` off module globals. */
export function runsFiltered(runs: Run[], kind: string): Run[] {
  const rows = kind === "all" ? runs.slice() : runs.filter((r) => r.kind === kind);
  rows.sort((a, b) => runActivity(b) - runActivity(a));
  return rows;
}

/** viewer.html: `function labTaskKey(run)`. */
export function labTaskKey(run: LabRun): string {
  return run.case_ids && run.case_ids.length ? run.case_ids.slice().sort().join(", ") : "(case pending)";
}

export interface LabTaskGroup {
  key: string;
  runs: LabRun[];
}

/** viewer.html: `function groupLabRunsByTask(runs)`. */
export function groupLabRunsByTask(runs: LabRun[]): LabTaskGroup[] {
  const byKey = new Map<string, LabRun[]>();
  runs.forEach((r) => {
    const k = labTaskKey(r);
    if (!byKey.has(k)) byKey.set(k, []);
    byKey.get(k)!.push(r);
  });
  const groups = [...byKey.entries()].map(([key, rs]) => ({
    key,
    runs: rs.slice().sort((a, b) => b.mtime_ms - a.mtime_ms),
  }));
  groups.sort((a, b) => b.runs[0].mtime_ms - a.runs[0].mtime_ms);
  return groups;
}

/** viewer.html: `function labKnobSummary(run)`. */
export function labKnobSummary(run: LabRun): string {
  const st = run.staffing;
  if (!st) return run.finished ? "staffing not recorded (older artifact)" : "staffing pending — first case hasn't completed yet";
  const probes = st.probes || [];
  const pbit = probes.length
    ? `${probes.length} probe${probes.length === 1 ? "" : "s"} (k=${probes.map((p) => p.k).join(",")})`
    : "no probes";
  const j = st.judge;
  const jbit = j ? `judge ${shortModel(j.model)} k=${j.k} · ${Math.round((j.n_ctx || 0) / 1000)}K ctx` : "";
  return [pbit, jbit].filter(Boolean).join(" · ");
}

/** viewer.html: `function labKnobDiff(prev, curr)`. */
export function labKnobDiff(prev: LabRun | undefined, curr: LabRun | undefined): string[] | null {
  if (!prev || !curr) return null;
  const changes: string[] = [];
  if ((prev.crew || null) !== (curr.crew || null)) changes.push(`crew ${labFieldVal(prev.crew)}→${labFieldVal(curr.crew)}`);
  if ((prev.exec_mode || null) !== (curr.exec_mode || null))
    changes.push(`exec_mode ${labFieldVal(prev.exec_mode)}→${labFieldVal(curr.exec_mode)}`);
  const ps = prev.staffing,
    cs = curr.staffing;
  if (ps && cs) {
    const byName = (arr?: { name: string }[]) => {
      const m: Record<string, { name: string; model: string; k: number; max_tokens?: number; n_ctx?: number }> = {};
      (arr || []).forEach((p: any) => {
        m[p.name] = p;
      });
      return m;
    };
    const pn = byName(ps.probes),
      cn = byName(cs.probes);
    const names = [...new Set([...Object.keys(pn), ...Object.keys(cn)])].sort();
    names.forEach((n) => {
      const a = pn[n],
        b = cn[n];
      if (a && b) {
        (["model", "k", "max_tokens", "n_ctx"] as const).forEach((f) => {
          if (a[f] !== b[f]) changes.push(`${n}.${f} ${labFieldVal(a[f])}→${labFieldVal(b[f])}`);
        });
      } else if (!a && b) {
        changes.push(`+probe ${n} (${shortModel(b.model)})`);
      } else if (a && !b) {
        changes.push(`-probe ${n}`);
      }
    });
    const aj = ps.judge,
      bj = cs.judge;
    if (aj && bj) {
      (["model", "k", "max_tokens", "n_ctx"] as const).forEach((f) => {
        if (aj[f] !== bj[f]) changes.push(`judge.${f} ${labFieldVal(aj[f])}→${labFieldVal(bj[f])}`);
      });
    } else if (!aj && bj) {
      changes.push(`+judge (${shortModel(bj.model)})`);
    } else if (aj && !bj) {
      changes.push("-judge");
    }
  } else if (ps !== cs) {
    changes.push("staffing snapshot became available");
  }
  return changes;
}

/** viewer.html: `function labCounts(run)` — text only (no HTML), the
 * "degenerate" flag is a boolean the component renders as its own element
 * rather than an inline HTML fragment (see `RunsBoard.tsx`). */
export function labCounts(run: LabRun): string {
  return `bundles ${run.bundles} · flags ${run.raw_flags}→${run.deduped_flags} · confirmed ${run.confirmed} · needs_check ${run.needs_check} · archived ${run.archived}`;
}
