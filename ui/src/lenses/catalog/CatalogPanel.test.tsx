import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor, fireEvent } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { CatalogPanel } from "./CatalogPanel";
import { todayUTC } from "./format";

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

function renderPanel() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
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

  it("treats a failed fetch on either endpoint as empty, matching legacy's Promise.allSettled swallow", async () => {
    stubFetch({ daysStatus: 500, missions: { missions: [], generated_at_ms: 0 } });
    renderPanel();
    screen.getByRole("button", { name: /browse history/i }).click();
    await waitFor(() => expect(screen.getByText(/no recorded days yet/i)).toBeInTheDocument());
    // The live row still renders — a failed days fetch doesn't blank the whole panel.
    expect(screen.getByText("● live")).toBeInTheDocument();
  });

  // QA must-fix: legacy has THREE ways to close #catpanel
  // (viewer.html:3002-3008 click-outside, viewer.html:3020-3023 Escape); the
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
