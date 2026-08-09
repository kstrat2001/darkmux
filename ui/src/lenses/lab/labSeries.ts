// Pure logic backing the lab lens — a TypeScript port of the legacy
// viewer's `shortModel` / `labFieldVal` / `labTaskKey` / `groupLabRunsByTask`
// / `labKnobSummary` / `labKnobDiff` (`crates/darkmux-serve/assets/viewer.html`).
//
// This is a parity port: where the legacy output looks odd, the odd output
// is the requirement (see `labSeries.test.ts`, the executable spec).
// Changing behavior is a separate change with its own reason.
import type { LabRun, LabSeat, LabSeries } from "./types";

/** Strip a LEADING `darkmux:` namespace prefix; absent model renders as "". */
export function shortModel(m: string | null | undefined): string {
  return String(m || "").replace(/^darkmux:/, "");
}

/**
 * Render one knob field value for display: null/undefined -> em dash,
 * a namespaced model id -> shortened, anything else passed through as-is
 * (a number stays a number — 0 is a real knob value, not "absent").
 */
export function labFieldVal(v: string | number | null | undefined): string | number {
  if (v == null) return "—";
  if (typeof v === "string" && v.startsWith("darkmux:")) return shortModel(v);
  return v;
}

/**
 * The series key: sorted case ids joined, so every run over the same corpus
 * groups together regardless of the order darkmux ran them in. A run whose
 * first case hasn't completed yet (case ids not known server-side) falls
 * into its own "(case pending)" bucket rather than silently merging into an
 * unrelated series.
 */
export function labTaskKey(run: LabRun): string {
  return run.case_ids && run.case_ids.length
    ? run.case_ids.slice().sort().join(", ")
    : "(case pending)";
}

/** Runs over the same corpus, newest-first, grouped and ordered by their newest run. */
export function groupLabRunsByTask(runs: LabRun[]): LabSeries[] {
  const byKey = new Map<string, LabRun[]>();
  runs.forEach((r) => {
    const k = labTaskKey(r);
    if (!byKey.has(k)) byKey.set(k, []);
    byKey.get(k)!.push(r);
  });
  const groups: LabSeries[] = [...byKey.entries()].map(([key, rs]) => ({
    key,
    runs: rs.slice().sort((a, b) => b.mtime_ms - a.mtime_ms),
  }));
  groups.sort((a, b) => b.runs[0].mtime_ms - a.runs[0].mtime_ms);
  return groups;
}

/** Human-readable summary of a run's recorded staffing knobs. */
export function labKnobSummary(run: LabRun): string {
  const st = run.staffing;
  // Two DISTINCT reasons the snapshot can be absent (verified live against a
  // real corpus): a run still in flight (no case has completed yet, so no
  // envelope carries a `staffing` snapshot) vs. a FINISHED run whose
  // envelope simply predates the field (#1247 Part 1 added it; older
  // artifacts deserialize `staffing: None`, not "still running").
  if (!st) {
    return run.finished
      ? "staffing not recorded (older artifact)"
      : "staffing pending — first case hasn't completed yet";
  }
  const probes = st.probes || [];
  const pbit = probes.length
    ? `${probes.length} probe${probes.length === 1 ? "" : "s"} (k=${probes.map((p) => p.k).join(",")})`
    : "no probes";
  const j = st.judge;
  const jbit = j
    ? `judge ${shortModel(j.model)} k=${j.k} · ${Math.round((j.n_ctx || 0) / 1000)}K ctx`
    : "";
  return [pbit, jbit].filter(Boolean).join(" · ");
}

const SEAT_DIFF_FIELDS: Array<keyof LabSeat> = ["model", "k", "max_tokens", "n_ctx"];

/**
 * The "what changed" line (operator direction, #1247): a run-over-run diff
 * of the RECORDED staffing snapshot — model/k/max_tokens/n_ctx per seat,
 * matched by seat name so a renamed/added/removed seat reads clearly rather
 * than as a positional diff. A single-variable change renders as a plain
 * diff line; the caller decides how to treat two-or-more changes.
 */
export function labKnobDiff(prev: LabRun | null, curr: LabRun | null): string[] | null {
  if (!prev || !curr) return null;
  const changes: string[] = [];
  if ((prev.crew || null) !== (curr.crew || null)) {
    changes.push(`crew ${labFieldVal(prev.crew)}→${labFieldVal(curr.crew)}`);
  }
  if ((prev.exec_mode || null) !== (curr.exec_mode || null)) {
    changes.push(`exec_mode ${labFieldVal(prev.exec_mode)}→${labFieldVal(curr.exec_mode)}`);
  }
  const ps = prev.staffing;
  const cs = curr.staffing;
  if (ps && cs) {
    const byName = (arr: LabSeat[] | null | undefined): Record<string, LabSeat> => {
      const m: Record<string, LabSeat> = {};
      (arr || []).forEach((p) => {
        m[p.name] = p;
      });
      return m;
    };
    const pn = byName(ps.probes);
    const cn = byName(cs.probes);
    const names = [...new Set([...Object.keys(pn), ...Object.keys(cn)])].sort();
    names.forEach((n) => {
      const a = pn[n];
      const b = cn[n];
      if (a && b) {
        SEAT_DIFF_FIELDS.forEach((f) => {
          if (a[f] !== b[f]) {
            changes.push(`${n}.${f} ${labFieldVal(a[f] as string | number | null | undefined)}→${labFieldVal(b[f] as string | number | null | undefined)}`);
          }
        });
      } else if (!a && b) {
        changes.push(`+probe ${n} (${shortModel(b.model)})`);
      } else if (a && !b) {
        changes.push(`-probe ${n}`);
      }
    });
    const aj = ps.judge;
    const bj = cs.judge;
    if (aj && bj) {
      SEAT_DIFF_FIELDS.forEach((f) => {
        if (aj[f] !== bj[f]) {
          changes.push(`judge.${f} ${labFieldVal(aj[f] as string | number | null | undefined)}→${labFieldVal(bj[f] as string | number | null | undefined)}`);
        }
      });
    } else if (!aj && bj) {
      changes.push(`+judge (${shortModel(bj.model)})`);
    } else if (aj && !bj) {
      changes.push("-judge");
    }
    // (both-null: no judge either side — nothing to diff.) A judge seat
    // appearing or disappearing between runs is exactly the methodology
    // drift this line exists to surface — silently diffing only when both
    // sides have one was a false "no knob change" (frontier review round,
    // #1262).
  } else if (ps !== cs) {
    changes.push("staffing snapshot became available");
  }
  return changes;
}
