import { describe, it, expect, vi, afterEach, afterAll } from "vitest";
import { act } from "@testing-library/react";
import { mountApp, isExtensionOrigin } from "./mountApp";

/**
 * These exercise `mountApp()` — the real entry-point logic `main.tsx`
 * delegates to (extracted specifically so this file can call it directly
 * instead of relying on an import's module-level side effects, which vitest
 * can't easily observe or reset between cases).
 */

/**
 * (#2501) Every React root a test mounts is unmounted before the test ends.
 *
 * `mountApp()` renders the whole app concurrently, so its first render is
 * still queued when a test returns. A root left mounted kept rendering after
 * the last test, and when jsdom tore its window down first the commit threw
 * `window is not defined`: the file exited 1 with every test passing. This
 * wraps `createRoot` to count roots that are still mounted, so a test that
 * leaves one behind fails here, at its own name.
 *
 * Unmounted is necessary, not sufficient: `root.unmount()` alone does not
 * cancel a render already queued, which still commits on a later scheduler
 * turn. So the wrapper also reads, the moment `unmount()` returns, whether
 * React still holds work for that root (`pendingLanes`, `callbackNode` on
 * React 18's internal root), and counts a root unmounted with work queued.
 * That is why every mount below runs inside `act`, and why the `afterAll`
 * teardown check further down exists.
 */
const liveRoots = vi.hoisted(() => new Set<object>());
const unmountedWithWorkQueued = vi.hoisted(() => ({ count: 0 }));
vi.mock("react-dom/client", async (importOriginal) => {
  const real = await importOriginal<typeof import("react-dom/client")>();
  const createRoot: typeof real.createRoot = (...args) => {
    const root = real.createRoot(...args);
    liveRoots.add(root);
    const unmount = root.unmount.bind(root);
    const internal = (root as unknown as { _internalRoot: { pendingLanes: number; callbackNode: unknown } })
      ._internalRoot;
    root.unmount = () => {
      liveRoots.delete(root);
      unmount();
      if (internal.pendingLanes !== 0 || internal.callbackNode !== null) unmountedWithWorkQueued.count++;
    };
    return root;
  };
  return { ...real, createRoot };
});

/**
 * Mount the app inside React's `act`, so the first render commits (and its
 * effects run) before `act` returns, instead of on scheduler turns nobody
 * waits for. `act` is React's own flush of the work it queued.
 *
 * The returned unmount runs as production runs it, outside `act`; with the
 * first render already committed there is nothing queued for it to leave
 * behind, which the `createRoot` wrapper above checks on every unmount.
 */
function mountInAct(): () => Promise<void> {
  let unmount = async () => {};
  act(() => {
    unmount = mountApp();
  });
  return unmount;
}

/**
 * Wait until every `selectionchange` already queued has been delivered.
 *
 * Focusing an element (the overlay focuses its first button) or blurring one
 * moves the document selection, and jsdom delivers `selectionchange` later,
 * from a zero-delay Node timer. React listens for that event on `document`
 * for the document's lifetime, mounted or not, and its handler reads
 * `window`: one still queued at teardown throws there. Node runs timers of
 * equal delay in the order they were queued, so a zero-delay timer queued
 * now runs after every one jsdom queued before it, however loaded the
 * machine is.
 */
function queuedSelectionChangesDelivered(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

/** Every test mounts through this, so `afterEach` can take each app down. */
const mounted: Array<() => Promise<void>> = [];
function mount(): void {
  mounted.push(mountInAct());
}

afterEach(async () => {
  for (const unmount of mounted.splice(0)) await unmount();
  const leaked = liveRoots.size;
  // Take a leaked root down anyway, so one test's leak fails that test alone.
  for (const root of [...liveRoots]) (root as { unmount: () => void }).unmount();
  const leftWorking = unmountedWithWorkQueued.count;
  unmountedWithWorkQueued.count = 0;
  expect(leaked, "a test left a React root mounted").toBe(0);
  expect(leftWorking, "a root was unmounted with a render still queued, which commits after the test").toBe(0);
  document.body.innerHTML = "";
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  vi.restoreAllMocks();
  await queuedSelectionChangesDelivered();
});

/**
 * (#2501) Nothing this file started may still run once its environment is
 * gone.
 *
 * After the last test, vitest's jsdom environment deletes `window`,
 * `document` and the rest of the DOM globals. Any React work still queued
 * then throws `window is not defined`, which fails the run with every test
 * passing, and only when that work happens to lose the race. This removes the
 * same globals itself, gives queued work a window to run in, and fails the
 * file if anything threw, every run, instead of the race deciding.
 */
const TEARDOWN_GLOBALS = ["window", "self", "top", "parent", "document"] as const;
/** How long queued work gets to show itself. A detection window, not a
 * wait the app depends on: React's scheduler runs a render in 5ms slices
 * across turns, and a whole-app first render takes several. */
const TEARDOWN_WINDOW_MS = 300;

async function errorsAfterSimulatedTeardown(): Promise<string[]> {
  const errors: string[] = [];
  const record = (e: unknown) => errors.push(e instanceof Error ? `${e.name}: ${e.message}` : String(e));
  // Node's own timer, captured before the globals go; it outlives them.
  const timeout = globalThis.setTimeout;
  const started = Date.now();
  const saved = TEARDOWN_GLOBALS.map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)] as const);
  process.on("uncaughtException", record);
  process.on("unhandledRejection", record);
  try {
    for (const [key] of saved) delete (globalThis as Record<string, unknown>)[key];
    while (Date.now() - started < TEARDOWN_WINDOW_MS) await new Promise((r) => timeout(r, 0));
  } finally {
    for (const [key, descriptor] of saved) if (descriptor) Object.defineProperty(globalThis, key, descriptor);
    process.off("uncaughtException", record);
    process.off("unhandledRejection", record);
  }
  return errors;
}

afterAll(async () => {
  expect(await errorsAfterSimulatedTeardown(), "work this file queued ran after teardown").toEqual([]);
});

describe("mountApp — #root missing", () => {
  it("paints the boot-error surface instead of leaving the page silently blank", () => {
    // No #root in the document at all — index.html's own contract broken.
    document.body.innerHTML = "";
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    mount();

    expect(document.querySelector('[role="alert"]')).not.toBeNull();
    expect(document.body.textContent).toMatch(/#root element missing/);
    expect(errSpy).toHaveBeenCalled();
  });
});

describe("mountApp — unmounting takes down what mounting set up (#2501)", () => {
  /** Dispatch a darkmux-shaped error and rejection at `window`, with the
   * test's own listener attached: vitest reports an `error` event that no
   * listener receives as an uncaught exception, and the spy also proves the
   * events really reached `window`. */
  function dispatchAfterUnmount() {
    const received = vi.fn();
    window.addEventListener("error", received);
    try {
      window.dispatchEvent(new ErrorEvent("error", { error: new Error("after unmount") }));
      const event = new Event("unhandledrejection") as Event & { reason?: unknown };
      event.reason = new Error("a rejection after unmount");
      window.dispatchEvent(event);
    } finally {
      window.removeEventListener("error", received);
    }
    expect(received).toHaveBeenCalled();
  }

  /** Track every timer and animation frame scheduled from here on, keyed by
   * id, until it fires or is canceled. `window` is the global in this
   * environment, so the app's bare `setTimeout(...)` calls land here. */
  function trackScheduledWork(): Map<unknown, string> {
    const pending = new Map<unknown, string>();
    const real = {
      setTimeout: window.setTimeout,
      setInterval: window.setInterval,
      requestAnimationFrame: window.requestAnimationFrame,
    };
    const realCancel = {
      clearTimeout: window.clearTimeout,
      clearInterval: window.clearInterval,
      cancelAnimationFrame: window.cancelAnimationFrame,
    };
    const track = <A extends unknown[], R>(name: keyof typeof real, schedule: (...a: A) => R) =>
      ((cb: (...x: unknown[]) => void, ...rest: unknown[]) => {
        const id: R = (schedule as (...a: unknown[]) => R).call(window, (...x: unknown[]) => {
          if (name !== "setInterval") pending.delete(id);
          cb(...x);
        }, ...rest);
        pending.set(id, `${name}(${String(rest[0] ?? "")})`);
        return id;
      }) as never;
    const untrack = (cancel: (id: never) => void) => ((id: unknown) => {
      pending.delete(id);
      cancel.call(window, id as never);
    }) as never;
    vi.spyOn(window, "setTimeout").mockImplementation(track("setTimeout", real.setTimeout));
    vi.spyOn(window, "setInterval").mockImplementation(track("setInterval", real.setInterval));
    vi.spyOn(window, "requestAnimationFrame").mockImplementation(
      track("requestAnimationFrame", real.requestAnimationFrame),
    );
    vi.spyOn(window, "clearTimeout").mockImplementation(untrack(realCancel.clearTimeout));
    vi.spyOn(window, "clearInterval").mockImplementation(untrack(realCancel.clearInterval));
    vi.spyOn(window, "cancelAnimationFrame").mockImplementation(untrack(realCancel.cancelAnimationFrame));
    return pending;
  }

  it("leaves no timer or animation frame scheduled once the rendered app is unmounted", async () => {
    // A timer outliving the app is the same leak in a slower form: it fires
    // after the app (or a test environment) is gone. The query cache's
    // five-minute garbage-collection timers were exactly that.
    const realSetTimeout = window.setTimeout;
    const pending = trackScheduledWork();
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    const unmount = mountInAct();
    expect(document.getElementById("root")!.children.length, "the app committed").toBeGreaterThan(0);
    expect(pending.size, "the mounted app schedules work, or this test proves nothing").toBeGreaterThan(0);
    await unmount();
    // The query cache flushes its change notifications on a zero-delay timer;
    // those fire on the next turn and reach no listener once unmounted. A
    // zero-delay timer queued now runs after them.
    await new Promise((resolve) => realSetTimeout(resolve, 0));

    expect([...pending.values()]).toEqual([]);
  });

  it("removes the global listeners, so an error after unmount paints nothing", async () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    await mountInAct()();
    dispatchAfterUnmount();

    expect(document.querySelector('[role="alert"]')).toBeNull();
  });

  it("removes the listeners on the #root-missing path too", async () => {
    document.body.innerHTML = "";
    vi.spyOn(console, "error").mockImplementation(() => {});
    await mountInAct()();
    document.body.innerHTML = "";

    dispatchAfterUnmount();

    expect(document.querySelector('[role="alert"]')).toBeNull();
  });
});

describe("mountApp — global safety net", () => {
  it("paints the boot-error surface on an uncaught window error after mount (a later-tick throw the React tree never saw)", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    mount();
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

    mount();

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
    mount();

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

    mount();
    window.dispatchEvent(new ErrorEvent("error", { error: null, message: "Script error." }));

    expect(document.querySelector(".bootcrash-overlay")).toBeNull();
    // Suppressed from the SCREEN, never from the console.
    expect(errSpy).toHaveBeenCalled();
  });

  it("still paints for a real error, so the suppression is not a blanket mute", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});

    mount();
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
    mount();

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
    mount();
    // `mount()` commits the real app inside `act`, so `#root` holds a live,
    // mounted tree: what `paintBootError` reads is "does #root have
    // children", from the live DOM.
    expect(document.getElementById("root")!.children.length).toBeGreaterThan(0);
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
    // The error lands inside `act`, after `render()` was called but before
    // React has committed anything: a failure before first paint.
    act(() => {
      mounted.push(mountApp());
      window.dispatchEvent(new ErrorEvent("error", { error: new Error("failed before first paint") }));
    });

    const buttons = [...document.querySelectorAll(".bootcrash-overlay button")].map((b) => b.textContent);
    expect(buttons).toEqual(["reload"]);
  });
});

describe("a browser extension's rejection is not darkmux failing (#2816)", () => {
  // Reported by the operator opening the viewer in Chrome with MetaMask
  // installed: a full-page "darkmux hit an unexpected error — unhandled
  // promise rejection / Failed to connect to MetaMask" over a viewer that was
  // running perfectly. MetaMask injects into every page and rejects on ones
  // that have nothing to do with wallets, so this lands on a large share of
  // Chrome installs, on FIRST OPEN, before the operator has done anything.
  //
  // The `error` listener had been fixed for its own version of this. This one
  // is the sibling that was missed, and its case is not detectable the same
  // way — a cross-origin script error arrives informationless, a rejected
  // promise carries a real reason with a real stack.
  const metaMaskRejection = () => {
    const e = new Error("Failed to connect to MetaMask");
    e.stack =
      "Error: Failed to connect to MetaMask\n    at Object.connect " +
      "(chrome-extension://nkbihfbeogaeaoehlefnkodbefgpgknn/scripts/inpage.js:7:84292)";
    return e;
  };

  // THE WIRING, not just the predicate. Testing `isExtensionOrigin` alone
  // left the listener free to ignore it: disabling the suppression at the
  // call site kept all 14 tests green. This drives the real
  // `unhandledrejection` listener and asserts NO overlay is painted, which is
  // the behavior the operator actually reported.
  it("paints NO overlay when the rejection came from an extension", () => {
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mount();

    const event = new Event("unhandledrejection") as Event & { reason?: unknown };
    event.reason = metaMaskRejection();
    window.dispatchEvent(event);

    expect(
      document.querySelector('[role="alert"]'),
      "an extension's failure must not put a darkmux error over a working viewer",
    ).toBeNull();
  });

  it("still paints for a rejection that IS darkmux's", () => {
    // The guard against over-suppression, at the same grain.
    document.body.innerHTML = '<div id="root"></div>';
    vi.spyOn(console, "error").mockImplementation(() => {});
    mount();

    const ours = new Error("a poll's rejected fetch");
    ours.stack = "Error: a poll's rejected fetch\n    at loadRuns (http://macbook-pro:8090/assets/next.js:1:1)";
    const event = new Event("unhandledrejection") as Event & { reason?: unknown };
    event.reason = ours;
    window.dispatchEvent(event);

    expect(document.querySelector('[role="alert"]')?.textContent).toMatch(/rejected fetch/);
  });

  it("suppresses a rejection whose stack comes from an extension", () => {
    expect(isExtensionOrigin(metaMaskRejection())).toBe(true);
  });

  it("recognises the other vendors' extension schemes too", () => {
    for (const scheme of ["moz-extension", "safari-web-extension", "ms-browser-extension"]) {
      const e = new Error("boom");
      e.stack = `Error: boom\n    at x (${scheme}://abc/inpage.js:1:1)`;
      expect(isExtensionOrigin(e), scheme).toBe(true);
    }
  });

  it("NEVER suppresses a rejection from darkmux's own code", () => {
    // The dangerous direction. A false negative paints an error the operator
    // can dismiss; a false positive swallows a real failure silently.
    const ours = new Error("fetch failed");
    ours.stack = "Error: fetch failed\n    at loadRuns (http://macbook-pro:8090/assets/next.js:1:1)";
    expect(isExtensionOrigin(ours)).toBe(false);
  });

  it("treats an unreadable reason as ours rather than guessing", () => {
    expect(isExtensionOrigin("just a string")).toBe(false);
    expect(isExtensionOrigin(null)).toBe(false);
    expect(isExtensionOrigin(undefined)).toBe(false);
    expect(isExtensionOrigin({ nope: 1 })).toBe(false);
    // An Error with no stack at all: cannot attribute it, so it is ours.
    const bare = new Error("no stack");
    bare.stack = undefined;
    expect(isExtensionOrigin(bare)).toBe(false);
  });
});
