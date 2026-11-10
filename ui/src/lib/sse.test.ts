import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { startFlowTail } from "./sse";

/** A minimal mock EventSource — just enough surface for `startFlowTail` to
 * drive (`onmessage` assignment + `close()`), plus a way for the test to fire
 * a message. Real `EventSource` doesn't exist in jsdom. */
class MockEventSource {
  url: string;
  onmessage: ((event: MessageEvent<string>) => void) | null = null;
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
}

/** (#2928) A mock that also carries named events, as a real EventSource does. */
class NamedEventSource extends MockEventSource {
  listeners = new Map<string, ((e: Event) => void)[]>();
  addEventListener(type: string, fn: (e: Event) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }
  emitNamed(type: string, data: string) {
    for (const fn of this.listeners.get(type) ?? []) fn({ data } as MessageEvent<string>);
  }
}

describe("(#2928) live frames", () => {
  beforeEach(() => {
    MockEventSource.instances = [];
  });

  it("a `live` frame reaches onLive and never the flow-tail cache", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];
    const got: string[] = [];
    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new NamedEventSource(url) as unknown as EventSource, {
      onLive: (d) => got.push(d),
    });
    const es = MockEventSource.instances[0] as NamedEventSource;
    es.emitNamed("live", '{"v":1}');
    expect(got).toEqual(['{"v":1}']);
    expect(queryClient.getQueryData(queryKey)).toBeUndefined();
    handle.close();
  });
});

describe("startFlowTail", () => {
  beforeEach(() => {
    MockEventSource.instances = [];
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("opens against /flow/:date/stream and appends parsed records to the query cache", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);

    expect(MockEventSource.instances).toHaveLength(1);
    expect(MockEventSource.instances[0].url).toBe("/flow/2026-08-09/stream");

    MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T00:00:00Z", execution_id: "exec-1" }));
    MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.complete", ts: "2026-08-09T00:00:05Z", execution_id: "exec-1" }));
    // Appends are batched per frame; closing writes whatever is waiting.
    handle.close();

    // Appended through the ingest boundary: each record carries its parsed time.
    expect(queryClient.getQueryData(queryKey)).toEqual([
      { action: "dispatch.start", ts: "2026-08-09T00:00:00Z", execution_id: "exec-1", tMs: Date.parse("2026-08-09T00:00:00Z") },
      { action: "dispatch.complete", ts: "2026-08-09T00:00:05Z", execution_id: "exec-1", tMs: Date.parse("2026-08-09T00:00:05Z") },
    ]);
  });

  // Every record used to be its own cache write, and every cache write
  // re-merges the whole 24h window downstream. Records that arrive inside
  // one frame are now one write.
  it("records arriving within one frame land in ONE cache write, after the frame", () => {
    vi.useFakeTimers();
    try {
      const queryClient = new QueryClient();
      const queryKey = ["flow", "2026-08-09", "tail"];
      const writes = vi.spyOn(queryClient, "setQueryData");
      const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
      for (let i = 0; i < 3; i++) {
        MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.turn.heartbeat", ts: `2026-08-09T00:00:0${i}Z`, session_id: "s1" }));
      }
      expect(writes).not.toHaveBeenCalled();
      vi.advanceTimersToNextFrame();
      expect(writes).toHaveBeenCalledTimes(1);
      expect((queryClient.getQueryData(queryKey) as unknown[]).length).toBe(3);
      handle.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("close() writes what is still waiting for its frame, so a day rollover loses nothing", () => {
    vi.useFakeTimers();
    try {
      const queryClient = new QueryClient();
      const queryKey = ["flow", "2026-08-09", "tail"];
      const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
      MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T23:59:59Z", session_id: "s1" }));
      handle.close();
      expect((queryClient.getQueryData(queryKey) as unknown[]).length).toBe(1);
      // And the cancelled frame does not write it a second time.
      vi.advanceTimersToNextFrame();
      expect((queryClient.getQueryData(queryKey) as unknown[]).length).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  // A hidden tab runs no animation frames, so a frame-only flush would hold
  // every record until the tab is shown again.
  it("a hidden tab still writes its records, on a timer instead of a frame", () => {
    vi.useFakeTimers();
    Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
    try {
      const queryClient = new QueryClient();
      const queryKey = ["flow", "2026-08-09", "tail"];
      const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
      vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 0); // frames never come
      MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T00:00:00Z", session_id: "s1" }));
      vi.advanceTimersByTime(1_000);
      expect((queryClient.getQueryData(queryKey) as unknown[] | undefined)?.length).toBe(1);
      handle.close();
    } finally {
      Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
      vi.useRealTimers();
    }
  });

  it("a tab going hidden writes what was waiting for a frame at once", () => {
    vi.useFakeTimers();
    try {
      const queryClient = new QueryClient();
      const queryKey = ["flow", "2026-08-09", "tail"];
      const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
      vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 0); // the frame never comes
      MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch.start", ts: "2026-08-09T00:00:00Z", session_id: "s1" }));
      Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
      document.dispatchEvent(new Event("visibilitychange"));
      expect((queryClient.getQueryData(queryKey) as unknown[] | undefined)?.length).toBe(1);
      handle.close();
    } finally {
      Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
      vi.useRealTimers();
    }
  });

  it("drops a malformed record instead of throwing", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);

    expect(() => MockEventSource.instances[0].emit("{not json")).not.toThrow();
    expect(queryClient.getQueryData(queryKey)).toBeUndefined();

    handle.close();
  });

  it("appends nothing for a line that parses but is not a record (the schema header, a bare value)", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);

    MockEventSource.instances[0].emit('{"_type":"schema"}');
    MockEventSource.instances[0].emit("42");
    expect(queryClient.getQueryData(queryKey)).toBeUndefined();

    handle.close();
  });

  it("reconnects (a fresh EventSource) when the tab becomes visible again", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];

    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
    expect(MockEventSource.instances).toHaveLength(1);
    const first = MockEventSource.instances[0];

    document.dispatchEvent(new Event("visibilitychange"));

    expect(MockEventSource.instances).toHaveLength(2);
    expect(first.closed).toBe(true);

    handle.close();
  });

  it("close() tears down the EventSource and the visibilitychange listener", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];
    const removeSpy = vi.spyOn(document, "removeEventListener");

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);
    handle.close();

    expect(MockEventSource.instances[0].closed).toBe(true);
    expect(removeSpy).toHaveBeenCalledWith("visibilitychange", expect.any(Function));
  });
});
