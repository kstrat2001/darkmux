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
 * - **Desktop (>768px):** the masthead's own ⓘ affordance (`Masthead.tsx`)
 *   is now the ONLY desktop trigger — it opens the shared
 *   `<Dialog id="imodalbg">` shell (`Dialog.tsx`/`lib/dialogManager.ts`)
 *   every other dialog in this app (Filters, Notes) already uses. This
 *   component used to ALSO render its own floating `machine · GPU 68%`/
 *   `Machine info` pill as a second desktop trigger for the same dialog;
 *   the operator found a second floating affordance for the exact same
 *   action redundant once the ⓘ existed, so that pill is removed — this
 *   component now renders ONLY `<Dialog>` on desktop, no visible trigger
 *   of its own. The dialog itself, its content, and its close/backdrop/
 *   Escape behavior are unchanged; only the extra button is gone.
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
 *
 * **Both surfaces now read a static `Machine info` label at rest — no live
 * numbers on the pill or the closed tab** (operator finding: a live
 * `GPU 68%` pill "looks too busy" for a resting indicator). Live numbers
 * exist ONLY inside the opened body. This is also what makes the daemon
 * poll gate correct: `useMachineStatsContent`'s `isOpen` input needs to
 * know whether THIS surface is currently visible, and since neither the
 * pill nor the tab shows a number at rest there is nothing to keep warm
 * while closed. On desktop that's `desktopOpen` (the same `dialogManager`
 * subscription driving `<Dialog>` itself); on phone, `PhoneDrawer` owns
 * its own open/tab state internally, so it reports back via
 * `onMachineOpenChange` — see that prop's own doc on `PhoneDrawer`.
 */
import { useState } from "react";
import { Dialog } from "./Dialog";
import { useOpenModalId } from "../lib/dialogManager";
import { useMachineStatsContent } from "./machineStatsContent";
import { PhoneDrawer } from "./PhoneDrawer";
import { useIsMobile } from "../hooks/useIsMobile";
import type { Route } from "../lib/route";
import type { ReactNode } from "react";
import type { LiveTailStatus } from "../hooks/useLiveTail";
import type {
  FlowRecord,
  MachineSpecs,
  PresenceBeat,
} from "../types/handwritten";

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
  /** (#2189, step drill-in) The mission lens's `StepHeaderBlock`, or
   * `null` outside a step selection -- threaded straight through to the
   * phone Events tab's `EventLogColumn` mount below, mirroring every
   * other `eventLog*` prop's own "same values as the desktop mount" doc. */
  eventLogHeaderExtra?: ReactNode;
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
  eventLogHeaderExtra = null,
}: MachineDrawerProps) {
  const measuredIsMobile = useIsMobile();
  const isMobile = isMobileOverride ?? measuredIsMobile;

  // Desktop's open/closed lives entirely in `dialogManager` (see this
  // module's own doc for why) — `useOpenModalId()` is the SAME reactive
  // subscription `<Dialog>` itself uses, so this component and the shell
  // it renders through can never disagree about whether they're open.
  const desktopOpen = useOpenModalId() === "imodalbg";
  // (#2107, #1833) `PhoneDrawer` owns its own open/activeTab state
  // internally (uncontrolled — see that component's own doc on why it
  // stays that way rather than being lifted wholesale); it mirrors just
  // the one bit this component needs — "is the Machine tab open right
  // now" — via `onMachineOpenChange`. Starts `false`, matching
  // `PhoneDrawer`'s own initial closed state, so the very first render
  // never polls before the drawer has actually been opened.
  const [phoneMachineTabOpen, setPhoneMachineTabOpen] = useState(false);
  const isStatsSurfaceOpen = isMobile ? phoneMachineTabOpen : desktopOpen;

  const { body } = useMachineStatsContent({
    route,
    routeRecords,
    flowWindow,
    localUid,
    liveMachines,
    specs,
    liveStatus,
    isOpen: isStatsSurfaceOpen,
    isMobile,
    nowMsOverride,
  });

  if (isMobile) {
    return (
      <PhoneDrawer
        route={route}
        liveStatus={liveStatus}
        machineTab={{ body }}
        onMachineOpenChange={setPhoneMachineTabOpen}
        events={{
          records: eventLogRecords,
          scopeLabel: eventLogScopeLabel,
          visible: eventLogVisible,
          loading: eventLogLoading,
          error: eventLogError,
          historical: eventLogHistorical,
          serverTruncated: eventLogServerTruncated,
          headerExtra: eventLogHeaderExtra,
        }}
      />
    );
  }

  // (#2108, operator finding) No desktop trigger of this component's own
  // any more — the masthead's own ⓘ (`Masthead.tsx`) is the sole desktop
  // affordance for this dialog now; this component only renders the
  // dialog shell itself, which stays reachable/openable exactly as before
  // via `dialogManager`'s shared `#imodalbg` id.
  return (
    <Dialog id="imodalbg" titleId="machine-info-title" title="Machine info">
      {body}
    </Dialog>
  );
}
