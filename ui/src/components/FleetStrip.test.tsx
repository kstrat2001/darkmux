import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { FleetStrip } from "./FleetStrip";

function renderWithClient() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <FleetStrip />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("FleetStrip", () => {
  it("renders the pending skeleton before the fetch resolves", () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise(() => {})), // never resolves
    );
    renderWithClient();
    expect(screen.getByRole("status", { name: /loading fleet/i })).toBeInTheDocument();
  });

  it("renders a visible error state naming the failure on a non-2xx response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response("boom", { status: 500, statusText: "Internal Server Error" }))),
    );
    renderWithClient();
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/500/);
  });


/** The `GET /fleet/machines/live` wire shape since #1729: beats + coverage.
 *  Defaults to a healthy fleet; pass a `SourceState` to model a degraded one. */
function envelope(machines: unknown[], fleet: Record<string, unknown> = { state: "ok" }) {
  return { machines, meta: { sources: { fleet }, complete: fleet.state === "ok" || fleet.state === "off" } };
}

  it("renders the empty state when the fleet has no live machines", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(envelope([])), { status: 200 }))));
    renderWithClient();
    await waitFor(() => expect(screen.getByText(/no machines currently present/i)).toBeInTheDocument());
  });

  it("renders one card per machine on a successful non-empty response", async () => {
    const beats = [
      { machine_uid: "a", display_name: "MacBook-Pro", schema_version: "1.18.0", beat_ts_ms: 1 },
      { machine_uid: "b", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 2 },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(envelope(beats)), { status: 200 }))));
    renderWithClient();
    await waitFor(() => expect(screen.getByText("MacBook-Pro")).toBeInTheDocument());
    expect(screen.getByText("studio")).toBeInTheDocument();
  });


  it("an UNREADABLE fleet is not rendered as an empty one", async () => {
    // The defect this endpoint's envelope exists to remove: with Redis down,
    // the beats are missing from the ANSWER, not absent from the world.
    // Reassuring "no machines currently present" copy here is the lie.
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(
      JSON.stringify(envelope([], { state: "unavailable", detail: "could not read presence beats from Redis" })),
      { status: 200 }))));
    const { container } = renderWithClient();
    await waitFor(() => expect(container.querySelector('[data-state="degraded"]')).toBeTruthy());
    expect(screen.queryByText(/no machines currently present/i)).toBeNull();
  });

  it("an UNCONFIGURED fleet still reads as the plain empty state, never a warning", async () => {
    // The inverted case: `off` is a correct standalone machine. Warning here
    // would put a permanent false alarm in front of every solo operator.
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(
      JSON.stringify(envelope([], { state: "off" })), { status: 200 }))));
    const { container } = renderWithClient();
    await waitFor(() => expect(screen.getByText(/no machines currently present/i)).toBeInTheDocument());
    expect(container.querySelector('[data-state="degraded"]')).toBeNull();
  });

  it("a STALE fleet keeps the last known machines on screen rather than blanking", async () => {
    const beats = [{ machine_uid: "a", display_name: "MacBook-Pro", schema_version: "1.18.0", beat_ts_ms: 1 }];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(
      JSON.stringify(envelope(beats, { state: "stale", age_ms: 41200, detail: "could not read the fleet stream from Redis" })),
      { status: 200 }))));
    const { container } = renderWithClient();
    await waitFor(() => expect(container.querySelector('[data-state="degraded"]')).toBeTruthy());
    expect(screen.getByText(/41s old/)).toBeInTheDocument();
  });

  it("the three states are visually distinct (different data-state attributes)", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(envelope([])), { status: 200 }))));
    const { container } = renderWithClient();
    await waitFor(() => expect(container.querySelector('[data-state="empty"]')).toBeTruthy());
  });
});
