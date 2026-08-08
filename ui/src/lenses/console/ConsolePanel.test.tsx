import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ConsolePanel } from "./ConsolePanel";
import type { PanelId } from "../../lib/route";

function renderPanel(initialPanelId: PanelId | "" = "") {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <ConsolePanel initialPanelId={initialPanelId} />
    </QueryClientProvider>,
  );
}

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

const MISSION_STATUS_BODY = {
  panel: "mission-status",
  argv: ["mission", "status"],
  captured_ts_ms: Date.UTC(2026, 0, 1, 12, 0, 0),
  gather_ms: 8,
  exit_code: 0,
  ansi_text: "mission status — 0 missions",
  stderr_tail: "",
  cols: 100,
  cache_ttl_ms: 3000,
  age_ms: 0,
  auto_refresh: true,
};

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ConsolePanel", () => {
  it("defaults to mission-status and fetches it on mount", async () => {
    const fetchMock = vi.fn((_url: string, _init?: RequestInit) => Promise.resolve(jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText("mission status — 0 missions")).toBeInTheDocument());
    // `cols` itself is jsdom-layout-dependent (jsdom reports 0 clientWidth,
    // clamped to the floor) — not asserted exactly here, see `panels.test.ts`
    // for the clamp arithmetic itself. What matters for this test is the
    // path + the required header.
    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toMatch(/^\/panel\/mission-status\?cols=\d+$/);
    expect(init?.headers).toMatchObject({ "X-Darkmux-Panel": "1" });
    expect(screen.getByText("mission status", { selector: ".runchip" })).toHaveClass("on");
  });

  it("selecting doctor (manual-only) does NOT auto-fetch — shows the not-yet-run placeholder", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() =>
      expect(screen.getByText(/not run yet — this panel probes the machine/)).toBeInTheDocument(),
    );
    expect(fetchMock).not.toHaveBeenCalled();
    // Chrome shows "run", never "re-run", while unloaded.
    expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument();
  });

  it("clicking doctor's run button fetches it exactly once, then shows re-run", async () => {
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        jsonResponse({ ...MISSION_STATUS_BODY, panel: "doctor", argv: ["doctor"], ansi_text: "darkmux doctor — 0 checks", auto_refresh: false, cache_ttl_ms: 0 }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() => expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "run" }));
    await waitFor(() => expect(screen.getByText(/darkmux doctor/)).toBeInTheDocument());
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "re-run" })).toBeInTheDocument();
    expect(screen.getByText("· manual-run only")).toBeInTheDocument();
  });

  it("clicking a different tab fetches that panel and marks it active", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "id description" }));
      }
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("id description")).toBeInTheDocument());
    expect(screen.getByText("roles").closest(".runchip")).toHaveClass("on");
    expect(screen.getByText("mission status").closest(".runchip")).not.toHaveClass("on");
  });

  it("a daemon error response renders the daemon's own message in the error state", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(new Response("panel \"mission-status\" timed out\n", { status: 504 }))),
    );
    renderPanel("");
    await waitFor(() => expect(screen.getByText('panel "mission-status" timed out')).toBeInTheDocument());
  });

  it("re-visiting an already-loaded panel reuses the cache (no second fetch)", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, ansi_text: "roles here" }));
      }
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());
    const callsAfterFirstVisit = fetchMock.mock.calls.length;

    fireEvent.click(screen.getByText("mission status"));
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());

    expect(fetchMock.mock.calls.length).toBe(callsAfterFirstVisit);
  });
});
