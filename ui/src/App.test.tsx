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
    await waitFor(() => expect(screen.getByText(/no machines currently present/i)).toBeInTheDocument());
  });

  it("renders a named placeholder for a lens this packet doesn't implement", () => {
    window.location.hash = "#lens=console";
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.getByText(/lens not ported yet: console/i)).toBeInTheDocument();
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
});
