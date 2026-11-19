import { describe, it, expect } from "vitest";
import type { ReactNode } from "react";
import { renderHook } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { FetchResult } from "../lib/fetcher";
import { useKeptRead } from "./useKeptRead";

type Body = { n: number };
const good = (n: number): FetchResult<Body> => ({ ok: true, data: { n } });
const failed: FetchResult<Body> = { ok: false, status: 500, message: "500 Internal Server Error" };
const KEY = ["runs"] as const;

function track(initial: FetchResult<Body> | undefined, client = new QueryClient()) {
  const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  return renderHook(({ data }) => useKeptRead(KEY, data), { initialProps: { data: initial }, wrapper });
}

describe("useKeptRead", () => {
  it("before any answer there is nothing kept and no failure", () => {
    const { result } = track(undefined);
    expect(result.current).toEqual({ good: null, failure: null });
  });

  it("a failure with no earlier good read keeps nothing and names the failure", () => {
    const { result } = track(failed);
    expect(result.current.good).toBeNull();
    expect(result.current.failure).toEqual(failed);
  });

  it("a failed refetch keeps the last good read, alongside the failure", () => {
    const { result, rerender } = track(good(1));
    expect(result.current).toEqual({ good: { n: 1 }, failure: null });
    rerender({ data: failed });
    expect(result.current.good).toEqual({ n: 1 });
    expect(result.current.failure).toEqual(failed);
  });

  it("the next good read replaces the kept one and clears the failure", () => {
    const { result, rerender } = track(good(1));
    rerender({ data: failed });
    rerender({ data: good(2) });
    expect(result.current).toEqual({ good: { n: 2 }, failure: null });
  });

  it("the kept read survives a remount on the same QueryClient", () => {
    const client = new QueryClient();
    const first = track(good(1), client);
    first.rerender({ data: failed });
    first.unmount();
    const { result } = track(failed, client);
    expect(result.current.good).toEqual({ n: 1 });
    expect(result.current.failure).toEqual(failed);
  });

  it("a different QueryClient keeps nothing from another", () => {
    const first = track(good(1));
    first.unmount();
    const { result } = track(failed);
    expect(result.current.good).toBeNull();
  });

  it("each query key keeps its own read", () => {
    const client = new QueryClient();
    const wrapper = ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
    renderHook(() => useKeptRead(["a"], good(1)), { wrapper });
    const { result } = renderHook(() => useKeptRead(["b"], failed), { wrapper });
    expect(result.current.good).toBeNull();
  });
});
