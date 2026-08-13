import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { PlaybackLens } from "./PlaybackLens";

/**
 * (#1800 P2) This file used to assert the "lens not ported yet" placeholder —
 * an honest test of honest behavior at the time. The lens now RENDERS, so
 * these assert the real thing instead.
 *
 * The golden `tests/parity/goldens/playback-date.txt` is the byte-level spec;
 * these cover the states a corpus-driven parity run does not reach (pending,
 * transport failure, empty day) plus the one property that is easy to get
 * wrong and invisible when wrong: a replay must not assert LIVE presence.
 */

function wrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

const DAY = [
  { ts: "2026-08-07T02:09:42Z", action: "dispatch.start", machine_uid: "m1" },
  { ts: "2026-08-07T18:28:15Z", action: "dispatch.complete", machine_uid: "m1" },
];

function mockDay(records: unknown[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      // Only the day fetch returns records; the LIVE presence endpoints must
      // never be consulted on a replay, so they 500 loudly if they are.
      if (String(url).startsWith("/flow/")) {
        // A BARE ARRAY — what `lib.rs`'s `flow_handler` actually returns.
        // An earlier version of this mock returned `{records}` here, which is
        // `/flow-session/`'s shape, and was green while the decode was broken.
        return { ok: true, status: 200, json: async () => records };
      }
      return { ok: false, status: 500, json: async () => ({}) };
    }),
  );
}

beforeEach(() => mockDay(DAY));
afterEach(() => vi.unstubAllGlobals());

describe("PlaybackLens", () => {
  it("renders the fleet hero over the day's records, not a placeholder", async () => {
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    await waitFor(() => expect(screen.queryByText(/lens not ported yet/i)).not.toBeInTheDocument());
    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
  });

  it("does NOT consult the live presence endpoints on a replay", async () => {
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    // Asserting today's presence over a replayed day is the "confidently
    // wrong" failure FleetLens's own doc names — a machine reading idle
    // because it is idle NOW, over records from a day it was busy.
    expect(calls.some((u) => u.includes("/fleet/machines/live"))).toBe(false);
    expect(calls.some((u) => u.includes("/fleet/sessions/live"))).toBe(false);
  });

  it("shows a loading state before the day resolves", () => {
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    expect(screen.getByRole("status", { name: /loading 2026-08-07/i })).toBeInTheDocument();
  });

  it("names the failure instead of rendering an empty hero", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 503, json: async () => ({}) }));
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    await waitFor(() => expect(screen.getByRole("alert")).toBeInTheDocument());
    expect(screen.getByText(/couldn't reach \/flow\/2026-08-07/i)).toBeInTheDocument();
  });

  it("says the day is empty rather than rendering a zeroed hero", async () => {
    mockDay([]);
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    await waitFor(() => expect(screen.getByText(/no records for 2026-08-07/i)).toBeInTheDocument());
    // A zeroed hero would read as "this day had no activity" with the same
    // confidence as a real empty day — but it would also render that way for
    // a day the fetch mangled. Distinct state, distinct render.
    expect(document.querySelector(".fleet-lens")).toBeNull();
  });
});
