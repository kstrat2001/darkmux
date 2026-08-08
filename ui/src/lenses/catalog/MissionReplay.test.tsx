import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MissionReplay } from "./MissionReplay";

function renderReplay(missionId: string) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <MissionReplay missionId={missionId} />
    </QueryClientProvider>,
  );
}

// jsdom throws "Not implemented: navigation" on a real location.href
// assignment — stub the setter so the redirect branch is observable without
// jsdom's noisy console error.
let hrefSets: string[] = [];
beforeEach(() => {
  hrefSets = [];
  Object.defineProperty(window, "location", {
    configurable: true,
    value: {
      ...window.location,
      set href(v: string) {
        hrefSets.push(v);
      },
      get href() {
        return "http://localhost/";
      },
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("MissionReplay", () => {
  it("renders pending while the fetch is in flight", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderReplay("m1");
    expect(screen.getByRole("status", { name: /loading mission m1/i })).toBeInTheDocument();
  });

  it("renders a visible error on a non-2xx response", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("boom", { status: 500, statusText: "err" }))));
    renderReplay("m1");
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/500/);
  });

  it("renders an honest empty state when the mission has no records — never navigates", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }), { status: 200 }))),
    );
    renderReplay("m1");
    await waitFor(() => expect(screen.getByText(/no records found for mission m1/i)).toBeInTheDocument());
    expect(hrefSets).toHaveLength(0);
  });

  it("navigates to /mission/<id>/graph when records are found — matching legacy's real redirect exactly", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          new Response(JSON.stringify({ records: [{ ts: "x" }], count: 1, truncated: false, generated_at_ms: 0 }), {
            status: 200,
          }),
        ),
      ),
    );
    renderReplay("my mission/with slash");
    await waitFor(() => expect(hrefSets).toContain("/mission/my%20mission%2Fwith%20slash/graph"));
  });

  it("URL-encodes the mission id in the fetch path", async () => {
    const fetchMock = vi.fn((_url: string) =>
      Promise.resolve(new Response(JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }), { status: 200 })),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderReplay("a/b c");
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(fetchMock.mock.calls[0][0]).toBe("/flow-mission/a%2Fb%20c");
  });
});
