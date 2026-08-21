import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClientProvider, QueryClient } from "@tanstack/react-query";
import { ActivityPanel } from "./ActivityPanel";

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
});

function renderActivity(props: { capped: boolean; onShowAll?: () => void } = { capped: true }) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <ActivityPanel {...props} />
    </QueryClientProvider>,
  );
}

function mockRuns(runs: unknown[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (String(url).startsWith("/runs")) {
        return Promise.resolve(new Response(JSON.stringify({ runs, generated_at_ms: Date.now() }), { status: 200 }));
      }
      return Promise.resolve(new Response("not recorded\n", { status: 404 }));
    }),
  );
}

describe("ActivityPanel (#1904)", () => {
  it("genuinely empty state: a daemon with no run of any kind, ever, gets an honest line", async () => {
    mockRuns([]);
    renderActivity();
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());
    expect(document.querySelector(".labrunrow")).toBeNull();
  });

  it("NOT blank when nothing is currently running but history exists — renders recent rows, newest first", async () => {
    mockRuns([
      { id: "old-1", kind: "mission", status: "complete", tracked: true, updated_ts: 100 },
      { id: "newer-1", kind: "dispatch", status: "complete", tracked: false, updated_ts: 300 },
      { id: "newest-1", kind: "lab", status: "error", tracked: true, updated_ts: 500 },
    ]);
    renderActivity();
    await waitFor(() => expect(document.querySelectorAll(".labrunrow").length).toBe(3));
    const rows = document.querySelectorAll(".labrunrow");
    expect(rows[0].textContent).toContain("newest-1");
    expect(rows[1].textContent).toContain("newer-1");
    expect(rows[2].textContent).toContain("old-1");
  });

  it("a running dispatch sorts to the top by recency and carries its kind chip", async () => {
    mockRuns([
      { id: "finished-long-ago", kind: "mission", status: "complete", tracked: true, updated_ts: 10 },
      { id: "running-now", kind: "dispatch", status: "running", tracked: false, role: "coder", updated_ts: 99999 },
    ]);
    renderActivity();
    await waitFor(() => expect(document.querySelectorAll(".labrunrow").length).toBe(2));
    const rows = document.querySelectorAll(".labrunrow");
    expect(rows[0].textContent).toContain("running-now");
    expect(rows[0].textContent).toContain("dispatch");
    expect(rows[0].textContent).toContain("running");
  });

  it("caps the default view at 10 and discloses the hidden count honestly (never reports the cap as the total)", async () => {
    const runs = Array.from({ length: 14 }, (_, i) => ({
      id: `r${i}`,
      kind: "mission",
      status: "complete",
      tracked: true,
      updated_ts: i,
    }));
    mockRuns(runs);
    const onShowAll = vi.fn();
    renderActivity({ capped: true, onShowAll });
    await waitFor(() => expect(document.querySelectorAll(".labrunrow").length).toBe(10));
    expect(screen.getByText("activity — 14 runs")).toBeInTheDocument();
    expect(screen.getByText("… 4 more (10 of 14 shown)")).toBeInTheDocument();
    fireEvent.click(screen.getByText("→ show every run"));
    expect(onShowAll).toHaveBeenCalledTimes(1);
  });

  it("the uncapped 'all activity' view renders every run with no disclosure line", async () => {
    const runs = Array.from({ length: 14 }, (_, i) => ({
      id: `r${i}`,
      kind: "mission",
      status: "complete",
      tracked: true,
      updated_ts: i,
    }));
    mockRuns(runs);
    renderActivity({ capped: false });
    await waitFor(() => expect(document.querySelectorAll(".labrunrow").length).toBe(14));
    expect(screen.queryByText(/more \(/)).toBeNull();
    expect(screen.queryByText("→ show every run")).toBeNull();
  });

  it("an untracked dispatch row opens the session drill (#1900-consistent)", async () => {
    mockRuns([{ id: "dispatch-1", kind: "dispatch", status: "running", tracked: false, updated_ts: 1 }]);
    renderActivity();
    await waitFor(() => expect(document.querySelector(".labrunrow")).not.toBeNull());
    fireEvent.click(document.querySelector(".labrunrow")!);
    expect(window.location.hash).toBe("#session=dispatch-1");
  });

  it("a tracked mission row opens the mission graph", async () => {
    mockRuns([{ id: "mission-1", kind: "mission", status: "complete", tracked: true, updated_ts: 1 }]);
    const meta = document.createElement("meta");
    meta.name = "darkmux-mode";
    meta.content = "live";
    document.head.appendChild(meta);
    try {
      renderActivity();
      await waitFor(() => expect(document.querySelector(".labrunrow")).not.toBeNull());
      fireEvent.click(document.querySelector(".labrunrow")!);
      expect(window.location.hash).toBe("#mission=mission-1");
    } finally {
      meta.remove();
    }
  });

  it("a lab run row opens the runs lens pinned to kind=lab with its own run deep link", async () => {
    mockRuns([{ id: "lab-run-dir-1", kind: "lab", status: "complete", tracked: true, updated_ts: 1 }]);
    renderActivity();
    await waitFor(() => expect(document.querySelector(".labrunrow")).not.toBeNull());
    fireEvent.click(document.querySelector(".labrunrow")!);
    expect(window.location.hash).toBe("#lens=runs&kind=lab&run=lab-run-dir-1");
  });

  it("an untracked peer mission (fleet-only, no local session) stays non-interactive", async () => {
    mockRuns([{ id: "peer-mission", kind: "mission", status: "running", tracked: false, updated_ts: 1 }]);
    renderActivity();
    await waitFor(() => expect(document.querySelector(".labrunrow")).not.toBeNull());
    const row = document.querySelector(".labrunrow")!;
    expect(row).toHaveClass("flat");
    fireEvent.click(row);
    expect(window.location.hash).toBe("");
  });
});
