import { describe, expect, it, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useLiveSessionIds } from "./useLiveSessionIds";

/**
 * (#2725) The hook read `/fleet/sessions/live`'s `sessions` array and threw
 * the rest of the response away — including `meta.sources.fleet`, the
 * daemon's own report of whether it could read the fleet substrate at all
 * (`fleet_sessions_live_handler` emits it from the same
 * `source_state::coverage_meta` the machines endpoint uses).
 *
 * Nothing was silently wrong ON SCREEN, because `App` mounts
 * `FleetCoverageNotice` app-wide and that notice reads the MACHINES half of
 * the same substrate. But that cover is incidental — a different query, a
 * different mount — and a consumer of this hook had no way to tell "nothing
 * is running" from "we could not look". `useSessionLiveness` was making
 * exactly that mistake (see its own tests).
 *
 * The inverted cases are the point of half this file: a healthy fleet, an
 * unconfigured one, and a pending read must all report NO coverage problem.
 * A hook that reported degraded coverage generously would put a warning on
 * every standalone machine, which is its own defect.
 */

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

/** One `/fleet/sessions/live` answer. `null` body = a transport failure (a
 *  non-2xx), which `fetchJson` turns into a settled `ok:false` rather than a
 *  throw — the #1812 shape this app is built on. */
function stub(body: Record<string, unknown> | null) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      body === null
        ? { ok: false, status: 503, text: async () => "presence down", json: async () => ({}) }
        : { ok: true, status: 200, json: async () => body },
    ),
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useLiveSessionIds — the coverage half it used to discard (#2725)", () => {
  it("reports the daemon's own `unavailable` report, in the shared vocabulary", async () => {
    stub({
      sessions: [],
      meta: { sources: { fleet: { state: "unavailable", detail: "PRESENCE_READ_FAILED" } }, complete: false },
    });
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() =>
      expect(result.current.coverage).toEqual({ state: "unavailable", detail: "PRESENCE_READ_FAILED" }),
    );
    expect(result.current.sessions.size).toBe(0);
  });

  it("reports a FAILED READ as unavailable too — an empty set from a read that never happened", async () => {
    // The two failures collapse onto one state deliberately: the daemon
    // saying IT could not reach the substrate, and this page not reaching the
    // daemon, are the same fact to a reader.
    stub(null);
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.coverage?.state).toBe("unavailable"));
    expect(result.current.sessions.size).toBe(0);
  });

  it("carries a `stale` report through unchanged rather than flattening it", async () => {
    stub({
      sessions: [],
      meta: { sources: { fleet: { state: "stale", age_ms: 42_000 } }, complete: true },
    });
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.coverage).toEqual({ state: "stale", age_ms: 42_000 }));
  });

  it("reports NO coverage problem on a healthy fleet, and still returns the sessions", async () => {
    // Inverted case 1. A healthy read must say nothing at all.
    stub({
      sessions: [{ session_id: "s-1" }, { session_id: "s-2" }],
      meta: { sources: { fleet: { state: "ok" } }, complete: true },
    });
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.sessions.size).toBe(2));
    expect(result.current.sessions.has("s-1")).toBe(true);
    expect(result.current.coverage).toBeNull();
  });

  it("collects the missions live executions run under, skipping beats that name none", async () => {
    stub({
      sessions: [{ session_id: "e-1", mission_id: "m-1" }, { session_id: "e-2" }],
      meta: { sources: { fleet: { state: "ok" } }, complete: true },
    });
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.sessions.size).toBe(2));
    expect([...result.current.missions]).toEqual(["m-1"]);
  });

  it("reports NO coverage problem when the fleet substrate is switched OFF", async () => {
    // Inverted case 2, and the one most likely to be got wrong: a standalone
    // machine has no fleet substrate BY DESIGN — that is the default, not a
    // degradation, and warning about it would be the bug.
    stub({ sessions: [], meta: { sources: { fleet: { state: "off" } }, complete: true } });
    const { result } = renderHook(() => useLiveSessionIds(true), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.sessions.size).toBe(0));
    expect(result.current.coverage).toBeNull();
  });

  it("makes no claim while disabled — a replay neither polls nor reports coverage", async () => {
    // Inverted case 3. `enabled: false` is the #1800 P2 replay gate: no
    // request is made, so there is nothing to report either way.
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    const { result } = renderHook(() => useLiveSessionIds(false), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.sessions.size).toBe(0));
    expect(result.current.coverage).toBeNull();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
