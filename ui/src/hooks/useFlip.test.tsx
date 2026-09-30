import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render } from "@testing-library/react";
import { useRef } from "react";
import { useFlip, FLIP_MS } from "./useFlip";

// jsdom has no layout: positions come from a table keyed by data-flip-key.
let places: Record<string, [number, number]> = {};
const animate = vi.fn<(frames: Keyframe[], opts: KeyframeAnimationOptions) => void>();

function Grid({ keys }: { keys: string[] }) {
  const ref = useRef<HTMLDivElement | null>(null);
  useFlip(ref);
  return (
    <div ref={ref} data-container>
      {keys.map((k) => (
        <div key={k} data-flip-key={k} />
      ))}
    </div>
  );
}

function mockMotion(reduced: boolean) {
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: reduced && q.includes("reduce") }));
}

beforeEach(() => {
  places = {};
  animate.mockReset();
  Object.defineProperty(HTMLElement.prototype, "offsetLeft", { configurable: true, get() { const d = (this as HTMLElement).dataset; return places[d.flipKey ?? (d.container !== undefined ? "__c" : "")]?.[0] ?? 0; } });
  Object.defineProperty(HTMLElement.prototype, "offsetTop", { configurable: true, get() { const d = (this as HTMLElement).dataset; return places[d.flipKey ?? (d.container !== undefined ? "__c" : "")]?.[1] ?? 0; } });
  Object.defineProperty(HTMLElement.prototype, "offsetWidth", { configurable: true, get() { return 1000; } });
  (HTMLElement.prototype as unknown as { animate: unknown }).animate = animate;
  mockMotion(false);
});
afterEach(() => {
  vi.unstubAllGlobals();
  delete (HTMLElement.prototype as unknown as { animate?: unknown }).animate;
});

describe("useFlip", () => {
  it("animates a moved item from its old place to its new one over 200 ms, ease-out", () => {
    places = { a: [0, 0], b: [300, 0] };
    const { rerender } = render(<Grid keys={["a", "b"]} />);
    expect(animate).not.toHaveBeenCalled();
    places = { a: [300, 0], b: [0, 0] };
    rerender(<Grid keys={["b", "a"]} />);
    expect(animate).toHaveBeenCalledTimes(2);
    const calls = animate.mock.calls.map(([frames, opts]) => ({ frames, opts }));
    expect(calls.map((c) => c.frames)).toContainEqual([{ transform: "translate(300px, 0px)" }, { transform: "translate(0, 0)" }]);
    expect(calls.map((c) => c.frames)).toContainEqual([{ transform: "translate(-300px, 0px)" }, { transform: "translate(0, 0)" }]);
    expect(calls[0].opts).toEqual({ duration: FLIP_MS, easing: "ease-out" });
  });

  it("does not animate an item that did not move", () => {
    places = { a: [0, 0], b: [300, 0] };
    const { rerender } = render(<Grid keys={["a", "b"]} />);
    rerender(<Grid keys={["a", "b"]} />);
    expect(animate).not.toHaveBeenCalled();
  });

  it("does not animate items when the whole container moves under them (something above changed height)", () => {
    places = { __c: [0, 300], a: [0, 300], b: [300, 300] };
    const { rerender } = render(<Grid keys={["a", "b"]} />);
    // The container and both items sit 317 px lower; relative to the container nothing moved.
    places = { __c: [0, 617], a: [0, 617], b: [300, 617] };
    rerender(<Grid keys={["a", "b"]} />);
    expect(animate).not.toHaveBeenCalled();
  });

  it("fades a newly appearing item in instead of popping it", () => {
    places = { a: [0, 0] };
    const { rerender } = render(<Grid keys={["a"]} />);
    places = { a: [0, 0], b: [300, 0] };
    rerender(<Grid keys={["a", "b"]} />);
    expect(animate).toHaveBeenCalledTimes(1);
    expect(animate.mock.calls[0][0]).toEqual([{ opacity: 0 }, { opacity: 1 }]);
  });

  it("applies no motion under prefers-reduced-motion", () => {
    mockMotion(true);
    places = { a: [0, 0], b: [300, 0] };
    const { rerender } = render(<Grid keys={["a", "b"]} />);
    places = { a: [300, 0], b: [0, 0] };
    rerender(<Grid keys={["b", "a", "c"]} />);
    expect(animate).not.toHaveBeenCalled();
  });
});
