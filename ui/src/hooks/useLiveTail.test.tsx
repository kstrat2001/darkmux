import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useLiveTail } from "./useLiveTail";
import { queryKeys } from "../lib/queryKeys";

/** A controllable mock `EventSource` — enough surface for `startFlowTail`
 * (`lib/sse.ts`) to drive, PLUS `onopen`/`onerror` simulation this packet's
 * handlers actually exercise (`sse.test.ts`'s own mock only needed
 * `onmessage`/`close`). */
class MockEventSource {
  url: string;
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;
  static instances: MockEventSource[] = [];

  constructor(url: string) {
    this.url = url;
    MockEventSource.instances.push(this);
  }

  close() {
    this.closed = true;
  }

  emit(data: string) {
    this.onmessage?.({ data } as MessageEvent<string>);
  }

  open() {
    this.onopen?.();
  }

  error() {
    this.onerror?.();
  }
}

function factory(url: string): EventSource {
  return new MockEventSource(url) as unknown as EventSource;
}

function wrapper(queryClient: QueryClient) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
  };
}

/** A `fetchJson`-shaped stub — records every call so tests can assert on
 * the `?since=` reconcile requests without a real network. */
function makeFetchImpl(responder: (path: string) => { ok: true; data: unknown } | { ok: false; status: number | null; message: string }) {
  const calls: string[] = [];
  const impl = (async (path: string) => {
    calls.push(path);
    return responder(path);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
  }) as any;
  return { impl, calls };
}

describe("useLiveTail", () => {
  beforeEach(() => {
    MockEventSource.instances = [];
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-08-09T12:00:00Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("opens a stream against today's date and appends SSE records into the flowTail cache", () => {
    const queryClient = new QueryClient();
    const { unmount } = renderHook(() => useLiveTail(true, { eventSourceFactory: factory, tickMs: 5000 }), {
      wrapper: wrapper(queryClient),
    });

    expect(MockEventSource.instances).toHaveLength(1);
    expect(MockEventSource.instances[0].url).toBe("/flow/2026-08-09/stream");

    act(() => {
      MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T12:00:01Z" }));
    });

    expect(queryClient.getQueryData(queryKeys.flowTail("2026-08-09"))).toEqual([
      { action: "dispatch.start", ts: "2026-08-09T12:00:01Z" },
    ]);

    unmount();
  });

  it("reports status live on open and reconnecting on error — never silently stays live over a dead stream", () => {
    const queryClient = new QueryClient();
    const { result, unmount } = renderHook(() => useLiveTail(true, { eventSourceFactory: factory, tickMs: 5000 }), {
      wrapper: wrapper(queryClient),
    });

    // Default before any open/error — matches legacy's `setBadges()` setting
    // "● live" text BEFORE `startLiveTail` even runs.
    expect(result.current).toBe("live");

    act(() => {
      MockEventSource.instances[0].error();
    });
    expect(result.current).toBe("reconnecting");

    act(() => {
      MockEventSource.instances[0].open();
    });
    expect(result.current).toBe("live");

    unmount();
  });

  it("does NOT reconcile on the very first connect, but DOES on every reconnect after (#1480 part 1)", async () => {
    const queryClient = new QueryClient();
    const { calls, impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    await act(async () => {
      MockEventSource.instances[0].open();
      await Promise.resolve();
    });
    expect(calls, "the FIRST open must not trigger a reconcile fetch").toHaveLength(0);

    await act(async () => {
      MockEventSource.instances[0].error();
      MockEventSource.instances[0].open();
      await Promise.resolve();
    });
    // Reconciles both [prevDate, date] — two fetches.
    expect(calls.some((c) => c.includes("/flow/2026-08-09?since="))).toBe(true);
    expect(calls.some((c) => c.includes("/flow/2026-08-08?since="))).toBe(true);

    unmount();
  });

  it("reconciles every 4th tick (~20s at the real 5s cadence) even without any reconnect", async () => {
    const queryClient = new QueryClient();
    const { calls, impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    // Ticks 1-3: no reconcile yet.
    for (let i = 0; i < 3; i++) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5000);
      });
    }
    expect(calls).toHaveLength(0);

    // 4th tick: the backstop fires.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(calls.some((c) => c.includes("/flow/2026-08-09?since="))).toBe(true);

    unmount();
  });

  it("merges reconciled records into the flowTail cache, deduped against what's already there", async () => {
    const queryClient = new QueryClient();
    const rec = { action: "dispatch.complete", ts: "2026-08-09T11:59:00Z" };
    const { impl } = makeFetchImpl((path) => (path.startsWith("/flow/2026-08-09?") ? { ok: true, data: [rec] } : { ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000 * 4);
    });

    expect(queryClient.getQueryData(queryKeys.flowTail("2026-08-09"))).toEqual([rec]);

    // A second reconcile round with the SAME record must not duplicate it.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000 * 4);
    });
    expect(queryClient.getQueryData(queryKeys.flowTail("2026-08-09"))).toEqual([rec]);

    unmount();
  });

  it("UTC date rollover: closes the old stream, invalidates flowDate for the new day pair, and opens a new stream", async () => {
    const queryClient = new QueryClient();
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");
    const { impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    expect(MockEventSource.instances).toHaveLength(1);
    expect(MockEventSource.instances[0].url).toBe("/flow/2026-08-09/stream");
    const firstStream = MockEventSource.instances[0];

    // Cross midnight UTC.
    vi.setSystemTime(new Date("2026-08-10T00:00:05Z"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });

    expect(firstStream.closed, "the old day's stream must be closed on rollover").toBe(true);
    expect(MockEventSource.instances).toHaveLength(2);
    expect(MockEventSource.instances[1].url).toBe("/flow/2026-08-10/stream");
    expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: queryKeys.flowDate("2026-08-10") });
    expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: queryKeys.flowDate("2026-08-09") });

    unmount();
  });

  it("a malformed SSE record is dropped, not thrown, and doesn't touch the cache", () => {
    const queryClient = new QueryClient();
    const { unmount } = renderHook(() => useLiveTail(true, { eventSourceFactory: factory, tickMs: 5000 }), {
      wrapper: wrapper(queryClient),
    });

    expect(() =>
      act(() => {
        MockEventSource.instances[0].emit("{not json");
      }),
    ).not.toThrow();
    expect(queryClient.getQueryData(queryKeys.flowTail("2026-08-09"))).toBeUndefined();

    unmount();
  });

  it("when disabled, never opens a stream or starts the ticker", async () => {
    const queryClient = new QueryClient();
    const { calls, impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(false, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    expect(MockEventSource.instances).toHaveLength(0);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000 * 8);
    });
    expect(calls).toHaveLength(0);
    expect(MockEventSource.instances).toHaveLength(0);

    unmount();
  });

  it("without EventSource support (no factory, jsdom has none), never crashes and stays reporting live", async () => {
    const queryClient = new QueryClient();
    expect(typeof globalThis.EventSource).toBe("undefined");

    const { result, unmount } = renderHook(() => useLiveTail(true, { tickMs: 5000 }), {
      wrapper: wrapper(queryClient),
    });

    expect(result.current).toBe("live");
    expect(MockEventSource.instances).toHaveLength(0);

    unmount();
  });

  it("unmount tears down the EventSource and clears the ticker (no further reconcile fetches)", async () => {
    const queryClient = new QueryClient();
    const { calls, impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );
    const stream = MockEventSource.instances[0];

    unmount();
    expect(stream.closed).toBe(true);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000 * 8);
    });
    expect(calls, "no reconcile fetch should fire after unmount").toHaveLength(0);
  });
});
