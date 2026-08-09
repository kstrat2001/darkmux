import { useMemo } from "react";
import { useHashRoute } from "./lib/useHashRoute";
import { useSyncHash } from "./lib/hashSync";
import { FleetStrip } from "./components/FleetStrip";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { NavChrome } from "./components/NavChrome";
import { LiveStatusBadge } from "./components/LiveStatusBadge";
import { MachineLens } from "./lenses/machine/MachineLens";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { ConsolePanel } from "./lenses/console/ConsolePanel";
import { CatalogPanel } from "./lenses/catalog/CatalogPanel";
import { MissionReplay } from "./lenses/catalog/MissionReplay";
import { SessionReplay } from "./lenses/catalog/SessionReplay";
import { PlaybackLens } from "./lenses/catalog/PlaybackLens";
import { useFlowWindow } from "./hooks/useFlowWindow";
import { useLiveMachines } from "./hooks/useLiveMachines";
import { useLiveTail } from "./hooks/useLiveTail";
import { computeMetaLines } from "./lib/metaLine";
import { localMachineUid, nameOf } from "./lib/flow";
import { isLiveRoute } from "./lib/route";
import { useQuery } from "@tanstack/react-query";
import { fetchJson } from "./lib/fetcher";
import { queryKeys } from "./lib/queryKeys";
import type { MachineSpecs } from "./types/handwritten";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) drives `#stage`; `fleet` (`FleetStrip`), `runs`
 * (`RunsBoard`, Packet 3), and `machine` (`MachineLens`, Packet 2) are real
 * regions driven by `useQuery`; `session`/`mission-redirect`/`playback`
 * (Packet 4) do REAL fetches/navigation wiring per the catalog+replay
 * lens's own doc comments; every other lens renders [[LensPlaceholder]]
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
 * `CatalogPanel` (Packet 4) mounts here rather than inside any one lens's
 * stage — it's global chrome (`viewer.html`'s `#catpanel`, a body-level
 * sibling of `#stage`, reachable from every lens), not a routed destination
 * itself.
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

  // (Packet 5) The SSE tail + reconcile backstop + date-rollover handler —
  // gated by `isLiveRoute` (see that function's own doc) so a genuinely
  // historical route (`playback`/`session`/`mission-redirect`) doesn't run
  // a live tail behind it, matching legacy's own `wantsPlayback` gate on
  // `startLiveTail`. Feeds `flowWindow` below via the Query cache
  // (`useFlowWindow`'s own doc), not a direct return-value dependency here.
  const liveStatus = useLiveTail(isLiveRoute(route));

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
        {/* (QA, packet 5) Only where a tail actually runs. `useLiveTail(false)`
            returns its initial "live" untouched, so rendering this
            unconditionally made a session-replay route claim `● live` with no
            stream, no reconcile, and no liveness of any kind — #1480's
            dishonesty in mirror image. */}
        {isLiveRoute(route) ? <LiveStatusBadge status={liveStatus} /> : null}
      </div>
      <CatalogPanel />
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
 * rather than a placeholder string invented for scaffold navigability.
 * `session`/`mission-redirect`/`playback` (Packet 4) fall through to the
 * same empty default — none of them are byte-parity targets for `#crumb`
 * (see each component's own doc for why), so inventing crumb text for them
 * would be UX decoration, not a port. */
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
      return <ConsolePanel initialPanelId={route.panelId} />;
    case "session":
      // Packet 4: a real fetch to /flow-session/<id> — see SessionReplay's
      // own doc for why the RENDER (not the fetch) is still a not-ported
      // notice.
      return <SessionReplay sessionId={route.sessionId} />;
    case "mission-redirect":
      // Packet 4: a real fetch to /flow-mission/<id>, conditionally
      // navigating to /mission/<id>/graph exactly like legacy's boot() does
      // — see MissionReplay's own doc for why this completes Packet 1's
      // deferred placeholder rather than staying inert.
      return <MissionReplay missionId={route.missionId} />;
    case "playback":
      // Packet 4: a bare #<date> hash — see PlaybackLens's own doc for why
      // this is a named not-ported notice rather than a full historical
      // render.
      return <PlaybackLens date={route.date} />;
    case "unknown":
      return <LensPlaceholder label="unrecognized" hash={route.hash} />;
  }
}
