import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";

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
  it("shows no live badge on a replay route, where no tail is running", async () => {
    window.location.hash = "#session=abc-123";
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("[]", { status: 200 }))));
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { container } = render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(container.querySelector(".crumbbar, #stage")).toBeTruthy());
    expect(container.querySelector("#modebadge")).toBeNull();
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

  it("mounts the event-log column but HIDDEN (eventlog--hidden) on the machine lens", async () => {
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
    expect(document.querySelector(".eventlog")?.className).toMatch(/eventlog--hidden/);
  });

  it("hides the event-log column (eventlog--hidden) on the console lens (QA correction — the packet brief wrongly claimed console keeps it)", async () => {
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
    expect(document.querySelector(".eventlog")?.className).toMatch(/eventlog--hidden/);
  });

  it("hides the event-log column (eventlog--hidden) on the runs lens", async () => {
    window.location.hash = "#lens=runs";
    vi.stubGlobal("fetch", mockFleetLikeFetch());
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(window.location.hash).toBe("#lens=runs"));
    expect(document.querySelector(".eventlog")?.className).toMatch(/eventlog--hidden/);
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
    expect(document.querySelector(".tlhdr span")?.textContent).toMatch(/^activity · /);
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

    // topbar: the source chip names the day, and the mode badge says what
    // mode this is. Both were absent/wrong — the chip showed a bare date and
    // no mode badge rendered at all on a replay.
    expect(document.querySelector(".catalog-toggle")?.textContent).toContain("FLOW · 2026-08-07");
    expect(document.getElementById("modebadge")?.textContent).toBe("▣ playback");

    // crumb: `◆ <primaryMission()>`. Non-empty precisely BECAUSE a replay is
    // not presence-scoped — the live arm filters to missions with a running
    // session and finds none, which is why goldens/fleet.txt's crumb is empty.
    expect(document.getElementById("crumb")?.textContent).toBe("◆ review-1");

    // meta: the replay census, from the day's own records. Two lines, and the
    // schema header counts as neither a record nor a machine.
    const meta = document.getElementById("meta")?.textContent ?? "";
    expect(meta).toContain("◆ review-1, review-2");
    expect(meta).toContain("flow · 2026-08-07");
    expect(meta).toContain("2 records · 1 machines");
    // The live arm's headline must NOT appear — it is what used to render here.
    expect(meta).not.toContain("last dispatch");
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
});
