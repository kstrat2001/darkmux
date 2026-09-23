// Pure logic backing the lab lens — a TypeScript port of the legacy
// viewer's `shortModel` (`crates/darkmux-serve/assets/viewer.html`).
//
// This module used to also carry `labFieldVal`/`labTaskKey`/
// `groupLabRunsByTask`/`labKnobSummary`/`labKnobDiff` — the pure logic
// behind the runs board's `◧ series` knob-diff sub-view. That view was a
// port of the retired review-bench funnel view and read every field it
// needed off `/lab/runs`'s funnel-era shape; a `coding-task`/`prompt` run
// (what `lab run <workload>` actually produces) never populates any of
// them, so a real run's series row read as a permanently-pending funnel
// case no matter how long ago it finished. Removed in the #2860 follow-up
// rather than patched — see `RunsBoard.tsx`'s own module doc for the full
// reasoning. `shortModel` alone survives: `../runs/format.ts`'s
// `runSubtitle` uses it for every run kind's model field, lab or not.

/** Strip a LEADING `darkmux:` namespace prefix; absent model renders as "". */
export function shortModel(m: string | null | undefined): string {
  return String(m || "").replace(/^darkmux:/, "");
}
