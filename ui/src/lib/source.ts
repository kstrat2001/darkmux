import { injectedMeta } from "./injectedMeta";

/** (#2086) WHERE this page's data comes from, resolved in one place.
 *
 * A page is served either by a darkmux daemon (every route is a real HTTP
 * endpoint) or as the daemon-less static build (darkmux.com/demo: committed
 * fixture files named by `<meta name="darkmux-*-src">`, injected by
 * `scripts/build-demo.sh`). Before this module every lens re-decided which
 * of the two it was on — `isStaticBuild()` and the per-fixture `static*Src()`
 * readers were consulted at 40-odd call sites in 16 files — and each new
 * consumer that forgot the branch shipped a demo defect (#2063, #2065). Now
 * a lens asks `getSource()` for the resource it needs; the build-type
 * question is answered here and nowhere else.
 *
 * Read fresh on every call (a handful of `querySelector`s): tests inject and
 * remove these metas per case, and a module-level cache is exactly the trap
 * `useHashRoute`'s memo already set once.
 */
export interface Source {
  /** `static` when the page ships a committed flow file (`darkmux-flow-src`),
   * the one signal a static build always sets; `daemon` otherwise. */
  kind: "daemon" | "static";
  /** The committed flow `.jsonl` a static build replays on every route. */
  flow: string | null;
  /** The day that file replays, derived at build time (`darkmux-flow-date`);
   * `null` when no meta names one — callers fall back. */
  date: string | null;
  /** Per-fixture sources. Read INDEPENDENTLY of `kind`: a harness page may
   * ship one fixture without a flow file (the panel and machine tests do),
   * and a consumer of that fixture honors it either way, exactly as the
   * per-fixture readers this module replaced did. */
  graphs: string | null;
  machine: string | null;
  panels: string | null;
  fleet: string | null;
  runs: string;
  labRuns: string;
}

export function getSource(): Source {
  const flow = injectedMeta("darkmux-flow-src");
  const d = injectedMeta("darkmux-flow-date");
  return {
    kind: flow === null ? "daemon" : "static",
    flow,
    date: d && /^\d{4}-\d{2}-\d{2}$/.test(d) ? d : null,
    graphs: injectedMeta("darkmux-graphs-src"),
    machine: injectedMeta("darkmux-machine-src"),
    panels: injectedMeta("darkmux-panels-src"),
    fleet: injectedMeta("darkmux-fleet-src"),
    runs: injectedMeta("darkmux-runs-src") ?? "/runs",
    labRuns: injectedMeta("darkmux-lab-runs-src") ?? "/lab/runs",
  };
}

/** The runs / lab-runs endpoints, which exist on both kinds of page: the
 * daemon route by default, the committed fixture on a static build. */
export function runsSrc(): string {
  return getSource().runs;
}
export function labRunsSrc(): string {
  return getSource().labRuns;
}
