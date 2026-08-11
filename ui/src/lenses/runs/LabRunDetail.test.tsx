import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { LabRunDetail } from "./LabRunDetail";

function renderDetail(dir: string, onBack: () => void = () => {}) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <LabRunDetail dir={dir} onBack={onBack} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

/**
 * `offset`-aware, like the real `/lab/run/events?offset=` endpoint
 * (`tail_lab_events`, `crates/darkmux-serve/src/lib.rs`): the seeded
 * `events` deliver ONCE (at `offset=0`), and every poll THEREAFTER — the
 * component's own self-rescheduling loop keeps polling on a timer, see
 * `LabRunDetail.tsx`'s own doc — gets an EMPTY `lines` array back, same as
 * a real server that has nothing new past what it already sent. A
 * non-offset-aware mock (returning the same seeded lines on every call)
 * would have the poll loop re-append the same events forever.
 */
function mockFetch(opts: {
  detail?: unknown;
  detailStatus?: number;
  events?: unknown[];
  finished?: boolean;
}) {
  const seeded = opts.events ?? [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url.startsWith("/lab/run/detail")) {
        return Promise.resolve(
          new Response(JSON.stringify(opts.detail ?? { dir: "d1", funnels: [], scores: null }), {
            status: opts.detailStatus ?? 200,
          }),
        );
      }
      if (url.startsWith("/lab/run/events")) {
        const offset = Number(new URL(url, "http://localhost").searchParams.get("offset") ?? "0");
        const lines = offset === 0 ? seeded : [];
        return Promise.resolve(
          new Response(JSON.stringify({ lines, next_offset: seeded.length, finished: !!opts.finished }), { status: 200 }),
        );
      }
      return Promise.resolve(new Response("not found", { status: 404 }));
    }),
  );
}

describe("LabRunDetail", () => {
  it("shows the pending state before the detail fetch resolves", () => {
    vi.stubGlobal("fetch", vi.fn(() => new Promise(() => {})));
    renderDetail("d1");
    expect(screen.getByRole("status", { name: /loading run d1/i })).toBeInTheDocument();
    expect(screen.getByText("‹ runs")).toBeInTheDocument();
  });

  it("renders a visible error on a non-2xx detail response", async () => {
    mockFetch({ detailStatus: 500 });
    renderDetail("d1");
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByRole("alert").textContent).toMatch(/500/);
  });

  it("shows a 'not started' pipeline stage and the live badge for a freshly-opened run", async () => {
    mockFetch({});
    renderDetail("d1");
    await waitFor(() => expect(screen.getByText("pipeline")).toBeInTheDocument());
    expect(screen.getByText("not started")).toBeInTheDocument();
    expect(screen.getByText("● live")).toBeInTheDocument();
    expect(screen.getByText(/try it yourself/i)).toBeInTheDocument();
    expect(screen.getByText(/darkmux lab eval --funnel/)).toBeInTheDocument();
    expect(screen.getByText(/no events yet/i)).toBeInTheDocument();
  });

  it("renders 'finished' when the envelope is present", async () => {
    mockFetch({ detail: { dir: "d1", funnels: [{ crew: "reviewer", mode: "auto", confirmed: 2, needs_check: 1, archived: 0 }], scores: null } });
    renderDetail("d1");
    await waitFor(() => expect(screen.getByText("finished")).toBeInTheDocument());
    expect(screen.getByText(/confirmed 2 · needs_check 1 · archived 0/)).toBeInTheDocument();
    expect(screen.getByText(/darkmux lab eval --funnel --roster-profile reviewer --exec-mode auto/)).toBeInTheDocument();
  });

  it("renders pipeline stages and the event feed from a real events poll", async () => {
    mockFetch({
      events: [{ ts: "2026-01-01T00:00:00Z", action: "step result", payload: { step_id: "bundle", items_in: 3, items_out: 3 } }],
    });
    renderDetail("d1");
    // "bundle" appears TWICE once the event lands — the pipeline stage name
    // AND the feed row's tag — `getAllByText`, not the singular form.
    await waitFor(() => expect(screen.getAllByText("bundle").length).toBe(2));
    // Same double-appearance as "bundle" — the pipeline stage's meta line
    // AND the feed row's text both say "3 → 3".
    expect(screen.getAllByText("3 → 3").length).toBe(2);
  });

  it("calls onBack when the '‹ runs' link is activated", async () => {
    mockFetch({});
    const onBack = vi.fn();
    renderDetail("d1", onBack);
    await waitFor(() => expect(screen.getByText("pipeline")).toBeInTheDocument());
    screen.getByText("‹ runs").click();
    expect(onBack).toHaveBeenCalledTimes(1);
  });

  it("resets accumulated events when the dir prop changes (a fresh drill-in)", async () => {
    mockFetch({ events: [{ ts: "1", action: "step result", payload: { step_id: "bundle" } }] });
    const { rerender } = renderDetail("d1");
    // "bundle" appears TWICE once the event lands — once as the pipeline
    // stage name, once as the feed row's tag — `getAllByText` (not the
    // singular form) is the correct query once a real page has more than
    // one legitimate match.
    await waitFor(() => expect(screen.getAllByText("bundle").length).toBeGreaterThan(0));

    mockFetch({ detail: { dir: "d2", funnels: [], scores: null }, events: [] });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    rerender(
      <QueryClientProvider client={queryClient}>
        <LabRunDetail dir="d2" onBack={() => {}} />
      </QueryClientProvider>,
    );
    // Wait for the SETTLED render, not just "· d2" (which is ALSO present
    // in the pending state's own stagehdr, so that alone would resolve too
    // early — before the reset-to-empty content has actually painted).
    await waitFor(() => expect(document.querySelector('[data-state="data"]')).not.toBeNull());
    expect(screen.getByText(/· d2/)).toBeInTheDocument();
    expect(screen.getByText("pipeline")).toBeInTheDocument();
    expect(screen.getByText("not started")).toBeInTheDocument();
  });
});
