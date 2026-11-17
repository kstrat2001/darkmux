import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { CatalogPanel } from "./CatalogPanel";
import { todayUTC } from "./format";
import { queryKeys } from "../../lib/queryKeys";
import { clk } from "../../lib/format";

function jsonResponse(body: unknown, status = 200) {
  return Promise.resolve(new Response(JSON.stringify(body), { status }));
}

function stubFetch(handlers: { days?: unknown; missions?: unknown; daysStatus?: number; missionsStatus?: number }) {
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL) => {
      const url = String(input);
      if (url.startsWith("/flow-days")) return jsonResponse(handlers.days ?? { days: [], generated_at_ms: 0 }, handlers.daysStatus ?? 200);
      if (url.startsWith("/flow-missions"))
        return jsonResponse(handlers.missions ?? { missions: [], generated_at_ms: 0 }, handlers.missionsStatus ?? 200);
      return Promise.reject(new Error(`unexpected fetch: ${url}`));
    }),
  );
}

function renderPanel(queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })) {
  return render(
    <QueryClientProvider client={queryClient}>
      <CatalogPanel />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  window.location.hash = "";
});

describe("CatalogPanel", () => {
  it("does not fetch or render #catpanel until the toggle is clicked", () => {
    stubFetch({});
    renderPanel();
    expect(document.getElementById("catpanel")).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("fetches both endpoints and renders the live row + days on toggle, marking today's own day row", async () => {
    // `today` is computed from the SAME `todayUTC()` the component itself
    // calls (not a hardcoded literal), so this assertion is correct
    // regardless of which real calendar day the suite happens to run on —
    // avoids the flaky-by-date trap a hardcoded fixture date would hit.
    const today = todayUTC();
    stubFetch({
      days: { days: [{ date: today, records: 5, dispatches: 1, missions: [] }], generated_at_ms: 0 },
    });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText("● live")).toBeInTheDocument());
    expect(screen.getByText(new RegExp(`^${today} · today$`))).toBeInTheDocument();
  });

  it("renders the catempty fallback when there are no recorded days", async () => {
    stubFetch({});
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText(/no recorded days yet/i)).toBeInTheDocument());
  });

  it("omits the missions section entirely when there are none", async () => {
    stubFetch({});
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(document.getElementById("catpanel")).toBeTruthy());
    expect(screen.queryByText(/^missions/)).toBeNull();
  });

  it("discloses the mission cap when the list is truncated (viewer.html's #1569 sweep behavior)", async () => {
    const missions = Array.from({ length: 60 }, (_, i) => ({
      mission_id: `m${i}`,
      records: 1,
      dispatches: 1,
      machines: [],
      first_ts: "",
      last_ts: "",
      first_date: "2026-08-08",
      last_date: "2026-08-08",
    }));
    stubFetch({ missions: { missions, generated_at_ms: 0 } });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText("missions · newest 50 of 60")).toBeInTheDocument());
    // Only the first 50 rendered, never all 60 — the cap is real, not just disclosed.
    expect(screen.getAllByText(/^▣ m/)).toHaveLength(50);
  });

  it("live row navigates via location.hash, not a full page navigation, and closes the panel", async () => {
    // (#2412 round 2, reviewer finding) jsdom's `location.hash` already
    // defaults to `""` — asserting `toBe("")` after the click proved
    // nothing, since a `goLive` that never ran would look identical. Start
    // on a REAL, non-empty hash so only an actual navigation clears it.
    window.location.hash = "#2026-08-07";
    stubFetch({});
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText("● live")).toBeInTheDocument());
    screen.getByText("● live").closest("button")!.click();
    await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());
    expect(window.location.hash).toBe("");
  });

  it("a day row sets location.hash to the bare date (the new playback route)", async () => {
    stubFetch({
      days: { days: [{ date: "2026-08-07", records: 5, dispatches: 1, missions: [] }], generated_at_ms: 0 },
    });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText("2026-08-07")).toBeInTheDocument());
    screen.getByText("2026-08-07").closest("button")!.click();
    expect(window.location.hash).toBe("#2026-08-07");
  });

  it("a mission row sets location.hash to mission=<id> (reuses the existing mission-redirect route)", async () => {
    stubFetch({
      missions: {
        missions: [
          {
            mission_id: "my-mission",
            records: 1,
            dispatches: 1,
            machines: [],
            first_ts: "",
            last_ts: "",
            first_date: "2026-08-08",
            last_date: "2026-08-08",
          },
        ],
        generated_at_ms: 0,
      },
    });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await screen.findByText("▣ my-mission");
    screen.getByText("▣ my-mission").closest("button")!.click();
    expect(window.location.hash).toBe("#mission=my-mission");
  });

  // A failed read is never shown as an empty history: "no recorded days yet"
  // off a 500 tells the operator they have no history when the daemon simply
  // did not answer.
  it("a failed /flow-days read says it could not load, with the status and reason, never 'no recorded days yet'", async () => {
    stubFetch({ daysStatus: 500, missions: { missions: [], generated_at_ms: 0 } });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/^couldn't load recorded days \(HTTP 500\): 500/);
    expect(screen.queryByText(/no recorded days yet/i)).toBeNull();
    // The live row still renders: a failed days fetch doesn't blank the whole panel.
    expect(screen.getByText("● live")).toBeInTheDocument();
  });

  it("a failed /flow-missions read says so, instead of silently dropping the missions section", async () => {
    stubFetch({ missionsStatus: 503, days: { days: [{ date: "2026-08-07", records: 5, dispatches: 1, missions: [] }], generated_at_ms: 0 } });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/^couldn't load missions \(HTTP 503\): 503/);
    // The days that did load still render.
    expect(screen.getByText("2026-08-07")).toBeInTheDocument();
  });

  it("an unreachable daemon (no HTTP status) names the network error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new TypeError("Failed to fetch"))),
    );
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getAllByRole("alert")).toHaveLength(2));
    expect(screen.getByText("couldn't load recorded days: Failed to fetch", { exact: false })).toBeInTheDocument();
    expect(screen.queryByText(/HTTP/)).toBeNull();
    expect(screen.queryByText(/no recorded days yet/i)).toBeNull();
  });

  it("retry re-reads a failed section and renders what it returns", async () => {
    let daysOk = false;
    vi.stubGlobal(
      "fetch",
      vi.fn((input: RequestInfo | URL) => {
        const url = String(input);
        if (url.startsWith("/flow-days"))
          return daysOk
            ? jsonResponse({ days: [{ date: "2026-08-07", records: 5, dispatches: 1, missions: [] }], generated_at_ms: 0 })
            : jsonResponse({}, 500);
        return jsonResponse({ missions: [], generated_at_ms: 0 });
      }),
    );
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await screen.findByRole("alert");
    daysOk = true;
    fireEvent.click(screen.getByRole("button", { name: /retry/i }));
    await waitFor(() => expect(screen.getByText("2026-08-07")).toBeInTheDocument());
    expect(screen.queryByRole("alert")).toBeNull();
    // Retry happens inside the panel: it must not close it.
    expect(document.getElementById("catpanel")).toBeTruthy();
  });

  it("a failed re-read after a good one keeps the days already shown, marked stale with the failure, and retry clears it", async () => {
    let daysOk = true;
    vi.stubGlobal(
      "fetch",
      vi.fn((input: RequestInfo | URL) => {
        const url = String(input);
        if (url.startsWith("/flow-days"))
          return daysOk
            ? jsonResponse({ days: [{ date: "2026-08-07", records: 5, dispatches: 1, missions: [] }], generated_at_ms: 1 })
            : jsonResponse({}, 500);
        return jsonResponse({ missions: [], generated_at_ms: 1 });
      }),
    );
    renderPanel();
    const toggle = screen.getByRole("button", { name: /browse history/i });
    fireEvent.click(toggle);
    await waitFor(() => expect(screen.getByText("2026-08-07")).toBeInTheDocument());
    // Close and reopen: every open re-reads, and this time the read fails.
    fireEvent.click(toggle);
    daysOk = false;
    fireEvent.click(toggle);
    await waitFor(() =>
      expect(
        screen.getByText(`showing recorded days from ${clk(1)}; couldn't refresh (HTTP 500): 500`, { exact: false }),
      ).toBeInTheDocument(),
    );
    expect(screen.getByText("2026-08-07")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();

    daysOk = true;
    fireEvent.click(screen.getByRole("button", { name: /retry/i }));
    await waitFor(() => expect(screen.queryByText(/couldn't refresh/)).toBeNull());
    expect(screen.getByText("2026-08-07")).toBeInTheDocument();
  });

  it("a genuinely empty history reads as empty, with no error", async () => {
    stubFetch({});
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText(/no recorded days yet/i)).toBeInTheDocument());
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByText(/couldn't load/i)).toBeNull();
  });

  it("while either read is in flight it shows the loading placeholder, neither empty nor error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => new Promise<Response>(() => {})),
    );
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    expect(await screen.findByRole("status", { name: "Loading history" })).toBeInTheDocument();
    expect(screen.queryByText(/no recorded days yet/i)).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("stays loading while one read has answered and the other is still in flight", async () => {
    const fetchSpy = vi.fn((input: RequestInfo | URL) =>
      String(input).startsWith("/flow-days") ? jsonResponse({ days: [], generated_at_ms: 0 }) : new Promise<Response>(() => {}),
    );
    vi.stubGlobal("fetch", fetchSpy);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderPanel(queryClient);
    screen.getByRole("button", { name: /browse history/i }).click();
    // The /flow-days answer is in the cache: only /flow-missions is pending.
    await waitFor(() => expect(queryClient.getQueryData(queryKeys.flowDays())).toBeDefined());
    expect(screen.getByRole("status", { name: "Loading history" })).toBeInTheDocument();
    expect(screen.queryByText(/no recorded days yet/i)).toBeNull();
  });

  // QA must-fix: legacy has THREE ways to close #catpanel
  //; the
  // panel originally dropped all of them. These three tests cover each
  // dismissal path independently, including the toggle-reclick case QA
  // named as the zero-coverage path.
  describe("dismissal", () => {
    it("Escape closes the panel", async () => {
      stubFetch({});
      renderPanel();
      screen.getByRole("button", { name: /browse history/i }).click();
      await waitFor(() => expect(document.getElementById("catpanel")).toBeTruthy());

      fireEvent.keyDown(document, { key: "Escape" });
      await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());
    });

    it("a click outside the panel (and outside the toggle) closes it", async () => {
      stubFetch({});
      renderPanel();
      screen.getByRole("button", { name: /browse history/i }).click();
      await waitFor(() => expect(document.getElementById("catpanel")).toBeTruthy());

      // document.body is neither the toggle nor a descendant of the
      // `.catalog-anchor` wrapper — a real "outside" click, matching
      // legacy's `!e.target.closest("#catpanel")` check (and implicitly
      // excluding the toggle itself, since the toggle isn't `#catpanel`
      // either — see CatalogPanel.tsx's own doc for why one containment
      // check on the shared anchor covers both exclusions).
      fireEvent.click(document.body);
      await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());
    });

    it("re-clicking the toggle while open closes it (zero-coverage path QA named)", async () => {
      stubFetch({});
      renderPanel();
      const toggle = screen.getByRole("button", { name: /browse history/i });
      fireEvent.click(toggle);
      await waitFor(() => expect(document.getElementById("catpanel")).toBeTruthy());

      // The SAME click both flips `open` via the toggle's own onClick AND
      // bubbles to the document-level outside-click listener — verifying
      // this doesn't double-toggle (re-open immediately) or throw is the
      // point: the click lands INSIDE `.catalog-anchor`, so the outside-click
      // listener no-ops and only the toggle's own handler closes it.
      fireEvent.click(toggle);
      await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());
    });

    it("a click on a row INSIDE the panel does not trigger the outside-click closer redundantly", async () => {
      stubFetch({});
      renderPanel();
      fireEvent.click(screen.getByRole("button", { name: /browse history/i }));
      await waitFor(() => expect(screen.getByText("● live")).toBeInTheDocument());

      // The live row's own onClick already closes the panel (asserted
      // elsewhere) — this test's point is narrower: clicking INSIDE the
      // panel must not throw or misbehave now that a document-level click
      // listener is also active.
      const liveRow = screen.getByText("● live").closest("button")!;
      expect(() => fireEvent.click(liveRow)).not.toThrow();
      await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());
    });

    it("Escape and outside-click listeners are removed once the panel closes (no stale global listeners)", async () => {
      stubFetch({});
      const removeSpy = vi.spyOn(document, "removeEventListener");
      renderPanel();
      screen.getByRole("button", { name: /browse history/i }).click();
      await waitFor(() => expect(document.getElementById("catpanel")).toBeTruthy());

      fireEvent.keyDown(document, { key: "Escape" });
      await waitFor(() => expect(document.getElementById("catpanel")).toBeNull());

      expect(removeSpy).toHaveBeenCalledWith("keydown", expect.any(Function));
      expect(removeSpy).toHaveBeenCalledWith("click", expect.any(Function));
      removeSpy.mockRestore();
    });
  });
});
