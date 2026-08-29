/**
 * (#2107, "one modal" packet) The global machine-stats surface — reachable
 * from every route, and now the ONE modal for both "what's my machine
 * doing" and "about this build" — `AboutDialog.tsx` is DELETED, its fields
 * moved into this component's own "about" section rather than kept as a
 * second, separately-triggered dialog.
 *
 * TWO ENTIRELY DIFFERENT skins by viewport, not one skin styled two ways —
 * a real phone-testing pass (2026-08-29) found the original "one fixed
 * pill everywhere" design actively broken on a phone: the pill floated
 * OVER page content and collided with the masthead's own TODAY/LIVE badge.
 *
 * - **Desktop (>768px):** a small `machine · GPU 68%` pill PLUS the
 *   masthead's own ⓘ affordance (`Masthead.tsx`) both open the SAME
 *   dialog — literally the shared `<Dialog id="imodalbg">` shell
 *   (`Dialog.tsx`/`lib/dialogManager.ts`) every other dialog in this app
 *   (Filters, Notes) already uses, rather than a bespoke modal of this
 *   component's own. That is what makes "one modal" true in the way that
 *   actually matters: `tests/e2e/viewer-keyboard.spec.js`'s keyboard/
 *   focus-trap/single-Escape-closes-topmost coverage, written against the
 *   shared dialog system, keeps exercising a REAL dialog rather than one
 *   this packet quietly routed around. Open/closed is NOT persisted
 *   across page loads — `dialogManager`'s store never has been (Filters/
 *   Notes don't remember state either), and a stats panel that reopened
 *   itself on every fresh load would be the one dialog in this app that
 *   did.
 * - **Phone (≤768px):** NO floating pill or ⓘ at all — a bespoke
 *   full-width BOTTOM BAR that is part of the page's own layout
 *   (`.app-shell` reserves its height via `padding-bottom`, see
 *   `styles.css`). The bar shows the compact live numbers and a grab
 *   handle; tapping it or swiping up opens a bottom sheet the handle then
 *   lets you drag toward `~85vh`/`~92vh` (snapping on release) or drag
 *   back down to close. This half stays outside `dialogManager` — its
 *   drag-to-expand mechanics and always-visible bar don't fit that
 *   system's "exactly one modal, backdrop, portal" shape, and no existing
 *   test suite has a phone-shaped expectation of it.
 *
 * Both skins share the SAME data/scope logic (`lib/machineDrawerScope
 * .ts`) and the SAME three [[Meter]]s — only the chrome around them, and
 * which extra "about" fields desktop also shows, differs.
 *
 * **Idle state (phone feedback, 2026-08-29):** the sampler only runs
 * DURING a dispatch (#557/#1064), so an idle machine's rolling 10-minute
 * window is legitimately empty most of the time — three meters all
 * reading "—" is not informative, it is a missing state. When the current
 * scope has no samples, the meters are replaced by a short idle line
 * (`idle · no samples in the last 10 min`) plus, when one exists, the
 * most recent reading found ANYWHERE in the wider window and its age
 * (`last sample 1h ago`) — see `lib/machineDrawerScope.ts::
 * findLastKnownSample`.
 *
 * Static builds need no special-casing here: `routeRecords`/`flowWindow`
 * are handed in from `App.tsx`, which already resolves both from the
 * committed fixtures on a static build and from the daemon otherwise (see
 * that file's own `getSource()`/`isStaticBuild` doc) — this component only
 * ever reads the records it's given.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { Dialog } from "./Dialog";
import { openModalEl, closeOpenModal, useOpenModalId } from "../lib/dialogManager";
import { Meter, compactMeterProps, fmtPct, COMPACT_METER_WIDTH, COMPACT_METER_HEIGHT } from "./Meter";
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
 * aging samples out, and the pill/bar's live value stays current, even
 * during a quiet period with no new flow records landing. Mirrors
 * `lenses/mission/MissionGraphLens.tsx`'s own `useNow` — a local tick
 * rather than depending on the app shell's per-render `Date.now()`, so
 * this component is self-contained and testable with an injected `nowMs`. */
function useTickingNow(everyMs: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), everyMs);
    return () => clearInterval(id);
  }, [everyMs]);
  return now;
}

/** The phone/desktop chrome split — the SAME 768px breakpoint
 * `styles.css`'s `.machine-drawer`/`.machine-bottombar` media query uses,
 * so the JS-rendered markup and the CSS agree on where the line is. */
const MOBILE_BREAKPOINT = 768;

function useIsMobile(breakpoint: number = MOBILE_BREAKPOINT): boolean {
  const [isMobile, setIsMobile] = useState(() => (typeof window !== "undefined" ? window.innerWidth <= breakpoint : false));
  useEffect(() => {
    const onResize = () => setIsMobile(window.innerWidth <= breakpoint);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [breakpoint]);
  return isMobile;
}

/** `DATA_SOURCE`'s live/reconnecting arm (viewer.html:3465's `mode==="live"`
 * branch), moved verbatim from the retired `AboutDialog.tsx` — this app has
 * no equivalent global, so this reads the SAME `liveStatus` the masthead
 * badge already renders (`LiveStatusBadge.tsx`), naming the daemon-less/
 * historical routes honestly rather than inventing a `DATA_SOURCE`-shaped
 * string this app doesn't track. */
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

export interface MachineDrawerProps {
  route: Route;
  routeRecords: FlowRecord[];
  flowWindow: FlowRecord[];
  localUid: string | null;
  /** For the header line's identity/hardware — the SAME two inputs
   * `nameOf`/`specOf` (`lib/flow.ts`, `lenses/fleet/cards.ts` — "hardware
   * as the fleet card prints it") already take everywhere else in this
   * app, so this reads identically to the fleet card and the machine
   * lens rather than re-deriving its own copy. */
  liveMachines: Map<string, PresenceBeat>;
  specs: MachineSpecs | null;
  /** For the "about" section's connection/mode rows — the retired
   * `AboutDialog`'s own inputs, unchanged. */
  liveStatus: LiveTailStatus;
  /** Test-only override — production omits this and the component ticks
   * its own clock (see [[useTickingNow]]). */
  nowMsOverride?: number;
  /** Test-only override for the phone/desktop split — production omits
   * this and measures `window.innerWidth` (see [[useIsMobile]]). */
  isMobileOverride?: boolean;
}

export function MachineDrawer({
  route,
  routeRecords,
  flowWindow,
  localUid,
  liveMachines,
  specs,
  liveStatus,
  nowMsOverride,
  isMobileOverride,
}: MachineDrawerProps) {
  const tickedNow = useTickingNow(5000);
  const nowMs = nowMsOverride ?? tickedNow;
  const measuredIsMobile = useIsMobile();
  const isMobile = isMobileOverride ?? measuredIsMobile;

  // Desktop's open/closed lives entirely in `dialogManager` (see this
  // module's own doc for why) — `useOpenModalId()` is the SAME reactive
  // subscription `<Dialog>` itself uses, so this component and the shell
  // it renders through can never disagree about whether they're open.
  const desktopOpen = useOpenModalId() === "imodalbg";
  // Mobile's own bespoke sheet keeps its own local, unpersisted state —
  // drag-to-expand and the always-visible bar don't fit `dialogManager`'s
  // shape (see this module's own doc).
  const [mobileOpen, setMobileOpen] = useState(false);
  const [full, setFull] = useState(false);
  const barRef = useRef<HTMLButtonElement | null>(null);

  const scope = useMemo(
    () => resolveDrawerScope(route, routeRecords, flowWindow, localUid, nowMs),
    [route, routeRecords, flowWindow, localUid, nowMs],
  );
  const agg = useMemo(() => aggregateHostSamples(scope.samples), [scope.samples]);
  const isIdle = scope.samples.length === 0;

  // (#2107) The header line — identity + version. Built from data this app
  // ALREADY tracks, the same way `MachineLens.tsx` does:
  //  - machine name / hardware: `nameOf`/`specOf`, scoped to the LOCAL
  //    machine (this header describes the physical host, regardless of
  //    which route's samples the meters below are showing).
  //  - `darkmux <version>` from the SAME `injectedMeta("darkmux-version")`
  //    the "about" section's `build` row also reads — `build_version()`
  //    already embeds the git SHA in that string server-side
  //    (`x.y.z (sha✱)`), so no separate field is needed for it.
  // Deliberately NOT included: a "daemon since <uptime>" figure. No route
  // in this app serves a process-start timestamp (verified by reading
  // `crates/darkmux-serve/src/lib.rs` — there is no `/version`/`/status`
  // handler and no uptime field anywhere in the served metas or
  // `/machine/specs`). Fabricating one client-side (e.g. "since this tab
  // connected") would assert something materially different from — and
  // easily confused with — the daemon's actual process uptime, which is
  // exactly the kind of confidently-wrong number this app's own honesty
  // rules (see `MachineHealthRegion.tsx`'s "absence, never zero") exist to
  // refuse. Left as a named follow-up rather than shipped as a guess.
  const machineName = localUid != null ? nameOf(flowWindow, liveMachines, localUid) : null;
  const hardware = localUid != null ? specOf(flowWindow, liveMachines, specs, localUid) : "";
  const verMeta = injectedMeta("darkmux-version");
  const schemaMeta = injectedMeta("darkmux-flow-schema");
  const headerLine = [machineName, hardware, verMeta ? `darkmux ${verMeta}` : null].filter(Boolean).join(" · ");

  // (phone feedback) The OPEN sheet's handle (mobile only): drag DOWN past
  // a threshold closes (with a live translateY preview while dragging);
  // drag UP past a threshold snaps to the "full" (~92vh) height instead of
  // the default content-sized (~85vh, capped) one.
  const dragStartY = useRef<number | null>(null);
  const [dragPreviewPx, setDragPreviewPx] = useState(0);
  const onHandleTouchStart = (e: React.TouchEvent) => {
    dragStartY.current = e.touches[0]?.clientY ?? null;
  };
  const onHandleTouchMove = (e: React.TouchEvent) => {
    const startY = dragStartY.current;
    if (startY == null) return;
    const dy = (e.touches[0]?.clientY ?? startY) - startY; // >0 = dragging down
    if (dy > 0) setDragPreviewPx(dy);
    else if (dy < -30) setFull(true); // dragged up past the threshold — snap to full immediately
  };
  const onHandleTouchEnd = (e: React.TouchEvent) => {
    const startY = dragStartY.current;
    dragStartY.current = null;
    setDragPreviewPx(0);
    if (startY == null) return;
    const endY = e.changedTouches[0]?.clientY ?? startY;
    if (endY - startY > 80) setMobileOpen(false);
  };

  // (phone feedback) The CLOSED bar: tap opens; a clear swipe-up also
  // opens (the bar's own affordance, distinct from the open sheet's own
  // handle above).
  const barTouchStartY = useRef<number | null>(null);
  const onBarTouchStart = (e: React.TouchEvent) => {
    barTouchStartY.current = e.touches[0]?.clientY ?? null;
  };
  const onBarTouchEnd = (e: React.TouchEvent) => {
    const startY = barTouchStartY.current;
    barTouchStartY.current = null;
    if (startY == null) return;
    const endY = e.changedTouches[0]?.clientY ?? startY;
    if (startY - endY > 30) setMobileOpen(true);
  };

  useEffect(() => {
    if (!mobileOpen) setFull(false);
  }, [mobileOpen]);
  useEffect(() => {
    if (!mobileOpen) barRef.current?.focus({ preventScroll: true });
  }, [mobileOpen]);

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

  // (phone feedback) Idle wording depends on WHICH scope is empty — a
  // finished dispatch/mission that genuinely never sampled reads
  // differently than an idle machine on the rolling window, where
  // `lastKnown` (a reading from outside the 10-minute cutoff) is also
  // meaningful.
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

  // (#2107 "one modal" packet) The retired `AboutDialog`'s own fields,
  // folded in as this dialog's lower section — every field that dialog
  // rendered: build, flow schema, connection, mode, machine, hardware,
  // links. `machine`/`hardware` here are gated the SAME way AboutDialog's
  // were (`isLiveRoute(route)` — a replay reports nothing, matching
  // legacy's "playback mode never starts that poll"), which is why they
  // can differ from `headerLine` above (that one uses `localUid`
  // unconditionally; this pair is the retired dialog's own live-route
  // gate, preserved so a replay's about section reads exactly as it did).
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

  if (isMobile) {
    return (
      <>
        {!mobileOpen && (
          <button
            ref={barRef}
            type="button"
            className="machine-bottombar"
            data-act="machine-bottombar"
            aria-haspopup="dialog"
            aria-expanded={false}
            onClick={() => setMobileOpen(true)}
            onTouchStart={onBarTouchStart}
            onTouchEnd={onBarTouchEnd}
          >
            <div className="machine-bottombar__handle-bar" />
            <div className="machine-bottombar__numbers">{compactLine}</div>
          </button>
        )}
        {mobileOpen && (
          <>
            <div className="machine-drawer__backdrop" data-act="machine-drawer-backdrop" onClick={() => setMobileOpen(false)} />
            <div
              className={`machine-drawer${full ? " machine-drawer--full" : ""}`}
              role="dialog"
              aria-modal="true"
              aria-label="Machine stats"
              style={dragPreviewPx > 0 ? { transform: `translateY(${dragPreviewPx}px)`, transition: "none" } : undefined}
            >
              <div
                className="machine-drawer__handle"
                data-act="machine-drawer-handle"
                onTouchStart={onHandleTouchStart}
                onTouchMove={onHandleTouchMove}
                onTouchEnd={onHandleTouchEnd}
              >
                <div className="machine-drawer__handle-bar" />
              </div>
              <div className="machine-drawer__head">
                <span className="machine-drawer__title">Machine stats</span>
                <button
                  type="button"
                  className="machine-drawer__close"
                  data-act="machine-drawer-close"
                  aria-label="Close"
                  onClick={() => setMobileOpen(false)}
                >
                  ×
                </button>
              </div>
              <div className="machine-drawer__body">
                {statsBody}
                <div className="dialog__rule" />
                {aboutSection}
              </div>
            </div>
          </>
        )}
      </>
    );
  }

  return (
    <>
      <button
        type="button"
        className="machine-pill"
        data-act="machine-drawer-pill"
        aria-haspopup="dialog"
        aria-expanded={desktopOpen}
        onClick={() => (desktopOpen ? closeOpenModal() : openModalEl("imodalbg"))}
      >
        <span className="machine-pill__dim">machine ·</span> GPU {fmtPct(gpuNow)}
      </button>
      <Dialog id="imodalbg" titleId="machine-stats-title" title="Machine stats">
        {statsBody}
        <div className="dialog__rule" />
        {aboutSection}
      </Dialog>
    </>
  );
}
