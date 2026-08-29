/**
 * (#2107 tabbed-drawer packet) The machine-stats BODY — identity line,
 * scope label, meters-or-idle-state, and the "about" section — extracted
 * out of `MachineDrawer.tsx` so it can be reused verbatim by TWO renderers
 * that need the exact same content in different chrome:
 *
 * - `MachineDrawer.tsx`'s desktop `<Dialog id="imodalbg">` body (unchanged
 *   from before this packet — same JSX, now built here instead of inline).
 * - `PhoneDrawer.tsx`'s Machine tab panel (new this packet — the tabbed
 *   bottom drawer's Machine tab is this exact content, not a
 *   re-derivation of it, per the "reuse the component; do not duplicate"
 *   instruction).
 *
 * Also the ONE place the compact bar label (`CPU 34 · GPU 68 · MEM 62`)
 * is computed — both the desktop pill and the phone drawer's closed
 * Machine tab show that same string, ticking on the same 5s clock.
 *
 * A hook, not a component: it needs `useState`/`useEffect`/`useMemo`
 * (the ticking clock, the scope resolution, the aggregation), and callers
 * need its JSX ALONGSIDE other sibling JSX they render themselves (a pill
 * button, a set of tab buttons) — wrapping it in an component that always
 * renders its OWN top-level element would force an extra wrapper div into
 * both call sites for no reason.
 */
import { useEffect, useMemo, useState } from "react";
import { Meter, compactMeterProps, fmtPct } from "./Meter";
import { COMPACT_METER_WIDTH, COMPACT_METER_HEIGHT } from "./Meter";
import { aggregateHostSamples } from "../lib/hostStats";
import { resolveDrawerScope } from "../lib/machineDrawerScope";
import { injectedMeta } from "../lib/injectedMeta";
import { nameOf } from "../lib/flow";
import { relAgoFrom } from "../lib/format";
import { specOf } from "../lenses/fleet/cards";
import { isLiveRoute, type Route } from "../lib/route";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../types/handwritten";

/** Re-render on a light interval so the rolling 10-minute window keeps
 * aging samples out, and the compact line's live value stays current, even
 * during a quiet period with no new flow records landing. Mirrors
 * `lenses/mission/MissionGraphLens.tsx`'s own `useNow` — a local tick
 * rather than depending on the app shell's per-render `Date.now()`, so
 * this stays testable with an injected `nowMs`. */
function useTickingNow(everyMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), everyMs);
    return () => clearInterval(id);
  }, [everyMs]);
  return now;
}

/** `DATA_SOURCE`'s live/reconnecting arm — see `MachineDrawer.tsx`'s
 * pre-extraction doc for the full history; unchanged by this move. */
function connectionText(route: Route, liveStatus: LiveTailStatus): string {
  if (isLiveRoute(route)) return liveStatus === "live" ? "live · connected" : "live · reconnecting";
  if (route.kind === "playback") return `flow · ${route.date ?? ""}`;
  if (route.kind === "dispatch") return `flow · dispatch ${route.dispatchId}`;
  if (route.kind === "mission") return `flow · mission ${route.missionId}`;
  return "no records yet";
}

function modeText(route: Route): string {
  if (isLiveRoute(route)) return "live";
  if (route.kind === "playback") return `playback · ${route.date ?? ""}`;
  if (route.kind === "dispatch" || route.kind === "mission") return "replay";
  return "";
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

export interface MachineStatsInput {
  route: Route;
  routeRecords: FlowRecord[];
  flowWindow: FlowRecord[];
  localUid: string | null;
  liveMachines: Map<string, PresenceBeat>;
  specs: MachineSpecs | null;
  liveStatus: LiveTailStatus;
  /** Test-only override — production omits this and the hook ticks its
   * own clock (see [[useTickingNow]]). */
  nowMsOverride?: number;
}

export interface MachineStatsContent {
  /** `CPU 34 · GPU 68 · MEM 62` — the desktop pill's and the phone bar's
   * Machine tab's shared label. */
  compactLine: string;
  /** For the desktop pill's own `GPU {fmtPct(gpuNow)}` text. */
  gpuNow: number | null;
  /** identity + scope + meters/idle + rule + about — the WHOLE dialog/tab
   * body, ready to drop into a `<Dialog>` or a tab panel unchanged. */
  body: React.ReactNode;
}

export function useMachineStatsContent({
  route,
  routeRecords,
  flowWindow,
  localUid,
  liveMachines,
  specs,
  liveStatus,
  nowMsOverride,
}: MachineStatsInput): MachineStatsContent {
  const tickedNow = useTickingNow(5000);
  const nowMs = nowMsOverride ?? tickedNow;

  const scope = useMemo(
    () => resolveDrawerScope(route, routeRecords, flowWindow, localUid, nowMs),
    [route, routeRecords, flowWindow, localUid, nowMs],
  );
  const agg = useMemo(() => aggregateHostSamples(scope.samples), [scope.samples]);
  const isIdle = scope.samples.length === 0;

  const machineName = localUid != null ? nameOf(flowWindow, liveMachines, localUid) : null;
  const hardware = localUid != null ? specOf(flowWindow, liveMachines, specs, localUid) : "";
  const verMeta = injectedMeta("darkmux-version");
  const schemaMeta = injectedMeta("darkmux-flow-schema");
  const headerLine = [machineName, hardware, verMeta ? `darkmux ${verMeta}` : null].filter(Boolean).join(" · ");

  const gpuNow = agg.gpu.now;
  const compactLine = `CPU ${fmtPct(agg.cpu.now)} · GPU ${fmtPct(agg.gpu.now)} · MEM ${fmtPct(agg.mem.now)}`;

  const meters = (
    <div className="meter-row">
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`CPU: ${scope.scopeLabel}`}
        {...compactMeterProps("CPU", "mm-gauge-fill-compact", "var(--accent, var(--good))", agg.cpu)}
      />
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`GPU: ${scope.scopeLabel}`}
        {...compactMeterProps("GPU", "mm-gauge-fill-compact", "var(--accent, var(--good))", agg.gpu)}
      />
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`MEM: ${scope.scopeLabel}`}
        {...compactMeterProps("MEM", "mm-gauge-fill-compact", "var(--accent, var(--good))", agg.mem)}
      />
    </div>
  );

  const idleLine =
    scope.lastKnown == null && scope.scopeLabel !== "last 10 min"
      ? `no host samples for ${scope.scopeLabel}`
      : "idle · no samples in the last 10 min";
  const lastKnownLine = scope.lastKnown
    ? `last sample ${relAgoFrom(nowMs, scope.lastKnown.ts)} — CPU ${fmtPct(scope.lastKnown.point.cpu ?? null)} · GPU ${fmtPct(scope.lastKnown.point.gpu ?? null)} · MEM ${fmtPct(scope.lastKnown.point.mem ?? null)}`
    : null;

  const statsBody = (
    <>
      {headerLine && <div className="machine-drawer__identity">{headerLine}</div>}
      <div className="machine-drawer__scope">{scope.scopeLabel}</div>
      {isIdle ? (
        <div className="machine-drawer__idle">
          <div className="machine-drawer__idle-line">{idleLine}</div>
          {lastKnownLine && <div className="machine-drawer__lastknown">{lastKnownLine}</div>}
        </div>
      ) : (
        meters
      )}
    </>
  );

  const aboutLive = isLiveRoute(route);
  const aboutMachine = aboutLive && specs?.machine_id ? specs.machine_id : "";
  const aboutHardware =
    aboutLive && specs?.cpu_brand
      ? `${specs.cpu_brand}${specs.ram_total_bytes ? ` · ${Math.round(specs.ram_total_bytes / 1073741824)} GB` : ""}`
      : "";
  const aboutSection = (
    <div className="dialog__rrdetail">
      <Kv label="build" value={verMeta ?? ""} />
      <Kv label="flow schema" value={schemaMeta ?? ""} />
      <Kv label="connection" value={connectionText(route, liveStatus)} />
      <Kv label="mode" value={modeText(route)} />
      <Kv label="machine" value={aboutMachine} />
      <Kv label="hardware" value={aboutHardware} />
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
  );

  const body = (
    <>
      {statsBody}
      <div className="dialog__rule" />
      {aboutSection}
    </>
  );

  return { compactLine, gpuNow, body };
}
