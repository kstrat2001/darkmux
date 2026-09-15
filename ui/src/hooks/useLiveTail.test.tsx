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

    // (2026-09-06) Default before the stream has ever actually opened is
    // "reconnecting", not an optimistic "live" — a route that boots
    // already-reconnecting (or never manages to open at all) must not
    // falsely claim "live" for even a moment. `onOpen` below is what
    // flips it.
    expect(result.current).toBe("reconnecting");

    act(() => {
      MockEventSource.instances[0].open();
    });
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

  it("without EventSource support (no factory, jsdom has none), never crashes and stays reporting reconnecting — the polling backstop is not a live stream", async () => {
    // (2026-09-06) `canStream` is false here, so `openTail` is never
    // called and `onOpen` never fires — the status must stay pessimistic
    // for the hook's whole life, not just its first render. Before this
    // fix, an optimistic-"live" initial state had no path back to
    // "reconnecting" for a route where streaming is structurally
    // impossible, so it falsely reported "live" forever.
    const queryClient = new QueryClient();
    expect(typeof globalThis.EventSource).toBe("undefined");

    const { result, unmount } = renderHook(() => useLiveTail(true, { tickMs: 5000 }), {
      wrapper: wrapper(queryClient),
    });

    expect(result.current).toBe("reconnecting");
    expect(MockEventSource.instances).toHaveLength(0);

    unmount();
  });

  // ── (#2683) The silence watchdog ────────────────────────────────────────
  //
  // `EventSource` only reports a drop it NOTICES. A half-open TCP connection
  // — the host slept, the path went away without an RST — delivers no `error`
  // event ever, so every test above this block passes while the hook happily
  // claims `live` over a connection that will never deliver another byte.
  //
  // The trap in fixing it is the INVERTED case: a healthy fleet that nobody
  // is dispatching to emits no records for hours, and a watchdog keyed on
  // "records arrived" would repaint the header on every quiet afternoon. So
  // the signal is CONTACT — the daemon answered, over either transport — and
  // both directions are pinned below.

  it("goes reconnecting when a stream that never errors stops delivering AND the daemon stops answering", async () => {
    const queryClient = new QueryClient();
    // Every reconcile fails: this is a daemon that is simply gone. Note it
    // fails the way `fetchJson` really fails — a resolved `ok:false`, not a
    // rejection (see `lib/fetcher.ts`).
    const { impl } = makeFetchImpl(() => ({ ok: false, status: null, message: "network error" }));

    const { result, unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    act(() => {
      MockEventSource.instances[0].open();
    });
    expect(result.current).toBe("live");

    // 40s — two whole reconcile windows — with no message and no successful
    // fetch. The EventSource is never told to error, and never does.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(40_000);
    });

    expect(result.current, "a silent connection must stop being called live").toBe("reconnecting");
    // …and `reconnecting` is made TRUE rather than merely said: a half-open
    // EventSource never retries on its own, so the watchdog tears it down
    // and opens a real replacement.
    expect(MockEventSource.instances[0].closed).toBe(true);
    expect(MockEventSource.instances).toHaveLength(2);
    expect(MockEventSource.instances[1].url).toBe("/flow/2026-08-09/stream");

    // The replacement connecting is what flips it back.
    act(() => {
      MockEventSource.instances[1].open();
    });
    expect(result.current).toBe("live");

    unmount();
  });

  it("stays live on a healthy but IDLE connection — no records is normal, not a failure (the inverted case)", async () => {
    const queryClient = new QueryClient();
    // Reconciles succeed and return NOTHING, which is exactly what a quiet
    // fleet looks like all day.
    const { impl } = makeFetchImpl(() => ({ ok: true, data: [] }));

    const { result, unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    act(() => {
      MockEventSource.instances[0].open();
    });
    expect(result.current).toBe("live");

    // Three times the watchdog window, with not one record in it.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(120_000);
    });

    expect(result.current, "an idle fleet is not a dead daemon").toBe("live");
    expect(MockEventSource.instances, "and nothing was torn down and reopened").toHaveLength(1);

    unmount();
  });

  it("SSE traffic alone keeps it live — the stream answering IS contact, even with the backstop failing", async () => {
    const queryClient = new QueryClient();
    const { impl } = makeFetchImpl(() => ({ ok: false, status: 500, message: "500 Internal Server Error" }));

    const { result, unmount } = renderHook(
      () => useLiveTail(true, { eventSourceFactory: factory, fetchImpl: impl, tickMs: 5000 }),
      { wrapper: wrapper(queryClient) },
    );

    act(() => {
      MockEventSource.instances[0].open();
    });

    // 30s of silence — inside the window — then one record, then 30s more.
    // Neither gap alone reaches the timeout, so the status never flips.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    act(() => {
      MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T12:00:30Z" }));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });

    expect(result.current).toBe("live");
    expect(MockEventSource.instances).toHaveLength(1);

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
