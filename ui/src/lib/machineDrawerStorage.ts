/**
 * (#2107) Per-viewer open/closed persistence for the global machine
 * drawer/modal — same shape as `lenses/mission/timeline.ts`'s
 * `initMinimap`/`persistMinimap` (storage injected for testability,
 * `try/catch` around every access since a private-browsing tab or a
 * blocked-storage browser throws on `localStorage` access, not just on
 * read).
 */
const DRAWER_OPEN_STORAGE_KEY = "dmux.machine-drawer-open";

export function initDrawerOpen(storage: Pick<Storage, "getItem"> = window.localStorage): boolean {
  try {
    return storage.getItem(DRAWER_OPEN_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

export function persistDrawerOpen(open: boolean, storage: Pick<Storage, "setItem"> = window.localStorage): void {
  try {
    storage.setItem(DRAWER_OPEN_STORAGE_KEY, open ? "1" : "0");
  } catch {
    // ignore — storage unavailable
  }
}
