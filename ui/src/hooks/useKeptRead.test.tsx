import { describe, it, expect } from "vitest";
import { renderHook } from "@testing-library/react";
import type { FetchResult } from "../lib/fetcher";
import { useKeptRead } from "./useKeptRead";

type Body = { n: number };
const good = (n: number): FetchResult<Body> => ({ ok: true, data: { n } });
const failed: FetchResult<Body> = { ok: false, status: 500, message: "500 Internal Server Error" };

function track(initial: FetchResult<Body> | undefined) {
  return renderHook(({ data }) => useKeptRead(data), { initialProps: { data: initial } });
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
});
