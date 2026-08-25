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

/** URL-AWARE on purpose. The two endpoints have DIFFERENT wire shapes, and a
 *  mock that returns one shape for both is green while the decode is broken —
 *  which is exactly what happened here, caught by a live probe at the merge
 *  gate rather than by this file:
 *
 *    GET /flow/<date>        -> a BARE JSON ARRAY   (lib.rs flow_handler)
 *    GET /flow-session/<id>  -> { records, count, ... }  (catalog_records_response)
 */
function mockFetch(records: unknown[]) {
  return vi.fn(async (url: string) => {
    const u = String(url);
    const body = u.startsWith("/flow/")
      ? records // bare array
      : { records, count: records.length, truncated: false, generated_at_ms: 0 };
    return { ok: true, status: 200, json: async () => body };
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
    // Without this a hook fetching `/flow/undefined` and receiving the canned
    // mock would pass identically — right records, wrong reason.
    expect(globalThis.fetch).toHaveBeenCalledWith("/flow/2026-08-01", undefined);
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

/**
 * (#1801) A static-build playback route (`route.date === null`, forced by
 * `route.ts` under `isStaticBuild()`) reads the committed `.jsonl` named by
 * `darkmux-flow-src` instead of `GET /flow/<date>` — there is no daemon
 * behind the static demo to serve that endpoint at all.
 */
describe("useRouteRecords — the static-demo flow-src route (#1801)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  });

  function mockStaticSrc(text: string) {
    return vi.fn(async (url: string) => {
      if (String(url) === "./demo-flow.jsonl") {
        return { ok: true, status: 200, text: async () => text };
      }
      throw new Error(`unexpected fetch in static test: ${url}`);
    });
  }

  it("reads records from the flow-src file, never GET /flow/<date>", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal("fetch", mockStaticSrc('{"ts":"2026-08-07T00:00:00Z","action":"dispatch.start"}\n'));
    const route: Route = { kind: "playback", date: null };

    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.historical).toBe(true);
    expect(result.current.records).toEqual([{ ts: "2026-08-07T00:00:00Z", action: "dispatch.start" }]);
    // The regression this test guards: falling through to `/flow/<date>`
    // (or `/flow/null`) instead of the static source.
    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(calls.some((u) => u.startsWith("/flow/"))).toBe(false);
  });

  it("is EMPTY, not the live window, when the static source is unreachable", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => ({ ok: false, status: 404, text: async () => "" })),
    );
    const route: Route = { kind: "playback", date: null };

    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.records).toEqual([]);
    expect(result.current.historical).toBe(true);
    expect(result.current.records).not.toEqual(LIVE.data);
  });

  // Inverted case: the SAME playback route shape, without the meta, must
  // keep hitting /flow/<date> exactly as it always has — a gate that fires
  // whenever `route.kind==="playback"` (rather than only under
  // `isStaticBuild()`) would make this regress silently.
  it("without the meta, a dated playback route still hits GET /flow/<date> as before", async () => {
    const route: Route = { kind: "playback", date: "2026-08-01" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.loading).toBe(false));

    expect(result.current.records).toEqual([{ action: "HISTORICAL-RECORD" }]);
    expect(globalThis.fetch).toHaveBeenCalledWith("/flow/2026-08-01", undefined);
  });
});

/**
 * A session drill-in that never refetches (the state before this block
 * existed). The operator's report was "the run lens is stuck — clock frozen,
 * no events — but fleet is still moving", and the two halves of that sentence
 * are one bug: the session query fetched once, so nothing derived from those
 * records could advance, while fleet read the polled live window.
 *
 * `historical` is asserted alongside the refetch because they must agree. A
 * running session presented as a replay is the same lie in the other
 * direction.
 */
function mockFetchLive(opts: { liveIds: string[]; records: () => unknown[] }) {
  return vi.fn(async (url: string) => {
    const u = String(url);
    if (u.startsWith("/fleet/sessions/live")) {
      return {
        ok: true,
        status: 200,
        json: async () => ({ sessions: opts.liveIds.map((id) => ({ session_id: id })) }),
      };
    }
    const recs = opts.records();
    const body = u.startsWith("/flow/") ? recs : { records: recs, count: recs.length, truncated: false, generated_at_ms: 0 };
    return { ok: true, status: 200, json: async () => body };
  });
}

describe("useRouteRecords — a session that is still running", () => {
  it("is not historical while presence still reports it live", async () => {
    vi.stubGlobal("fetch", mockFetchLive({ liveIds: ["s-live"], records: () => [{ action: "a" }] }));
    const route: Route = { kind: "session", sessionId: "s-live" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.historical).toBe(false));
    // Still that session's OWN records — liveness must not silently swap in
    // the live window, which is the regression the block above guards.
    expect(result.current.records).toEqual([{ action: "a" }]);
  });

  it("stays historical when presence does not list it", async () => {
    vi.stubGlobal("fetch", mockFetchLive({ liveIds: ["someone-else"], records: () => [{ action: "a" }] }));
    const route: Route = { kind: "session", sessionId: "s-done" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.historical).toBe(true);
  });

  it("picks up events that arrive after the first fetch — the freeze itself", async () => {
    let batch = [{ action: "turn-1" }];
    vi.stubGlobal("fetch", mockFetchLive({ liveIds: ["s-live"], records: () => batch }));

    const route: Route = { kind: "session", sessionId: "s-live" };
    const { result } = renderHook(() => useRouteRecords(route, LIVE), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.records).toEqual([{ action: "turn-1" }]));

    // The dispatch keeps working and emits another turn.
    batch = [{ action: "turn-1" }, { action: "turn-2" }];

    await waitFor(() => expect(result.current.records).toHaveLength(2), { timeout: 15_000 });
    expect(result.current.records).toEqual([{ action: "turn-1" }, { action: "turn-2" }]);
  }, 20_000);
});
