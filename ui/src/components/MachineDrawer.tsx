/**
 * (#2107) The global machine-stats drawer/modal.
 *
 * A masthead pill (`machine · GPU 68%`, the live CURRENT value) reachable
 * from every route — not scoped to any one lens. Tapping/clicking it opens
 * a panel with three [[Meter]]s (CPU/GPU/MEM), each showing `now`/`avg`/
 * `max` over a WINDOW that depends on the current route (see
 * `lib/machineDrawerScope.ts`'s own doc): a mission's or dispatch's own
 * samples when the operator is looking at one, else a rolling last-10-
 * minute tail of the live flow window (this machine only).
 *
 * One component, two skins via CSS alone (`styles.css`'s `.machine-drawer`
 * + its `@media (max-width: 768px)` override) — desktop gets a centered
 * modal (backdrop, Escape/backdrop-click dismiss, `aria-modal`, a minimal
 * focus move on open/close); phones get the same DOM as a bottom sheet
 * (a drag handle, swipe-down dismiss). Open/closed is remembered per
 * viewer in `localStorage` (`lib/machineDrawerStorage.ts`) — the pill
 * itself is ALWAYS rendered, open or closed.
 *
 * Static builds need no special-casing here: `routeRecords`/`flowWindow`
 * are handed in from `App.tsx`, which already resolves both from the
 * committed fixtures on a static build and from the daemon otherwise (see
 * that file's own `getSource()`/`isStaticBuild` doc) — this component only
 * ever reads the records it's given.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { Meter } from "./Meter";
import { aggregateHostSamples } from "../lib/hostStats";
import { resolveDrawerScope } from "../lib/machineDrawerScope";
import { initDrawerOpen, persistDrawerOpen } from "../lib/machineDrawerStorage";
import { injectedMeta } from "../lib/injectedMeta";
import { nameOf } from "../lib/flow";
import { specOf } from "../lenses/fleet/cards";
import type { Route } from "../lib/route";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../types/handwritten";

/** Re-render on a light interval so the rolling 10-minute window keeps
 * aging samples out, and the pill's live value stays current, even during
 * a quiet period with no new flow records landing. Mirrors
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
  /** Test-only override — production omits this and the component ticks
   * its own clock (see [[useTickingNow]]). */
  nowMsOverride?: number;
}

export function MachineDrawer({ route, routeRecords, flowWindow, localUid, liveMachines, specs, nowMsOverride }: MachineDrawerProps) {
  const tickedNow = useTickingNow(5000);
  const nowMs = nowMsOverride ?? tickedNow;

  const [open, setOpen] = useState(() => initDrawerOpen());
  const pillRef = useRef<HTMLButtonElement | null>(null);
  const closeRef = useRef<HTMLButtonElement | null>(null);

  const scope = useMemo(
    () => resolveDrawerScope(route, routeRecords, flowWindow, localUid, nowMs),
    [route, routeRecords, flowWindow, localUid, nowMs],
  );
  const agg = useMemo(() => aggregateHostSamples(scope.samples), [scope.samples]);

  // (#2107) The header line — identity + version, reachable from every
  // viewport (phones only ever reach this via the pill, since the
  // masthead's own ⓘ affordance is desktop-only — see `Masthead.tsx`'s
  // doc). Built from data this app ALREADY tracks, the same way
  // `AboutDialog.tsx`/`MachineLens.tsx` do:
  //  - machine name / hardware: `nameOf`/`specOf`, scoped to the LOCAL
  //    machine (this header describes the physical host, regardless of
  //    which route's samples the meters below are showing).
  //  - `darkmux <version>` from the SAME `injectedMeta("darkmux-version")`
  //    the masthead reads — `build_version()` already embeds the git SHA
  //    in that string server-side (`x.y.z (sha✱)`), so no separate field
  //    is needed for it.
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
  const headerLine = [machineName, hardware, verMeta ? `darkmux ${verMeta}` : null].filter(Boolean).join(" · ");

  const setOpenPersisted = (next: boolean) => {
    setOpen(next);
    persistDrawerOpen(next);
  };

  // Escape closes from anywhere while open; focus moves to the close
  // button on open and back to the pill on close — a minimal version of a
  // focus trap (full Tab-cycling containment is not implemented; the panel
  // holds few enough focusable elements — close button + meters, which
  // carry no interactive controls of their own — that escaping the trap by
  // tabbing out is a low-severity gap, not a dead end: Escape and the
  // backdrop remain reachable either way).
  useEffect(() => {
    if (!open) return;
    closeRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpenPersisted(false);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open) pillRef.current?.focus({ preventScroll: true });
    // Only fires on the open->close transition (pillRef is stable); does
    // NOT run on mount, since `open` starts however `initDrawerOpen`
    // resolved rather than transitioning from a prior `true`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Swipe-down-to-dismiss on the handle (mobile sheet only — the handle is
  // hidden by CSS above 768px, so this listener is inert on desktop).
  const touchStartY = useRef<number | null>(null);
  const onHandleTouchStart = (e: React.TouchEvent) => {
    touchStartY.current = e.touches[0]?.clientY ?? null;
  };
  const onHandleTouchEnd = (e: React.TouchEvent) => {
    const startY = touchStartY.current;
    touchStartY.current = null;
    if (startY == null) return;
    const endY = e.changedTouches[0]?.clientY ?? startY;
    if (endY - startY > 40) setOpenPersisted(false);
  };

  const gpuNow = agg.gpu.now;
  const pillValue = gpuNow === null ? "—" : `${Math.round(gpuNow)}%`;

  return (
    <>
      <button
        ref={pillRef}
        type="button"
        className="machine-pill"
        data-act="machine-drawer-pill"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpenPersisted(!open)}
      >
        <span className="machine-pill__dim">machine ·</span> GPU {pillValue}
      </button>
      {open && (
        <>
          <div className="machine-drawer__backdrop" data-act="machine-drawer-backdrop" onClick={() => setOpenPersisted(false)} />
          <div className="machine-drawer" role="dialog" aria-modal="true" aria-label="Machine stats">
            <div
              className="machine-drawer__handle"
              data-act="machine-drawer-handle"
              onTouchStart={onHandleTouchStart}
              onTouchEnd={onHandleTouchEnd}
            >
              <div className="machine-drawer__handle-bar" />
            </div>
            <div className="machine-drawer__head">
              <span className="machine-drawer__title">Machine stats</span>
              <button
                ref={closeRef}
                type="button"
                className="machine-drawer__close"
                data-act="machine-drawer-close"
                aria-label="Close"
                onClick={() => setOpenPersisted(false)}
              >
                ×
              </button>
            </div>
            {headerLine && <div className="machine-drawer__identity">{headerLine}</div>}
            <div className="machine-drawer__scope">{scope.scopeLabel}</div>
            <div className="meter-row">
              <Meter label="CPU" now={agg.cpu.now} avg={agg.cpu.avg} max={agg.cpu.high} />
              <Meter label="GPU" now={agg.gpu.now} avg={agg.gpu.avg} max={agg.gpu.high} />
              <Meter label="MEM" now={agg.mem.now} avg={agg.mem.avg} max={agg.mem.high} />
            </div>
          </div>
        </>
      )}
    </>
  );
}
