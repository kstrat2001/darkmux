import { describe, it, expect, afterEach } from "vitest";
import { render, screen, act } from "@testing-library/react";
import { Dialog } from "./Dialog";
import { openModalEl, closeOpenModal, getOpenId } from "../lib/dialogManager";

afterEach(() => {
  // dialogManager's open/close state is a module singleton — it outlives
  // `render()`/unmount, so a leaked open dialog would contaminate the next
  // test. Same reason `Masthead.test.tsx` and `EventLogColumn.test.tsx` do it.
  closeOpenModal({ restore: false });
});

/**
 * The two defects these pin were both found by review against a live daemon,
 * and both come from the same root cause: every `Dialog` call site nests
 * inside a component that can stop being displayed, while `openId` lives in a
 * module singleton that knows nothing about React's tree.
 */
describe("Dialog lifecycle", () => {
  /** `NotesDialog` lives inside `FleetLens`, which UNMOUNTS on a route change.
   * Before the unmount effect, `openId` stayed set to a dialog that no longer
   * existed: every subsequent Escape anywhere on the page was silently
   * consumed closing it, and navigating back re-rendered the dialog OPEN with
   * no user action. */
  it("unmounting while open closes the dialog, so it cannot resurrect on remount", () => {
    const { unmount } = render(
      <Dialog id="nmodalbg" titleId="t" title="notes">
        body
      </Dialog>,
    );
    openModalEl("nmodalbg");
    expect(getOpenId()).toBe("nmodalbg");

    unmount();
    expect(getOpenId()).toBeNull();

    // The resurrection itself: remounting must render CLOSED.
    render(
      <Dialog id="nmodalbg" titleId="t" title="notes">
        body
      </Dialog>,
    );
    expect(document.getElementById("nmodalbg")!.style.display).toBe("none");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  /** The portal is what makes the module doc's "body-level" claim true. It is
   * load-bearing for `FiltersDialog`, which nests inside an `EventLogColumn`
   * that stays mounted but goes `display:none` on lenses that hide the event
   * log — an open dialog inside a hidden ancestor is open-but-invisible, and
   * eats the next Escape. Rendering to `body` puts it outside any ancestor
   * that can hide it. */
  it("renders into document.body, not into its parent's subtree", () => {
    const { container } = render(
      <div className="hiding-ancestor">
        <Dialog id="modalbg" titleId="t" title="filters">
          body
        </Dialog>
      </div>,
    );
    const backdrop = document.getElementById("modalbg")!;
    expect(backdrop.parentElement).toBe(document.body);
    // And genuinely NOT under the component's own container — the assertion
    // above would still pass if `container` were itself body-parented.
    expect(container.querySelector("#modalbg")).toBeNull();
  });

  /** The e2e keyboard specs read `getElementById(id).style.display` directly
   * (`openCount()`), and the Tab-trap looks its scope up by id. Both survive
   * the portal only because the backdrop keeps its id and its inline display
   * toggle — so pin that contract here rather than discovering it in CI. */
  it("keeps the id + inline display toggle the e2e helpers and the Tab-trap depend on", () => {
    render(
      <Dialog id="modalbg" titleId="t" title="filters">
        body
      </Dialog>,
    );
    const backdrop = document.getElementById("modalbg")!;
    expect(backdrop.style.display).toBe("none");

    // `openModalEl` mutates the module store outside React's knowledge, so the
    // subscribed re-render has to be flushed for a DOM assertion. In the app
    // this happens via a click handler, which is already act-wrapped.
    act(() => openModalEl("modalbg"));
    expect(document.getElementById("modalbg")!.style.display).toBe("flex");
    expect(screen.getByRole("dialog")).toBeTruthy();
  });
});
