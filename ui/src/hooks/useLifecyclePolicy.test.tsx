import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useLifecyclePolicy } from "./useLifecyclePolicy";
import { DEFAULT_POLICY } from "../lib/lifecycle";

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

const respond = (body: unknown) =>
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(body), { status: 200 }))));

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useLifecyclePolicy", () => {
  it("is the policy /runs publishes, once it answers", async () => {
    respond({ runs: [], generated_at_ms: 1, policy: { stale_after_ms: 120_000, budget_wait_grace_ms: 5_000 } });
    const { result } = renderHook(() => useLifecyclePolicy(), { wrapper: wrapper() });
    expect(result.current).toBe(DEFAULT_POLICY);
    await waitFor(() => expect(result.current).toEqual({ staleAfterMs: 120_000, budgetWaitGraceMs: 5_000 }));
  });

  it("is the default from a daemon that publishes none", async () => {
    respond({ runs: [], generated_at_ms: 1 });
    const { result } = renderHook(() => useLifecyclePolicy(), { wrapper: wrapper() });
    await waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalled());
    expect(result.current).toBe(DEFAULT_POLICY);
  });

  it("is one object per policy value, so a memo keyed on it holds across renders", async () => {
    respond({ runs: [], generated_at_ms: 1, policy: { stale_after_ms: 120_000, budget_wait_grace_ms: 5_000 } });
    const { result, rerender } = renderHook(() => useLifecyclePolicy(), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.staleAfterMs).toBe(120_000));
    const first = result.current;
    rerender();
    expect(result.current).toBe(first);
  });
});
