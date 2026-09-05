import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent, cleanup, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { clkhm } from "./lib/format";
import { fmtElapsed } from "./lib/format";

/**
 * Regression test for the `useSyncExternalStore` snapshot-stability bug
 * caught live by this packet's Playwright render-sanity proof: `parseRoute()`
 * returned a fresh object on every call, which is NOT a stable snapshot and
 * threw React error #185 ("Maximum update depth exceeded") the instant `App`
 * mounted — a real page load never got past a blank screen. `jsdom` (this
 * test's environment) reproduces the same React invariant, so this is a fast
 * unit-level guard even though the ORIGINAL bug was only actually caught by
 * the slower live-browser proof (see the packet report for why: no existing
 * test rendered `<App>` itself, only its leaf components).
 */
afterEach(() => {
  // Unmount between tests: without a setup file RTL does not auto-clean, and
  // a previous test's App stays mounted, still subscribed to the hash and the
  // fetch stub of the test that follows.
  cleanup();
  vi.unstubAllGlobals();
  window.location.hash = "";
});

/**
 * `.machine-lens__hdr`'s "fleet › machine · <label>" line (drill-in
 * packet) — its LEADING "fleet" segment is now a real `<button>` (the
 * back-link, see `MachineLens.tsx`'s own doc), so the line is no longer
 * one flat run of sibling text nodes the way it was pre-drill-in.
 * `screen.getByText`'s DEFAULT matcher (`getNodeText`, `@testing-library/
 * dom`) deliberately reads only an element's OWN DIRECT text-node
 * children, never a nested element's — a documented RTL behavior (not a
 * bug), and exactly why a regex spanning "fleet" + " › machine" no longer
 * matches either the button (whose own text is just "fleet") or the outer
 * div (whose own direct-text-node content is now " › machine · <label>",
 * missing "fleet"). A REAL browser's `innerText` (what the parity harness
 * actually compares) has no such limitation — this is a jsdom/RTL query
 * mechanics gotcha, not a behavior change goldens would catch. Fixed by
 * matching against the CONTAINER's real `textContent` (which, like
 * `innerText`, recurses through nested elements) instead of relying on
 * `getByText`'s element-scoped default. */
function machineHeaderMatches(re: RegExp): boolean {
  const hdr = document.querySelector(".machine-lens__hdr");
  return !!hdr && re.test(hdr.textContent || "");
}

/** (#1729) The presence endpoints answer an ENVELOPE, not a bare array. The
 *  hooks tolerate a bare array via `?? []`, which means a stale stub goes
 *  silently empty instead of failing — the exact trap that let the real
 *  breakage sit behind 222 green tests. Stubs speak the real shape. */
const FLEET_OFF = (key: "machines" | "sessions") => ({
  [key]: [],
  meta: { sources: { fleet: { state: "off" } }, complete: true },
});

describe("App", () => {

  it("mounts without an infinite update-depth error and renders the fleet lens by default", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(document.getElementById("stage")).toBeTruthy();
    // Packet 8: the default route is `FleetLens` (the savings hero +
    // machine cards + activity timeline), superseding the scaffold's
    // original `FleetStrip` presence-only region — see that component's
    // own doc. With every endpoint answering a blank `[]`, the hero still
    // renders (always-render-even-at-zero, per its own doc) and the
    // timeline falls to its empty-fleet branch.
    await waitFor(() => expect(screen.getByText(/tokens · last/i)).toBeInTheDocument());
    expect(screen.getByText(/waiting for the first flow record/i)).toBeInTheDocument();
  });

  it("renders the real console lens (not a placeholder) for #lens=console", async () => {
    window.location.hash = "#lens=console";
    // App-level `useFlowWindow`/`useLiveMachines`/`machineSpecs` (Packet 2's
    // GLOBAL `#meta` chrome, wired into every route, not just `#lens=machine`)
    // fire alongside the console lens's own `/panel/*` fetch — a mock that
    // ONLY answers `/panel/*` throws inside `useLiveMachines` (`query.data.data
    // is not iterable`) the instant this route mounts. Route on URL: the panel
    // endpoint gets real content, everything else gets an empty-but-valid `[]`
    // (same blanket default the sibling "machine lens" test below uses).
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        // (#1905 step 3) The console's default landing panel is `run-list`
        // — a real CLI panel, fetched through `/panel/run-list` exactly
        // like any other panel. No `/runs` fetch happens on this route at
        // all any more (the #1904 client-rendered activity view that used
        // to read `/runs` directly is deleted).
        if (typeof url === "string" && url.startsWith("/panel/")) {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                panel: "run-list",
                argv: ["run", "list"],
                opts: { kind: "all", all: "recent" },
                captured_ts_ms: Date.now(),
                gather_ms: 1,
                exit_code: 0,
                ansi_text: "no runs",
                stderr_tail: "",
                cols: 100,
                cache_ttl_ms: 3000,
                age_ms: 0,
                auto_refresh: true,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.queryByText(/lens not ported yet/i)).not.toBeInTheDocument();
    // (#1905 step 3) The default landing content is `run-list`'s own CLI
    // output now, not a client-rendered activity view's empty state.
    await waitFor(() => expect(screen.getByText("no runs")).toBeInTheDocument());
  });

  it("renders the real session run view for #session=<id> (drill-in packet — SessionReplay is no longer a placeholder)", async () => {
    // `#lens=console` was this test's original target before Packet 6 ported
    // the console lens for real, then `#session=<id>` rendered a bare
    // `LensPlaceholder` before the drill-in packet landed `SessionReplay`'s
    // REAL render (`runRegions()`, see that component's own doc) — this is
    // the App-routing regression guard for that path: a session route
    // reaches a genuine rendered view, not a placeholder or a blank page.
    // The DETAILED derivation (brief rows, metrics, detections) is
    // `sessionRun.test.ts`'s job (including a byte-parity check against a
    // real recorded legacy golden); this test only proves App wires the
    // route to the real component.
    window.location.hash = "#session=abc-123";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (typeof url === "string" && url.startsWith("/flow-session/")) {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                records: [{ ts: "2026-01-01T00:00:00Z", session_id: "abc-123", action: "dispatch.start", handle: "coder" }],
                count: 1,
                truncated: false,
                generated_at_ms: Date.now(),
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.queryByText(/lens not ported yet/i)).not.toBeInTheDocument();
    await waitFor(() => expect(document.querySelector('.session-run[data-state="data"]')).not.toBeNull());
    // "CODER" is one of several sibling text nodes inside `.session-run__header`
    // (alongside the pill `<span>` and the meta `<span>`) — `getByText`'s
    // default matcher reads only an element's OWN direct text-node children
    // (see `machineHeaderMatches`'s own doc for the same gotcha), so a plain
    // textContent check on the header is the reliable form here too.
    expect(document.querySelector(".session-run__header")?.textContent).toContain("CODER");
    expect(screen.getByText("signals")).toBeInTheDocument();
  });

  it("renders the machine lens (Packet 2) instead of a placeholder", async () => {
    window.location.hash = "#lens=machine";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.queryByText(/lens not ported yet/i)).not.toBeInTheDocument();
    // The stagehdr line renders immediately (synchronous, no fetch needed
    // for its fallback text) even before the specs/flow-window queries
    // settle — see `MachineLens`'s `label` fallback ("this machine").
    expect(machineHeaderMatches(/fleet › machine/)).toBe(true);
  });

  // (operator finding, phone screenshot) `#crumb` used to repeat the
  // machine name (`routeChrome`'s `machine` branch, `App.tsx`) — folded
  // into the tab row on desktop, but given its OWN full-width row by the
  // mobile stylesheet (`.app-shell__crumb { flex: 1 1 100%; }` under
  // `max-width: 768px`), so a phone showed a whole standalone line reading
  // just the machine name, directly above `MachineLens`'s own
  // `.machine-lens__hdr` breadcrumb ("fleet › machine — <spec>"), which had
  // already dropped its own copy of the name for the identical reason. The
  // fix is App.tsx not rendering `<header id="crumb">` AT ALL on the
  // machine route — jsdom performs no layout, so this doesn't simulate the
  // phone breakpoint, but a genuinely absent element renders at every
  // width, mobile included; the real, viewport-verified behavior is the
  // `next-parity.spec.ts` machine-lens tests (desktop) and this fix's own
  // real-browser screenshots (phone + desktop).
  it("(operator finding) never renders #crumb on the machine lens — no machine-name row above the breadcrumb, at any width", async () => {
    window.location.hash = "#lens=machine";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(machineHeaderMatches(/fleet › machine/)).toBe(true));
    expect(document.getElementById("crumb")).toBeNull();
    // The sibling `#meta` summary row is untouched by this fix — it must
    // still be present and still sit inside the sticky block.
    expect(document.getElementById("meta")).toBeTruthy();
  });

  it("renders a named placeholder (with the raw hash) for an unrecognized route, never a blank page", () => {
    window.location.hash = "#lens=totally-bogus";
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.getByText(/lens not ported yet: unrecognized/i)).toBeInTheDocument();
    expect(screen.getByText(/lens=totally-bogus/)).toBeInTheDocument();
  });

  // (#1920) The placeholder used to send the operator to "the legacy
  // viewer at GET /" — a page that stopped existing when viewer.html was
  // deleted (#1865). `GET /` now serves this SAME app, so that note lied.
  // The fix names what actually happened and points at a real, on-screen
  // way out (the nav tabs `NavChrome` always renders above this component).
  it("names the hash as unrecognized and points at the nav tabs, never the deleted legacy viewer", () => {
    window.location.hash = "#lens=totally-bogus";
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.getByText(/doesn't recognize that hash/i)).toBeInTheDocument();
    expect(screen.getByText(/pick a lens from the tabs above/i)).toBeInTheDocument();
    expect(screen.queryByText(/legacy viewer/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/see.*get \//i)).not.toBeInTheDocument();
    // The nav tabs really are on screen — the note's claim is true, not
    // just plausible-sounding.
    expect(screen.getByRole("link", { name: "fleet" })).toBeInTheDocument();
  });

  // Packet 1.5: nav chrome + hash write-back, wired at the App root.
  //
  // `mockFleetLikeFetch` returns a bare `[]` ONLY for the fleet-shaped
  // endpoints these tests actually exercise, and a 404 for everything else
  // (`/machine/resources`, `/lab/runs`, ...) — mirroring
  // `tests/parity/lib/mock-routes.js`'s `installBlankRoutes` pattern rather
  // than the blanket `"[]"` stub the OTHER tests in this file use for their
  // narrower assertions. A blanket 200-with-`[]` for `/machine/resources`
  // parses fine as JSON but is the WRONG SHAPE (`MachineResources` is an
  // object, not an array) — `MachineLens`'s `healthLines` reads
  // `resources.machine.unpriced_models` and throws once that query settles,
  // which unmounts the whole tree by the time these tests' later
  // `document.getElementById(...)` assertions run. A 404 takes the
  // already-handled `resourcesErrored` branch instead, exactly like
  // `next-parity.spec.ts`'s own blank-daemon redprove test.
  function mockFleetLikeFetch() {
    return vi.fn((url: string) => {
      const path = String(url);
      if (path.includes("/lab/runs")) return Promise.resolve(new Response(JSON.stringify({ configured: true, dir: "", exists: true, runs: [] }), { status: 200 }));
      if (path.includes("/runs")) return Promise.resolve(new Response(JSON.stringify({ runs: [] }), { status: 200 }));
      if (path.includes("/fleet/machines/live"))
        return Promise.resolve(new Response(JSON.stringify(FLEET_OFF("machines")), { status: 200 }));
      if (path.includes("/fleet/sessions/live"))
        return Promise.resolve(new Response(JSON.stringify(FLEET_OFF("sessions")), { status: 200 }));
      if (path.includes("/fleet/") || path.includes("/flow")) return Promise.resolve(new Response("[]", { status: 200 }));
      return Promise.resolve(new Response("not recorded in this mock\n", { status: 404 }));
    });
  }

  it("clicking the machine tab navigates the whole app (route AND #stage change) and updates the address bar", async () => {
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(machineHeaderMatches(/fleet › machine/)).toBe(false);

    fireEvent.click(document.getElementById("lens-machine")!);

    expect(window.location.hash).toBe("#lens=machine");
    await waitFor(() => expect(machineHeaderMatches(/fleet › machine/)).toBe(true));
    expect(document.getElementById("lens-machine")!.className).toMatch(/\bon\b/);
  });

  it("arriving on the legacy #lens=lab alias upgrades the address bar to the canonical #lens=runs&kind=lab", async () => {
    window.location.hash = "#lens=lab";
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&kind=lab"));
    expect(document.getElementById("lens-runs")!.className).toMatch(/\bon\b/);
  });

  it("a runs-lens kind-chip click writes the hash directly, without a route change", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs"));

    const missionChip = await waitFor(() => {
      const el = document.querySelector('[data-arg="mission"]');
      expect(el, '.runchip[data-arg="mission"] must be present once the runs board has loaded').not.toBeNull();
      return el as HTMLElement;
    });
    fireEvent.click(missionChip);

    await waitFor(() => expect(window.location.hash).toBe("#lens=runs&kind=mission"));
    // The nav tab stays on "runs" (no route change occurred — the runs lens
    // is still the active lens, just re-filtered client-side).
    expect(document.getElementById("lens-runs")!.className).toMatch(/\bon\b/);
  });

  // (QA, packet 5) The live badge must not speak for a view that has no tail.
  // `useLiveTail(false)` returns its INITIAL "live" untouched, so rendering
  // the badge unconditionally made a replay route claim `● live` with no
  // stream, no reconcile, and no liveness of any kind — #1480's dishonesty
  // pointed the other way.
  // (header owns liveness, operator 2026-09-03) A daemon dispatch page whose
  // day is NOT yet known is a live page like any other: the app-level tail
  // runs there and the header says so. (It used to be excluded so the lens
  // could own liveness; see `isLiveRoute`.) The replay case — day known,
  // transport shown — is covered below ("names its day in the chip").
  it("shows the live badge on a dispatch page whose day is not yet known", async () => {
    window.location.hash = "#session=abc-123";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { container } = render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(container.querySelector(".crumbbar, #stage")).toBeTruthy());
    await waitFor(() => expect(container.querySelector("#modebadge")).toBeTruthy());
  });

  it("DOES show the live badge on a live route — the inverted case", async () => {
    // Guards the gate from over-firing: the default fleet view is live, and
    // silently dropping its badge would be its own dishonesty.
    window.location.hash = "";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { container } = render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(container.querySelector("#modebadge")).toBeTruthy());
  });

  // (Chrome packet) The masthead is now App-level chrome, mounted
  // unconditionally above the crumbbar.
  it("renders the masthead brand on the default route (machine/fleet routes are covered byte-for-byte by next-parity)", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(screen.getByText("darkmux")).toBeInTheDocument());
  });

  // (Chrome packet) `showsEventLog` is a pure-function unit-tested directly
  // in `lib/route.test.ts`; these assert the WIRING — that `App.tsx` mounts
  // `EventLogColumn` on EVERY route (never conditionally, per that
  // component's own `visible` doc — legacy's real `#logscope` stays present
  // even when its ancestor is CSS-hidden), and that the `eventlog--hidden`
  // class (not a missing DOM node) is what actually hides it on
  // `runs`/`console`/`machine`. `#logscope`'s continued PRESENCE, with a
  // real value, on every route is itself the fix for the stray-uppercase-
  // "FLEET" bug — that bug was about a `#logscope` rendered in the WRONG
  // PLACE (loose, above the stage) with the WRONG SCOPE (always "FLEET"
  // regardless of lens), not about existing at all.
  it("mounts the event-log column visibly (not eventlog--hidden) on the default fleet route", async () => {
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.getElementById("logbody")).toBeTruthy());
    expect(document.getElementById("logscope")?.hasAttribute("hidden")).toBe(true);
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
  });


  /* (#1066) These three used to assert `eventlog--hidden` on runs/console/
     machine. That was PARITY with `viewer.html`'s `runs-mode`/`machine-mode`,
     measured directly at the time via `getComputedStyle('.log').display` —
     correct while that viewer still served users. It was deleted in #1865, so
     the rule was matching a thing that no longer exists, against an operator
     asking for the opposite: the events panel as a collapsible mainstay on
     all tabs.

     The assertion is INVERTED rather than deleted, deliberately: the column
     must still be MOUNTED on every route (legacy keeps `#logscope` in the DOM
     with real text even when its ancestor is hidden, and the machine-lens
     byte-parity goldens depend on that), so "not hidden" is a different claim
     from "absent" and both still need proving.

     `mission` keeps its exclusion and keeps its test — that one is structural,
     not parity: `MissionGraphLens` mounts its own instance, and two would
     disagree about scope (#1868). */
  it("keeps the event-log column VISIBLE on the machine lens (#1066)", async () => {
    window.location.hash = "#lens=machine";
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(machineHeaderMatches(/fleet › machine/)).toBe(true));
    expect(document.getElementById("logbody")).toBeTruthy();
    // `#logscope` stays PRESENT (so this port's parity extraction matches
    // legacy's) but is now EMPTY everywhere: the outer UI owns context. What
    // this test still guards is the real DOM/CSS state — the column is
    // mounted and merely unpainted, not unmounted.
    expect(document.getElementById("logscope")).toBeTruthy();
    expect(document.getElementById("logscope")?.hasAttribute("hidden")).toBe(true);
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
  });

  it("keeps the event-log column VISIBLE on the console lens (#1066)", async () => {
    window.location.hash = "#lens=console";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        // (#1905 step 3) The console's default landing panel is `run-list`
        // — a real CLI panel, fetched through `/panel/run-list` like any
        // other. No `/runs` fetch happens on this route any more.
        if (typeof url === "string" && url.startsWith("/panel/")) {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                panel: "run-list",
                argv: ["run", "list"],
                opts: { kind: "all", all: "recent" },
                captured_ts_ms: Date.now(),
                gather_ms: 1,
                exit_code: 0,
                ansi_text: "no runs",
                stderr_tail: "",
                cols: 100,
                cache_ttl_ms: 3000,
                age_ms: 0,
                auto_refresh: true,
              }),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    // (#1905 step 3) The default landing content is `run-list`'s own CLI
    // output now, not a client-rendered activity view's empty state.
    await waitFor(() => expect(screen.getByText("no runs")).toBeInTheDocument());
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
  });

  it("keeps the event-log column VISIBLE on the runs lens (#1066)", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs"));
    expect(document.querySelector(".eventlog")?.className).not.toMatch(/eventlog--hidden/);
  });

  /**
   * (#1800 P2, QA gate) The composed-app assertion the LENS-level test could
   * not make. `FleetLens` gates its own presence hooks with `enabled: false`
   * on a replay — and that was already true when the gate caught this. It was
   * not enough: `App` held its OWN `useLiveMachines()` observer on the same
   * query key, unconditionally, on every route. A disabled TanStack observer
   * still reads the shared cache, so the enabled one kept the data warm and
   * kept polling `/fleet/machines/live` behind the replay (measured: 2 polls
   * in 9s, and a fleet card for a machine with zero records that day).
   *
   * The lens's own test rendered in ISOLATION, where no second observer
   * exists, and passed throughout. So this assertion belongs HERE, at the
   * level where the bug was reachable — not one component down.
   */
  it("a replay route never touches the live presence endpoints, even from App", async () => {
    window.location.hash = "#2026-08-07";
    const fetchSpy = vi.fn((url: string) => {
      const path = String(url);
      if (path === "/flow/2026-08-07") {
        return Promise.resolve(
          new Response(
            JSON.stringify([
              { ts: "2026-08-07T02:09:42.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start" },
              { ts: "2026-08-07T18:28:15.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.complete" },
            ]),
            { status: 200 },
          ),
        );
      }
      return Promise.resolve(new Response("[]", { status: 200 }));
    });
    vi.stubGlobal("fetch", fetchSpy);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );

    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());

    const urls = fetchSpy.mock.calls.map((c) => String(c[0]));
    // The list is EXHAUSTIVE on purpose. The first version of this assertion
    // named only `/fleet/*`, and `/machine/specs` — a third live-only endpoint,
    // ungated by the same oversight — sailed through it and was caught by CI
    // instead, as a one-line golden diff ("Apple M5 Max · 128 GB" where legacy
    // reads "hardware not reported"). An allowlist of the endpoints you
    // remembered to name is not a gate. Legacy's own rule is the general one:
    // `pollLiveMachines`, `pollLiveSessions` and `pollMachineSpecs` are all
    // live-mode-only polls, and a replay starts none of them.
    for (const live of ["/fleet/machines/live", "/fleet/sessions/live", "/machine/specs"]) {
      expect(urls.some((u) => u.includes(live)), `a replay must not fetch ${live}`).toBe(false);
    }
    // …and the day WAS actually fetched, so a hook quietly requesting nothing
    // at all cannot pass this by doing no work.
    expect(urls).toContain("/flow/2026-08-07");
  });

  /**
   * The replay-mode RENDER, asserted at App level for the same reason: every
   * one of these strings comes from a `liveMode ? ... : ...` branch that the
   * port had collapsed to its live arm. `goldens/playback-date.txt` is the
   * byte-level spec; this is the fast guard beneath it.
   */
  it("a replay renders the REPLAY arm of the fleet hero, not the live one", async () => {
    window.location.hash = "#2026-08-07";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "/flow/2026-08-07") {
          return Promise.resolve(
            new Response(
              JSON.stringify([
                // The flow file's leading schema header. It has no
                // `machine_uid`, so an unshaped read renders it as a phantom
                // "unknown" machine card and a third timeline lane.
                { _type: "schema", darkmux_version: "2.6.0", version: "1.18.0" },
                { ts: "2026-08-07T02:09:42.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.start" },
                { ts: "2026-08-07T09:00:00.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", action: "dispatch.complete" },
                { ts: "2026-08-07T10:00:00.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.start" },
                { ts: "2026-08-07T18:28:15.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s2", action: "dispatch.complete" },
              ]),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );

    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());

    // The hero eyebrow drops the window suffix — these numbers cover the
    // recorded day, not the last 24 hours.
    expect(screen.getByText("tokens")).toBeInTheDocument();
    expect(screen.queryByText(/tokens · last/i)).not.toBeInTheDocument();

    // The card counts the DAY's sessions and calls them specialists.
    expect(document.querySelector(".mach .runs")?.textContent).toBe("2 specialists");

    // The `_type` header contributed no machine card.
    expect(document.querySelectorAll(".mach")).toHaveLength(1);

    // The timeline spans the day and carries no window control.
    expect(document.querySelector(".tlhdr span")?.textContent).toBe("activity");
    expect(document.querySelector(".twin")).toBeNull();
    // Both of the day's sessions drew a bar. The NOW-anchored live arm would
    // have filtered every one of them out for ending before `now - 24h`.
    expect(document.querySelectorAll(".fleettl .lane")).toHaveLength(1);
    expect(document.querySelectorAll(".sbar")).toHaveLength(2);
  });

  /**
   * (#1800) The replay CHROME — topbar, crumb, meta. Legacy branches all three
   * on live-vs-replay, and the port took the live arm on every route, so a
   * `#<date>` page had three surfaces disagreeing about what day it showed:
   * a stage rendering 2026-08-07 beside a status bar describing today.
   *
   * `goldens/playback-date.txt` is the byte-level spec and the parity suite
   * enforces all four regions against a real browser; this is the fast guard
   * beneath it, and the one that names WHICH surface broke when it breaks.
   *
   * (#2120, operator decision — "the transport IS the summary") GRADUATED:
   * `#crumb` and `#meta` used to carry the replay's own summary (`◆
   * <mission>` / the folded census line) — now the sticky row's playback
   * transport carries the mission as a human label instead (`Scrubber`'s
   * own `label` prop), `#crumb` is empty on a playback route
   * (`routeChrome`'s own doc), and `#meta` doesn't render AT ALL while the
   * transport is mounted (this file's own doc on the `!transportShown`
   * gate) — the day/span/census/raw-id information those two elements used
   * to carry lives in the Machine info modal's `playback` kv row now
   * (`machineStatsContent.test.ts`/`MachineDrawer.test.tsx` cover that row
   * directly).
   */
  it("a replay's chrome describes the REPLAYED day, not today", async () => {
    window.location.hash = "#2026-08-07";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "/flow/2026-08-07") {
          return Promise.resolve(
            new Response(
              JSON.stringify([
                { _type: "schema", darkmux_version: "2.6.0" },
                { ts: "2026-08-07T02:09:42.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", mission_id: "review-1", session_id: "s1", action: "dispatch.start" },
                { ts: "2026-08-07T18:28:15.000Z", machine_uid: "m1", machine_id: "MacBook-Pro", mission_id: "review-2", session_id: "s1", action: "dispatch.complete" },
              ]),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );

    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());

    // topbar: the source chip names the day. The mode badge does NOT render
    // here (operator, 2026-09-01): the transport is mounted on this route —
    // asserted below — and the controls state the mode unambiguously, so the
    // badge would only repeat them. It still renders where the transport is
    // absent; that carve-out is what keeps #1801's regression fixed rather
    // than reintroduced ("a page that is neither live nor visibly playback").
    expect(document.querySelector(".catalog-toggle")?.textContent).toContain("2026-08-07");
    expect(document.getElementById("modebadge")).toBeNull();

    // crumb: empty now — the raw-id `◆ <mission>` this used to carry moved
    // into the Machine info modal's `playback` row only (the transport
    // carries no label, asserted below).
    expect(document.getElementById("crumb")?.textContent).toBe("");

    // meta: gone entirely — the transport is mounted (a real day loaded on
    // a daemon's `#<date>` route), so `#meta` doesn't render at all.
    expect(document.getElementById("meta")).toBeNull();

    // (#2121, operator) The transport carries NO label at any width: the
    // clock is bare and the mission's title lives only in the Machine info
    // playback row. The demo speaks for itself as it runs.
    const clock = document.querySelector('[data-testid="scrubber-clock"]');
    expect(clock?.textContent).toMatch(/^\d\d:\d\d$/);
  });

  /**
   * (#1869 code review) `EventLogColumn` is global chrome, mounted here in
   * `App` and fed `routeRecords.records` — the WHOLE day on a playback
   * route, never scoped by the scrubber. `FleetLens`'s own hero already
   * scopes itself to the playhead (`scopedData`, `FleetLens.tsx:336`); this
   * is the sibling gap that left, at range=0, the hero saying "0 local
   * tokens, 0 dispatches" beside a log still listing rows from six hours
   * later. Measured live on the daemon before this fix: scrubber read
   * "16:56 · 3/257 rec" (3 records at-or-before the playhead) while the log
   * still said "50 of 257 events" and rendered rows stamped `23:11:22`.
   *
   * Three records spanning the whole recorded day, at tMin/mid/tMax: at the
   * un-scrubbed default (playhead pinned at tMax) all three are visible;
   * scrubbed to tMin (range value 0), only the record AT tMin remains.
   */
  it("scrubbing a playback route to tMin scopes the event log to the playhead, not the whole day", async () => {
    window.location.hash = "#2026-08-07";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "/flow/2026-08-07") {
          return Promise.resolve(
            new Response(
              JSON.stringify([
                { ts: "2026-08-07T02:09:42.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
                { ts: "2026-08-07T10:00:00.000Z", category: "dispatch", action: "dispatch.reasoning", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
                { ts: "2026-08-07T18:28:15.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
              ]),
              { status: 200 },
            ),
          );
        }
        return Promise.resolve(new Response("[]", { status: 200 }));
      }),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );

    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
    // Un-scrubbed default: playhead pinned at tMax, so all three records
    // are at-or-before it.
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3));
    expect(document.getElementById("qcount")?.textContent).toBe("3 events");

    fireEvent.change(screen.getByRole("slider"), { target: { value: "0" } });

    // Only the record AT tMin (02:09:42) is at-or-before the playhead now —
    // the log must agree with the hero, not keep showing the whole day.
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(1));
    expect(document.getElementById("qcount")?.textContent).toBe("1 events");
  });

  /**
   * (#1066 QA history) This test used to assert the OPPOSITE — that the
   * App-level column stayed HIDDEN on the mission route, because
   * `MissionGraphLens` mounted a SECOND `EventLogColumn` of its own and two
   * visible logs on one page would disagree about scope (#1868). That
   * second mount is retired (operator finding, post-#2107/#2108): the phone
   * drawer's Events tab has no route-specific escape hatch the way
   * desktop's inline mission panel did, so mission being the one route
   * still excluded from the mainstay column regressed into a real bug — a
   * nonzero record count in the drawer's tab label with a genuinely BLANK
   * body underneath. `MissionGraphLens` now reports its own scoped events
   * upward (`onEvents`) instead of rendering them, so the App-level column
   * is the ONLY display surface for mission too, same as every other route.
   *
   * Kept red-provable the same way the original was: this fails if
   * `eventLogVisible`/`showsEventLog` regresses back to hiding mission, AND
   * if the mission-id scope label or the `historical` override stop
   * threading through `App.tsx`'s own mission branch.
   */
  it("shows the App-level event log on the mission route, scoped to the mission id, never claiming a rolling window", async () => {
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    window.location.hash = "#mission=m1";
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(document.querySelector(".eventlog")).toBeTruthy());
    expect(document.querySelector(".eventlog")!.className).not.toMatch(/eventlog--hidden/);
    // `#logscope` is hidden markup (`EventLogColumn.tsx`'s own doc) but
    // still carries the real `scopeLabel` text — the mission id here, not
    // the empty string `routeChrome` returns for `mission`.
    expect(document.getElementById("logscope")?.textContent).toBe("m1");
    // Mission records are a cross-day fold, never a rolling live window —
    // the header must not carry the "last Nh" suffix live routes get.
    expect(document.querySelector(".eventlog__head h3")?.textContent).not.toMatch(/last \d+h/i);
  });

  // (#2189, step drill-in) The mainstay column narrows to EXACTLY the
  // selected step's own records (`payload.step_id` equality — App.tsx's own
  // doc on `eventLogRecords`'s mission branch), with a header block above
  // it and a one-tap way back to the whole mission. `deep-link` here means
  // `#mission=<id>&step=<id>` is set BEFORE the app ever renders — the
  // exact shape a bookmark/reload reproduces.
  function mockMissionStepFixture() {
    const graph = {
      mission_id: "m1",
      mission_status: "active",
      nodes: [
        { id: "p1", label: "phase", kind: "phase", status: "running", depth: 0 },
        { id: "a", label: "task-a", kind: "task", status: "running", parentId: "p1", depth: 0, steps: [{ id: "step-a", label: "Unit A", kind: "dispatch.internal", status: "running" }] },
        { id: "b", label: "task-b", kind: "task", status: "running", parentId: "p1", depth: 0, steps: [{ id: "step-b", label: "Unit B", kind: "dispatch.internal", status: "running" }] },
        { id: "c", label: "task-c", kind: "task", status: "planned", parentId: "p1", depth: 0, steps: [{ id: "step-c", label: "Unit C", kind: "dispatch.internal", status: "planned" }] },
      ],
      edges: [
        { id: "e1", source: "p1", target: "a", kind: "contains" },
        { id: "e2", source: "p1", target: "b", kind: "contains" },
      ],
    };
    const recs = [
      { ts: "2026-08-19T00:00:01Z", action: "dispatch.start", mission_id: "m1", payload: { step_id: "step-a" } },
      { ts: "2026-08-19T00:00:02Z", action: "dispatch.complete", mission_id: "m1", payload: { step_id: "step-a", total_turns: 2 } },
      { ts: "2026-08-19T00:00:03Z", action: "dispatch.start", mission_id: "m1", payload: { step_id: "step-b" } },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/mission/m1/graph.json") return Promise.resolve(new Response(JSON.stringify(graph), { status: 200 }));
        if (path === "/flow-mission/m1") return Promise.resolve(new Response(JSON.stringify({ records: recs, count: recs.length, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        if (path.startsWith("/flow/")) return Promise.resolve(new Response("[]", { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("(#2189) deep-linking #mission=<id>&step=<id> lands drilled in: the mainstay column shows only that step's records", async () => {
    mockMissionStepFixture();
    window.location.hash = "#mission=m1&step=step-a";
    renderApp();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2));
    // The header block names the unit and offers a way back.
    await waitFor(() => expect(document.querySelector('[data-act="step-header"]')).not.toBeNull());
    expect(document.querySelector('[data-act="step-header"]')!.textContent).toMatch(/Unit A/);
    expect(document.querySelector('[data-act="step-back"]')).not.toBeNull();
  });

  it("(#2189) a selected step with zero records renders the header AND an explicit empty state, never a blank body", async () => {
    mockMissionStepFixture();
    window.location.hash = "#mission=m1&step=step-c";
    renderApp();
    await waitFor(() => expect(document.querySelector(".eventlog__empty")).not.toBeNull());
    expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(0);
  });

  it("(#2189) the back control clears the step and restores the whole mission's records", async () => {
    mockMissionStepFixture();
    window.location.hash = "#mission=m1&step=step-a";
    renderApp();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2));
    fireEvent.click(document.querySelector('[data-act="step-back"]')!);
    await waitFor(() => expect(window.location.hash).toBe("#mission=m1"));
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3));
    expect(document.querySelector('[data-act="step-header"]')).toBeNull();
  });

  // (#2223) The drill-in goes as deep as the data allows. #2189 could only
  // scope the events column because a step carries no dispatch id of its
  // own; when the step's records DO name a real dispatch, tapping it must
  // land on that dispatch's detail view instead -- the model's token
  // counts, context headroom, host peaks and signals, which is the view the
  // operator was reaching for.
  function mockMissionDispatchFixture(sessionId: string) {
    const graph = {
      mission_id: "m1",
      mission_status: "active",
      nodes: [
        { id: "p1", label: "phase", kind: "phase", status: "running", depth: 0 },
        { id: "a", label: "task-a", kind: "task", status: "running", parentId: "p1", depth: 0, steps: [{ id: "step-a", label: "Unit A", kind: "dispatch.internal", status: "running" }] },
      ],
      edges: [{ id: "e1", source: "p1", target: "a", kind: "contains" }],
    };
    const recs = [
      { ts: "2026-08-19T00:00:01Z", action: "dispatch.start", mission_id: "m1", session_id: sessionId, payload: { step_id: "step-a" } },
      { ts: "2026-08-19T00:00:02Z", action: "dispatch.complete", mission_id: "m1", session_id: sessionId, payload: { step_id: "step-a", total_turns: 2 } },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/mission/m1/graph.json") return Promise.resolve(new Response(JSON.stringify(graph), { status: 200 }));
        if (path === "/flow-mission/m1") return Promise.resolve(new Response(JSON.stringify({ records: recs, count: recs.length, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        if (path.startsWith("/flow/")) return Promise.resolve(new Response("[]", { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("(#2223) tapping a step backed by a real dispatch opens that dispatch's detail view", async () => {
    mockMissionDispatchFixture("crew-dispatch-coder-1788254029192466-0");
    window.location.hash = "#mission=m1";
    renderApp();
    await waitFor(() => expect(document.querySelector('[data-act="step-row"]')).not.toBeNull());
    // Wait for the RECORDS, not just the row: the graph renders from
    // `graph.json` while the records arrive on their own request, and a
    // click landing in that gap takes the no-dispatch fallback for a
    // reason that has nothing to do with what these tests assert. Without
    // this the scoping case below passes vacuously.
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2));
    fireEvent.click(document.querySelector('[data-act="step-row"]')!);
    await waitFor(() => expect(window.location.hash).toBe("#dispatch=crew-dispatch-coder-1788254029192466-0"));
  });

  it("(#2223) a generic-launch step drills into its emitter-default `step-<id>` dispatch session", async () => {
    // `step-step-a` is the session `session_id::step` mints for a
    // `dispatch.internal` step with no configured session — which is what
    // EVERY generic `mission launch <config>` step rides. The fixture's
    // dispatch bookends attest a real dispatch ran under it, so the tap
    // must reach the detail view. (An earlier version of this test used
    // the same fixture to pin the OPPOSITE behavior, mislabeled as "graph-
    // minted" — the adversarial review caught that this shape is byte-for-
    // byte a real generic-launch dispatch.)
    mockMissionDispatchFixture("step-step-a");
    window.location.hash = "#mission=m1";
    renderApp();
    await waitFor(() => expect(document.querySelector('[data-act="step-row"]')).not.toBeNull());
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(2));
    fireEvent.click(document.querySelector('[data-act="step-row"]')!);
    await waitFor(() => expect(window.location.hash).toBe("#dispatch=step-step-a"));
  });

  it("(#2223) a step with records but NO dispatch evidence still scopes, never routing to an empty detail view", async () => {
    // This fixture's records carry dispatch actions but no `session_id`
    // at all, so no dispatch session can be resolved — the tap keeps
    // #2189's scoping. (The richer no-evidence cases — a procedural step
    // whose records are all bookkeeping actions — are pinned unit-level in
    // graph.test.ts; this test proves the App-level fallback WIRING.)
    mockMissionStepFixture();
    window.location.hash = "#mission=m1";
    renderApp();
    await waitFor(() => expect(document.querySelector('[data-act="step-row"]')).not.toBeNull());
    // Wait for the RECORDS, not just the row — a click landing in the gap
    // between graph.json and /flow-mission takes this same fallback for a
    // reason unrelated to what this test asserts. This fixture carries 3
    // mission-wide records (2 for step-a, 1 for step-b); the unscoped
    // column shows all 3.
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3));
    fireEvent.click(document.querySelector('[data-act="step-row"]')!);
    await waitFor(() => expect(window.location.hash).toBe("#mission=m1&step=step-a"));
    expect(document.querySelector('[data-act="step-header"]')).not.toBeNull();
  });
  // (#2072/#2073) Static-build tests run LAST: `useHashRoute` caches the parsed
  // route per hash string, and a hash parsed while the static meta is present
  // resolves to the playback route; a later test on the same hash would
  // inherit it. In production the build flag never changes at runtime.
  /** (U4-1) Measured on the served demo: `#lens=fleet` showed "50 of 6092
   * events" at rest while `#lens=runs`/`machine`/`console` all showed
   * "0 EVENTS" — and clicking rewind made the count appear, so the records
   * were loaded the whole time. The mechanism: on a static build `#lens=fleet`
   * canonicalizes to the PLAYBACK route, whose `useRouteRecords` branch reads
   * the committed file; every explicitly-named lens keeps its own route kind
   * and fell through to `flowWindow.data`, which is empty by construction on
   * a daemon-less build (`useFlowWindow`'s queries are `enabled: daemonBacked`).
   * The at-rest event set is the same day's file on every static route. */
  it("(U4-1) a static build's runs lens shows the committed day's events at rest, like fleet does", async () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    window.location.hash = "#lens=runs&u4";
    const jsonl = [
      { ts: "2026-08-26T10:00:00.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.start", handle: "coder" },
      { ts: "2026-08-26T10:01:00.000Z", machine_uid: "u1", session_id: "s1", action: "dispatch.complete", payload: { total_tokens: 10 } },
    ]
      .map((r) => JSON.stringify(r))
      .join("\n");
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        Promise.resolve(
          String(url) === "./demo-flow.jsonl"
            ? new Response(jsonl, { status: 200 })
            : // The runs/lab-runs fixtures the RUNS lens itself reads — an
              // object with a `runs` array, not a bare `[]` (which crashes the
              // board into its error boundary and would leave this test
              // asserting the log from behind a broken lens).
              new Response(JSON.stringify({ runs: [] }), { status: 200 }),
        ),
      ),
    );
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    try {
      render(
        <QueryClientProvider client={queryClient}>
          <App />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBeGreaterThan(0));
    } finally {
      meta.remove();
      window.location.hash = "";
    }
  });

  it("(#2072) a static build never says 'waiting for a machine' — there is no daemon to wait for", async () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    try {
      render(
        <QueryClientProvider client={queryClient}>
          <App />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.querySelector(".app-shell__navtabs")).toBeTruthy());
      expect(document.body.textContent).not.toContain("waiting for a machine");
    } finally {
      meta.remove();
    }
  });

  it("(#2073) the playback crumb is marked as a replay crumb so narrow viewports can drop the duplicate of the meta line's lead", async () => {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    try {
      render(
        <QueryClientProvider client={queryClient}>
          <App />
        </QueryClientProvider>,
      );
      await waitFor(() => expect(document.getElementById("crumb")).toBeTruthy());
      expect(document.getElementById("crumb")!.className).toMatch(/\bis-replay\b/);
    } finally {
      meta.remove();
    }
  });

  function renderApp() {
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
  }

  // (daemon replay day) A dispatch or mission page on a DAEMON derives its
  // day from the replayed records, loads it, and gets the transport (dispatch)
  // and the dated chip with the playback badge (both). Daemon routes are not
  // static, so these run before the static cases below.
  // `missionClosed` (default true): the mission slice carries a terminal
  // `mission close` record, so the mission page is a RECORDING that names
  // its day. Pass false for a mission still running — header owns liveness,
  // and a running mission is live whatever day its records carry.
  function mockDaemonReplay(missionClosed = true) {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        const recs = [
          { ts: "2026-08-07T09:00:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", mission_id: "m-one" },
          { ts: "2026-08-07T09:30:00.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1", mission_id: "m-one" },
          ...(missionClosed
            ? [{ ts: "2026-08-07T09:31:00.000Z", category: "mission", action: "mission close", machine_uid: "m1", machine_id: "MacBook-Pro", mission_id: "m-one" }]
            : []),
        ];
        if (path === "/flow-session/s1") return Promise.resolve(new Response(JSON.stringify({ records: recs, count: 2, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        if (path === "/flow-mission/m-one") return Promise.resolve(new Response(JSON.stringify({ records: recs, count: 2, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        if (path === "/flow/2026-08-07") return Promise.resolve(new Response(JSON.stringify(recs), { status: 200 }));
        if (path.startsWith("/flow/")) return Promise.resolve(new Response("[]", { status: 200 }));
        if (path === "/fleet/sessions/live") return Promise.resolve(new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        if (path === "/fleet/machines/live") return Promise.resolve(new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        // Anything else (the mission graph, runs, specs) is absent: the lenses
        // render their honest not-found states rather than choking on "[]".
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
  }

  it("a daemon dispatch page names its day in the chip and gets the transport — WITHOUT a redundant playback badge", async () => {
    // (operator, 2026-09-01) The badge and the transport controls both state
    // the mode, and the controls do it unambiguously. Where they are on
    // screen the badge is noise, so it is suppressed there — and ONLY there.
    // The companion test below covers the case that keeps it: a route with no
    // transport, which would otherwise be neither live nor visibly playback
    // (the #1801 regression the badge exists to prevent).
    mockDaemonReplay();
    window.location.hash = "#dispatch=s1";
    renderApp();
    await waitFor(() => expect(document.querySelector(".catalog-toggle")?.textContent).toBe("2026-08-07"));
    await screen.findByRole("group", { name: "playback transport" });
    expect(document.querySelector("#modebadge")).toBeNull();
  });

  it("a run that crossed the loaded day's end renders whole at rest AND after a play-through; only a moved playhead cuts it", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const session = [
      { ts: "2026-08-07T23:30:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-mid" },
      { ts: "2026-08-07T23:50:00.000Z", category: "dispatch", action: "dispatch.reasoning", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-mid" },
      { ts: "2026-08-08T00:20:00.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-mid" },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/flow-session/s-mid") return Promise.resolve(new Response(JSON.stringify({ records: session, count: 3, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        // The daemon's day file holds only Aug 7: the run's last record is on Aug 8.
        if (path === "/flow/2026-08-07") return Promise.resolve(new Response(JSON.stringify(session.slice(0, 2)), { status: 200 }));
        if (path === "/fleet/sessions/live") return Promise.resolve(new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        if (path === "/fleet/machines/live") return Promise.resolve(new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    window.location.hash = "#dispatch=s-mid";
    try {
      renderApp();
      await screen.findByRole("group", { name: "playback transport" });
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3)); // whole, at rest
      fireEvent.click(screen.getByRole("button", { name: /^play$/i }));
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(1)); // rewound: cut
      await vi.advanceTimersByTimeAsync(15000);
      await waitFor(() => expect(screen.getByRole("button", { name: /^play$/i })).toBeInTheDocument());
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3)); // played through: whole again, Aug 8 record included
    } finally {
      vi.useRealTimers();
    }
  });

  it("a dispatch that is still running is a live view: no date, the LIVE badge, no transport", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        const recs = [{ ts: "2026-08-07T09:00:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-live" }];
        if (path === "/flow-session/s-live") return Promise.resolve(new Response(JSON.stringify({ records: recs, count: 1, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        if (path === "/fleet/sessions/live") return Promise.resolve(new Response(JSON.stringify({ sessions: [{ session_id: "s-live", machine_uid: "m1", beat_ts_ms: Date.now() }], meta: { sources: { fleet: { state: "ok" } }, complete: true } }), { status: 200 }));
        if (path === "/fleet/machines/live") return Promise.resolve(new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "ok" } }, complete: true } }), { status: 200 }));
        if (path.startsWith("/flow/")) return Promise.resolve(new Response(JSON.stringify(recs), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    window.location.hash = "#dispatch=s-live";
    renderApp();
    await waitFor(() => expect(document.querySelectorAll(".eventlog__rec").length).toBeGreaterThan(0));
    await waitFor(() => expect(document.querySelector(".catalog-toggle")?.textContent).toBe("TODAY"));
    // (header owns liveness, 2026-09-03) Running ⇒ live ⇒ the header badge,
    // the same one every other live page shows. Never the PLAYBACK badge.
    await waitFor(() => expect(document.querySelector("#modebadge")?.textContent).toMatch(/live|reconnecting/i));
    expect(screen.queryByRole("group", { name: "playback transport" })).not.toBeInTheDocument();
  });

  it("a daemon mission page that is still RUNNING is live: the badge shows and the chip carries no date", async () => {
    // (header owns liveness, operator 2026-09-04: "live would be shown
    // instead of date") The mission below has records from 2026-08-07 but no
    // terminal record, so it is a live view — the ONE liveness badge shows
    // and the chip does not name a day. Before this, any mission with
    // records read as a recording and the header hid the badge.
    mockDaemonReplay(false);
    window.location.hash = "#mission=m-one";
    renderApp();
    // Settle first: every page is live before its data arrives, so a badge
    // read before the mission slice lands proves nothing. Wait for the
    // mission query to have been answered, then a tick for the memo.
    await waitFor(() => expect((fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.some((c) => String(c[0]) === "/flow-mission/m-one")).toBe(true));
    await new Promise((r) => setTimeout(r, 50));
    await waitFor(() => expect(document.querySelector(".catalog-toggle")?.textContent).toBe("TODAY"));
    expect(document.querySelector("#modebadge")?.textContent?.toLowerCase()).toContain("live");
    expect(screen.queryByRole("group", { name: "playback transport" })).not.toBeInTheDocument();
  });

  it("a daemon mission page names its day in the chip once the mission has CLOSED, with NO playback badge and no transport", async () => {
    // (operator, 2026-09-01) A mission is an overview, not a scrubbable
    // recording — playback lives in the drill-in detail view. So the badge
    // was not merely redundant here, it was FALSE: a `▶` glyph promising a
    // control this route will never have. The chip carries the mode instead
    // (a date means recorded; `TODAY` is the live view's word).
    mockDaemonReplay();
    window.location.hash = "#mission=m-one";
    renderApp();
    await waitFor(() => expect(document.querySelector(".catalog-toggle")?.textContent).toBe("2026-08-07"));
    expect(document.querySelector("#modebadge")).toBeNull();
    expect(screen.queryByRole("group", { name: "playback transport" })).not.toBeInTheDocument();
  });

  // (#2071) The transport lives in the shell's sticky block on every route
  // of a loaded day. Static cases stay at the end of the file (the
  // `useHashRoute` memo, see above).
  const DAY_FOR_TRANSPORT = [
    { ts: "2026-08-07T00:00:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
    { ts: "2026-08-07T00:30:00.000Z", category: "dispatch", action: "dispatch.reasoning", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
    { ts: "2026-08-07T00:45:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s2" },
    { ts: "2026-08-07T01:00:00.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s1" },
  ];
  function mockStaticDay() {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "./demo-flow.jsonl") {
          return Promise.resolve(new Response(DAY_FOR_TRANSPORT.map((r) => JSON.stringify(r)).join("\n") + "\n", { status: 200 }));
        }
        if (String(url) === "/runs") return Promise.resolve(new Response(JSON.stringify({ runs: [], generated_at_ms: 1 }), { status: 200 }));
        if (String(url) === "/lab/runs") return Promise.resolve(new Response(JSON.stringify({ configured: false, dir: null, exists: false, runs: [] }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    return meta;
  }

  it("(#2108, operator finding — desktop tab-row fold, rounds 1 + 2) #crumb AND #meta both live inside .app-shell__sticky now, folded into the tab row; .app-shell__crumbbar no longer exists", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderApp();
    await waitFor(() => expect(document.querySelector(".app-shell__navtabs")).toBeTruthy());
    expect(document.querySelector(".app-shell__crumbbar")).toBeNull();
    const crumb = document.querySelector("#crumb")!;
    const meta = document.querySelector("#meta")!;
    expect(crumb.closest(".app-shell__sticky")).toBeTruthy();
    expect(meta.closest(".app-shell__sticky")).toBeTruthy();
  });

  it("(#2108, operator finding — round 2) the summary (#meta) and the tab list (.app-shell__navtabs) share the SAME container, not just the same page — the structural proof that they render on one row", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderApp();
    await waitFor(() => expect(document.querySelector(".app-shell__navtabs")).toBeTruthy());
    const navtabs = document.querySelector(".app-shell__navtabs")!;
    const meta = document.querySelector("#meta")!;
    const sticky = document.querySelector(".app-shell__sticky")!;
    // Same immediate row container — this is the real proof available in
    // jsdom, which performs no layout (so a literal offsetTop comparison
    // below would pass trivially for ANY two elements at 0,0 regardless of
    // whether they actually share a row; it's asserted too, for the
    // record, but the parentElement check is what's load-bearing).
    expect(navtabs.parentElement).toBe(sticky);
    expect(meta.parentElement).toBe(sticky);
    expect((navtabs as HTMLElement).offsetTop).toBe((meta as HTMLElement).offsetTop);
  });

  it("(#2071) a live daemon route renders no transport — nothing to scrub", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    renderApp();
    await waitFor(() => expect(document.querySelector(".app-shell__navtabs")).toBeTruthy());
    expect(screen.queryByRole("group", { name: "playback transport" })).not.toBeInTheDocument();
  });

  it("(#2071) a static build renders the transport beside the tabs in the sticky block, on a NON-playback route", async () => {
    const meta = mockStaticDay();
    window.location.hash = "#lens=runs";
    try {
      renderApp();
      const group = await screen.findByRole("group", { name: "playback transport" });
      const sticky = group.closest(".app-shell__sticky");
      expect(sticky).toBeTruthy();
      expect(sticky!.querySelector(".app-shell__navtabs")).toBeTruthy();
      expect(screen.getByRole("button", { name: /^play$/i })).toBeInTheDocument();
    } finally {
      meta.remove();
    }
  });

  it("(#2071) on a static dispatch route, scrubbing to the start cuts that session's events to the playhead", async () => {
    const meta = mockStaticDay();
    window.location.hash = "#dispatch=s1";
    try {
      renderApp();
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3)); // s1's three, never s2's
      fireEvent.change(screen.getByRole("slider"), { target: { value: "0" } });
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(1));
    } finally {
      meta.remove();
    }
  });

  it("(#2071) on a static dispatch route the run detail itself replays at the playhead: scrubbed to the start it is RUNNING", async () => {
    const meta = mockStaticDay();
    window.location.hash = "#dispatch=s1";
    try {
      renderApp();
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(3));
      const pillAtEnd = document.querySelector(".session-run__header .pill")!.textContent;
      expect(pillAtEnd).not.toMatch(/running/i);
      fireEvent.change(screen.getByRole("slider"), { target: { value: "0" } });
      await waitFor(() => expect(document.querySelector(".session-run__header .pill")!.textContent).toMatch(/running/i));
    } finally {
      meta.remove();
    }
  });

  it("(#2071) the mission route shows no transport — nothing there answers it", async () => {
    const meta = mockStaticDay();
    window.location.hash = "#lens=runs";
    try {
      renderApp();
      // First prove the day is loaded: the transport is up on the runs route.
      await screen.findByRole("group", { name: "playback transport" });
      // Then the route gate, on the same loaded day: switching to a mission
      // takes the transport down (a vacuous "never appeared" would pass
      // before the day loaded, which is why the order matters).
      await act(async () => {
        window.location.hash = "#mission=m1";
        window.dispatchEvent(new HashChangeEvent("hashchange"));
      });
      await waitFor(() => expect(screen.queryByRole("group", { name: "playback transport" })).not.toBeInTheDocument());
      expect(document.querySelector(".app-shell__sticky .app-shell__navtabs")).toBeTruthy();
    } finally {
      meta.remove();
    }
  });

  // (#2346) Superseded the old day-wide scrub-before-start test below: with
  // the transport bounded to the OPEN dispatch's own span, s2's range is
  // its own single record (00:45-00:45) — there is no "before the day's
  // start but before this run's start" position left to scrub to. The
  // "not started yet" empty state is still exercised (for a MISSION focus)
  // by `SessionReplay.test.tsx`'s own #2346 suite.
  it("(#2346) a dispatch route's scrubber is bounded to that run's OWN span, not the day's — scrubbing it can never read 'not started yet'", async () => {
    const meta = mockStaticDay();
    window.location.hash = "#dispatch=s2"; // s2's only record is at 00:45; the day runs 00:00-01:00
    try {
      renderApp();
      await waitFor(() => expect(document.querySelector(".session-run__header .pill")).toBeTruthy());
      // A zero-span range (tMin === tMax, s2's one record) pins the thumb at
      // the end — the same rule a fresh, unscrubbed focus gets (#1640).
      expect(screen.getByRole("slider")).toHaveValue("100");
      fireEvent.change(screen.getByRole("slider"), { target: { value: "0" } });
      expect(screen.queryByRole("status", { name: /not started yet/i })).not.toBeInTheDocument();
      expect(document.body.textContent).not.toMatch(/stopped rendering/i);
    } finally {
      meta.remove();
    }
  });

  // (#2346) The bug report's own shape: a run that ended hours before the
  // day's last record. A day-wide scrubber would pin the playhead at the
  // day's own last record (20:43 in the report) while the run detail's own
  // WALL CLOCK read 10:06:37 — two clocks, two subjects. This fixture
  // reproduces it: `s-narrow` runs 08:11:48-10:06:37, `s-other` (a
  // different session) is the day's true last record at 20:43.
  const DAY_WITH_GAP = [
    { ts: "2026-08-07T08:11:48.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-narrow" },
    { ts: "2026-08-07T10:06:37.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-narrow" },
    { ts: "2026-08-07T20:43:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-other" },
  ];
  function mockDayWithGap() {
    const meta = document.createElement("meta");
    meta.name = "darkmux-flow-src";
    meta.content = "./demo-flow.jsonl";
    document.head.appendChild(meta);
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "./demo-flow.jsonl") {
          return Promise.resolve(new Response(DAY_WITH_GAP.map((r) => JSON.stringify(r)).join("\n") + "\n", { status: 200 }));
        }
        if (String(url) === "/runs") return Promise.resolve(new Response(JSON.stringify({ runs: [], generated_at_ms: 1 }), { status: 200 }));
        if (String(url) === "/lab/runs") return Promise.resolve(new Response(JSON.stringify({ configured: false, dir: null, exists: false, runs: [] }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    return meta;
  }

  it("(#2346) the scrubber's range AND clock scope to the open dispatch, not the whole day — the two clocks agree", async () => {
    const meta = mockDayWithGap();
    window.location.hash = "#dispatch=s-narrow";
    try {
      renderApp();
      await waitFor(() => expect(document.querySelector(".session-run__header .pill")).toBeTruthy());
      const slider = screen.getByRole("slider");
      const runEndMs = Date.parse("2026-08-07T10:06:37.000Z");
      const dayEndMs = Date.parse("2026-08-07T20:43:00.000Z");
      const runStartMs = Date.parse("2026-08-07T08:11:48.000Z");
      // The range is s-narrow's own span — NOT the day's (which runs to
      // 20:43) — so, at rest (unscrubbed), the thumb sits at 100 either
      // way; what proves the bound is the aria-valuetext naming the RUN's
      // own end, not the day's.
      expect(slider).toHaveAttribute("aria-valuetext", expect.stringContaining(clkhm(runEndMs)));
      expect(slider).not.toHaveAttribute("aria-valuetext", expect.stringContaining(clkhm(dayEndMs)));
      // The clock readout carries the run's own elapsed time beside the
      // time of day — 08:11:48 to 10:06:37 — the SAME quantity the run
      // detail's own WALL CLOCK tile shows (`fmtElapsed`, one producer).
      expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${fmtElapsed(runEndMs - runStartMs)} · ${clkhm(runEndMs)}`);
    } finally {
      meta.remove();
    }
  });

  it("(#2346) a playback (day-focus) route's clock names only the time of day — no elapsed segment", async () => {
    const meta = mockDayWithGap();
    window.location.hash = "#lens=runs";
    try {
      renderApp();
      await screen.findByRole("group", { name: "playback transport" });
      const dayEndMs = Date.parse("2026-08-07T20:43:00.000Z");
      expect(screen.getByTestId("scrubber-clock").textContent).toBe(clkhm(dayEndMs));
    } finally {
      meta.remove();
    }
  });

  // (#2346, redesigned after a live-render finding) A DAEMON's own
  // `/flow/<date>` is a CAPPED, TIME-WINDOWED slice — the operator's own
  // run (evidence: `dispatch start` 2026-09-04T00:11:48Z, `dispatch
  // complete` 02:06:37Z, `wall_ms` 6888067) started hours before the loaded
  // window's floor, so NONE of its records were ever in `dayRecords`. The
  // first cut of this fix derived the focus range by filtering
  // `dayRecords`; every fixture above (`DAY_FOR_TRANSPORT`, `DAY_WITH_GAP`)
  // happens to hold the open session's own records too, so that version
  // passed every one of those tests and still reproduced the bug live.
  // This fixture deliberately keeps the day window and the session's own
  // records DISJOINT — the day names only an UNRELATED session, and the
  // real run's records arrive solely through `/flow-session/<id>`
  // (`routeRecords.records`), exactly mirroring the live daemon's shape.
  it("(#2346) a dispatch whose records are NOT in the loaded day window still gets its OWN span — the live-render finding", async () => {
    const runRecords = [
      {
        ts: "2026-09-04T00:11:48.000Z",
        category: "dispatch",
        action: "dispatch start",
        machine_uid: "m1",
        machine_id: "MacBook-Pro",
        session_id: "s-narrow",
      },
      {
        ts: "2026-09-04T02:06:37.000Z",
        category: "dispatch",
        action: "dispatch complete",
        machine_uid: "m1",
        machine_id: "MacBook-Pro",
        session_id: "s-narrow",
        payload: { wall_ms: 6_888_067 },
      },
    ];
    // The loaded DAY WINDOW carries only a wholly unrelated session, hours
    // later — s-narrow's own records are nowhere in it (the capped,
    // time-windowed `/flow/<date>` shape).
    const dayWindow = [
      { ts: "2026-09-04T20:00:00.000Z", category: "dispatch", action: "dispatch.start", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-other" },
      { ts: "2026-09-04T21:00:00.000Z", category: "dispatch", action: "dispatch.complete", machine_uid: "m1", machine_id: "MacBook-Pro", session_id: "s-other" },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const path = String(url);
        if (path === "/flow-session/s-narrow") {
          return Promise.resolve(new Response(JSON.stringify({ records: runRecords, count: runRecords.length, truncated: false, generated_at_ms: 1 }), { status: 200 }));
        }
        if (path === "/flow/2026-09-04") return Promise.resolve(new Response(JSON.stringify(dayWindow), { status: 200 }));
        if (path === "/fleet/sessions/live") return Promise.resolve(new Response(JSON.stringify({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        if (path === "/fleet/machines/live") return Promise.resolve(new Response(JSON.stringify({ machines: [], meta: { sources: { fleet: { state: "off" } }, complete: true } }), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    window.location.hash = "#dispatch=s-narrow";
    renderApp();
    await waitFor(() => expect(document.querySelector(".session-run__header .pill")).toBeTruthy());
    const slider = screen.getByRole("slider");
    const runStartMs = Date.parse("2026-09-04T00:11:48.000Z");
    const runEndMs = Date.parse("2026-09-04T02:06:37.000Z");
    const dayEndMs = Date.parse("2026-09-04T21:00:00.000Z");
    // The range names the RUN's own bookends, not the disjoint day window's.
    expect(slider).toHaveAttribute("aria-valuetext", expect.stringContaining(clkhm(runStartMs)));
    expect(slider).toHaveAttribute("aria-valuetext", expect.stringContaining(clkhm(runEndMs)));
    expect(slider).not.toHaveAttribute("aria-valuetext", expect.stringContaining(clkhm(dayEndMs)));
    // At rest, the elapsed readout is the run's own recorded `wall_ms`
    // (6888067ms = "1:54:48") — ONE SECOND shy of the raw bookend
    // subtraction (runEndMs - runStartMs = 6889000ms = "1:54:49"), because
    // the flow record `ts`s are second-precision while `wall_ms` is the
    // runtime's own sub-second measured duration. Asserting the two DIFFER
    // (not merely asserting the final string) is what proves `wall_ms` is
    // actually preferred here, rather than the assertion happening to pass
    // by coincidence — preferring it is what makes this readout agree with
    // the run detail's own WALL CLOCK tile exactly, rather than merely
    // closely.
    expect(fmtElapsed(6_888_067)).not.toBe(fmtElapsed(runEndMs - runStartMs));
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${fmtElapsed(6_888_067)} · ${clkhm(runEndMs)}`);
  });

  // (#2347 review, MUST FIX (b)) Leaving an AT-REST dispatch view for the
  // day view: sA's own end falls WELL INSIDE the day's own range and is not
  // equal to the day's own tMax, so the old preserve-or-snap effect's
  // in-range branch fired and wrongly left the day view SCRUBBED at the
  // run's own end — cutting the fleet hero, the runs board, and the event
  // log to that instant instead of showing the whole day. In the static
  // demo this is "tap a run row, tap Runs".
  it("(#2347 MUST FIX) leaving an at-rest dispatch view for the day view is NOT left scrubbed at the run's own end", async () => {
    const meta = mockDayWithGap();
    window.location.hash = "#dispatch=s-narrow";
    try {
      renderApp();
      await waitFor(() => expect(document.querySelector(".session-run__header .pill")).toBeTruthy());
      // Sanity: the dispatch view itself is at rest before leaving it —
      // confirms this test exercises "leaving an AT-REST view", not a
      // scrubbed one (a scrubbed one already worked before this fix).
      expect(screen.getByRole("slider")).toHaveValue("100");

      await act(async () => {
        window.location.hash = "#lens=runs";
        window.dispatchEvent(new HashChangeEvent("hashchange"));
      });
      await screen.findByRole("group", { name: "playback transport" });

      const dayEndMs = Date.parse("2026-08-07T20:43:00.000Z");
      // Pinned at the DAY's own end, not left scrubbed at the run's own
      // (much earlier) end — both the clock text and the slider's own
      // position (100, the same "pinned at end" the day view starts at)
      // prove it, not just one or the other.
      await waitFor(() => expect(screen.getByTestId("scrubber-clock").textContent).toBe(clkhm(dayEndMs)));
      expect(screen.getByRole("slider")).toHaveValue("100");
    } finally {
      meta.remove();
    }
  });

  it("(#2071) play advances the playhead from the shell's transport and stops at the end", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const meta = mockStaticDay();
    try {
      renderApp();
      await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
      fireEvent.click(screen.getByRole("button", { name: /^play$/i }));
      await waitFor(() => expect(screen.getByRole("slider")).toHaveValue("0"));
      expect(screen.getByRole("button", { name: /^pause$/i })).toBeInTheDocument();
      await vi.advanceTimersByTimeAsync(400); // the fixture day is one hour; at 1h/s it is over in one real second
      const mid = Number(screen.getByRole("slider").getAttribute("value"));
      expect(mid).toBeGreaterThan(0);
      expect(mid).toBeLessThan(100);
      await vi.advanceTimersByTimeAsync(15000);
      await waitFor(() => expect(screen.getByRole("slider")).toHaveValue("100"));
      expect(screen.getByRole("button", { name: /^play$/i })).toBeInTheDocument();
      // At the end nothing is cut: the whole day's log is back (a run that
      // crossed the loaded day's end must render whole after a play-through).
      await waitFor(() => expect(document.querySelectorAll(".eventlog__rec")).toHaveLength(DAY_FOR_TRANSPORT.length));
    } finally {
      meta.remove();
      vi.useRealTimers();
    }
  });
});
