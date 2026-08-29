/**
 * (#2107 tabbed-drawer packet, revised #2107/#1833 "open-gated" packet) The
 * machine-stats BODY — identity line, scope label, meters-or-idle-state,
 * and the "about" section — extracted out of `MachineDrawer.tsx` so it can
 * be reused verbatim by TWO renderers that need the exact same content in
 * different chrome:
 *
 * - `MachineDrawer.tsx`'s desktop `<Dialog id="imodalbg">` body (unchanged
 *   from before this packet — same JSX, now built here instead of inline).
 * - `PhoneDrawer.tsx`'s Machine tab panel (new this packet — the tabbed
 *   bottom drawer's Machine tab is this exact content, not a
 *   re-derivation of it, per the "reuse the component; do not duplicate"
 *   instruction).
 *
 * **No more compact bar label.** Both the desktop pill and the phone
 * drawer's closed Machine tab used to show a live `CPU 34 · GPU 68 · MEM
 * 62` line; the operator found it "looks too busy" for a resting
 * indicator, so both now read a static `Machine info` — see
 * `MachineDrawer.tsx`/`PhoneDrawer.tsx`. Live numbers exist ONLY inside the
 * opened body this hook returns.
 *
 * **Polling is gated on `isOpen`.** This hook is called unconditionally
 * from `MachineDrawer.tsx` (Rules of Hooks — it can't be called only when
 * a dialog happens to be open), but the daemon poll it drives
 * (`useDaemonLoad`) must NOT run while nobody can see the result. The
 * caller passes whether ITS surface — the desktop dialog, or the phone
 * drawer's Machine tab — is currently visible; see `MachineStatsInput
 * .isOpen`'s own doc for how that's threaded up from `PhoneDrawer.tsx`,
 * which owns its own open/tab state internally.
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
import { aggregateHostSamples, type HostAggregate } from "../lib/hostStats";
import { resolveDrawerScope } from "../lib/machineDrawerScope";
import { injectedMeta } from "../lib/injectedMeta";
import { nameOf } from "../lib/flow";
import { relAgoFrom } from "../lib/format";
import { specOf } from "../lenses/fleet/cards";
import { isLiveRoute, type Route } from "../lib/route";
import { useDaemonLoad } from "../hooks/useDaemonLoad";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type {
  FlowRecord,
  MachineLoad,
  MachineSpecs,
  PresenceBeat,
} from "../types/handwritten";

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
  if (isLiveRoute(route))
    return liveStatus === "live" ? "live · connected" : "live · reconnecting";
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

/** (#2107, #1833) Merge the daemon's continuous `load` reading with the
 * route-scoped per-dispatch aggregate into the ONE `HostAggregate` the
 * meters render — a pure function so the merge rule is unit-testable
 * without mounting anything.
 *
 * - Mission/dispatch route: `avg`/`high`/`p95` stay the dispatch's OWN
 *   samples (that scope IS the run's own record set — the daemon's window
 *   spans unrelated time before/after it); only `now` is overridden by the
 *   daemon's live reading when present, since the daemon is always the
 *   freshest possible "right this instant" number.
 * - Every other route: the daemon's `window` IS the aggregate (mean → avg,
 *   max → high, p95 → p95), and `now` comes from the daemon too — the
 *   daemon samples continuously regardless of whether a dispatch happens
 *   to be running, so it is always the more current answer than a rolling
 *   slice of per-dispatch flow records.
 * - No daemon `load` at all (older daemon, disabled sampler, or the fetch
 *   hasn't resolved yet): unchanged fallback to the dispatch-derived
 *   aggregate — today's pre-#2107 behavior, byte for byte.
 */
export function effectiveHostAggregate(
  isMissionOrDispatch: boolean,
  dispatchAgg: HostAggregate,
  load: MachineLoad | null,
): HostAggregate {
  if (load == null) return dispatchAgg;
  if (isMissionOrDispatch) {
    return {
      cpu: { ...dispatchAgg.cpu, now: load.now.cpu_pct },
      mem: { ...dispatchAgg.mem, now: load.now.mem_pct },
      gpu: { ...dispatchAgg.gpu, now: load.now.gpu_pct },
      count: dispatchAgg.count,
    };
  }
  return {
    cpu: {
      now: load.now.cpu_pct,
      avg: load.window.cpu.mean_pct,
      high: load.window.cpu.max_pct,
      p95: load.window.cpu.p95_pct,
    },
    mem: {
      now: load.now.mem_pct,
      avg: load.window.mem.mean_pct,
      high: load.window.mem.max_pct,
      p95: load.window.mem.p95_pct,
    },
    gpu: {
      now: load.now.gpu_pct,
      avg: load.window.gpu.mean_pct,
      high: load.window.gpu.max_pct,
      p95: load.window.gpu.p95_pct,
    },
    count: load.window.samples,
  };
}

/** (#2107, #1833, warm-up finding) Label the daemon sampler's ACTUAL window
 * span — never a hardcoded "last 10 min" the daemon may not have earned
 * yet. The ring holds up to 10 minutes of history (`RING_CAPACITY` in
 * `crates/darkmux-serve/src/host_sampler.rs`), but a daemon that just
 * started (or one an operator just restarted) has sampled for less than
 * that, and `load.window.span_ms` says exactly how much — this formats
 * THAT number rather than the ring's ceiling, so the label never claims
 * more history than the meters actually average over. Rounds to the
 * nearest minute; a span under a minute reads as "less than 1 min" rather
 * than rounding down to a misleading "0 min". Capped at 10 (the ring's own
 * ceiling) so a clock/measurement wobble can't read "11 min". */
export function daemonWindowLabel(spanMs: number): string {
  if (spanMs < 60_000) return "last <1 min · daemon sampler";
  const minutes = Math.min(10, Math.round(spanMs / 60_000));
  return `last ${minutes} min · daemon sampler`;
}

export interface MachineStatsInput {
  route: Route;
  routeRecords: FlowRecord[];
  flowWindow: FlowRecord[];
  localUid: string | null;
  liveMachines: Map<string, PresenceBeat>;
  specs: MachineSpecs | null;
  liveStatus: LiveTailStatus;
  /** (#2107, #1833) Whether the caller's own surface — the desktop
   * `<Dialog id="imodalbg">`, or the phone drawer's Machine tab — is
   * currently OPEN (visible to the operator). Gates `useDaemonLoad`'s
   * polling: `false` means zero `/machine/resources` fetches, `true`
   * starts polling at `DAEMON_LOAD_POLL_MS` immediately. This hook is
   * called unconditionally regardless of open state (Rules of Hooks), so
   * the caller — which DOES know whether it's currently showing this
   * content — has to say so explicitly rather than the hook guessing. */
  isOpen: boolean;
  /** Test-only override — production omits this and the hook ticks its
   * own clock (see [[useTickingNow]]). */
  nowMsOverride?: number;
}

export interface MachineStatsContent {
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
  isOpen,
  nowMsOverride,
}: MachineStatsInput): MachineStatsContent {
  const tickedNow = useTickingNow(5000);
  const nowMs = nowMsOverride ?? tickedNow;

  const scope = useMemo(
    () => resolveDrawerScope(route, routeRecords, flowWindow, localUid, nowMs),
    [route, routeRecords, flowWindow, localUid, nowMs],
  );
  const dispatchAgg = useMemo(
    () => aggregateHostSamples(scope.samples),
    [scope.samples],
  );
  const isMissionOrDispatch =
    route.kind === "mission" || route.kind === "dispatch";
  // (#2107, #1833) The daemon's continuous host sampler — polled ONLY
  // while `isOpen` (see [[MachineStatsInput.isOpen]]'s own doc), so
  // "idle · no samples" between dispatches stops being the default state
  // on every non-mission/non-dispatch view WHILE OPEN, without polling a
  // number nobody can see while closed. `null` when disabled/unreachable/
  // not-yet-resolved/closed, in which case [[effectiveHostAggregate]]
  // falls back to the pre-#2107 dispatch-derived aggregate unchanged.
  const daemonLoad = useDaemonLoad(isOpen);
  const agg = useMemo(
    () => effectiveHostAggregate(isMissionOrDispatch, dispatchAgg, daemonLoad),
    [isMissionOrDispatch, dispatchAgg, daemonLoad],
  );
  // A mission/dispatch route is idle only when that run genuinely has no
  // samples of its own (the daemon's `now` override above doesn't change
  // that — the daemon isn't scoped to the run). Every other route is idle
  // only when there's neither a dispatch-derived rolling sample NOR a
  // daemon reading — once the daemon is sampling continuously, a
  // non-mission/non-dispatch view is never idle again.
  const isIdle = isMissionOrDispatch
    ? scope.samples.length === 0
    : scope.samples.length === 0 && daemonLoad == null;

  const machineName =
    localUid != null ? nameOf(flowWindow, liveMachines, localUid) : null;
  const hardware =
    localUid != null ? specOf(flowWindow, liveMachines, specs, localUid) : "";
  const verMeta = injectedMeta("darkmux-version");
  const schemaMeta = injectedMeta("darkmux-flow-schema");
  const headerLine = [
    machineName,
    hardware,
    verMeta ? `darkmux ${verMeta}` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  // (#2107, #1833, warm-up finding) On a non-mission/non-dispatch view where
  // the daemon actually supplies the aggregate, the label reflects the
  // ring's ACTUAL span (`daemonWindowLabel`) — never a hardcoded "last 10
  // min" claim the ring may not have earned yet. Every other case keeps
  // `scope.scopeLabel` unchanged ("this mission" / "this dispatch" / the
  // pre-daemon "last 10 min" fallback).
  const scopeLabel =
    !isMissionOrDispatch && daemonLoad != null
      ? daemonWindowLabel(daemonLoad.window.span_ms)
      : scope.scopeLabel;
  const samplerCostLine =
    !isMissionOrDispatch && daemonLoad != null
      ? `sampler cost ${Math.round(daemonLoad.sampler_cost_ms_mean * 10) / 10} ms/sample`
      : null;

  const meters = (
    <div className="meter-row">
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`CPU: ${scopeLabel}`}
        {...compactMeterProps(
          "CPU",
          "mm-gauge-fill-compact",
          "var(--accent, var(--good))",
          agg.cpu,
        )}
      />
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`GPU: ${scopeLabel}`}
        {...compactMeterProps(
          "GPU",
          "mm-gauge-fill-compact",
          "var(--accent, var(--good))",
          agg.gpu,
        )}
      />
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        width={COMPACT_METER_WIDTH}
        height={COMPACT_METER_HEIGHT}
        ariaLabel={`MEM: ${scopeLabel}`}
        {...compactMeterProps(
          "MEM",
          "mm-gauge-fill-compact",
          "var(--accent, var(--good))",
          agg.mem,
        )}
      />
    </div>
  );

  // (#2107, #1833) On a non-mission/non-dispatch view with NO daemon load at
  // all — an older daemon that predates the sampler, one disabled via
  // `runtime.host_sampler_interval_ms: 0`, or a fixture/fetch that hasn't
  // resolved (or hasn't been ASKED — see `isOpen`) — the wording says so
  // explicitly rather than reusing the generic "idle" line, which used to be
  // the ONLY reason that line ever showed on a rolling-scope view and is now
  // a narrower, more honest claim.
  const idleLine =
    scope.lastKnown == null && scope.scopeLabel !== "last 10 min"
      ? `no host samples for ${scope.scopeLabel}`
      : !isMissionOrDispatch && daemonLoad == null
        ? "daemon does not sample yet"
        : "idle · no samples in the last 10 min";
  const lastKnownLine = scope.lastKnown
    ? `last sample ${relAgoFrom(nowMs, scope.lastKnown.ts)} — CPU ${fmtPct(scope.lastKnown.point.cpu ?? null)} · GPU ${fmtPct(scope.lastKnown.point.gpu ?? null)} · MEM ${fmtPct(scope.lastKnown.point.mem ?? null)}`
    : null;

  const statsBody = (
    <>
      {headerLine && (
        <div className="machine-drawer__identity">{headerLine}</div>
      )}
      <div className="machine-drawer__scope">{scopeLabel}</div>
      {samplerCostLine && (
        <div className="machine-drawer__sampler-cost">{samplerCostLine}</div>
      )}
      {isIdle ? (
        <div className="machine-drawer__idle">
          <div className="machine-drawer__idle-line">{idleLine}</div>
          {lastKnownLine && (
            <div className="machine-drawer__lastknown">{lastKnownLine}</div>
          )}
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
          <a
            href="https://github.com/kstrat2001/darkmux"
            target="_blank"
            rel="noopener"
          >
            github
          </a>{" "}
          ·{" "}
          <a href="https://darkmux.com/guide/" target="_blank" rel="noopener">
            guide
          </a>{" "}
          ·{" "}
          <a
            href="https://darklyenergized.substack.com"
            target="_blank"
            rel="noopener"
          >
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

  return { body };
}
