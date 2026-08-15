/**
 * The single source of truth for "which of the app's three legacy-style
 * modal dialogs (Filters/Notes/About) is open" — ported from viewer.html's
 * `openModalEl`/`closeOpenModal`/`restoreModalFocus`/`MODAL_IDS` (#1640).
 *
 * Three independent trigger components need this — the event log's Filters
 * button (`EventLogColumn.tsx`), the fleet hero's "history →" Notes link
 * (`FleetLens.tsx`), and the masthead's build chip (`Masthead.tsx`). Legacy
 * enforces "exactly one dialog open at a time" through a single set of
 * module-scoped globals; three independent React `useState`s could never
 * enforce that same invariant across three unrelated component subtrees, so
 * this stays an external store (subscribed to via `useOpenModalId`,
 * `useSyncExternalStore`) rather than component-local state.
 *
 * The keyboard machinery — Tab-trap, Shift+Tab wrap, single-Escape-closes-
 * the-open-dialog, focus-restore-on-close — is installed ONCE at module
 * load, mirroring legacy's own top-level `document.addEventListener` calls
 * (viewer.html:2944-2980).
 *
 * **Port note — `openModalEl` is also assigned to `window`.** This is a
 * deliberate, narrow exception to this port's otherwise-firm "no page
 * globals" posture (see `tests/e2e/viewer-session-url.spec.js`'s own module
 * doc for that posture's rationale — legacy drove imperative globals like
 * `window.drillSession`/`window.goRuns`, and the port dropped all of them by
 * design). `tests/e2e/viewer-keyboard.spec.js`'s third test
 * ("one Escape closes the dialog the operator is actually looking at")
 * drives the About dialog directly through this global, because there is no
 * real click affordance for it reachable in that harness — its own header
 * comment explains why (the build chip needs an injected build id the
 * harness doesn't provide, the same root cause legacy hit when THAT test was
 * first written). Unlike `drillSession`/`goRuns`, there is no hash/URL
 * equivalent for "open a transient dialog" to drive instead — dialogs are
 * not routed state in either app. Exposing this one function (not the
 * broader `state`-shaped object legacy exposed) is flagged explicitly in the
 * packet report for sign-off rather than assumed silently correct.
 */
import { useSyncExternalStore } from "react";

export type ModalId = "modalbg" | "nmodalbg" | "imodalbg";
export const MODAL_IDS: ModalId[] = ["modalbg", "nmodalbg", "imodalbg"];

let openId: ModalId | null = null;
let returnFocus: HTMLElement | null = null;
const listeners = new Set<() => void>();

function notify(): void {
  listeners.forEach((l) => l());
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function getOpenId(): ModalId | null {
  return openId;
}

/** React binding — `useSyncExternalStore` over the module-level store above.
 * Every `<Dialog>` instance calls this to know whether IT is the open one. */
export function useOpenModalId(): ModalId | null {
  return useSyncExternalStore(subscribe, getOpenId, getOpenId);
}

export function isModalOpen(id: ModalId): boolean {
  return openId === id;
}

/**
 * `openModalEl()` — viewer.html:2919-2931 (#1640). EXACTLY ONE dialog may be
 * open. Opening any dialog while another is already open closes the first
 * one WITHOUT restoring focus (legacy's `closeOpenModal({restore:false})`) —
 * "topmost" and "only" become the same thing by construction, and the
 * ORIGINAL return-focus target (captured on the FIRST open) survives a
 * swap between dialogs rather than being overwritten by the second one.
 */
export function openModalEl(id: ModalId): void {
  // Guard an unknown id. The TypeScript signature says `ModalId`, but the
  // whole point of this function is that it is reachable from `window` by
  // untyped callers, where the type is not enforced. Without this,
  // `window.openModalEl('bogus')` sets `openId` to an element that does not
  // exist: nothing renders, and the next Escape is silently swallowed
  // closing the dialog that isn't there. Legacy no-ops on a missing element
  // (`const m=$(id); if(!m)return;` — viewer.html:2928); this is that.
  if (!MODAL_IDS.includes(id)) return;
  if (openId !== null) {
    // Swap: close the currently-open one silently, keep `returnFocus` as-is.
    openId = null;
  } else {
    const active = document.activeElement;
    returnFocus = active instanceof HTMLElement && active !== document.body ? active : null;
  }
  openId = id;
  notify();
}

/**
 * `closeOpenModal()` — viewer.html:2967-2977. Closes whichever dialog is
 * open (there is only ever one, by construction — see `openModalEl`).
 * `{restore:false}` keeps the remembered focus target instead of consuming
 * it, for the "swapping to another dialog" case above. Returns `true` iff a
 * dialog was actually open (legacy uses this to let Escape fall through to
 * the catalog panel when nothing was open — this port's Escape handler
 * below does the same).
 */
export function closeOpenModal(opts?: { restore?: boolean }): boolean {
  if (openId === null) return false;
  openId = null;
  notify();
  const restore = opts?.restore !== false;
  if (restore) {
    const el = returnFocus;
    returnFocus = null;
    if (el && el.isConnected && typeof el.focus === "function") el.focus();
  }
  return true;
}

/** `isModalOpen`/focusable-query subset of viewer.html:2936-2958's Tab-trap —
 * matches the exact selector + "actually visible or currently focused"
 * filter legacy uses, so a hidden-but-present control (e.g. a `hidden`
 * `<details>` child) is never treated as a tab stop. */
function focusableIn(scope: Element): HTMLElement[] {
  return [
    ...scope.querySelectorAll<HTMLElement>(
      'a[href],button:not([disabled]),input:not([disabled]),select,textarea,[tabindex]:not([tabindex="-1"])',
    ),
  ].filter((el) => el.offsetParent !== null || el === document.activeElement);
}

/**
 * `document.addEventListener("keydown", ..., true)` — viewer.html:2944-2958
 * (#1640). Capture-phase so it sees Tab before anything else. Keeps Tab (and
 * Shift+Tab) cycling within the open dialog's own focusable set; focus that
 * has somehow ended up OUTSIDE the dialog (page just loaded, or it escaped
 * earlier) is pulled back to the FIRST control rather than left to wander.
 */
function handleTabTrap(e: KeyboardEvent): void {
  if (e.key !== "Tab") return;
  const id = openId;
  if (id === null) return;
  const scope = document.getElementById(id);
  if (!scope) return;
  const f = focusableIn(scope);
  if (!f.length) return;
  const first = f[0];
  const last = f[f.length - 1];
  if (!scope.contains(document.activeElement)) {
    e.preventDefault();
    first.focus();
    return;
  }
  if (e.shiftKey && document.activeElement === first) {
    e.preventDefault();
    last.focus();
  } else if (!e.shiftKey && document.activeElement === last) {
    e.preventDefault();
    first.focus();
  }
}

/** viewer.html:3055-3057's Escape arm — bubble phase, no capture. Closes
 * whichever dialog is open; a no-op when nothing is. (Legacy also closes the
 * catalog panel here — that panel owns its own Escape handler independently
 * in this port, see `CatalogPanel.tsx`, so this stays scoped to dialogs.) */
function handleEscape(e: KeyboardEvent): void {
  if (e.key !== "Escape") return;
  closeOpenModal();
}

let installed = false;

/** Installs the two document-level listeners exactly once per module
 * instance (idempotent — safe to call more than once, e.g. from a test that
 * re-imports the module in isolation). Exported so a test can assert the
 * listeners are live without relying on import-time side effects alone. */
export function installGlobalModalKeyboardHandlers(): void {
  if (installed || typeof document === "undefined") return;
  installed = true;
  document.addEventListener("keydown", handleTabTrap, true);
  document.addEventListener("keydown", handleEscape);
}

if (typeof window !== "undefined") {
  installGlobalModalKeyboardHandlers();
  // See this module's own doc for why this one function — and only this
  // one — is exposed as a page global.
  (window as unknown as { openModalEl: typeof openModalEl }).openModalEl = openModalEl;
}
