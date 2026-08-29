import { describe, it, expect, vi, afterEach } from "vitest";
import { render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useDaemonLoad, DAEMON_LOAD_POLL_MS } from "./useDaemonLoad";

/** A minimal harness so `useDaemonLoad`'s polling behavior — the thing
 * under test here — can be exercised directly, without a real dialog/drawer
 * in the tree. `enabled` is a plain prop so a test can flip it via
 * `rerender`, simulating "the surface opened" / "the surface closed". */
function Harness({ enabled }: { enabled: boolean }) {
  const load = useDaemonLoad(enabled);
  return <div data-act="load-now">{load?.now.cpu_pct ?? "none"}</div>;
}

function renderHarness(enabled: boolean) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={queryClient}>
      <Harness enabled={enabled} />
    </QueryClientProvider>,
  );
  return {
    ...utils,
    rerenderEnabled: (next: boolean) =>
      utils.rerender(
        <QueryClientProvider client={queryClient}>
          <Harness enabled={next} />
        </QueryClientProvider>,
      ),
  };
}

const LOAD_PAYLOAD = {
  schema_version: "1",
  generated_at_ms: 1,
  gather_ms: 1,
  limit_bytes: 1,
  limit_source: "test",
  pool: { capacity_bytes: 1, used_bytes: 1, available_bytes: 1, free_bytes: 1 },
  pressure: {
    swap_used_bytes: 0,
    compressor_bytes: 0,
    margin_percent: 90,
    red: false,
  },
  models: [],
  machine: {
    potential_bytes: 1,
    unpriced_models: 0,
    current_bytes: 1,
    state: "green",
  },
  attribution: "test",
  messages: [],
  cache_ttl_ms: 2000,
  load: {
    now: { cpu_pct: 12, mem_pct: 34, gpu_pct: 56, sampled_at_ms: 4000 },
    window: {
      cpu: { mean_pct: 10, p95_pct: 15, max_pct: 20 },
      mem: { mean_pct: 30, p95_pct: 35, max_pct: 40 },
      gpu: { mean_pct: 50, p95_pct: 55, max_pct: 60 },
      samples: 3,
      interval_ms: 2000,
      span_ms: 4000,
    },
    sampler_cost_ms_mean: 4.2,
  },
};

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
  document
    .querySelectorAll('meta[name^="darkmux-"]')
    .forEach((el) => el.remove());
});

describe("useDaemonLoad polling gate (#2107, #1833 warm-up finding)", () => {
  it("closed (enabled=false): schedules ZERO fetches", () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    renderHarness(false);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("open (enabled=true): fetches immediately, then polls every DAEMON_LOAD_POLL_MS", async () => {
    vi.useFakeTimers();
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        new Response(JSON.stringify(LOAD_PAYLOAD), { status: 200 }),
      );
    vi.stubGlobal("fetch", fetchMock);

    renderHarness(true);
    await vi.advanceTimersByTimeAsync(0); // flush the immediate first fetch
    expect(fetchMock).toHaveBeenCalledTimes(1);

    await vi.advanceTimersByTimeAsync(DAEMON_LOAD_POLL_MS);
    expect(fetchMock).toHaveBeenCalledTimes(2);

    await vi.advanceTimersByTimeAsync(DAEMON_LOAD_POLL_MS);
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("open then close: polling stops immediately — no further fetches after enabled flips false", async () => {
    vi.useFakeTimers();
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        new Response(JSON.stringify(LOAD_PAYLOAD), { status: 200 }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const { rerenderEnabled } = renderHarness(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    rerenderEnabled(false);
    const countAtClose = fetchMock.mock.calls.length;

    // Wait through several would-be poll intervals — a still-firing
    // interval would have called fetch 3+ more times by now.
    await vi.advanceTimersByTimeAsync(DAEMON_LOAD_POLL_MS * 3);
    expect(fetchMock).toHaveBeenCalledTimes(countAtClose);
  });

  it("re-opening after a close resumes polling (fetches again immediately)", async () => {
    vi.useFakeTimers();
    const fetchMock = vi
      .fn()
      .mockResolvedValue(
        new Response(JSON.stringify(LOAD_PAYLOAD), { status: 200 }),
      );
    vi.stubGlobal("fetch", fetchMock);

    const { rerenderEnabled } = renderHarness(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).toHaveBeenCalledTimes(1);

    rerenderEnabled(false);
    const countAtClose = fetchMock.mock.calls.length;
    await vi.advanceTimersByTimeAsync(DAEMON_LOAD_POLL_MS);
    expect(fetchMock).toHaveBeenCalledTimes(countAtClose);

    rerenderEnabled(true);
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock.mock.calls.length).toBeGreaterThan(countAtClose);
  });
});
