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

describe("ActivityPanel (#1904 QA fix)", () => {
  // Blocking finding from code review: `all` defaults to `[]` while the
  // `/runs` fetch is still in flight (and permanently on a failed fetch),
  // so `total === 0` and the component asserted the SAME "no activity
  // recorded yet — nothing has run on this daemon" claim on EVERY entry —
  // a stated historical fact about the daemon that was simply untrue while
  // loading, and stuck untrue forever on an error. These two tests pin
  // real pending/error branches instead.
  it("while loading, shows a loading state — NOT the 'nothing has run' claim", async () => {
    let resolveFetch: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        () =>
          new Promise<Response>((resolve) => {
            resolveFetch = resolve;
          }),
      ),
    );
    renderActivity();
    // Still pending — must NOT claim daemon history is empty.
    expect(screen.queryByText(/no activity recorded yet/i)).toBeNull();
    expect(document.querySelector('[data-state="activity-pending"]')).not.toBeNull();
    resolveFetch!(new Response(JSON.stringify({ runs: [], generated_at_ms: Date.now() }), { status: 200 }));
    await waitFor(() => expect(screen.getByText(/no activity recorded yet/i)).toBeInTheDocument());
  });

  it("a failed /runs fetch shows an honest error — NOT the 'nothing has run' claim", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("daemon unreachable", { status: 500 }))));
    renderActivity();
    await waitFor(() => expect(document.querySelector('[data-state="activity-error"]')).not.toBeNull());
    expect(screen.queryByText(/no activity recorded yet/i)).toBeNull();
    expect(document.querySelector(".panelerr")).not.toBeNull();
  });

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

  // (#1904 QA fix) Was: the disclosure line was gated on `onShowAll` being
  // present at all (`{hidden > 0 && onShowAll && (...)}`), so a capped
  // caller that passed no `onShowAll` got NEITHER the "… N more" line NOR
  // the link — the cap silently reported as the total, exactly the
  // #1876/#1891 rule this module's own doc invokes. Only `ConsolePanel.tsx`
  // happens to always pass `onShowAll` today, so this was latent, not
  // exercised — the prop is documented as optional and nothing enforced
  // the pairing.
  it("discloses the hidden count even when the caller passes no onShowAll (the cap must never silently read as the total)", async () => {
    const runs = Array.from({ length: 14 }, (_, i) => ({ id: `r${i}`, kind: "mission", status: "complete", tracked: true, updated_ts: i }));
    mockRuns(runs);
    renderActivity({ capped: true });
    await waitFor(() => expect(document.querySelectorAll(".labrunrow").length).toBe(10));
    expect(screen.getByText("… 4 more (10 of 14 shown)")).toBeInTheDocument();
    // No link to click through to, since there's nowhere for it to go —
    // but the COUNT itself must still be honest.
    expect(screen.queryByText("→ show every run")).toBeNull();
  });

  // (#1904 QA fix) Was: `activityRunActivate` silently no-op'd for a
  // tracked mission/dispatch row when `missionGraphReachable()` is false
  // (the daemon-less static demo) — the row rendered interactive (`RunRow`'s
  // `interactive` gate has nothing to do with graph reachability) but did
  // nothing on click, a dead affordance. `RunsBoard.tsx`'s own
  // `activateRun` shows a notice for the identical case; this pins the
  // same behavior here via the now-shared `runDestination`.
  it("a tracked mission click on a daemon-less page shows the SAME unreachable notice RunsBoard shows, instead of doing nothing", async () => {
    mockRuns([{ id: "mission-1", kind: "mission", status: "complete", tracked: true, updated_ts: 1 }]);
    // No <meta name="darkmux-mode"> injected — missionGraphReachable() is
    // false, matching the daemon-less static demo build.
    renderActivity();
    await waitFor(() => expect(document.querySelector(".labrunrow")).not.toBeNull());
    fireEvent.click(document.querySelector(".labrunrow")!);
    expect(window.location.hash).toBe(""); // no navigation happened
    expect(screen.getByText(/mission graph needs a running daemon/i)).toBeInTheDocument();
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
