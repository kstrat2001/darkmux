import { useEffect, useRef, type ReactNode } from "react";
import { closeOpenModal, useOpenModalId, type ModalId } from "../lib/dialogManager";

/**
 * The shared modal shell — viewer.html's `.modalbg`/`.modal`/`.mhd`/`.mx`
 * structure (`#modalbg`/`#nmodalbg`/`#imodalbg`), one component instead of
 * three near-duplicate blocks of markup. Three call sites use this:
 * `FiltersDialog` (in `EventLogColumn.tsx`), `NotesDialog` (in
 * `FleetLens.tsx`), `AboutDialog` (in `Masthead.tsx`).
 *
 * **Always mounted, `style.display` toggled — never conditionally
 * unmounted.** This matches legacy's own static body-level markup (the
 * backdrop `<div id="modalbg">` etc. exist in the DOM whether open or
 * closed, viewer.html:861-881) and is load-bearing for two things:
 *   1. `tests/e2e/viewer-keyboard.spec.js`'s `openCount()` helper reads
 *      `document.getElementById(id).style.display` — an inline `"flex"`
 *      when open, `"none"` when closed — so the backdrop element itself
 *      must exist with that exact toggle, not appear/disappear from the DOM.
 *   2. `dialogManager.ts`'s Tab-trap looks up the open dialog's scope via
 *      `document.getElementById(id)` — a stable id target, not a
 *      conditionally-rendered one.
 *
 * Focus-on-open mirrors legacy's `openModalEl`:
 * `m.querySelector(".mx").focus()` (viewer.html:2930) — the close button
 * gets focus the instant the dialog becomes the open one.
 */
export function Dialog({
  id,
  titleId,
  title,
  wide,
  children,
  footer,
}: {
  id: ModalId;
  titleId: string;
  title: ReactNode;
  /** Notes is wider than Filters/About in legacy (`.nmodal`, viewer.html:574
   *  — prose reads better wide; the checkbox grid and the kv rows don't). */
  wide?: boolean;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const openModalId = useOpenModalId();
  const open = openModalId === id;
  const closeRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    if (open) closeRef.current?.focus();
  }, [open]);

  return (
    <div
      className="dialog-backdrop"
      id={id}
      style={{ display: open ? "flex" : "none" }}
      onClick={(e) => {
        // viewer.html:2978-2980 (#1132) — click on the backdrop itself
        // closes; a click that bubbled up from inside `.dialog` must not.
        if (e.target === e.currentTarget) closeOpenModal();
      }}
    >
      {open ? (
        <div className={`dialog${wide ? " dialog--wide" : ""}`} role="dialog" aria-modal="true" aria-labelledby={titleId}>
          <div className="dialog__head">
            <span id={titleId}>{title}</span>
            <button type="button" className="dialog__close" ref={closeRef} aria-label="close" onClick={() => closeOpenModal()}>
              ✕
            </button>
          </div>
          <div className="dialog__body">{children}</div>
          {footer ? <div className="dialog__footer">{footer}</div> : null}
        </div>
      ) : null}
    </div>
  );
}
