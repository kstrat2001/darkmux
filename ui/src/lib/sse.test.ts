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

    MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch start", ts: "2026-08-09T00:00:00Z" }));
    MockEventSource.instances[0].emit(JSON.stringify({ action: "dispatch complete", ts: "2026-08-09T00:00:05Z" }));

    expect(queryClient.getQueryData(queryKey)).toEqual([
      { action: "dispatch start", ts: "2026-08-09T00:00:00Z" },
      { action: "dispatch complete", ts: "2026-08-09T00:00:05Z" },
    ]);

    handle.close();
  });

  it("drops a malformed record instead of throwing", () => {
    const queryClient = new QueryClient();
    const queryKey = ["flow", "2026-08-09", "tail"];

    const handle = startFlowTail(queryClient, queryKey, "2026-08-09", (url) => new MockEventSource(url) as unknown as EventSource);

    expect(() => MockEventSource.instances[0].emit("{not json")).not.toThrow();
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
