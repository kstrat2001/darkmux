import { describe, it, expect, vi, afterEach } from "vitest";
import { mountApp } from "./mountApp";

/**
 * These exercise `mountApp()` — the real entry-point logic `main.tsx`
 * delegates to (extracted specifically so this file can call it directly
 * instead of relying on an import's module-level side effects, which vitest
 * can't easily observe or reset between cases).
 */

afterEach(() => {
  document.body.innerHTML = "";
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  vi.restoreAllMocks();
});

describe("mountApp — #root missing", () => {
  it("paints the boot-error surface instead of leaving the page silently blank", () => {
    // No #root in the document at all — index.html's own contract broken.
    document.body.innerHTML = "";
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    mountApp();

    expect(document.querySelector('[role="alert"]')).not.toBeNull();
    expect(document.body.textContent).toMatch(/#root element missing/);
    expect(errSpy).toHaveBeenCalled();
  });
});

describe("mountApp — global safety net", () => {
  it("paints the boot-error surface on an uncaught window error after mount (a later-tick throw the React tree never saw)", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    mountApp();
    expect(document.querySelector('[role="alert"]')).toBeNull();

    const err = new Error("a later-tick throw, e.g. a stream record handler");
    window.dispatchEvent(new ErrorEvent("error", { error: err, message: err.message }));

    const alertEl = document.querySelector('[role="alert"]');
    expect(alertEl).not.toBeNull();
    expect(alertEl?.textContent).toMatch(/a later-tick throw/);
  });

  it("paints the boot-error surface on an unhandled promise rejection", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    mountApp();

    // jsdom does not implement a full PromiseRejectionEvent constructor;
    // the handler only reads `.reason` off the event, so a plain Event with
    // that property attached exercises the same code path.
    const event = new Event("unhandledrejection") as Event & { reason?: unknown };
    event.reason = new Error("a poll's rejected fetch, never awaited");
    window.dispatchEvent(event);

    const alertEl = document.querySelector('[role="alert"]');
    expect(alertEl).not.toBeNull();
    expect(alertEl?.textContent).toMatch(/poll's rejected fetch/);
  });

  it("does not stack a second overlay on a second global error", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mountApp();

    window.dispatchEvent(new ErrorEvent("error", { error: new Error("first") }));
    window.dispatchEvent(new ErrorEvent("error", { error: new Error("second") }));

    expect(document.querySelectorAll('[role="alert"]').length).toBe(1);
  });
});

describe("mountApp — a cross-origin script error is not a darkmux failure", () => {
  it("does not paint over a working viewer when the browser withholds the error (an extension's script)", () => {
    // A script from an origin with no CORS headers throws: the browser sets
    // `error` to null and `message` to the literal "Script error.", by
    // design. That carries no information about darkmux and is most often a
    // browser extension. Painting a full-page "darkmux failed to start" for
    // it is a false alarm over a perfectly working viewer — reproduced in a
    // real browser against the built artifact before this was fixed, because
    // the suppression tested `event.error ?? event.message` for null, which
    // the `??` had already replaced with the non-null string "Script error.".
    document.body.innerHTML = '<div id="root"></div>';
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    mountApp();
    window.dispatchEvent(new ErrorEvent("error", { error: null, message: "Script error." }));

    expect(document.querySelector(".bootcrash-overlay")).toBeNull();
    // Suppressed from the SCREEN, never from the console.
    expect(errSpy).toHaveBeenCalled();
  });

  it("still paints for a real error, so the suppression is not a blanket mute", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    mountApp();
    window.dispatchEvent(new ErrorEvent("error", { error: new Error("a real same-origin throw") }));

    expect(document.querySelector(".bootcrash-overlay")).not.toBeNull();
    expect(document.body.textContent).toMatch(/a real same-origin throw/);
  });
});

describe("mountApp — the overlay takes focus and the app behind it stops being reachable", () => {
  it("moves focus into the surface and marks #root inert", () => {
    // Without this the overlay was visual-only: measured in a real browser,
    // focus stayed on the input the operator had been typing in and Tab
    // walked 36 controls hidden behind the overlay.
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mountApp();

    window.dispatchEvent(new ErrorEvent("error", { error: new Error("boom") }));

    const overlay = document.querySelector(".bootcrash-overlay");
    expect(overlay?.contains(document.activeElement)).toBe(true);
    expect(document.getElementById("root")?.getAttribute("inert")).toBe("");
    expect(document.getElementById("root")?.getAttribute("aria-hidden")).toBe("true");
  });
});

describe("mountApp — a later-tick throw over a LIVE app is escapable", () => {
  function mountOverLiveApp() {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mountApp();
    // React 18's render is async, so `#root` is still ours in this tick.
    // Standing in for a committed app tree: what `paintBootError` reads is
    // simply "does #root have children", from the live DOM.
    document.getElementById("root")!.innerHTML = "<div>a live, mounted app</div>";
    window.dispatchEvent(new ErrorEvent("error", { error: new Error("a poll threw on tick 7") }));
  }

  it("offers dismiss, because reload would throw away state the app underneath still holds", () => {
    mountOverLiveApp();
    const buttons = [...document.querySelectorAll(".bootcrash-overlay button")].map((b) => b.textContent);
    expect(buttons).toContain("dismiss");
    expect(buttons).toContain("reload");
  });

  it("dismiss removes the overlay and gives the app behind it back", () => {
    mountOverLiveApp();
    const dismiss = [...document.querySelectorAll<HTMLButtonElement>(".bootcrash-overlay button")].find(
      (b) => b.textContent === "dismiss",
    );
    dismiss!.click();

    expect(document.querySelector(".bootcrash-overlay")).toBeNull();
    expect(document.getElementById("root")?.hasAttribute("inert")).toBe(false);
    expect(document.getElementById("root")?.hasAttribute("aria-hidden")).toBe(false);
    // And the overlay is not a one-shot: a later error must still be able to
    // paint, which the live-DOM idempotence check (not a flag) allows.
    window.dispatchEvent(new ErrorEvent("error", { error: new Error("a second, later failure") }));
    expect(document.querySelector(".bootcrash-overlay")).not.toBeNull();
  });

  it("offers NO dismiss when #root is empty — a real boot failure has nothing to go back to", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mountApp();
    window.dispatchEvent(new ErrorEvent("error", { error: new Error("failed before first paint") }));

    const buttons = [...document.querySelectorAll(".bootcrash-overlay button")].map((b) => b.textContent);
    expect(buttons).toEqual(["reload"]);
  });
});
