import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
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

  it("renders a named placeholder for a lens this packet doesn't implement (session drill-in)", () => {
    // `#lens=console` was this test's original target before Packet 6 ported
    // the console lens for real — kept AS a placeholder-mechanism check, but
    // re-pointed at a route that's still genuinely unported (`#session=<id>`,
    // see `renderRoute`'s `LensPlaceholder` branch) rather than asserting
    // stale behavior against a route that no longer matches it.
    window.location.hash = "#session=abc-123";
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <App />
      </QueryClientProvider>,
    );
    expect(screen.getByText(/lens not ported yet: session drill-in abc-123/i)).toBeInTheDocument();
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
});
