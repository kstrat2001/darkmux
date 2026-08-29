/**
 * (#2107 tabbed-drawer packet; SIMPLIFIED #2108, operator correction) The
 * phone bottom drawer's persisted open height — same storage discipline
 * the retired `machineDrawerStorage.ts` used (try/catch around every
 * access, since a private-browsing tab or a storage-blocked browser
 * throws rather than quietly no-opping; storage injected for testability
 * rather than reached for as a bare global).
 *
 * ONE SHARED value now, not per-tab. #2107 originally keyed this per tab
 * ("Machine tab's compact meters and Events tab's scrolling list want
 * genuinely different resting heights"), but a real-device review found
 * the opposite problem: switching tabs while the sheet was open visibly
 * RESIZED it (and replayed the slide transition) whenever the two tabs'
 * stored heights differed — reading as a glitch, not a feature. The sheet
 * is now ONE fixed height regardless of which tab is active; the tab
 * content scrolls inside it rather than the sheet sizing to content.
 */
/** Which tab is active — no longer used to KEY storage (see this
 * module's own doc), but still the shared vocabulary `PhoneDrawer.tsx`
 * uses for its own `activeTab` state. */
export type DrawerTabId = "machine" | "events";

const KEY = "dmux.phone-drawer.height";

export function loadDrawerHeightPct(storage: Pick<Storage, "getItem"> = window.localStorage): number | null {
  try {
    const raw = storage.getItem(KEY);
    if (raw == null) return null;
    const n = Number(raw);
    return Number.isFinite(n) ? n : null;
  } catch {
    return null;
  }
}

export function saveDrawerHeightPct(pct: number, storage: Pick<Storage, "setItem"> = window.localStorage): void {
  try {
    storage.setItem(KEY, String(Math.round(pct)));
  } catch {
    // ignore — storage unavailable
  }
}
