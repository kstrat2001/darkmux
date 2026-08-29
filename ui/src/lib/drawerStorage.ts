/**
 * (#2107 tabbed-drawer packet) Per-TAB persisted height for the phone
 * bottom drawer — same storage discipline the retired
 * `machineDrawerStorage.ts` used (try/catch around every access, since a
 * private-browsing tab or a storage-blocked browser throws rather than
 * quietly no-opping; storage injected for testability rather than reached
 * for as a bare global).
 *
 * Keyed PER TAB deliberately: the Machine tab's compact meters and the
 * Events tab's scrolling list want genuinely different resting heights —
 * an operator who drags Machine open to a small 20vh peek and Events open
 * to a tall 80vh reading list should get each one back exactly as left,
 * not one shared number fighting both use cases.
 */
export type DrawerTabId = "machine" | "events";

const KEY_PREFIX = "dmux.phone-drawer.height.";

function keyFor(tab: DrawerTabId): string {
  return `${KEY_PREFIX}${tab}`;
}

export function loadDrawerHeightPct(tab: DrawerTabId, storage: Pick<Storage, "getItem"> = window.localStorage): number | null {
  try {
    const raw = storage.getItem(keyFor(tab));
    if (raw == null) return null;
    const n = Number(raw);
    return Number.isFinite(n) ? n : null;
  } catch {
    return null;
  }
}

export function saveDrawerHeightPct(tab: DrawerTabId, pct: number, storage: Pick<Storage, "setItem"> = window.localStorage): void {
  try {
    storage.setItem(keyFor(tab), String(Math.round(pct)));
  } catch {
    // ignore — storage unavailable
  }
}
