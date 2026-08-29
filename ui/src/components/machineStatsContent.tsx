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
import {
  Meter,
  compactMeterProps,
  fmtPct,
  simpleBand,
  angleForPct,
} from "./Meter";
import { InlineOrCells, type InlineOrCellsItem } from "./InlineOrCells";
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
  ThermalState,
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

/** (#2108, host-sample-shape v2) `mW` below 1000, `W` at one decimal from
 * 1000 up — matches the spec's own "W with one decimal for ≥1000 mW, mW
 * below" instruction. `null` (unmeasured channel) returns `null`, never a
 * zeroed placeholder — the caller hides the row instead. */
function fmtMw(mw: number | null): string | null {
  if (mw === null) return null;
  return mw >= 1000 ? `${(mw / 1000).toFixed(1)} W` : `${Math.round(mw)} mW`;
}

/** `energy_mwh` (milliwatt-hours) → "Energy (window)" — `Wh` at two
 * decimals once the reading crosses 1000 mWh, `mWh` (rounded) below. */
function fmtEnergyMwh(mwh: number | null): string | null {
  if (mwh === null) return null;
  return mwh >= 1000
    ? `${(mwh / 1000).toFixed(2)} Wh`
    : `${Math.round(mwh)} mWh`;
}

/** `gpu_mem_bytes` → `GB` at one decimal from 1 GB up, `MB` (one decimal)
 * below — the GPU's in-use system memory is typically tens to low
 * hundreds of MB, but a heavy workload can cross into GB. */
function fmtGpuMemBytes(bytes: number | null): string | null {
  if (bytes === null) return null;
  return bytes >= 1_000_000_000
    ? `${(bytes / 1_000_000_000).toFixed(1)} GB`
    : `${(bytes / 1_000_000).toFixed(1)} MB`;
}

/** `above_nominal_ms` (the window's time spent above the nominal thermal
 * state) → a short human duration. `0` reads as "0s" rather than being
 * hidden — a genuinely-zero reading is a real claim ("never left
 * nominal"), distinct from the field being entirely absent. */
function fmtAboveNominal(ms: number): string {
  if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
  return `${Math.round(ms / 60_000)} min`;
}

/** Title-cases a `ThermalState` for display — "nominal" → "Nominal". An
 * unrecognized future state (the `| string` fallback on the type) still
 * renders readably rather than throwing. */
function fmtThermalState(state: ThermalState): string {
  return state.length === 0 ? state : state[0].toUpperCase() + state.slice(1);
}

/** Semantic color bucket per the spec: nominal reads quiet (no alarm
 * color), fair is a caution (`--warn`), serious/critical are both the
 * same "this needs attention" severity (`--bad`) — matching this file's
 * existing three-bucket severity convention elsewhere in the app
 * (`is-ok`/`is-warn`/`is-bad`). An unrecognized state falls back to the
 * quiet/nominal treatment rather than alarming on something unknown. */
function thermalSeverityClass(state: ThermalState): string {
  if (state === "fair") return "thermal-pill--fair";
  if (state === "serious" || state === "critical") return "thermal-pill--bad";
  return "thermal-pill--nominal";
}

/** (#2108, host-sample-shape v2) The thermal/power/CPU-cluster rows —
 * host-level readings the daemon's continuous sampler produces
 * independently of any one dispatch, extracted into its own component so
 * `useMachineStatsContent`'s body stays readable. Every field is
 * independently optional (see `MachineLoad`'s own doc); each row hides
 * itself the instant its own source data is null rather than the whole
 * block gating on one field — a host that reports thermal but not power
 * still gets a thermal row. `null` `load` (daemon not sampling, closed
 * surface, or a pre-v2 daemon) renders nothing at all.
 *
 * Deliberately reads `load.now`/`load.window` DIRECTLY rather than going
 * through `effectiveHostAggregate`'s dispatch/daemon merge — that merge
 * exists to answer "what did THIS dispatch's own CPU/GPU/MEM look like,"
 * a question thermal/power/clusters don't have (they're never scoped to a
 * dispatch on the wire), so mission/dispatch routes render these rows too
 * whenever the daemon itself is reachable. */
function HostExtras({
  load,
  isMobile,
}: {
  load: MachineLoad | null;
  /** (#2108, operator finding — wrap fix) Threaded down from
   * `useMachineStatsContent`'s own `isMobile` input so the dotted-list
   * rows below can switch to `InlineOrCells`' cell-grid form at the same
   * breakpoint the rest of the drawer/lens uses. See that component's
   * own doc for why a phrase must never break mid-item. */
  isMobile: boolean;
}) {
  if (load === null) return null;
  const { thermal, power_mw, cpu_clusters } = load.now;
  // (#2108 review finding 7) `load.window` itself, and the v2-only
  // thermal/power/energy sub-fields on it, are typed as always-present —
  // but a pre-#2108 v1-shaped daemon/fixture (this file's sibling doc on
  // `effectiveHostAggregate` has the full story) can hand this component
  // a `window` missing all three. `?.`/`?? null` degrades each to "not
  // measured" (hides its row below) instead of throwing on `undefined`.
  const windowThermal = load.window?.thermal ?? null;
  const windowPower = load.window?.power_mw ?? null;
  const energyLine = fmtEnergyMwh(load.window?.energy_mwh ?? null);
  // `?? null` here too: a v1-shaped `now` predates `gpu_mhz`/
  // `gpu_mem_bytes` entirely, and `undefined` fails both formatters' `===
  // null` checks (`fmtGpuMemBytes` below), rendering a literal "NaN MHz ·
  // NaN MB" instead of hiding the row — caught by this file's own v1
  // render test.
  const gpuMhz = load.now.gpu_mhz ?? null;
  const gpuMem = fmtGpuMemBytes(load.now.gpu_mem_bytes ?? null);
  const gpuExtraItems: InlineOrCellsItem[] = [
    gpuMhz !== null
      ? { cellLabel: "MHz", cellValue: `${Math.round(gpuMhz)}`, inline: `${Math.round(gpuMhz)} MHz` }
      : null,
    gpuMem !== null ? { cellLabel: "memory", cellValue: gpuMem, inline: gpuMem } : null,
  ].filter((x): x is InlineOrCellsItem => x !== null);

  const powerTotalItems: InlineOrCellsItem[] = power_mw
    ? [
        { cellLabel: "now", cellValue: fmtMw(power_mw.total) ?? "—", inline: `${fmtMw(power_mw.total)} now` },
        ...(windowPower
          ? [
              {
                cellLabel: "avg",
                cellValue: fmtMw(windowPower.total?.mean ?? null) ?? "—",
                inline: `${fmtMw(windowPower.total?.mean ?? null)} avg`,
              },
              {
                cellLabel: "p95",
                cellValue: fmtMw(windowPower.total?.p95 ?? null) ?? "—",
                inline: `${fmtMw(windowPower.total?.p95 ?? null)} p95`,
              },
              {
                cellLabel: "max",
                cellValue: fmtMw(windowPower.total?.max ?? null) ?? "—",
                inline: `${fmtMw(windowPower.total?.max ?? null)} max`,
              },
            ]
          : []),
      ]
    : [];
  const channelItems: InlineOrCellsItem[] = power_mw
    ? [
        fmtMw(power_mw.cpu) !== null
          ? { cellLabel: "CPU", cellValue: fmtMw(power_mw.cpu)!, inline: `CPU ${fmtMw(power_mw.cpu)}` }
          : null,
        fmtMw(power_mw.gpu) !== null
          ? { cellLabel: "GPU", cellValue: fmtMw(power_mw.gpu)!, inline: `GPU ${fmtMw(power_mw.gpu)}` }
          : null,
        fmtMw(power_mw.ane) !== null
          ? { cellLabel: "ANE", cellValue: fmtMw(power_mw.ane)!, inline: `ANE ${fmtMw(power_mw.ane)}` }
          : null,
      ].filter((x): x is InlineOrCellsItem => x !== null)
    : [];

  return (
    <>
      {gpuExtraItems.length > 0 && (
        <div className="machine-drawer__gpu-extra">
          GPU <InlineOrCells items={gpuExtraItems} isMobile={isMobile} />
        </div>
      )}
      {thermal && (
        <div className="thermal-row">
          <span
            className={`thermal-pill ${thermalSeverityClass(thermal.state)}`}
          >
            {fmtThermalState(thermal.state)}
          </span>
          {thermal.cpu_speed_limit_pct < 100 && (
            <span className="thermal-row__limit">
              {`CPU speed limit ${Math.round(thermal.cpu_speed_limit_pct)}%`}
            </span>
          )}
          {windowThermal && (
            <div className="thermal-row__window">
              <InlineOrCells
                items={[
                  {
                    cellLabel: "worst",
                    cellValue: fmtThermalState(windowThermal.worst_state),
                    inline: `worst ${fmtThermalState(windowThermal.worst_state)}`,
                  },
                  {
                    cellLabel: "above nominal",
                    cellValue: fmtAboveNominal(windowThermal.above_nominal_ms),
                    inline: `${fmtAboveNominal(windowThermal.above_nominal_ms)} above nominal`,
                  },
                ]}
                isMobile={isMobile}
              />
            </div>
          )}
        </div>
      )}
      {(power_mw || energyLine) && (
        <div className="power-block">
          {power_mw && (
            <div className="dialog__kv">
              <b>Power</b>
              <InlineOrCells items={powerTotalItems} isMobile={isMobile} />
            </div>
          )}
          {power_mw && (
            <div className="dialog__kv">
              <b>Channels</b>
              <InlineOrCells items={channelItems} isMobile={isMobile} />
            </div>
          )}
          {energyLine && (
            <div className="dialog__kv">
              <b>Energy (window)</b>
              <span>{energyLine}</span>
            </div>
          )}
        </div>
      )}
      {cpu_clusters && cpu_clusters.length > 0 && (
        <div className="cluster-block">
          <div className="cluster-block__title">CPU clusters</div>
          <div className="meter-row">
            {cpu_clusters.map((c) => (
              <div className="cluster-tile" key={c.name}>
                <Meter
                  wrapperClassName="mm-gauge mm-gauge--compact"
                  width={COMPACT_METER_WIDTH}
                  height={COMPACT_METER_HEIGHT}
                  ariaLabel={`${c.name}: ${fmtPct(c.pct)}`}
                  label={c.name}
                  numerals={{ now: c.pct, avg: null, max: null }}
                  hideAvgMax
                  bands={simpleBand(
                    "mm-gauge-fill-compact",
                    "var(--accent, var(--good))",
                    c.pct,
                  )}
                  needleAngleDeg={c.pct == null ? undefined : angleForPct(c.pct)}
                />
                <div className="cluster-tile__caption">
                  {c.mhz !== null
                    ? `${c.cores} cores · ${Math.round(c.mhz)} MHz`
                    : `${c.cores} cores`}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </>
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
  // (#2108 review finding 7) `load.window` and its `{cpu,gpu,mem}_pct`
  // sub-objects are typed as always-present (the CURRENT/v2 wire shape,
  // `MachineLoad`'s own doc above), but the type only binds the compiler
  // — a daemon still on the pre-#2108 v1 shape (or a mismatched/hand-
  // edited fixture; see the sibling Rust-side fix in
  // `scripts/demo-env/build.py`) can hand this hook a `load` missing
  // `window` entirely, or a `window` missing its `*_pct` keys, and `.mean`
  // on `undefined` throws — taking the whole machine panel down with it.
  // Optional chaining + `?? null` degrades every reading to "not
  // measured" instead (this file's own absence-never-zero convention).
  return {
    cpu: {
      now: load.now.cpu_pct,
      avg: load.window?.cpu_pct?.mean ?? null,
      high: load.window?.cpu_pct?.max ?? null,
      p95: load.window?.cpu_pct?.p95 ?? null,
    },
    mem: {
      now: load.now.mem_pct,
      avg: load.window?.mem_pct?.mean ?? null,
      high: load.window?.mem_pct?.max ?? null,
      p95: load.window?.mem_pct?.p95 ?? null,
    },
    gpu: {
      now: load.now.gpu_pct,
      avg: load.window?.gpu_pct?.mean ?? null,
      high: load.window?.gpu_pct?.max ?? null,
      p95: load.window?.gpu_pct?.p95 ?? null,
    },
    count: load.window?.samples ?? 0,
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
  /** (#2108, operator finding — wrap fix) Whether the caller's own
   * surface is currently rendering the PHONE chrome — the same
   * `useIsMobile` breakpoint `MachineDrawer.tsx` already measures for its
   * own dialog/phone-drawer split, threaded down here so this shared body
   * can switch its own dotted-list rows (header line, thermal window,
   * power total/channels, the GPU MHz/memory line) to `InlineOrCells`'
   * cell-grid form at the SAME breakpoint, rather than re-measuring
   * `window.innerWidth` a second time in a hook that's already handed a
   * `nowMsOverride` for testability. */
  isMobile: boolean;
  /** Test-only override — production omits this and the hook ticks its
   * own clock (see [[useTickingNow]]). */
  nowMsOverride?: number;
}

export interface MachineStatsContent {
  /** identity + scope + meters/idle + rule + about — the WHOLE dialog/tab
   * body, ready to drop into a `<Dialog>` or a tab panel unchanged. */
  body: React.ReactNode;
  /** (#2108, operator finding — MachineLens/sheet unification) JUST the
   * gauges (CPU/GPU/MEM, or the idle line) plus `HostExtras`
   * (thermal/power/CPU-cluster) — no identity line, no scope label, no
   * "about" section. `MachineLens.tsx` renders THIS, not `body`, in place
   * of its own old flow-aggregation-only live-load section: the lens
   * already has its own header/ledger/peers/history, so pulling in the
   * drawer's full `body` would duplicate the identity/about content the
   * lens already shows in its own words. The sheet and the ⓘ modal keep
   * using `body` unchanged. */
  liveBlock: React.ReactNode;
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
  isMobile,
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
  // (#2108 review finding 7) `daemonLoad.window` is typed as always-present;
  // `?? 0` covers a v1-shaped/absent `window` the same way the sibling
  // guards above do (`daemonWindowLabel(0)` reads "last <1 min", the
  // correct degraded answer — never a thrown TypeError).
  const scopeLabel =
    !isMissionOrDispatch && daemonLoad != null
      ? daemonWindowLabel(daemonLoad.window?.span_ms ?? 0)
      : scope.scopeLabel;
  const samplerCostLine =
    !isMissionOrDispatch && daemonLoad != null
      ? `sampler cost ${Math.round(daemonLoad.now.sampler_cost_ms * 10) / 10} ms/sample`
      : null;

  // (#2108, host-sample-shape v2) Thermal/power/CPU-cluster rows are host
  // readings, not scoped to any one dispatch — they render off the raw
  // `daemonLoad` directly (never `agg`/`isIdle`, which are the
  // dispatch-vs-daemon MERGE this hook already does for CPU/GPU/MEM), so
  // they show on a mission/dispatch route too: "what is the HOST doing
  // right now" is orthogonal to "what did THIS dispatch use." Each row
  // hides independently on its own null field — see this module's own
  // `HostExtras` doc.
  const hostExtras = <HostExtras load={daemonLoad} isMobile={isMobile} />;

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

  // (#2108, operator finding — wrap fix) "MacBook-Pro · Apple M5 Max ·
  // 128 GB · darkmux 3.3.0 (ea3caf27)" wrapped after "128 GB ·" on a
  // phone. Mobile stacks TWO lines with no separators instead — machine
  // name, then hardware (which keeps its own short internal " · ", e.g.
  // "Apple M5 Max · 128 GB" — that one doesn't wrap on its own) — and
  // drops the version line entirely rather than trying to fit a third
  // fact: it's already the `build` row in the "about" kv block below,
  // so nothing is lost, only de-duplicated. Desktop keeps the single
  // dotted `headerLine` unchanged.
  const identityBlock = isMobile ? (
    (machineName || hardware) && (
      <div className="machine-drawer__identity machine-drawer__identity--mobile">
        {machineName && (
          <div className="machine-drawer__identity-line">{machineName}</div>
        )}
        {hardware && (
          <div className="machine-drawer__identity-line">{hardware}</div>
        )}
      </div>
    )
  ) : (
    headerLine && (
      <div className="machine-drawer__identity">{headerLine}</div>
    )
  );

  const statsBody = (
    <>
      {identityBlock}
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
      {hostExtras}
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
    </div>
  );

  const body = (
    <>
      {statsBody}
      <div className="dialog__rule" />
      {aboutSection}
    </>
  );

  const liveBlock = (
    <>
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
      {hostExtras}
    </>
  );

  return { body, liveBlock };
}
