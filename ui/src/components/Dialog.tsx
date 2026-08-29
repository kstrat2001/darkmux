import { useEffect, useRef, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { closeOpenModal, isModalOpen, useOpenModalId, type ModalId } from "../lib/dialogManager";

/**
 * The shared modal shell — viewer.html's `.modalbg`/`.modal`/`.mhd`/`.mx`
 * structure (`#modalbg`/`#nmodalbg`/`#imodalbg`), one component instead of
 * three near-duplicate blocks of markup. Three call sites use this:
 * `FiltersDialog` (in `EventLogColumn.tsx`), `NotesDialog` (in
 * `FleetLens.tsx`), `AboutDialog` (in `Masthead.tsx`).
 *
 * **Rendered through a portal into `document.body`, always mounted,
 * `style.display` toggled — never conditionally unmounted.** This matches
 * legacy's own static body-level markup (the backdrop `<div id="modalbg">`
 * etc. exist in the DOM whether open or closed, viewer.html:861-881) and is
 * load-bearing for two things:
 *   1. `tests/e2e/viewer-keyboard.spec.js`'s `openCount()` helper reads
 *      `document.getElementById(id).style.display` — an inline `"flex"`
 *      when open, `"none"` when closed — so the backdrop element itself
 *      must exist with that exact toggle, not appear/disappear from the DOM.
 *   2. `dialogManager.ts`'s Tab-trap looks up the open dialog's scope via
 *      `document.getElementById(id)` — a stable id target, not a
 *      conditionally-rendered one.
 *
 * The portal is what makes "body-level" TRUE rather than merely claimed, and
 * it fixes a real defect found in review: each call site nests inside a
 * component that can stop being displayed. `FiltersDialog` lives inside
 * `EventLogColumn`, which stays mounted but goes `display:none` on the
 * lenses that hide the event log — so a dialog left open across a lens
 * change became OPEN BUT INVISIBLE, silently eating the next Escape
 * anywhere on the page. Portaled to `body`, an open dialog stays visibly
 * open across a lens change, which is exactly what legacy did.
 *
 * The portal is invisible to everything that matters: the e2e helpers and
 * the Tab-trap both find the backdrop by `document.getElementById(id)`, and
 * the trap's `scope.contains(activeElement)` check works on the portaled
 * subtree because the scope is looked up by id, not by React tree position.
 *
 * A host that truly UNMOUNTS is the other half, and the portal cannot help
 * there — `NotesDialog` lives inside `FleetLens`, which unmounts on a route
 * change, taking the portal with it while `openId` stayed set. Returning to
 * fleet then resurrected the dialog with no user action. The unmount effect
 * below closes it instead, without consuming the remembered focus target
 * (the element that opened it is gone too).
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
  className,
  children,
  footer,
}: {
  id: ModalId;
  titleId: string;
  title: ReactNode;
  /** Notes is wider than Filters/About in legacy (`.nmodal`, viewer.html:574
   *  — prose reads better wide). Fixed at `.dialog--wide`'s own 540px. */
  wide?: boolean;
  /** (#2116) An extra class appended after `wide`'s own, so a caller can
   * add its OWN width rule without widening every `.dialog` (About,
   * Machine info) or coupling to Notes' fixed 540px. `FiltersDialog` is
   * the first user — the activity facet can run to ~40 checkboxes on a
   * busy day, which needs `min(90vw, 720px)` on desktop and its own
   * multi-column grid; About and Machine info stay short kv lists that
   * never wanted the extra room. */
  className?: string;
  children: ReactNode;
  footer?: ReactNode;
}) {
  const openModalId = useOpenModalId();
  const open = openModalId === id;
  const closeRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    if (open) closeRef.current?.focus();
  }, [open]);

  // Unmounting while open must not leave `openId` pointing at a dialog that
  // no longer exists — see this component's doc. `restore:false` because the
  // trigger that opened it is being unmounted alongside it, so there is
  // nothing left to hand focus back to.
  useEffect(
    () => () => {
      if (isModalOpen(id)) closeOpenModal({ restore: false });
    },
    [id]
  );

  return createPortal(
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
        <div
          className={`dialog${wide ? " dialog--wide" : ""}${className ? ` ${className}` : ""}`}
          role="dialog"
          aria-modal="true"
          aria-labelledby={titleId}
        >
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
    </div>,
    document.body
  );
}
