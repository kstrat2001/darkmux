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
afterEach(() => {
  vi.unstubAllGlobals();
  // (#1869) A couple of the transport tests below use fake timers to drive
  // the play-loop's setInterval deterministically — reset unconditionally
  // so a fake-timers test never leaks its clock into the next test file.
  vi.useRealTimers();
});

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

/**
 * (#1801) `date === null` is how `route.ts` marks a static-demo playback
 * route (`isStaticBuild()` forced it, no server-assigned date exists). This
 * component must read `darkmux-flow-src` instead of `GET /flow/<date>` — a
 * static demo has no daemon behind it to serve that endpoint.
 */
describe("PlaybackLens — the static-demo flow-src route (#1801)", () => {
  function injectMeta(name: string, content: string) {
    const el = document.createElement("meta");
    el.setAttribute("name", name);
    el.setAttribute("content", content);
    document.head.appendChild(el);
  }

  afterEach(() => {
    document.head.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
  });

  function mockStaticSrc(body: string) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        const text = async (): Promise<string> => (String(url) === "./demo-flow.jsonl" ? body : "");
        if (String(url) === "./demo-flow.jsonl") return { ok: true, status: 200, text };
        // The live presence endpoints (and any /flow/<date> call) must never
        // be reached from the static branch — the same guard the "does NOT
        // consult the live presence endpoints" test above uses.
        return { ok: false, status: 500, json: async () => ({}), text };
      }),
    );
  }

  it("renders the fleet hero from the flow-src file, and never calls GET /flow/null", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    mockStaticSrc(
      [
        '{"ts":"2026-08-07T02:09:42Z","action":"dispatch.start","machine_uid":"m1"}',
        '{"ts":"2026-08-07T18:28:15Z","action":"dispatch.complete","machine_uid":"m1"}',
      ].join("\n"),
    );
    render(<PlaybackLens date={null} />, { wrapper: wrapper() });
    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());

    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(calls).toContain("./demo-flow.jsonl");
    // The regression this guards: `date ?? ""` encoding into a bare `/flow/`
    // request (or a literal `/flow/null`) instead of the date-keyed query
    // staying disabled entirely. NOT asserting the absence of every
    // `/flow/*` call — `FleetLens` itself unconditionally calls
    // `useFlowWindow` regardless of `historical` (its RESULT is discarded
    // when `records` is supplied, but the hook still fires its own
    // yesterday/today fetches — a pre-existing quirk this packet found but
    // did not fix; see the packet report).
    expect(calls).not.toContain("/flow/");
    expect(calls).not.toContain("/flow/null");
  });

  it("says the source is empty rather than rendering a zeroed hero, and never crashes", async () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    mockStaticSrc("");
    render(<PlaybackLens date={null} />, { wrapper: wrapper() });
    await waitFor(() => expect(screen.getByText(/no records in the static playback source/i)).toBeInTheDocument());
    expect(document.querySelector(".fleet-lens")).toBeNull();
  });

  it("shows a loading state before the static fetch resolves", () => {
    injectMeta("darkmux-flow-src", "./demo-flow.jsonl");
    mockStaticSrc('{"ts":"2026-08-07T00:00:00Z","action":"dispatch.start"}\n');
    render(<PlaybackLens date={null} />, { wrapper: wrapper() });
    expect(screen.getByRole("status", { name: /loading playback/i })).toBeInTheDocument();
  });

  // Inverted case: the component's OWN branch gate is `staticFlowSrc()`,
  // not the `date` prop — the "renders the fleet hero over the day's
  // records" test at the top of this file already covers `date="2026-08-07"`
  // with no meta injected (this file's default state, since every other
  // `describe` here never injects one), and asserts it still calls
  // `GET /flow/2026-08-07`. Restated here as its own assertion so this
  // describe block is self-contained proof the gate is two-sided, not just
  // an inference from file ordering.
  it("without the meta, a dated route still hits GET /flow/<date> as before", async () => {
    mockDay(DAY);
    render(<PlaybackLens date="2026-08-07" />, { wrapper: wrapper() });
    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
    const calls = (globalThis.fetch as unknown as { mock: { calls: unknown[][] } }).mock.calls.map((c) => String(c[0]));
    expect(calls.some((u) => u.startsWith("/flow/2026-08-07"))).toBe(true);
  });
});

/**
 * (#1869) The playback transport — the issue's own acceptance test plus
 * play/rewind/speed. `DAY_WITH_SESSION` is deliberately ONE session with a
 * clean gap between its token telemetry (00:00:01) and its close edge
 * (01:00:00) — the midpoint of the day (value=50 on the range, since
 * `tMin`=00:00:00 and `tMax`=01:00:00 here) lands at 00:30:00: after the
 * telemetry, before the close.
 */
describe("PlaybackLens — the playback transport (#1869)", () => {
  const DAY_WITH_SESSION = [
    {
      ts: "2026-08-07T00:00:00.000Z",
      machine_uid: "u1",
      machine_id: "MacBook-Pro",
      session_id: "s1",
      action: "dispatch.start",
      handle: "coder",
    },
    {
      ts: "2026-08-07T00:00:01.000Z",
      machine_uid: "u1",
      session_id: "s1",
      category: "telemetry",
      source: "tokens",
      payload: { turn_seq: 1, prompt_tokens: 500, completion_tokens: 100, total_tokens: 600 },
    },
    {
      ts: "2026-08-07T01:00:00.000Z",
      machine_uid: "u1",
      session_id: "s1",
      action: "dispatch.complete",
      payload: { total_tokens: 600 },
    },
  ];

  it("moving the playhead back past a session's close edge flips its bar to in-flight and drops its tokens from the hero", async () => {
    mockDay(DAY_WITH_SESSION);
    // (#2071) The shell owns the transport now; this lens renders at the
    // playhead it is handed. `null` is the un-scrubbed default (the day's
    // true max); the midpoint lands after the telemetry, before the close.
    const { rerender } = render(<PlaybackLens date="2026-08-07" playhead={null} />, { wrapper: wrapper() });
    await waitFor(() => expect(document.querySelector(".fleet-lens")).toBeTruthy());
    await waitFor(() => expect(screen.getByText("local tokens").previousSibling?.textContent).toBe("600"));
    expect(document.querySelector(".sbar")).toHaveClass("done");
    expect(screen.getByText("unattributed").previousSibling?.textContent).toBe("0");
    expect(screen.getByText("unattributed").parentElement!.className).toMatch(/\bzero\b/);

    rerender(<PlaybackLens date="2026-08-07" playhead={Date.parse("2026-08-07T00:30:00.000Z")} />);
    await waitFor(() => expect(document.querySelector(".sbar")).toHaveClass("run"));
    expect(screen.getByText("local tokens").previousSibling?.textContent).toBe("0");
    expect(screen.getByText("unattributed").previousSibling?.textContent).toBe("600");
  });
});
