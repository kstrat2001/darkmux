import { useMemo } from "react";
import { useHashRoute } from "./lib/useHashRoute";
import { useSyncHash } from "./lib/hashSync";
import { FleetStrip } from "./components/FleetStrip";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { NavChrome } from "./components/NavChrome";
import { MachineLens } from "./lenses/machine/MachineLens";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { useFlowWindow } from "./hooks/useFlowWindow";
import { useLiveMachines } from "./hooks/useLiveMachines";
import { computeMetaLines } from "./lib/metaLine";
import { localMachineUid, nameOf } from "./lib/flow";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "./lib/fetcher";
import { queryKeys } from "./lib/queryKeys";
import type { MachineSpecs } from "./types/handwritten";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) drives `#stage`; `fleet` (`FleetStrip`), `runs`
 * (`RunsBoard`, Packet 3), and `machine` (`MachineLens`, Packet 2) are real
 * regions driven by `useQuery`; every other lens renders [[LensPlaceholder]]
 * naming what still needs to be built, per the render-sanity contract
 * (never a blank page).
 *
 * `#crumb` and `#logscope` are LENS-SPECIFIC (legacy: `renderCrumb()`'s
 * `$("crumb").innerHTML=...` per `state.level`, and each `render*()`
 * function's own `$("logscope").textContent=...`) — computed here per
 * route rather than inside each lens component, since the target DOM
 * elements are App-level siblings of `#stage`, not descendants of it.
 * `#meta` is GLOBAL (legacy's `renderMeta()` runs on every render()
 * regardless of `state.level` — confirmed: `goldens/fleet.txt` and
 * `goldens/machine.txt` carry byte-identical `=== meta ===` sections), so
 * it's computed here unconditionally rather than per-route. The underlying
 * `useFlowWindow`/`useLiveMachines`/`machineSpecs` queries are ALSO used
 * inside `MachineLens` — TanStack Query dedupes by queryKey, so this is
 * cache reuse, not a second network round trip.
 *
 * Packet 1.5 additions (nav chrome + hash write-back — the scaffold gap
 * both the machine and runs lens packets independently flagged as a hard
 * blocker for the eventual `/next` → `/` flip):
 *
 * - `<NavChrome>` (see that component's own doc) is a new sibling INSIDE a
 *   `.app-shell__crumbbar` wrapper alongside `#crumb`/`#meta` — a pure DOM
 *   restructuring, not a content change: the parity extractor
 *   (`tests/parity/lib/extract-lens.js`) selects `#crumb`/`#meta` BY ID
 *   regardless of parent, so moving their container doesn't touch
 *   byte-parity. `#logscope`/`#stage` are untouched siblings, same as
 *   before.
 * - `useSyncHash` (see `lib/hashSync.ts`) is the `/next` port of legacy's
 *   `syncLabHash()` — reflects the current `Route` back into `location.hash`
 *   via `replaceState` on every route change, so every view is bookmarkable
 *   (matches legacy's own reasoning: the phone dashboard is the first-class
 *   consumer). This is also what performs the legacy `#lens=lab` →
 *   `#lens=runs&kind=lab` upgrade, since arriving on the alias parses to
 *   the canonical `Route` already and the write-back just names it.
 *   `RunsBoard`'s kind chips are the one piece of lens state that changes
 *   WITHOUT a route change (no `hashchange` fires) — that write goes
 *   straight from `RunsBoard.tsx`'s `selectKind` to `hashSync.ts`'s
 *   `writeHash`, not through this route-keyed effect (see that file's own
 *   doc for why).
 */
export function App() {
  const route = useHashRoute();
  const nowMs = Date.now();

  const flowWindow = useFlowWindow(nowMs);
  const liveMachines = useLiveMachines();
  const specsQuery = useQuery({
    queryKey: queryKeys.machineSpecs(),
    queryFn: () => fetchJson<MachineSpecs>("/machine/specs"),
  });
  const specs = specsQuery.data?.ok ? specsQuery.data.data : null;

  const localUid = useMemo(
    () => localMachineUid(flowWindow.data, liveMachines, specs?.machine_id ?? null),
    [flowWindow.data, liveMachines, specs],
  );
  const localName = localUid != null ? nameOf(flowWindow.data, liveMachines, localUid) : null;

  const metaLines = useMemo(() => computeMetaLines(flowWindow.data, liveMachines, nowMs), [flowWindow.data, liveMachines, nowMs]);

  const { crumb, logscope } = routeChrome(route, localName);

  useSyncHash(route);

  return (
    <div className="app-shell">
      <div className="app-shell__crumbbar">
        <NavChrome route={route} />
        <header className="app-shell__crumb" id="crumb">
          {crumb}
        </header>
        <div className="app-shell__meta" id="meta">
          {/* `whiteSpace: "pre"` — the idle headline's literal double space
              before "· last run" (see `metaLine.ts`'s module doc) is an
              artifact of legacy's icon SPAN breaking the whitespace-collapse
              run; default `white-space: normal` would collapse it back to
              one space here, since there's no element in the way. Preserving
              it verbatim is simpler and more robust than reproducing the
              icon-boundary quirk with a real (empty) element. */}
          {metaLines.map((line, i) => (
            <div key={i} style={{ whiteSpace: "pre" }}>
              {line}
            </div>
          ))}
        </div>
      </div>
      {/* Visible (never `display:none`) — `innerText`, which the parity
          harness extracts, returns "" for a hidden element; see
          `tests/parity/lib/extract-lens.js`'s `regionText`. Legacy's own
          `#logscope` lives inside a visible sidebar heading, not hidden
          either. */}
      <span className="app-shell__logscope" id="logscope">
        {logscope}
      </span>
      <main className="app-shell__stage" id="stage">
        {renderRoute(route)}
      </main>
    </div>
  );
}

/** `renderCrumb()` (viewer.html:2476-2568) + each lens's own
 * `$("logscope").textContent=` assignment, folded into one lookup keyed on
 * [[Route]]. Only `machine` has a real (non-empty) mapping ported so far —
 * every other route's crumb/logscope stays empty, matching legacy's actual
 * default for most levels (see e.g. `goldens/fleet.txt`'s `(empty)` crumb)
 * rather than a placeholder string invented for scaffold navigability. */
function routeChrome(route: Route, localMachineName: string | null): { crumb: string; logscope: string } {
  if (route.kind === "machine") {
    // `$("crumb").innerHTML = state.machine!=null ? escN(state.machine) :
    // "this machine"` (viewer.html:2537); `$("logscope").textContent =
    // m!=null?nameOf(m):"machine"` (viewer.html:1799).
    return { crumb: localMachineName ?? "this machine", logscope: localMachineName ?? "machine" };
  }
  return { crumb: "", logscope: "" };
}

function renderRoute(route: Route) {
  switch (route.kind) {
    case "fleet":
      return <FleetStrip />;
    case "runs":
      return <RunsBoard initialKind={route.runsKind} />;
    case "machine":
      return <MachineLens />;
    case "console":
      return <LensPlaceholder label={`console panel "${route.panelId || "mission-status"}"`} />;
    case "session":
      return <LensPlaceholder label={`session drill-in ${route.sessionId}`} />;
    case "mission-redirect":
      // The legacy viewer does a FULL NAVIGATION here (`location.href =
      // "/mission/<id>/graph"`) — out of scope for this packet (see
      // tests/parity/README.md's lens inventory). A future packet wires the
      // same redirect; until then this is a named placeholder, not silence.
      return <LensPlaceholder label={`mission graph redirect for ${route.missionId} (out of scope — see /mission/:id/graph)`} />;
    case "unknown":
      return <LensPlaceholder label="unrecognized" hash={route.hash} />;
  }
}
