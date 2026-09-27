import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { useFlowWindow } from "./useFlowWindow";
import { prevDateUTC, todayUTC } from "../lib/flow";

// (#2965) A failed `/flow/<day>` read settles the window exactly as a
// successful one does, and contributes no records: to every consumer it
// looked like a quiet day. `failure` is what tells the two apart. A day
// with no file is NOT a failure: the daemon answers it `200 []`.

const NOW = Date.parse("2026-08-08T12:00:30.000Z");

function wrapper(queryClient: QueryClient) {
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

function stubFlow(status: { today: number; yesterday: number }) {
  const today = todayUTC();
  const yesterday = prevDateUTC(today);
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const s = String(url) === `/flow/${today}` ? status.today : String(url) === `/flow/${yesterday}` ? status.yesterday : 404;
      return Promise.resolve(
        s === 200
          ? new Response("[]", { status: 200 })
          : new Response("boom", { status: s, statusText: "Internal Server Error" }),
      );
    }),
  );
}

describe("useFlowWindow — a failed day read is reported, not folded into a quiet day (#2965)", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(NOW);
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  const render = () =>
    renderHook(() => useFlowWindow(NOW), {
      wrapper: wrapper(new QueryClient({ defaultOptions: { queries: { retry: false } } })),
    });

  it("both days answer 200 []: settled, no failure — the inverted case", async () => {
    stubFlow({ today: 200, yesterday: 200 });
    const { result } = render();
    await waitFor(() => expect(result.current.settled).toBe(true));
    expect(result.current.failure).toBeNull();
  });

  it("both days fail: settled, and the failure names the status", async () => {
    stubFlow({ today: 500, yesterday: 500 });
    const { result } = render();
    await waitFor(() => expect(result.current.settled).toBe(true));
    expect(result.current.failure).toEqual({ status: 500, message: "500 Internal Server Error", today: true, yesterday: true });
    expect(result.current.data).toEqual([]);
  });

  for (const which of ["today", "yesterday"] as const) {
    it(`only ${which} fails: still a failure, since that day's records are missing`, async () => {
      stubFlow({ today: which === "today" ? 500 : 200, yesterday: which === "yesterday" ? 500 : 200 });
      const { result } = render();
      await waitFor(() => expect(result.current.settled).toBe(true));
      expect(result.current.failure).toEqual({ status: 500, message: "500 Internal Server Error", today: which === "today", yesterday: which === "yesterday" });
    });
  }
});
