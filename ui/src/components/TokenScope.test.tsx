import { describe, it, expect, vi, afterEach } from "vitest";
import { render, act } from "@testing-library/react";
import { TokenScope } from "./TokenScope";

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

  it("PROMPT shows the brain until a prompt size arrives, then the count", () => {
    const a = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel={null} />);
    expect(a.container.querySelector("[data-scope-icon]")?.getAttribute("data-scope-icon")).toBe("brain");
    expect(a.container.querySelector(".token-scope-n")).toBeNull();
    const b = render(<TokenScope tokensPerSec={0} size="tile" state="prompt" centerLabel="36k" centerUnit="processing" />);
    expect(b.container.querySelector("[data-scope-icon]")).toBeNull();
    expect(b.container.querySelector(".token-scope-n")?.textContent).toBe("36k");
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
