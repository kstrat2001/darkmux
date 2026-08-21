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
  window.location.hash = "";
});

// (#1904) `/runs` — the ONLY fetch a bare landing (no `panel=`) makes now
// that the default is the client-rendered activity view, not the
// `mission-status` CLI panel. Most tests below don't care about its
// content, so this stands in a well-shaped, empty response wherever a
// test's own fetch mock doesn't override `/runs` itself.
function runsJson(runs: unknown[] = []) {
  return jsonResponse({ runs, generated_at_ms: Date.UTC(2026, 0, 1, 12, 0, 0) });
}

describe("ConsolePanel", () => {
  // (#1904) Three false starts preceded this shape (documented in
  // `ActivityPanel.tsx`'s own module doc): a section bolted above the old
  // default, then a mission-status fallback, before the operator settled on
  // the console's default being the activity view itself — no panel fetch
  // at all until the operator explicitly picks a CLI panel tab.
  it("lands on the activity view by default — fetches ONLY /runs, no /panel/* call, until a CLI tab is explicitly picked", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());
    expect(fetchMock.mock.calls.some(([u]) => String(u).startsWith("/panel/"))).toBe(false);
    expect(screen.getByText("activity", { selector: ".runchip" })).toHaveClass("on");
    expect(screen.getByText("mission status", { selector: ".runchip" })).not.toHaveClass("on");
  });

  it("a running dispatch is visible on the DEFAULT console view, without picking any panel", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson([{ id: "d1", kind: "dispatch", status: "running", tracked: false, role: "coder", updated_ts: 1 }])
          : jsonResponse(MISSION_STATUS_BODY),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    // Same page, no interaction — the running row is already there.
    await waitFor(() => expect(document.querySelector(".consoleactivity .labrunrow")).not.toBeNull());
    expect(document.querySelector(".consoleactivity .labrunrow")!.textContent).toContain("d1");
  });

  it("selecting doctor (manual-only) does NOT auto-fetch — shows the not-yet-run placeholder", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() => expect(screen.getByText(/not run yet — this panel probes the machine/)).toBeInTheDocument());
    // The manual-only contract (#1286) is about PANEL probes, never fired by
    // selecting the tab.
    expect(fetchMock.mock.calls.some(([u]) => String(u).startsWith("/panel/"))).toBe(false);
    expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument();
  });

  it("clicking doctor's run button fetches it exactly once, then shows re-run", async () => {
    const fetchMock = vi.fn((url: string) =>
      Promise.resolve(
        url.startsWith("/runs")
          ? runsJson()
          : jsonResponse({ ...MISSION_STATUS_BODY, panel: "doctor", argv: ["doctor"], ansi_text: "darkmux doctor — 0 checks", auto_refresh: false, cache_ttl_ms: 0 }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("doctor");
    await waitFor(() => expect(screen.getByRole("button", { name: "run" })).toBeInTheDocument());

    fireEvent.click(screen.getByRole("button", { name: "run" }));
    await waitFor(() => expect(screen.getByText(/darkmux doctor/)).toBeInTheDocument());
    expect(fetchMock.mock.calls.filter(([u]) => String(u).startsWith("/panel/"))).toHaveLength(1);
    expect(screen.getByRole("button", { name: "re-run" })).toBeInTheDocument();
    expect(screen.getByText("· manual-run only")).toBeInTheDocument();
  });

  it("clicking a CLI panel tab fetches it and marks it active, deactivating the activity tab", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, panel: "role-list", argv: ["role", "list"], ansi_text: "id description" }));
      }
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("id description")).toBeInTheDocument());
    expect(screen.getByText("roles").closest(".runchip")).toHaveClass("on");
    expect(screen.getByText("activity", { selector: ".runchip" })).not.toHaveClass("on");
  });

  it("a daemon error response renders the daemon's own message when explicitly selecting mission-status", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        Promise.resolve(url.startsWith("/runs") ? runsJson() : new Response('panel "mission-status" timed out\n', { status: 504 })),
      ),
    );
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText('panel "mission-status" timed out')).toBeInTheDocument());
  });

  it("re-visiting an already-loaded CLI panel reuses the cache (no second fetch)", async () => {
    const fetchMock = vi.fn((url: string) => {
      if (url.startsWith("/panel/role-list")) {
        return Promise.resolve(jsonResponse({ ...MISSION_STATUS_BODY, ansi_text: "roles here" }));
      }
      if (url.startsWith("/runs")) return Promise.resolve(runsJson());
      return Promise.resolve(jsonResponse(MISSION_STATUS_BODY));
    });
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("");
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());
    fireEvent.click(screen.getByText("mission status"));
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    // Both CLI panels have now been fetched once each (plus the one /runs
    // call from the initial activity landing) — this is the baseline every
    // further tab switch should reuse rather than add to.
    const callsAfterBothVisited = fetchMock.mock.calls.length;

    fireEvent.click(screen.getByText("roles"));
    await waitFor(() => expect(screen.getByText("roles here")).toBeInTheDocument());

    expect(fetchMock.mock.calls.length).toBe(callsAfterBothVisited);
  });

  // (#1904) The escape hatch, mirroring the `mission-status`/
  // `mission-status-all` tab pairing already in `PANELS`: both a dedicated
  // "all activity" tab AND the default view's own inline link reach it.
  it("the 'all activity' tab and the default view's own 'show every run' link both reach the uncapped view", async () => {
    const runs = Array.from({ length: 12 }, (_, i) => ({ id: `r${i}`, kind: "mission", status: "complete", tracked: true, updated_ts: i }));
    vi.stubGlobal("fetch", vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson(runs) : jsonResponse(MISSION_STATUS_BODY))));
    renderPanel("");
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(10));

    fireEvent.click(screen.getByText("→ show every run"));
    await waitFor(() => expect(document.querySelectorAll(".consoleactivity .labrunrow").length).toBe(12));
    expect(screen.getByText("all activity", { selector: ".runchip" })).toHaveClass("on");

    // Direct deep link to the tab reaches the same uncapped state.
    vi.unstubAllGlobals();
  });

  it("mission-status stays selectable and unaffected by the default's change", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(url.startsWith("/runs") ? runsJson() : jsonResponse(MISSION_STATUS_BODY)));
    vi.stubGlobal("fetch", fetchMock);
    renderPanel("mission-status");
    await waitFor(() => expect(screen.getByText(/mission status —/)).toBeInTheDocument());
    expect(screen.getByText("mission status", { selector: ".runchip" })).toHaveClass("on");
  });
});
