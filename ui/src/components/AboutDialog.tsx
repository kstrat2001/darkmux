import { injectedMeta } from "../lib/injectedMeta";
import { isLiveRoute, type Route } from "../lib/route";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type { MachineSpecs } from "../types/handwritten";
import { Dialog } from "./Dialog";

/** `openInfo()` — viewer.html:1617-1643 (#1132). Build/status snapshot: the
 * injected version/schema metas (same live mechanism `Masthead.tsx` already
 * reads via `injectedMeta`), a connection/mode summary derived from THIS
 * app's own route + live-tail status (the port has no single `DATA_SOURCE`
 * global the way legacy does — `connectionText`/`modeText` below are the
 * honest equivalent built from what this app actually tracks), the local
 * machine's specs when on a live route (`/machine/specs`, gated the same
 * way `App.tsx`/`FleetLens.tsx` already gate it — a replay reports nothing
 * here, matching legacy's own "playback mode never starts that poll"), and
 * the same four external links legacy's modal footer carries. */
export function AboutDialog({ route, liveStatus, specs }: { route: Route; liveStatus: LiveTailStatus; specs: MachineSpecs | null }) {
  const ver = injectedMeta("darkmux-version");
  const schema = injectedMeta("darkmux-flow-schema");
  const live = isLiveRoute(route);
  const machine = live && specs?.machine_id ? specs.machine_id : "";
  const hardware =
    live && specs?.cpu_brand
      ? `${specs.cpu_brand}${specs.ram_total_bytes ? ` · ${Math.round(specs.ram_total_bytes / 1073741824)} GB` : ""}`
      : "";

  return (
    <Dialog id="imodalbg" titleId="about-title" title="about · darkmux">
      <div className="dialog__rrdetail" id="infobody">
        <Kv label="build" value={ver ?? ""} />
        <Kv label="flow schema" value={schema ?? ""} />
        <Kv label="connection" value={connectionText(route, liveStatus)} />
        <Kv label="mode" value={modeText(route)} />
        <Kv label="machine" value={machine} />
        <Kv label="hardware" value={hardware} />
        <div className="dialog__rule" />
        <div className="dialog__kv">
          <b>links</b>
          <span>
            <a href="https://github.com/kstrat2001/darkmux" target="_blank" rel="noopener">
              github
            </a>{" "}
            ·{" "}
            <a href="https://darkmux.com/guide/" target="_blank" rel="noopener">
              guide
            </a>{" "}
            ·{" "}
            <a href="https://darklyenergized.substack.com" target="_blank" rel="noopener">
              articles
            </a>{" "}
            ·{" "}
            <a href="https://darkmux.com/" target="_blank" rel="noopener">
              home
            </a>
          </span>
        </div>
      </div>
    </Dialog>
  );
}

function Kv({ label, value }: { label: string; value: string }) {
  if (!value) return null;
  return (
    <div className="dialog__kv">
      <b>{label}</b>
      <span>{value}</span>
    </div>
  );
}

/** `DATA_SOURCE`'s live/reconnecting arm (viewer.html:3465's `mode==="live"`
 * branch) — this app has no equivalent global, so this reads the SAME
 * `liveStatus` the masthead badge already renders (`LiveStatusBadge.tsx`),
 * naming the daemon-less/historical routes honestly rather than inventing a
 * `DATA_SOURCE`-shaped string this app doesn't track. */
function connectionText(route: Route, liveStatus: LiveTailStatus): string {
  if (isLiveRoute(route)) return liveStatus === "live" ? "live · connected" : "live · reconnecting";
  if (route.kind === "playback") return `flow · ${route.date ?? ""}`;
  if (route.kind === "session") return `flow · session ${route.sessionId}`;
  if (route.kind === "mission-redirect") return `flow · mission ${route.missionId}`;
  return "no records yet";
}

function modeText(route: Route): string {
  if (isLiveRoute(route)) return "live";
  if (route.kind === "playback") return `playback · ${route.date ?? ""}`;
  if (route.kind === "session" || route.kind === "mission-redirect") return "replay";
  return "";
}
