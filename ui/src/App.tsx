import { useHashRoute } from "./lib/useHashRoute";
import { FleetStrip } from "./components/FleetStrip";
import { LensPlaceholder } from "./components/LensPlaceholder";
import { RunsBoard } from "./lenses/runs/RunsBoard";
import { CatalogPanel } from "./lenses/catalog/CatalogPanel";
import { MissionReplay } from "./lenses/catalog/MissionReplay";
import { SessionReplay } from "./lenses/catalog/SessionReplay";
import { PlaybackLens } from "./lenses/catalog/PlaybackLens";
import type { Route } from "./lib/route";

/**
 * The app shell. A `switch` over the parsed [[Route]] (see `lib/route.ts` for
 * the hash-grammar port) — `fleet` (`FleetStrip`) and `runs` (`RunsBoard`,
 * Packet 3) are real regions driven by `useQuery`; `session`/`mission-
 * redirect`/`playback` (Packet 4) do REAL fetches/navigation wiring per the
 * catalog+replay lens's own doc comments; every other lens renders
 * [[LensPlaceholder]] naming what still needs to be built, per the
 * render-sanity contract (never a blank page).
 *
 * `CatalogPanel` (Packet 4) mounts here rather than inside any one lens's
 * stage — it's global chrome (`viewer.html`'s `#catpanel`, a body-level
 * sibling of `#stage`, reachable from every lens), not a routed destination
 * itself.
 */
export function App() {
  const route = useHashRoute();

  return (
    <div className="app-shell">
      <header className="app-shell__crumb" id="crumb">
        darkmux {routeLabel(route)}
      </header>
      <CatalogPanel />
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
    case "playback":
      return `· playback ${route.date}`;
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
      return <LensPlaceholder label={`console panel "${route.panelId || "mission-status"}"`} />;
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
