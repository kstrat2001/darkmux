import { useHashRoute } from "./lib/useHashRoute";
import { FleetStrip } from "./components/FleetStrip";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { ConsolePanel } from "./lenses/console/ConsolePanel";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) — `fleet` (`FleetStrip`) and `runs` (`RunsBoard`,
 * Packet 3) are real regions driven by `useQuery`; every other lens renders
 * [[LensPlaceholder]] naming what still needs to be built, per the
 * render-sanity contract (never a blank page).
 */
export function App() {
  const route = useHashRoute();

  return (
    <div className="app-shell">
      <header className="app-shell__crumb" id="crumb">
        darkmux {routeLabel(route)}
      </header>
      <main className="app-shell__stage" id="stage">
        {renderRoute(route)}
      </main>
    </div>
  );
}

function routeLabel(route: Route): string {
  switch (route.kind) {
    case "fleet":
      return "· fleet";
    case "runs":
      return `· runs (${route.runsKind})`;
    case "machine":
      return "· machine";
    case "console":
      return `· console${route.panelId ? ` · ${route.panelId}` : ""}`;
    case "session":
      return `· session ${route.sessionId}`;
    case "mission-redirect":
      return `· mission ${route.missionId}`;
    case "unknown":
      return "· unrecognized route";
  }
}

function renderRoute(route: Route) {
  switch (route.kind) {
    case "fleet":
      return <FleetStrip />;
    case "runs":
      return <RunsBoard initialKind={route.runsKind} />;
    case "machine":
      return <LensPlaceholder label="machine" />;
    case "console":
      return <ConsolePanel initialPanelId={route.panelId} />;
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
