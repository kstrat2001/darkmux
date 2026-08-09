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
    await waitFor(() => expect(screen.getByText(/by your fleet/i)).toBeInTheDocument());
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
        if (typeof url === "string" && url.startsWith("/panel/")) {
          return Promise.resolve(
            new Response(
              JSON.stringify({
                panel: "mission-status",
                argv: ["mission", "status"],
                captured_ts_ms: Date.now(),
                gather_ms: 1,
                exit_code: 0,
                ansi_text: "no missions",
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
    await waitFor(() => expect(screen.getByText("no missions")).toBeInTheDocument());
  });

  it("renders a named placeholder once SessionReplay's real fetch resolves populated (Packet 4 completed the fetch wiring, not the render)", async () => {
    // `#lens=console` was this test's original target before Packet 6 ported
    // the console lens for real, then briefly `#session=<id>` (Packet 4
    // still rendered a bare `LensPlaceholder` there at the time) before
    // Packet 4 landed `SessionReplay` — a REAL fetch to `/flow-session/<id>`
    // that only falls through to `LensPlaceholder` once the fetch resolves
    // with a non-empty count (see that component's own doc). Mock the
    // session endpoint with a populated response so the assertion still
    // exercises the placeholder-mechanism end state, not SessionReplay's own
    // loading/error branches (which is Packet 4's concern to test, not this
    // file's App-routing regression guard).
    window.location.hash = "#session=abc-123";
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (typeof url === "string" && url.startsWith("/flow-session/")) {
          return Promise.resolve(
            new Response(JSON.stringify({ records: [{}], count: 1, truncated: false, generated_at_ms: Date.now() }), { status: 200 }),
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
    await waitFor(() => expect(screen.getByText(/lens not ported yet: session drill-in abc-123/i)).toBeInTheDocument());
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
    await waitFor(() => expect(screen.getByText(/fleet › machine/)).toBeInTheDocument());
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
    expect(screen.queryByText(/fleet › machine/)).not.toBeInTheDocument();

    fireEvent.click(document.getElementById("lens-machine")!);

    expect(window.location.hash).toBe("#lens=machine");
    await waitFor(() => expect(screen.getByText(/fleet › machine/)).toBeInTheDocument());
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
});
