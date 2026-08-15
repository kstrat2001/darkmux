import { describe, it, expect, afterEach, beforeEach } from "vitest";
import { openModalEl, closeOpenModal, getOpenId, isModalOpen, MODAL_IDS } from "./dialogManager";

/** Builds a minimal DOM shape matching what `Dialog.tsx` renders: a trigger
 * button outside the dialog, and a backdrop element (the dialog's own id)
 * containing a small focusable set (matching legacy's own Tab-trap fixture
 * shape — an input, a button, a link). */
function buildDom() {
  document.body.innerHTML = `
    <button id="trigger">open</button>
    <div id="modalbg">
      <input id="first-focusable" />
      <button id="mid-focusable">mid</button>
      <a id="last-focusable" href="#">last</a>
    </div>
    <div id="nmodalbg"></div>
    <div id="imodalbg"></div>
  `;
  // jsdom does no layout, so every element's `offsetParent` is always
  // `null` — `focusableIn`'s "visible or currently focused" filter
  // (`el.offsetParent !== null || el === document.activeElement`) would
  // otherwise treat every non-focused element here as invisible and see a
  // one-element set no matter which one has focus, masking the real
  // wrap-around logic this suite exists to test. A real browser (this
  // repo's actual Playwright e2e coverage) computes `offsetParent`
  // correctly; this stub only compensates for jsdom's lack of layout, not
  // for anything about the elements themselves.
  for (const id of ["first-focusable", "mid-focusable", "last-focusable"]) {
    Object.defineProperty(document.getElementById(id)!, "offsetParent", {
      get: () => document.body,
      configurable: true,
    });
  }
}

function press(key: string, opts: Partial<KeyboardEventInit> = {}) {
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...opts });
  document.dispatchEvent(event);
  return event;
}

beforeEach(() => {
  buildDom();
});

afterEach(() => {
  closeOpenModal({ restore: false });
  document.body.innerHTML = "";
});

describe("dialogManager — exclusivity", () => {
  it("MODAL_IDS lists exactly the three legacy dialog ids", () => {
    expect(MODAL_IDS).toEqual(["modalbg", "nmodalbg", "imodalbg"]);
  });

  it("openModalEl opens the named dialog", () => {
    expect(getOpenId()).toBeNull();
    openModalEl("modalbg");
    expect(getOpenId()).toBe("modalbg");
    expect(isModalOpen("modalbg")).toBe(true);
    expect(isModalOpen("nmodalbg")).toBe(false);
  });

  // RED-PROVED: with the exclusivity guard removed (`openModalEl` just sets
  // `openId = id` unconditionally, dropping the `if (openId !== null)`
  // branch), this test still passes trivially for a SINGLE open call, but
  // `openCount`-shaped assertions in the e2e suite (two dialogs open at
  // once) depend on this exact behavior — verified by temporarily deleting
  // the `if (openId !== null) { openId = null; }` branch in
  // `dialogManager.ts` and confirming `getOpenId()` after the second
  // `openModalEl` call is unaffected either way for THIS assertion, but the
  // real regression (Escape closing the wrong dialog with two open) is
  // caught by `tests/e2e/viewer-keyboard.spec.js`'s third test instead —
  // see this file's own module doc for why "only one can ever be open" is
  // the structural invariant, not a race to catch here.
  it("opening a second dialog while one is open closes the first — only one is ever open", () => {
    openModalEl("modalbg");
    expect(getOpenId()).toBe("modalbg");
    openModalEl("imodalbg");
    expect(getOpenId()).toBe("imodalbg");
    expect(isModalOpen("modalbg")).toBe(false);
  });

  it("closeOpenModal closes whichever dialog is open and reports true", () => {
    openModalEl("modalbg");
    expect(closeOpenModal()).toBe(true);
    expect(getOpenId()).toBeNull();
  });

  it("closeOpenModal reports false when nothing is open", () => {
    expect(closeOpenModal()).toBe(false);
  });

  it("window.openModalEl is exposed as a global (port note: intentional, narrow exception — see this module's own doc)", () => {
    expect(typeof (window as unknown as { openModalEl?: unknown }).openModalEl).toBe("function");
    (window as unknown as { openModalEl: typeof openModalEl }).openModalEl("modalbg");
    expect(getOpenId()).toBe("modalbg");
  });
});

describe("dialogManager — focus restore on close (viewer-keyboard.spec.js test 4)", () => {
  it("closing (default restore:true) returns focus to the element that had it when the dialog opened", () => {
    const trigger = document.getElementById("trigger") as HTMLButtonElement;
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    openModalEl("modalbg");
    (document.getElementById("mid-focusable") as HTMLElement).focus();
    expect(document.activeElement?.id).toBe("mid-focusable");

    closeOpenModal();
    expect(document.activeElement).toBe(trigger);
  });

  // RED-PROVED: commenting out the `if (restore) { ...; el.focus(); }` block
  // in `closeOpenModal` (i.e. dropping focus-restore entirely) makes this
  // assertion fail — `document.activeElement` stays on `document.body`
  // instead of returning to `trigger` — confirmed by temporarily deleting
  // that block and re-running; restored afterward.
  //
  // RED-PROVED a second, different way: this test is the one that actually
  // exercises the "keep the ORIGINAL target, don't recapture on a swap"
  // half of `openModalEl` — moving focus to something INSIDE the first
  // dialog before swapping is load-bearing. A first draft of this test
  // moved focus nowhere between the two `openModalEl` calls, which left
  // `document.activeElement` unchanged either way and made the assertion
  // pass whether or not `openModalEl` recaptured `returnFocus` on the
  // second call — a vacuous test (see this file's own module doc). Deleting
  // the `if (openId !== null) { openId = null; }` guard (so `openModalEl`
  // ALWAYS recaptures `document.activeElement`, even mid-swap) makes THIS
  // version fail: `returnFocus` becomes `mid-focusable` (focus at the
  // moment of the second open) instead of `trigger`, so the final close
  // restores focus to a control inside the now-closed first dialog instead
  // of back to where the operator actually started — confirmed by
  // temporarily deleting that guard and re-running; restored afterward.
  it("restore:false keeps the ORIGINAL return-focus target for a LATER close (the dialog-swap case)", () => {
    const trigger = document.getElementById("trigger") as HTMLButtonElement;
    trigger.focus();

    openModalEl("modalbg");
    // Something inside the FIRST dialog has focus now (matching reality —
    // `Dialog.tsx` focuses its own close button on open).
    (document.getElementById("mid-focusable") as HTMLElement).focus();
    // Swap to About without going through a real close click — this is
    // exactly what a second `openModalEl` call does internally.
    openModalEl("imodalbg");
    closeOpenModal(); // the REAL close, of the second dialog
    expect(document.activeElement).toBe(trigger);
  });
});

describe("dialogManager — Tab trap (viewer-keyboard.spec.js tests 1-2)", () => {
  it("Tab from the last focusable element wraps to the first", () => {
    openModalEl("modalbg");
    (document.getElementById("last-focusable") as HTMLElement).focus();
    const e = press("Tab");
    expect(e.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe("first-focusable");
  });

  it("Shift+Tab from the first focusable element wraps to the last", () => {
    openModalEl("modalbg");
    (document.getElementById("first-focusable") as HTMLElement).focus();
    const e = press("Tab", { shiftKey: true });
    expect(e.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe("last-focusable");
  });

  it("Tab in the middle of the dialog does not intervene", () => {
    openModalEl("modalbg");
    (document.getElementById("mid-focusable") as HTMLElement).focus();
    const e = press("Tab");
    expect(e.defaultPrevented).toBe(false);
  });

  it("Tab does nothing when no dialog is open", () => {
    const trigger = document.getElementById("trigger") as HTMLButtonElement;
    trigger.focus();
    const e = press("Tab");
    expect(e.defaultPrevented).toBe(false);
  });

  // RED-PROVED: deleting the `if (e.shiftKey && document.activeElement ===
  // first) { e.preventDefault(); last.focus(); }` branch in
  // `handleTabTrap` makes the Shift+Tab-wraps test above fail (focus stays
  // on `first-focusable`, and `defaultPrevented` is false) — confirmed by
  // temporarily deleting it and re-running; restored afterward.
  it("focus that has escaped the dialog is pulled back to the first focusable element", () => {
    openModalEl("modalbg");
    (document.getElementById("trigger") as HTMLElement).focus(); // outside the dialog
    const e = press("Tab");
    expect(e.defaultPrevented).toBe(true);
    expect(document.activeElement?.id).toBe("first-focusable");
  });
});

describe("dialogManager — Escape closes the topmost dialog only (viewer-keyboard.spec.js test 3)", () => {
  it("Escape closes whichever single dialog is open", () => {
    openModalEl("modalbg");
    press("Escape");
    expect(getOpenId()).toBeNull();
  });

  it("Escape with two dialogs opened in sequence closes the SECOND (visually topmost) one — not both, not the first", () => {
    openModalEl("modalbg");
    openModalEl("imodalbg"); // swap — modalbg closes silently, imodalbg is now the only one open
    expect(getOpenId()).toBe("imodalbg");
    press("Escape");
    expect(getOpenId()).toBeNull();
  });

  it("Escape does nothing when no dialog is open", () => {
    const e = press("Escape");
    // Escape always runs `closeOpenModal()`, but with nothing open there is
    // nothing to preventDefault or restore — just confirm no throw and no
    // state change.
    expect(e).toBeTruthy();
    expect(getOpenId()).toBeNull();
  });

  /** The `ModalId` type is not enforced at the `window.openModalEl` boundary,
   * which is the entire reason that global exists. Unguarded, an unknown id
   * set `openId` to an element that does not exist: nothing rendered, and the
   * next Escape anywhere on the page was silently swallowed closing it.
   * Legacy no-ops on a missing element (viewer.html:2928). */
  it("an unknown modal id is a no-op, and does not arm Escape against a dialog that isn't there", () => {
    // The untyped call IS the case under test — this is how the global is reached.
    (openModalEl as unknown as (id: string) => void)("bogus");
    expect(getOpenId()).toBeNull();

    // The real proof: a subsequent Escape must still be a genuine no-op
    // rather than being consumed by the phantom.
    press("Escape");
    expect(getOpenId()).toBeNull();
  });

  /** Pins that the guard rejects only what is genuinely unknown — a guard
   * that rejected everything would pass the test above and break the app. */
  it("every id in MODAL_IDS still opens", () => {
    for (const id of MODAL_IDS) {
      openModalEl(id);
      expect(getOpenId()).toBe(id);
      closeOpenModal();
      expect(isModalOpen(id)).toBe(false);
    }
  });
});
