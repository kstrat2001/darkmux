// Playback parity audit (2026-09-24, findings #1-#4 at commit 02ce641d).
// Claim under test: at the SAME recorded instant, a replay renders exactly
// what a live viewer saw — same pill state, same "so far" clock, same fleet
// card. Live = system clock frozen at X, fetch returns records up to X,
// playhead=null. Playback = system clock X+6h, fetch returns the whole day,
// playhead=X. This file is the audit's own probe, committed as the
// regression test for change A ("one clock, one derivation").
process.env.TZ = "UTC";
import { describe, it, expect, vi, afterEach } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { render, act, cleanup } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SessionReplay } from "./SessionReplay";
import { buildFleetCard } from "../fleet/cards";
import { liveSessionSet, T, normalizeRecords } from "../../lib/flow";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
// `ui/src/lenses/catalog/` -> repo root is four levels up.
const REPO_ROOT = path.resolve(__dirname, "../../../..");
const SID = "darkmux-coding-refresh-rotation-1790218778756";
const ALL = readFileSync(path.join(REPO_ROOT, "tests/parity/corpus/pepper.jsonl"), "utf8")
  .trim()
  .split("\n")
  .map((l) => JSON.parse(l));
const upTo = (x: number) => ALL.filter((r) => !(T(r.ts) > x));

async function snap(records: unknown[], nowMs: number, playhead: number | null) {
  vi.useFakeTimers();
  vi.setSystemTime(nowMs);
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const el = (p: number | null) => (
    <QueryClientProvider client={qc}>
      <SessionReplay sessionId={SID} playhead={p} />
    </QueryClientProvider>
  );
  const { rerender } = render(el(playhead));
  await vi.waitFor(() => expect(document.querySelector(".session-run")).toBeInTheDocument());
  const pill = document.querySelector(".session-run__header .pill, .session-run__header [data-live]") as HTMLElement | null;
  const out = {
    pillText: pill?.textContent,
    dataLive: pill?.getAttribute("data-live"),
    title: pill?.getAttribute("title"),
    system: [...document.querySelectorAll('.metrics[data-scope="system"] .met')].map((e) => e.textContent).join(" | "),
    scope: document.querySelector('[data-testid="run-token-scope"]')?.textContent ?? null,
  };
  // One more real second of wall clock — the "so far" clock must advance in
  // BOTH modes (finding #2), not just live. At the live edge (`playhead ===
  // null`) that comes from the shared ticking clock alone. In playback,
  // this component only ever knows "now" as its `playhead` PROP (Change A:
  // `clockNow = playhead ?? wallNow`) — the shell's transport is what
  // supplies an ever-increasing one during real playback (the default
  // speed is exactly 1 recorded second per 1 real second), and there is no
  // transport mounted in this isolated component test to do that. So this
  // rerenders with the playhead advanced by the same 1000ms, standing in
  // for one real second of the transport's own play tick at that default
  // speed — an ADVANCE, not a SEEK, so it must not suppress any animation
  // (Change B).
  act(() => {
    vi.advanceTimersByTime(1000);
  });
  if (playhead !== null) {
    rerender(el(playhead + 1000));
  }
  (out as Record<string, unknown>).systemAfter1s = [...document.querySelectorAll('.metrics[data-scope="system"] .met')]
    .map((e) => e.textContent)
    .join(" | ");
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  return out;
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("parity: run page, live vs playback at the same recorded instant", () => {
  for (const [name, iso] of [
    ["mid-generation", "2026-09-24T03:00:20.500Z"],
    ["inside 15s thermal rest", "2026-09-24T03:01:40.000Z"],
  ] as const) {
    it(name, async () => {
      const X = Date.parse(iso);
      const live = await snap(upTo(X), X, null);
      const play = await snap(ALL, X + 6 * 3600_000, X);
      expect(play).toEqual(live);
    });
  }
});

describe("parity: fleet card, live vs playback at the same recorded instant", () => {
  it("mid-generation", () => {
    const X = Date.parse("2026-09-24T03:00:20.500Z");
    const m = "unknown";
    // live: the window as received by X; presence lists the session; t = window tMax (FleetLens live arm)
    const liveData = normalizeRecords(upTo(X));
    const NALL = normalizeRecords(ALL);
    const liveSet = liveSessionSet(liveData, new Set([SID]), X, true);
    const liveTMax = Math.max(...liveData.map((r) => T(r.ts)));
    const live = buildFleetCard(liveData, new Map(), null, liveSet, false, m, true, liveTMax);
    // playback: the whole day, no presence, playhead X (PlaybackLens -> FleetLens historical)
    const playSet = liveSessionSet(NALL, new Set(), X + 6 * 3600_000, false);
    const play = buildFleetCard(NALL, new Map(), null, playSet, false, m, false, X);
    const pick = (c: typeof live) => ({
      active: c.active,
      stat: c.stat,
      runsCount: c.runsCount,
      runsLabel: c.runsLabel,
      liveTokRate: c.liveTokRate == null ? null : Math.round(c.liveTokRate),
      liveTokStalled: c.liveTokStalled,
      runningSessionIds: c.runningSessionIds,
    });
    expect(pick(play)).toEqual(pick(live));
  });
});

describe("TOK/S tile: a state is a caption under the tube, never text inside it", () => {
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  // Operator, on a phone: "prompt" set at the number's size ran straight
  // through the ring. The number is the reading and sits inside; a state is
  // a caption about the reading and sits under the tube.
  it("waiting for the first token: empty tube center, 'reading prompt' caption", async () => {
    const start = T((ALL.find((r) => r.session_id === SID && /dispatch.start|dispatch start/.test(String(r.action))) as { ts: string }).ts);
    const at = start + 1_000;
    vi.useFakeTimers();
    vi.setSystemTime(at);
    const records = upTo(at);
    vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ records }), { status: 200 }))));
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <SessionReplay sessionId={SID} playhead={null} />
      </QueryClientProvider>,
    );
    await vi.waitFor(() => expect(document.querySelector('[data-testid="run-token-scope"]')).toBeInTheDocument());
    const tile = document.querySelector('[data-testid="run-token-scope"]')!;
    expect(tile.querySelector(".token-scope-n")).toBeNull();
    expect(tile.querySelector(".scopetile__state")?.textContent).toBe("reading prompt");
  });
});
