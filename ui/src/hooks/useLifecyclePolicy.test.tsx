import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { __resetLifecyclePolicy, useLifecyclePolicy } from "./useLifecyclePolicy";
import { DEFAULT_POLICY } from "../lib/lifecycle";

const respond = (body: unknown, status = 200) => {
  const fetch = vi.fn(() => Promise.resolve(new Response(JSON.stringify(body), { status })));
  vi.stubGlobal("fetch", fetch);
  return fetch;
};

beforeEach(() => __resetLifecyclePolicy());
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("useLifecyclePolicy", () => {
  it("is the policy /health publishes, read from the cheap route, not /runs", async () => {
    const fetch = respond({ darkmux_version: "x", lifecycle_policy: { stale_after_ms: 120_000, budget_wait_grace_ms: 5_000 } });
    const { result } = renderHook(() => useLifecyclePolicy());
    expect(result.current).toBe(DEFAULT_POLICY);
    await waitFor(() => expect(result.current).toEqual({ staleAfterMs: 120_000, budgetWaitGraceMs: 5_000 }));
    expect(fetch.mock.calls.map((c) => String((c as unknown[])[0]))).toEqual(["/health"]);
  });

  it("reads it once per page, however many surfaces ask", async () => {
    const fetch = respond({ lifecycle_policy: { stale_after_ms: 120_000, budget_wait_grace_ms: 5_000 } });
    const a = renderHook(() => useLifecyclePolicy());
    renderHook(() => useLifecyclePolicy());
    await waitFor(() => expect(a.result.current.staleAfterMs).toBe(120_000));
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("says once, on the console, when the daemon publishes no policy, and keeps the default", async () => {
    respond({ darkmux_version: "old" });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { result } = renderHook(() => useLifecyclePolicy());
    await waitFor(() => expect(warn).toHaveBeenCalledTimes(1));
    expect(String(warn.mock.calls[0][0])).toMatch(/no lifecycle policy/);
    expect(result.current).toBe(DEFAULT_POLICY);
  });

  it("says so for a failed read too", async () => {
    respond({ error: "down" }, 500);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    renderHook(() => useLifecyclePolicy());
    await waitFor(() => expect(warn).toHaveBeenCalledTimes(1));
  });
});
