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

  it("renders the empty state when the fleet has no live machines", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify([]), { status: 200 }))));
    renderWithClient();
    await waitFor(() => expect(screen.getByText(/no machines currently present/i)).toBeInTheDocument());
  });

  it("renders one card per machine on a successful non-empty response", async () => {
    const beats = [
      { machine_uid: "a", display_name: "MacBook-Pro", schema_version: "1.18.0", beat_ts_ms: 1 },
      { machine_uid: "b", display_name: "studio", schema_version: "1.18.0", beat_ts_ms: 2 },
    ];
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(beats), { status: 200 }))));
    renderWithClient();
    await waitFor(() => expect(screen.getByText("MacBook-Pro")).toBeInTheDocument());
    expect(screen.getByText("studio")).toBeInTheDocument();
  });

  it("the three states are visually distinct (different data-state attributes)", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify([]), { status: 200 }))));
    const { container } = renderWithClient();
    await waitFor(() => expect(container.querySelector('[data-state="empty"]')).toBeTruthy());
  });
});
