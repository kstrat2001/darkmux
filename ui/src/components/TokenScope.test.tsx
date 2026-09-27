import { describe, it, expect, vi, afterEach } from "vitest";
import { render, act } from "@testing-library/react";
import { TokenScope, waveAt } from "./TokenScope";
import { maxOf, minOf } from "../lib/numbers";

// (#2890) What the operator SEES inside the tube, per state. The canvas
// itself cannot draw under jsdom (no 2D context), so these assert on the
// center overlay and the state the bezel exposes to CSS.

function center(container: HTMLElement) {
  return container.querySelector(".token-scope-center");
}

describe("TokenScope center, per state (#2890)", () => {
  it("TOOLS shows a glowing icon for the tool and no text", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="read" />);
    const icon = container.querySelector("[data-tool-icon]");
    expect(icon?.getAttribute("data-tool-icon")).toBe("read");
    expect(center(container)?.textContent).toBe("");
  });

  it("TOOLS falls back to the gear for any other tool or no name yet", () => {
    const a = render(<TokenScope tokensPerSec={0} size="card" state="tools" toolName="create_mod" />);
    expect(a.container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("other");
    const b = render(<TokenScope tokensPerSec={0} size="card" state="tools" />);
    expect(b.container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("other");
  });

  it("TOOLS ignores a center label: the icon is the whole message", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="bash" centerLabel="12" />);
    expect(container.querySelector(".token-scope-n")).toBeNull();
    expect(container.querySelector(".token-scope-screen")?.textContent).toBe("");
    expect(container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("bash");
  });

  it("(#2890) PROMPT shows the brain for the whole phase, even if handed a center", () => {
    const a = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel={null} />);
    expect(a.container.querySelector("[data-scope-icon]")?.getAttribute("data-scope-icon")).toBe("brain");
    expect(a.container.querySelector(".token-scope-n")).toBeNull();
    const b = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel="36k" centerUnit="processing" />);
    expect(b.container.querySelector("[data-scope-icon]")?.getAttribute("data-scope-icon")).toBe("brain");
    expect(b.container.querySelector(".token-scope-n")).toBeNull();
    expect(b.container.querySelector(".token-scope-screen")?.textContent).toBe("");
  });

  it("(#2889, #2890) TOOLS while the call is GENERATED: a wrench, not the tool's icon, with \"tool gen\" under it", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="edit" toolWriting centerUnit="tool gen" />);
    // Not the edit tool's pencil: beside "writing" it read as an edit in progress.
    expect(container.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("toolgen");
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-writing")).toBe("true");
    const cap = container.querySelector(".token-scope-u--tools");
    expect(cap?.textContent).toBe("tool gen");
    expect(cap?.getAttribute("data-on")).toBe("true");
  });

  it("(#2890) generated -> run: the wrench and its caption crossfade out, the tool's own icon comes in", () => {
    const { container, rerender } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="edit" toolWriting centerUnit="tool gen" />);
    rerender(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="edit" />);
    const ghost = container.querySelector(".token-scope-fade--out");
    expect(ghost?.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("toolgen");
    expect(ghost?.querySelector(".token-scope-u--tools")?.textContent).toBe("tool gen");
    const live = [...container.querySelectorAll(".token-scope-center")].find((el) => !el.closest(".token-scope-fade--out"));
    expect(live?.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("edit");
    expect(live?.querySelector(".token-scope-u--tools")).toBeNull();
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-writing")).toBe("false");
  });

  it("(#2915) PROMPT while compacting: the utility treatment, its word in the center, no brain", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" utility centerLabel={null} centerUnit="compacting" />);
    const bezel = container.querySelector(".token-scope-bezel");
    expect(bezel?.getAttribute("data-tone")).toBe("utility");
    expect(bezel?.getAttribute("data-utility")).toBe("true");
    expect(container.querySelector("[data-scope-icon]")).toBeNull();
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("compacting");
    expect(container.querySelector(".token-scope-n")).toBeNull();
  });

  it("(#2915) the utility treatment is PROMPT's only: ignored in any other state", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="stalled" utility />);
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-tone")).toBe("stalled");
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-utility")).toBe("false");
  });

  it("no brain outside PROMPT", () => {
    for (const state of ["generating", "tools", "rest", "stalled", "finished", "idle", "nosignal"] as const) {
      const { container } = render(<TokenScope tokensPerSec={0} size="tile" state={state} />);
      expect(container.querySelector("[data-scope-icon]")).toBeNull();
    }
  });

  it("GEN shows the rate with its unit on the tile", () => {
    const { container } = render(<TokenScope tokensPerSec={180} size="tile" state="generating" centerLabel="180" centerUnit="tok/s" />);
    expect(container.querySelector(".token-scope-n")?.textContent).toBe("180");
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("tok/s");
    expect(container.querySelector("[data-tool-icon]")).toBeNull();
  });

  it("a carried rate still dims the number", () => {
    const { container } = render(<TokenScope tokensPerSec={90} size="tile" state="generating" centerLabel="90" centerCarried />);
    expect(container.querySelector(".token-scope-n")?.getAttribute("data-carried")).toBe("true");
  });

  it("FINISHED keeps the average in the center with its unit", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="finished" centerLabel="64" centerUnit="avg tok/s" />);
    expect(container.querySelector(".token-scope-n")?.textContent).toBe("64");
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("avg tok/s");
    expect(container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("finished");
  });

  it("REST, STALL and NO SIGNAL have an empty center with no label", () => {
    for (const state of ["rest", "stalled", "nosignal"] as const) {
      const { container, unmount } = render(<TokenScope tokensPerSec={0} size="tile" state={state} />);
      expect(center(container)).toBeNull();
      unmount();
    }
  });

  it("without a `state` prop, the existing props still pick the state", () => {
    const stalled = render(<TokenScope tokensPerSec={0} size="card" stalled tone="stalled" />);
    expect(stalled.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("stalled");
    const resting = render(<TokenScope tokensPerSec={0} size="card" resting tone="rest" />);
    expect(resting.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("rest");
    const tools = render(<TokenScope tokensPerSec={0} size="card" tone="tools" />);
    expect(tools.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("tools");
    const none = render(<TokenScope tokensPerSec={0} size="card" tone="none" />);
    expect(none.container.querySelector(".token-scope-bezel")?.getAttribute("data-state")).toBe("idle");
  });
});

describe("TokenScope center crossfade (#2890)", () => {
  it("keeps the old center fading out for one short crossfade at a change of kind", () => {
    vi.useFakeTimers();
    try {
      const { container, rerender } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="read" />);
      rerender(<TokenScope tokensPerSec={120} size="tile" state="generating" centerLabel="120" centerUnit="tok/s" />);
      const out = container.querySelector(".token-scope-fade--out");
      expect(out?.querySelector("[data-tool-icon]")?.getAttribute("data-tool-icon")).toBe("read");
      expect(container.querySelector(".token-scope-fade--in .token-scope-u")?.textContent).toBe("tok/s");
      act(() => {
        vi.advanceTimersByTime(250);
      });
      expect(container.querySelector(".token-scope-fade--out")).toBeNull();
      expect(container.querySelector("[data-tool-icon]")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("a number changing within one state does not fade", () => {
    const { container, rerender } = render(<TokenScope tokensPerSec={100} size="tile" state="generating" centerLabel="100" centerUnit="tok/s" />);
    rerender(<TokenScope tokensPerSec={140} size="tile" state="generating" centerLabel="140" centerUnit="tok/s" />);
    expect(container.querySelector(".token-scope-fade--out")).toBeNull();
  });
});

// (#2890 review) The canvas lifecycle. jsdom returns null from
// `getContext("2d")`, so without a stub the whole effect returns early and
// none of the loop / observer / listener wiring runs in a test at all.
describe("TokenScope canvas lifecycle (#2890)", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    delete (document as unknown as { hidden?: boolean }).hidden;
  });

  function harness() {
    // A 2D context whose every method is a chainable no-op (a gradient's
    // `addColorStop` included); property writes are kept.
    const store: Record<string | symbol, unknown> = {};
    const ctx: unknown = new Proxy(store, {
      get: (t, k) => (k in t ? t[k] : () => ctx),
      set: (t, k, v) => {
        t[k] = v;
        return true;
      },
    });
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx as CanvasRenderingContext2D);
    // Motion allowed: the loop only runs when reduced motion is off.
    vi.spyOn(window, "matchMedia").mockImplementation(
      (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {} }) as unknown as MediaQueryList,
    );
    let nextId = 0;
    const frames = new Map<number, FrameRequestCallback>();
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
      nextId += 1;
      frames.set(nextId, cb);
      return nextId;
    });
    const caf = vi.spyOn(window, "cancelAnimationFrame").mockImplementation((id) => {
      frames.delete(id);
    });
    const observe = vi.fn();
    const disconnect = vi.fn();
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe = observe;
        disconnect = disconnect;
        unobserve() {}
      },
    );
    let hidden = false;
    Object.defineProperty(document, "hidden", { configurable: true, get: () => hidden });
    const add = vi.spyOn(document, "addEventListener");
    const remove = vi.spyOn(document, "removeEventListener");
    const setHidden = (h: boolean) => {
      hidden = h;
      act(() => {
        document.dispatchEvent(new Event("visibilitychange"));
      });
    };
    const visibilityHandler = () => add.mock.calls.find((c) => c[0] === "visibilitychange")?.[1];
    return { raf, caf, frames, observe, disconnect, add, remove, setHidden, visibilityHandler };
  }

  it("starts the loop and observes its box on mount", () => {
    const h = harness();
    const { container } = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    expect(h.raf).toHaveBeenCalledTimes(1);
    expect(h.observe).toHaveBeenCalledWith(container.querySelector("canvas"));
    expect(h.visibilityHandler()).toBeTypeOf("function");
    // A frame schedules the next one: it is a loop, not a single draw.
    const [[id, cb]] = [...h.frames];
    h.frames.delete(id); // the browser consumes a frame when it fires it
    act(() => cb(16));
    expect(h.raf).toHaveBeenCalledTimes(2);
    expect(h.frames.size).toBe(1);
  });

  it("hiding the document stops the loop; showing it again restarts it", () => {
    const h = harness();
    render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    h.frames.clear();
    const firstId = h.raf.mock.results[0].value as number;
    h.setHidden(true);
    expect(h.caf).toHaveBeenCalledWith(firstId);
    h.setHidden(false);
    expect(h.raf).toHaveBeenCalledTimes(2);
  });

  it("unmounting cancels the pending frame, disconnects the observer, and removes the listener", () => {
    const h = harness();
    const { unmount } = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    const handler = h.visibilityHandler();
    const pending = h.raf.mock.results[0].value as number;
    unmount();
    expect(h.caf).toHaveBeenCalledWith(pending);
    expect(h.frames.size).toBe(0);
    expect(h.disconnect).toHaveBeenCalled();
    expect(h.remove).toHaveBeenCalledWith("visibilitychange", handler);
  });
});

describe("(#2890) thinking", () => {
  it("marks the bezel while generating and thinking, never in another state", () => {
    const gen = render(<TokenScope tokensPerSec={70} size="tile" state="generating" thinking centerLabel="70" centerUnit="tok/s" />);
    expect(gen.container.querySelector(".token-scope-bezel")?.getAttribute("data-thinking")).toBe("true");
    // The words and number are unchanged: thinking is color, not text.
    expect(gen.container.querySelector(".token-scope-n")?.textContent).toBe("70");
    expect(gen.container.querySelector(".token-scope-u")?.textContent).toBe("tok/s");
    const tools = render(<TokenScope tokensPerSec={0} size="tile" state="tools" thinking />);
    expect(tools.container.querySelector(".token-scope-bezel")?.getAttribute("data-thinking")).toBe("false");
  });
});


describe("(#2890) waveAt: whole lobes, no seam, the same all the way round", () => {
  it("closes after a full turn, settled or mid-crossfade", async () => {
    const { waveAt } = await import("./TokenScope");
    for (const [from, to, mix] of [[5, 5, 1], [5, 6, 0.4], [3, 8, 0.9]] as const) {
      expect(waveAt(0, from, to, mix, 1.3)).toBeCloseTo(waveAt(Math.PI * 2, from, to, mix, 1.3), 9);
    }
  });
  it("a settled wave looks the same on every side (rotational symmetry)", async () => {
    const { waveAt } = await import("./TokenScope");
    const n = 5;
    for (const t of [0.2, 1.1, 2.7]) {
      expect(waveAt(t, n, n, 1, 0.7, false)).toBeCloseTo(waveAt(t + (2 * Math.PI) / n, n, n, 1, 0.7, false), 9);
    }
  });
});

describe("(#2890) a unit on its own", () => {
  it("renders the unit with no number (idle)", () => {
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="idle" centerUnit="idle" />);
    expect(container.querySelector(".token-scope-n")).toBeNull();
    expect(container.querySelector(".token-scope-u")?.textContent).toBe("idle");
  });
});

describe("(#2911) waveAt: the harmonic shimmer, stated", () => {
  /** Each lobe's peak height, for a settled `n`-lobe wave at `phase`. */
  function lobePeaks(n: number, phase: number, harmonic: boolean): number[] {
    const peaks: number[] = [];
    for (let j = 0; j < n; j++) {
      // The base wave peaks at k·t − phase = π/2 + 2πj; look ±half a lobe
      // around it.
      const center = (Math.PI / 2 + phase + 2 * Math.PI * j) / n;
      let peak = -Infinity;
      for (let s = -200; s <= 200; s++) peak = Math.max(peak, waveAt(center + (s / 200) * (Math.PI / n), n, n, 1, phase, harmonic));
      peaks.push(peak);
    }
    return peaks;
  }
  it("without the harmonic every lobe is the same height (the symmetry the other tests prove)", () => {
    for (const p of lobePeaks(5, 0.7, false)) expect(p).toBeCloseTo(1, 6);
  });
  it("with the harmonic the lobes vary in height by up to ±12% (the intended shimmer), and are not all equal", () => {
    for (const phase of [0, 0.7, 2.9]) {
      const peaks = lobePeaks(5, phase, true);
      for (const p of peaks) {
        expect(p).toBeGreaterThanOrEqual(1 - 0.12 - 1e-6);
        expect(p).toBeLessThanOrEqual(1 + 0.12 + 1e-6);
      }
      expect((maxOf(peaks) ?? NaN) - (minOf(peaks) ?? NaN)).toBeGreaterThan(0.05);
    }
  });
});

describe("(#2911) reduced motion is followed at runtime, not read once at mount", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });
  it("a runtime switch to reduced motion stops the loop and draws one static frame; switching back restarts it", () => {
    const calls: string[] = [];
    const ctx: unknown = new Proxy({} as Record<string | symbol, unknown>, {
      get: (t, k) => (k in t ? t[k] : (...args: unknown[]) => { if (k === "fillRect" && args.length === 4) calls.push("fillRect"); return ctx; }),
      set: (t, k, v) => { t[k] = v; return true; },
    });
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx as CanvasRenderingContext2D);
    let matches = false;
    const listeners = new Set<() => void>();
    vi.spyOn(window, "matchMedia").mockImplementation(
      (q: string) =>
        ({
          get matches() { return matches; },
          media: q,
          addEventListener: (_: string, cb: () => void) => listeners.add(cb),
          removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
        }) as unknown as MediaQueryList,
    );
    const frames = new Map<number, FrameRequestCallback>();
    let nextId = 0;
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => { nextId += 1; frames.set(nextId, cb); return nextId; });
    vi.spyOn(window, "cancelAnimationFrame").mockImplementation((id) => { frames.delete(id); });
    vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} unobserve() {} });
    Object.defineProperties(HTMLCanvasElement.prototype, {
      clientWidth: { configurable: true, get: () => 200 },
      clientHeight: { configurable: true, get: () => 200 },
    });
    try {
      render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
      expect(frames.size).toBe(1);
      const drawsBefore = calls.length;
      matches = true;
      act(() => { for (const l of listeners) l(); });
      // The loop is gone and exactly one settled frame was painted.
      expect(frames.size).toBe(0);
      expect(calls.length).toBe(drawsBefore + 1);
      matches = false;
      act(() => { for (const l of listeners) l(); });
      expect(frames.size).toBe(1);
    } finally {
      delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientWidth;
      delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientHeight;
    }
  });

  it("under reduced motion a change of state or rate redraws the static frame once, and nothing else does", () => {
    const calls: string[] = [];
    const ctx: unknown = new Proxy({} as Record<string | symbol, unknown>, {
      get: (t, k) => (k in t ? t[k] : (...args: unknown[]) => { if (k === "fillRect" && args.length === 4) calls.push("fillRect"); return ctx; }),
      set: (t, k, v) => { t[k] = v; return true; },
    });
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx as CanvasRenderingContext2D);
    vi.spyOn(window, "matchMedia").mockImplementation(
      (q: string) =>
        ({ matches: true, media: q, addEventListener: () => {}, removeEventListener: () => {} }) as unknown as MediaQueryList,
    );
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 1);
    vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} unobserve() {} });
    Object.defineProperties(HTMLCanvasElement.prototype, {
      clientWidth: { configurable: true, get: () => 200 },
      clientHeight: { configurable: true, get: () => 200 },
    });
    try {
      const r = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
      const afterMount = calls.length;
      expect(afterMount).toBeGreaterThan(0);
      // Same props: no repaint.
      r.rerender(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
      expect(calls.length).toBe(afterMount);
      // A new rate: one repaint.
      r.rerender(<TokenScope tokensPerSec={20} size="tile" state="generating" />);
      expect(calls.length).toBe(afterMount + 1);
      // A new state: one more.
      r.rerender(<TokenScope tokensPerSec={0} size="tile" state="stalled" />);
      expect(calls.length).toBe(afterMount + 2);
      // And never a loop.
      expect(raf).not.toHaveBeenCalled();
    } finally {
      delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientWidth;
      delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientHeight;
    }
  });
});

// (#2961) REST's seconds hand, drawn and counted from the rest's end time on
// the page clock.
describe("(#2961) REST's seconds hand", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientWidth;
    delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientHeight;
  });

  /** A 200px canvas whose `arc` and `ellipse` calls are recorded, the page's
   *  monotonic clock pinned at 0, and frames fired by hand. */
  function harness(reduced: boolean) {
    const arcs: { x: number; y: number; r: number }[] = [];
    let ellipses = 0;
    let lineTos = 0;
    // The alpha of every stroked ellipse (the drawn circle's segments and its
    // glow stroke), in drawing order.
    let strokes: { alpha: number; a0: number; a1: number; blur: number }[] = [];
    let path: { a0: number; a1: number } | null = null;
    const ctx: unknown = new Proxy({} as Record<string | symbol, unknown>, {
      get: (t, k) =>
        k in t
          ? t[k]
          : (...a: number[]) => {
              if (k === "arc") arcs.push({ x: a[0], y: a[1], r: a[2] });
              if (k === "ellipse") {
                ellipses += 1;
                path = { a0: a[5], a1: a[6] };
              }
              if (k === "lineTo") lineTos += 1;
              if (k === "beginPath") path = null;
              if (k === "stroke" && path) {
                const m = /rgba\([^)]*,([\d.e-]+)\)$/.exec(String(t.strokeStyle));
                strokes.push({ alpha: m ? Number(m[1]) : NaN, ...path, blur: Number(t.shadowBlur ?? 0) });
              }
              return ctx;
            },
      set: (t, k, v) => {
        t[k] = v;
        return true;
      },
    });
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(ctx as CanvasRenderingContext2D);
    vi.spyOn(window, "matchMedia").mockImplementation(
      (q: string) => ({ matches: reduced, media: q, addEventListener() {}, removeEventListener() {} }) as unknown as MediaQueryList,
    );
    vi.spyOn(performance, "now").mockReturnValue(0);
    const frames = new Map<number, FrameRequestCallback>();
    let nextId = 0;
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((cb) => {
      nextId += 1;
      frames.set(nextId, cb);
      return nextId;
    });
    vi.spyOn(window, "cancelAnimationFrame").mockImplementation((id) => {
      frames.delete(id);
    });
    vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} unobserve() {} });
    Object.defineProperties(HTMLCanvasElement.prototype, {
      clientWidth: { configurable: true, get: () => 200 },
      clientHeight: { configurable: true, get: () => 200 },
    });
    const step = (wallMs: number) => {
      const [[id, cb]] = [...frames];
      frames.delete(id);
      arcs.length = 0;
      ellipses = 0;
      lineTos = 0;
      strokes = [];
      act(() => cb(wallMs));
    };
    // The hand's head: the dot of radius max(1.6, R * 0.045), R = 80.
    const head = () => arcs.find((a) => Math.abs(a.r - 3.6) < 1e-9) ?? null;
    return { step, head, arcs, frames, ellipses: () => ellipses, lineTos: () => lineTos, strokes: () => strokes };
  }

  const END = 1_000_000;
  const num = (c: HTMLElement) => c.querySelector(".token-scope-n");

  it("the number drops in the frame the hand reaches 12 o'clock, and flares on that tick only", () => {
    const h = harness(false);
    // 5.5 s left at wall 0: "6s" until wall 500, "5s" from it.
    const { container } = render(
      <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="6s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 5_500, wallMs: 0, rate: 1 }} />,
    );
    for (const t of [100, 200, 300, 400, 499]) h.step(t);
    expect(num(container)?.textContent).toBe("6s");
    // The first number of the rest never flares.
    expect(num(container)?.getAttribute("data-flare")).toBeNull();
    h.step(500);
    expect(num(container)?.textContent).toBe("5s");
    expect(num(container)?.getAttribute("data-flare")).toBe("true");
    // The hand is at 12 o'clock in that same frame: straight above center.
    const top = h.head()!;
    expect(top.x).toBeCloseTo(100, 6);
    expect(top.y).toBeLessThan(100);
    // A quarter second later it is at 3 o'clock, and the number holds.
    h.step(750);
    const q = h.head()!;
    expect(q.x).toBeGreaterThan(100);
    expect(q.y).toBeCloseTo(100, 6);
    expect(num(container)?.textContent).toBe("5s");
  });

  // (#2961, design B) The dot draws the circle; the breathing ring is gone.
  it("(#2902 step 5) a day-long rest counts down in the tube as a compact duration, never raw seconds", () => {
    const h = harness(false);
    const left = (23 * 3600 + 53 * 60) * 1000 + 500;
    const { container } = render(
      <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="23h 53m" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - left, wallMs: 0, rate: 1 }} />,
    );
    for (const t of [100, 200, 300]) h.step(t);
    // 23h 53m 1s left: minutes round up, never under-reporting (6th review).
    expect(num(container)?.textContent).toBe("23h 54m");
  });

  it("during a countdown there is no breathing ring: only the drawn circle, from 12 to the dot, dimmer by age", () => {
    const h = harness(false);
    // 5.5 s left at wall 0.
    render(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="6s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 5_500, wallMs: 0, rate: 1 }} />);
    for (const t of [100, 200, 300, 400]) h.step(t);
    // Wall 750: 4.75 s left, a quarter of the circle drawn.
    h.step(750);
    // The breathing ring is a 240-point polyline; none of it is drawn.
    expect(h.lineTos()).toBe(0);
    const segs = h.strokes().filter((x) => x.blur === 0);
    const glow = h.strokes().filter((x) => x.blur > 0);
    // One soft glow stroke, never a blur per segment.
    expect(glow).toHaveLength(1);
    // The segments cover 12 o'clock to the dot: a quarter turn.
    expect(segs[0].a0).toBeCloseTo(-Math.PI / 2, 9);
    expect(segs[segs.length - 1].a1).toBeCloseTo(-Math.PI / 2 + Math.PI / 2 + 0.004, 9);
    // Dimmer by age: oldest (at 12) about 0.95 * (1 - 0.75 * 0.25), newest about 0.95.
    expect(segs[0].alpha).toBeCloseTo(0.95 * (1 - 0.75 * 0.25), 1);
    expect(segs[segs.length - 1].alpha).toBeCloseTo(0.95, 1);
    for (let i = 1; i < segs.length; i++) expect(segs[i].alpha).toBeGreaterThan(segs[i - 1].alpha);
  });

  it("at a tick the finished circle (with its age gradient) fades out over ~220 ms while the next one starts", () => {
    const h = harness(false);
    // 5.5 s left at wall 0: ticks at 500 and 1500.
    render(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="6s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 5_500, wallMs: 0, rate: 1 }} />);
    for (let t = 100; t <= 1400; t += 100) h.step(t);
    // 100 ms before the second tick: 0.9 of the circle (ceil(0.9 * 96) = 87
    // segments), and no fading one.
    expect(h.strokes().filter((x) => x.blur === 0)).toHaveLength(87);
    h.step(1500 + 22);
    // 22 ms after the tick: the full circle (96 segments) at ~90%, plus the
    // new second's stroke (a few segments).
    const at = h.strokes().filter((x) => x.blur === 0);
    const closed = at.slice(0, 96);
    expect(closed[0].a0).toBeCloseTo(-Math.PI / 2, 9);
    expect(closed[95].a1).toBeCloseTo(-Math.PI / 2 + 2 * Math.PI + 0.004, 9);
    const k = 1 - 22 / 220;
    expect(closed[95].alpha).toBeCloseTo(0.95 * k, 1);
    expect(closed[0].alpha).toBeCloseTo(0.95 * 0.25 * k, 1);
    expect(at.length).toBeGreaterThan(96);
    // 250 ms after the tick the finished circle is gone.
    h.step(1750);
    expect(h.strokes().filter((x) => x.blur === 0).length).toBeLessThan(40);
  });

  it("the rest's first second has no fading circle and no tick glow (there was no circle before it)", () => {
    const h = harness(false);
    // Mount 10 ms after a whole second: a tick-aligned moment, but the first.
    render(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="5s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 4_990, wallMs: 0, rate: 1 }} />);
    h.step(0);
    h.step(10);
    expect(h.strokes().filter((x) => x.blur === 0).length).toBeLessThan(10);
    // No radial glow at 12: the only full-radius arcs are the dot.
    expect(h.arcs.filter((a) => Math.abs(a.r - 80 * 0.22) < 1e-9)).toHaveLength(0);
  });

  it("follows the page clock's rate: at 5s/s a tick every 200 ms of wall time", () => {
    const h = harness(false);
    const { container } = render(
      <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="9s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 8_500, wallMs: 0, rate: 5 }} />,
    );
    for (const t of [20, 40, 60, 80, 99]) h.step(t);
    expect(num(container)?.textContent).toBe("9s");
    h.step(100);
    expect(num(container)?.textContent).toBe("8s");
    h.step(299);
    expect(num(container)?.textContent).toBe("8s");
    h.step(300);
    expect(num(container)?.textContent).toBe("7s");
  });

  it("paused (rate 0): the hand and the number stand still at the playhead, with no tick glow", () => {
    const h = harness(false);
    const { container, rerender } = render(
      <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="4s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 3_250, wallMs: 0, rate: 0 }} />,
    );
    // The angle, not the point: the ring keeps breathing while paused.
    const angleOf = (a: { x: number; y: number }) => Math.atan2(a.y - 100, a.x - 100);
    h.step(100);
    const at = angleOf(h.head()!);
    // 3.25 s left: three quarters of the second swept, at 9 o'clock.
    expect(Math.abs(at)).toBeCloseTo(Math.PI, 6);
    for (const t of [1_000, 5_000, 60_000]) {
      h.step(t);
      expect(num(container)?.textContent).toBe("4s");
      expect(angleOf(h.head()!)).toBeCloseTo(at, 6);
    }
    // A scrub to a new playhead re-anchors the clock there.
    rerender(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="2s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 1_500, wallMs: 0, rate: 0 }} />);
    h.step(61_000);
    expect(num(container)?.textContent).toBe("2s");
  });

  it("reduced motion: one still frame, the dot at 12, no ring, stroke, glow or flare; the number is the caller's", () => {
    const h = harness(true);
    const { container, rerender } = render(
      <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="7s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 6_750, wallMs: 0, rate: 1 }} />,
    );
    const top = h.head()!;
    expect(top.x).toBeCloseTo(100, 6);
    expect(top.y).toBeLessThan(100);
    expect(h.ellipses()).toBe(0);
    expect(h.lineTos()).toBe(0);
    expect(num(container)?.textContent).toBe("7s");
    rerender(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="6s" centerUnit="resting" restEndMs={END} clock={{ kind: "playback", tMs: END - 5_750, wallMs: 0, rate: 1 }} />);
    expect(num(container)?.textContent).toBe("6s");
    expect(num(container)?.getAttribute("data-flare")).toBeNull();
  });

  it("without an end time REST keeps the caller's number and the older drifting dot", () => {
    const h = harness(false);
    const { container } = render(<TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel="3s" centerUnit="resting" />);
    // Long enough for any eased hand-over to have finished.
    for (let t = 100; t <= 1500; t += 100) h.step(t);
    expect(num(container)?.textContent).toBe("3s");
    expect(h.head()).toBeNull();
    expect(h.ellipses()).toBe(0);
    // The breathing ring is still there, with its small drifting dot.
    expect(h.lineTos()).toBeGreaterThanOrEqual(240);
    expect(h.arcs.some((a) => Math.abs(a.r - 80 * 0.03) < 1e-9)).toBe(true);
  });

  // (#2961 review) The clock and the effects, as the operator sees them.
  const pbClock = (tMs: number, wallMs: number, rate: number) => ({ kind: "playback" as const, tMs, wallMs, rate });
  const rest = (clock: ReturnType<typeof pbClock>, label = "6s") => (
    <TokenScope tokensPerSec={0} size="tile" state="rest" centerLabel={label} centerUnit="resting" restEndMs={END} clock={clock} />
  );
  const closedCircle = (strokes: { blur: number; a0: number; a1: number }[]) =>
    strokes.filter((x) => x.blur === 0 && Math.abs(x.a1 - (-Math.PI / 2 + 2 * Math.PI + 0.004)) < 1e-9);

  it("M1 probe: a clock read at wall 1497 but rendered later never climbs the countdown back or flares twice", () => {
    const h = harness(false);
    // 5.5 s left at wall 0: "4s" from wall 1500.
    const { container, rerender } = render(rest(pbClock(END - 5_500, 0, 1)));
    for (let t = 100; t <= 1400; t += 100) h.step(t);
    h.step(1500);
    expect(num(container)?.textContent).toBe("4s");
    const flared = num(container);
    // The page's next clock reading: taken at wall 1497 (END − 4003), and
    // rendered at 1505.
    rerender(rest(pbClock(END - 4_003, 1_497, 1), "5s"));
    for (const t of [1502, 1506, 1510]) {
      h.step(t);
      expect(num(container)?.textContent, `wall ${t}`).toBe("4s");
    }
    // The same element: the flare key did not bump again.
    expect(num(container)).toBe(flared);
  });

  it("pausing (the same playhead, rate 0) stops the hand and the number", () => {
    const h = harness(false);
    const { container, rerender } = render(rest(pbClock(END - 5_500, 0, 1)));
    for (const t of [100, 200, 300, 400]) h.step(t);
    rerender(rest(pbClock(END - 5_100, 400, 0)));
    h.step(450);
    const angleOf = (a: { x: number; y: number }) => Math.atan2(a.y - 100, a.x - 100);
    const at = angleOf(h.head()!);
    for (const t of [900, 1_600, 30_000]) {
      h.step(t);
      expect(angleOf(h.head()!)).toBeCloseTo(at, 9);
      expect(num(container)?.textContent).toBe("6s");
    }
  });

  it("the number commits in the same frame as the hand, without waiting for React's scheduler (flushSync)", () => {
    const h = harness(false);
    const { container } = render(rest(pbClock(END - 5_500, 0, 1)));
    for (const t of [100, 200, 300, 499]) h.step(t);
    expect(num(container)?.textContent).toBe("6s");
    // Fire the tick frame OUTSIDE act(): only a synchronous commit shows the
    // new number when the DOM is read right after the frame returns.
    const [[id, cb]] = [...h.frames];
    h.frames.delete(id);
    const prevActEnv = (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT;
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = false;
    try {
      cb(500);
      expect(num(container)?.textContent).toBe("5s");
    } finally {
      (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = prevActEnv;
    }
  });

  // (#2961 review, C4) The final drop looks like every other tick.
  it("the drop to 0 flares, and the last circle fades out with the glow at 12 instead of vanishing", () => {
    const h = harness(false);
    const { container } = render(rest(pbClock(END - 1_500, 0, 1), "2s"));
    for (let t = 100; t <= 1400; t += 100) h.step(t);
    const before = num(container);
    expect(before?.textContent).toBe("1s");
    h.step(1_510);
    expect(num(container)?.textContent).toBe("0s");
    expect(num(container)).not.toBe(before); // a new element: the flare restarts
    expect(num(container)?.getAttribute("data-flare")).toBe("true");
    const closed = closedCircle(h.strokes());
    expect(closed.length).toBe(1); // the newest segment of the fading circle
    expect(h.strokes().filter((x) => x.blur === 0).length).toBe(96);
    expect(h.arcs.filter((a) => Math.abs(a.r - 80 * 0.22) < 1e-9)).toHaveLength(1);
    h.step(1_800);
    expect(h.strokes().filter((x) => x.blur === 0)).toHaveLength(0);
  });

  it("entering REST, the breathing ring eases out rather than popping off", () => {
    const h = harness(false);
    const { rerender } = render(<TokenScope tokensPerSec={0} size="tile" state="tools" toolName="bash" />);
    for (let t = 100; t <= 1000; t += 100) h.step(t);
    expect(h.lineTos()).toBeGreaterThanOrEqual(240);
    rerender(rest(pbClock(END - 5_500, 1_000, 1)));
    h.step(1_016);
    // One frame in: the ring is still being drawn (fading), beside the hand.
    expect(h.lineTos()).toBeGreaterThanOrEqual(240);
    expect(h.head()).not.toBeNull();
    for (let t = 1_100; t <= 2_000; t += 100) h.step(t);
    expect(h.lineTos()).toBe(0);
  });

  it("at fast playback the flare is shortened to end before the next tick", () => {
    const h = harness(false);
    const { container } = render(rest(pbClock(END - 5_500, 0, 5)));
    for (let t = 20; t <= 99; t += 20) h.step(t);
    h.step(100);
    expect(num(container)?.getAttribute("data-flare")).toBe("true");
    expect((num(container) as HTMLElement).style.animationDuration).toBe("160ms");
  });
});

// (#2962) Under reduced motion the scope paints one settled frame per
// change, and that frame must show ONLY the current state. The background
// fill is translucent (the animated path's afterglow), so without a clear
// most of the previous frame stays visible under the new one: a ghost.
describe("(#2962) reduced motion leaves no ghost of the previous state", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientWidth;
    delete (HTMLCanvasElement.prototype as unknown as Record<string, unknown>).clientHeight;
  });

  // A 2D context that records calls and models how much of what was on the
  // canvas before survives: a clear over the whole bitmap removes it, and a
  // whole-canvas fill of alpha `a` (source-over) leaves `1 - a` of it.
  function harness(DPR = 2) {
    const CSS = 200;
    let canvasEl: HTMLCanvasElement | null = null;
    const sim = { carry: 1, backgroundFills: 0, clears: 0, ops: [] as string[] };
    const state: Record<string | symbol, unknown> = { fillStyle: "#000", globalCompositeOperation: "source-over" };
    // The current transform: [a, d, e, f] (no skew), or null once a
    // translate/scale/rotate makes it something this model does not follow.
    let tf: [number, number, number, number] | null = [1, 1, 0, 0];
    const stack: [number, number, number, number][] = [];
    const covers = (x: number, y: number, w: number, h: number) => {
      if (!tf || !canvasEl) return false;
      const [a, d, e, f] = tf;
      return x * a + e <= 0 && y * d + f <= 0 && (x + w) * a + e >= canvasEl.width && (y + h) * d + f >= canvasEl.height;
    };
    const methods: Record<string, (...args: number[]) => void> = {
      setTransform: (a, _b, _c, d, e, f) => { tf = [a, d, e, f]; },
      resetTransform: () => { tf = [1, 1, 0, 0]; },
      save: () => { if (tf) stack.push(tf); },
      restore: () => { tf = stack.pop() ?? tf; },
      translate: () => { tf = null; },
      scale: () => { tf = null; },
      rotate: () => { tf = null; },
      transform: () => { tf = null; },
      clearRect: (x, y, w, h) => {
        sim.ops.push("clearRect");
        if (covers(x, y, w, h)) { sim.carry = 0; sim.clears += 1; }
      },
      fillRect: (x, y, w, h) => {
        if (!covers(x, y, w, h)) return;
        sim.ops.push("fillRect");
        sim.backgroundFills += 1;
        const m = /^rgba\([^,]+,[^,]+,[^,]+,\s*([\d.]+)\)$/.exec(String(state.fillStyle));
        const alpha = m ? Number(m[1]) : 1;
        if (state.globalCompositeOperation === "source-over") sim.carry *= 1 - Math.min(1, alpha);
      },
    };
    const ctx: unknown = new Proxy(state, {
      get: (t, k) => (k in t ? t[k] : typeof k === "string" && k in methods ? (...a: number[]) => { methods[k](...a); return ctx; } : () => ctx),
      set: (t, k, v) => { t[k] = v; return true; },
    });
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(function (this: HTMLCanvasElement) {
      canvasEl = this;
      return ctx as CanvasRenderingContext2D;
    } as unknown as HTMLCanvasElement["getContext"]);
    vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue({ width: CSS, height: CSS, top: 0, left: 0, right: CSS, bottom: CSS, x: 0, y: 0, toJSON() {} } as DOMRect);
    Object.defineProperties(HTMLCanvasElement.prototype, {
      clientWidth: { configurable: true, get: () => CSS },
      clientHeight: { configurable: true, get: () => CSS },
    });
    vi.stubGlobal("devicePixelRatio", DPR);
    vi.spyOn(window, "matchMedia").mockImplementation(
      (q: string) => ({ matches: true, media: q, addEventListener: () => {}, removeEventListener: () => {} }) as unknown as MediaQueryList,
    );
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation(() => 1);
    const resize: { fire: (() => void) | null } = { fire: null };
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(cb: () => void) { resize.fire = cb; }
        observe() {}
        disconnect() {}
        unobserve() {}
      },
    );
    return { sim, raf, canvas: () => canvasEl, resize };
  }

  it("mounting paints exactly one settled frame, not two", () => {
    const h = harness();
    render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    expect(h.sim.backgroundFills).toBe(1);
    expect(h.raf).not.toHaveBeenCalled();
  });

  it("a change of state paints exactly one frame, and none of the previous state survives it", () => {
    const h = harness();
    const r = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    // The canvas is sized in device pixels, so the model checks the full bitmap.
    expect(h.canvas()?.width).toBe(400);
    expect(h.sim.backgroundFills).toBe(1);
    // State A is on screen; change to state B.
    h.sim.carry = 1;
    h.sim.backgroundFills = 0;
    r.rerender(<TokenScope tokensPerSec={0} size="tile" state="stalled" />);
    expect(h.sim.backgroundFills).toBe(1);
    expect(h.sim.carry).toBe(0);
    expect(h.raf).not.toHaveBeenCalled();
  });

  it("a change of rate alone does the same: one frame, no ghost, never a loop", () => {
    const h = harness();
    const r = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    h.sim.carry = 1;
    h.sim.backgroundFills = 0;
    r.rerender(<TokenScope tokensPerSec={5} size="tile" state="generating" />);
    expect(h.sim.backgroundFills).toBe(1);
    expect(h.sim.carry).toBe(0);
    // Same props again: nothing is painted at all.
    r.rerender(<TokenScope tokensPerSec={5} size="tile" state="generating" />);
    expect(h.sim.backgroundFills).toBe(1);
    expect(h.raf).not.toHaveBeenCalled();
  });

  it("the clear comes before the frame's own background, not after it", () => {
    const h = harness();
    const r = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    h.sim.ops.length = 0;
    r.rerender(<TokenScope tokensPerSec={0} size="tile" state="rest" />);
    expect(h.sim.ops.slice(0, 2)).toEqual(["clearRect", "fillRect"]);
  });

  it("a resize repaints the one settled frame even though nothing else changed", () => {
    const h = harness();
    render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    h.sim.backgroundFills = 0;
    act(() => { h.resize.fire?.(); });
    // A resized canvas is blank, so the same state is painted again, once.
    expect(h.sim.backgroundFills).toBe(1);
    expect(h.raf).not.toHaveBeenCalled();
  });

  it("below a device pixel ratio of 1 (a zoomed-out page) the clear still covers the whole bitmap", () => {
    const h = harness(0.5);
    const r = render(<TokenScope tokensPerSec={50} size="tile" state="generating" />);
    expect(h.canvas()?.width).toBe(100);
    h.sim.carry = 1;
    r.rerender(<TokenScope tokensPerSec={0} size="tile" state="stalled" />);
    expect(h.sim.carry).toBe(0);
  });
});
