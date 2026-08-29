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
 *   component's own.
 * - **Phone (≤768px):** the tabbed bottom drawer (#2107 tabbed-drawer
 *   packet) — `PhoneDrawer.tsx`. This component only DECIDES that the
 *   phone skin is active and hands `PhoneDrawer` the two panels it hosts:
 *   the Machine tab's stats content (`useMachineStatsContent`, the SAME
 *   hook the desktop dialog's body comes from — one computation, two
 *   renderers) and the Events tab's `EventLogColumn` props (threaded down
 *   from `App.tsx`, which is also where the App-level INLINE
 *   `EventLogColumn` mount gets suppressed on a phone — see that file's
 *   own doc for why there is only ever one events-pane mount at a time).
 *   The earlier single-purpose "bottombar + one sheet" phone chrome this
 *   component used to render directly is retired along with this packet —
 *   see `PhoneDrawer.tsx`'s own doc for the tab/drag semantics that
 *   replaced it.
 *
 * Both skins share the SAME data/scope logic
 * (`machineStatsContent.tsx`/`lib/machineDrawerScope.ts`) — only the
 * chrome around it differs.
 *
 * Static builds need no special-casing here: `routeRecords`/`flowWindow`
 * are handed in from `App.tsx`, which already resolves both from the
 * committed fixtures on a static build and from the daemon otherwise (see
 * that file's own `getSource()`/`isStaticBuild` doc) — this component only
 * ever reads the records it's given.
 */
import { Dialog } from "./Dialog";
import { openModalEl, closeOpenModal, useOpenModalId } from "../lib/dialogManager";
import { fmtPct } from "./Meter";
import { useMachineStatsContent } from "./machineStatsContent";
import { PhoneDrawer } from "./PhoneDrawer";
import { useIsMobile } from "../hooks/useIsMobile";
import type { Route } from "../lib/route";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type { FlowRecord, MachineSpecs, PresenceBeat } from "../types/handwritten";

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
   * its own clock (see `machineStatsContent.tsx`'s own `useTickingNow`). */
  nowMsOverride?: number;
  /** Test-only override for the phone/desktop split — production omits
   * this and measures `window.innerWidth` (see [[useIsMobile]]). */
  isMobileOverride?: boolean;
  /** (#2107 tabbed-drawer packet) The phone drawer's Events tab —
   * `EventLogColumn`'s own props, threaded down from `App.tsx`. Unused on
   * desktop (the pill/dialog carries no events pane) — see `App.tsx`'s own
   * doc for why the inline `EventLogColumn` mount and this one are
   * mutually exclusive by viewport, never both mounted at once. */
  eventLogRecords: FlowRecord[];
  eventLogScopeLabel: string;
  eventLogVisible: boolean;
  eventLogLoading: boolean;
  eventLogError: { status: number | null; message: string } | null;
  eventLogHistorical: boolean;
  eventLogServerTruncated?: boolean;
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
  eventLogRecords,
  eventLogScopeLabel,
  eventLogVisible,
  eventLogLoading,
  eventLogError,
  eventLogHistorical,
  eventLogServerTruncated = false,
}: MachineDrawerProps) {
  const measuredIsMobile = useIsMobile();
  const isMobile = isMobileOverride ?? measuredIsMobile;

  // Desktop's open/closed lives entirely in `dialogManager` (see this
  // module's own doc for why) — `useOpenModalId()` is the SAME reactive
  // subscription `<Dialog>` itself uses, so this component and the shell
  // it renders through can never disagree about whether they're open.
  const desktopOpen = useOpenModalId() === "imodalbg";

  const { compactLine, gpuNow, body } = useMachineStatsContent({
    route,
    routeRecords,
    flowWindow,
    localUid,
    liveMachines,
    specs,
    liveStatus,
    nowMsOverride,
  });

  if (isMobile) {
    return (
      <PhoneDrawer
        route={route}
        liveStatus={liveStatus}
        machineTab={{ compactLine, body }}
        events={{
          records: eventLogRecords,
          scopeLabel: eventLogScopeLabel,
          visible: eventLogVisible,
          loading: eventLogLoading,
          error: eventLogError,
          historical: eventLogHistorical,
          serverTruncated: eventLogServerTruncated,
        }}
      />
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
        {body}
      </Dialog>
    </>
  );
}
