import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useRouteRecords } from "./useRouteRecords";
import type { Route } from "../lib/route";
import type { FlowWindowResult } from "./useFlowWindow";

/**
 * (#1800 P1) The bug this hook exists to remove: `showsEventLog()` is true for
 * `playback` and `session`, and App fed the event log the LIVE rolling window
 * on every route. So a session route rendered "session replay" in the stage
 * while the column beside it listed unrelated live traffic.
 *
 * These assert the routing decision directly, because the failure mode is
 * silent — wrong records render exactly as convincingly as right ones.
 */

const LIVE: FlowWindowResult = {
  settled: true,
  tMax: 0,
  data: [{ action: "LIVE-RECORD" }] as never,
};

function wrapper() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

function mockFetch(records: unknown[]) {
  return vi.fn().mockResolvedValue({
    ok: true,
    status: 200,
    json: async () => ({ records, count: records.length, truncated: false, generated_at_ms: 0 }),
  });
}

beforeEach(() => {
  vi.stubGlobal("fetch", mockFetch([{ action: "HISTORICAL-RECORD" }]));
});
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useRouteRecords", () => {
  it("gives a LIVE route the live window", () => {
    const route: Route = { kind: "fleet" } as Route;
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });
    expect(result.current.records).toEqual(LIVE.data);
    expect(result.current.historical).toBe(false);
  });

  it("gives a SESSION route that session's records, never the live window", async () => {
    const route: Route = { kind: "session", sessionId: "s-1" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.historical).toBe(true);
    expect(result.current.records).toEqual([{ action: "HISTORICAL-RECORD" }]);
    // The regression that motivated the hook: live records leaking into a
    // historical route's event log.
    expect(result.current.records).not.toEqual(LIVE.data);
  });

  it("gives a PLAYBACK route that day's records, never the live window", async () => {
    const route: Route = { kind: "playback", date: "2026-08-01" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.historical).toBe(true);
    expect(result.current.records).toEqual([{ action: "HISTORICAL-RECORD" }]);
    expect(result.current.records).not.toEqual(LIVE.data);
  });

  it("shows EMPTY rather than live records when a historical fetch FAILS", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 500, json: async () => ({}) }));
    const route: Route = { kind: "session", sessionId: "missing" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.loading).toBe(false));

    // Falling back to the live window here would be the original bug wearing
    // an error handler — empty is honest, wrong is not.
    expect(result.current.records).toEqual([]);
    expect(result.current.historical).toBe(true);
  });

  it("reports loading while a historical slice is in flight, so empty is distinguishable", () => {
    const route: Route = { kind: "playback", date: "2026-08-01" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });
    expect(result.current.loading).toBe(true);
    expect(result.current.records).toEqual([]);
  });
});
