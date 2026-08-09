import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useFlowWindow } from "./useFlowWindow";

// (QA, packet 5) The window must follow the UTC day ITSELF.
//
// Before this, `today` was derived once per render and the rollover relied on
// `useLiveTail` invalidating the new day's query — which is a no-op, because
// at the rollover instant that query has never existed. On an idle daemon the
// window therefore kept yesterday's keys indefinitely while the reopened
// stream wrote the new day's first records into a slot nothing subscribed to.
// Records arriving, and invisible: the exact defect class this viewer arc
// exists to remove.
//
// The assertion is on the REQUESTED DATES, not on an internal call, because
// the fetch is the observable effect an operator's screen depends on.

function wrapper(queryClient: QueryClient) {
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("useFlowWindow — UTC date rollover", () => {
  let requested: string[] = [];

  beforeEach(() => {
    requested = [];
    vi.useFakeTimers({ shouldAdvanceTime: true });
    vi.setSystemTime(Date.parse("2026-08-08T23:59:55.000Z"));
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const m = String(url).match(/^\/flow\/(\d{4}-\d{2}-\d{2})/);
        if (m) requested.push(m[1]);
        return Promise.resolve(new Response(JSON.stringify([]), { status: 200 }));
      }),
    );
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("starts fetching the NEW day's window after midnight, without anything else re-rendering it", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(() => useFlowWindow(Date.now()), { wrapper: wrapper(qc) });

    await waitFor(() => expect(requested).toContain("2026-08-08"));
    expect(requested).not.toContain("2026-08-09");

    // Cross midnight. Nothing else happens — no records, no presence churn,
    // no navigation. An idle fleet at midnight is the whole failure case.
    vi.setSystemTime(Date.parse("2026-08-09T00:00:05.000Z"));
    await vi.advanceTimersByTimeAsync(6000);

    await waitFor(() => expect(requested).toContain("2026-08-09"), { timeout: 3000 });
  });
});
