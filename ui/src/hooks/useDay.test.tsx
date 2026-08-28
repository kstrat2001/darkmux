import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useDay } from "./useDay";

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  return ({ children }: { children: ReactNode }) => <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}
function injectMeta(name: string, content: string) {
  const el = document.createElement("meta");
  el.setAttribute("name", name);
  el.setAttribute("content", content);
  document.head.appendChild(el);
}

/** (#2086) One hook for the loaded day, whichever kind of page. */
describe("useDay", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((m) => m.remove());
  });

  it("a live daemon route has no day: null records, not loading, no fetch", async () => {
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    const { result } = renderHook(() => useDay(null), { wrapper: wrapper() });
    expect(result.current).toEqual({ records: null, raw: null, loading: false, date: null, error: null });
    expect(fetch).not.toHaveBeenCalled();
  });

  it("a daemon playback route fetches /flow/<date> and normalizes it", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (String(url) === "/flow/2026-08-07") return Promise.resolve(new Response(JSON.stringify([{ ts: "2026-08-07T01:00:00Z", action: "dispatch.start", session_id: "s1" }]), { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    const { result } = renderHook(() => useDay("2026-08-07"), { wrapper: wrapper() });
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.date).toBe("2026-08-07");
    expect(result.current.records?.some((r) => r.action === "dispatch.start")).toBe(true);
    expect(result.current.error).toBeNull();
  });

  it("a daemon fetch failure is reported, not swallowed into an empty day", async () => {
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("nope", { status: 500 }))));
    const { result } = renderHook(() => useDay("2026-08-07"), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.records).toBeNull();
    expect(result.current.error?.status).toBe(500);
  });

  it("a static build replays its committed file on EVERY route, with the build-time day, never asking a daemon", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    injectMeta("darkmux-flow-date", "2026-08-26");
    const seen: string[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        seen.push(String(url));
        if (String(url) === "./demo-flow.jsonl") return Promise.resolve(new Response('{"ts":"2026-08-26T01:00:00Z","action":"dispatch.start","session_id":"s1"}\n', { status: 200 }));
        return Promise.resolve(new Response("not found", { status: 404 }));
      }),
    );
    // `requestedDate` is ignored on a static build: there is one file.
    const { result } = renderHook(() => useDay("2026-08-07"), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.date).toBe("2026-08-26");
    expect(result.current.records?.length).toBeGreaterThan(0);
    expect(seen).toEqual(["./demo-flow.jsonl"]);
  });

  it("a static build without a flow-date meta names the day from the file's first record", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response('{"ts":"2026-08-09T05:00:00Z","action":"dispatch.start"}\n', { status: 200 }))));
    const { result } = renderHook(() => useDay(null), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));
    expect(result.current.date).toBe("2026-08-09");
  });

  it("exposes the RAW day beside the normalized one: normalized carries the synthetic runtime row, raw does not", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(['{"ts":"2026-08-09T05:00:00Z","action":"dispatch.start","session_id":"s1"}', '{"ts":"2026-08-09T05:00:01Z","action":"dispatch.turn","session_id":"s1","payload":{"turn_seq":1}}'].join("\n") + "\n", { status: 200 }))));
    const { result } = renderHook(() => useDay(null), { wrapper: wrapper() });
    await waitFor(() => expect(result.current.loading).toBe(false));
    const runtime = (rs: { source?: string }[] | null) => (rs ?? []).filter((r) => r.source === "runtime").length;
    expect(runtime(result.current.raw)).toBe(0);
    expect(runtime(result.current.records)).toBe(1);
  });
});
