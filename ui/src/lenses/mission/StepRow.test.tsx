/**
 * (U3-6) Two clocks, two subjects — and until now, no labels.
 *
 * The mission graph's per-step badge is the STEP SPAN: `stepStartMs` →
 * `stepEndMs`, which brackets the whole step (setup, the model's own work,
 * and the gate around it). The session drill-in's WALL CLOCK tile is the
 * DISPATCH's own `wall_ms` — the runtime's measure of the model execution
 * alone. On a real mission the same step read 10:36 in one surface and 10:07
 * in the other, with nothing on either screen saying they described different
 * spans.
 *
 * These assert the label on the graph half. `SessionReplay.test.tsx` asserts
 * the session half. Both go through the SAME mechanism — a `data-hint` short
 * label (rendered by one CSS rule, so it never enters `textContent` and the
 * frozen parity goldens stay byte-identical) plus a `title` that spells the
 * span out.
 */
import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { StepMeterEl } from "./StepRow";
import type { StepMeter } from "./graph";

const meter = (over: Partial<StepMeter>): StepMeter => ({
  show: true,
  tokens: 0,
  turns: 0,
  tools: 0,
  cloud: false,
  generating: false,
  elapsedMs: 0,
  wallMs: 0,
  ...over,
});

describe("(U3-6) the step badge says WHICH clock it is", () => {
  it("labels a finished step's duration 'step' time, not a bare number", () => {
    const { container } = render(<StepMeterEl meter={meter({ wallMs: 636_000 })} />);
    const wall = container.querySelector(".wall");
    expect(wall, "the finished step's wall badge").toBeTruthy();
    expect(wall?.textContent).toBe("10:36");
    // The short label every sibling badge in this row already carries
    // ("tok", "turns", "tools") — the duration was the one unlabeled number.
    expect(wall?.getAttribute("data-hint")).toBe("step");
    // And the long form, which is where the DISTINCTION actually lands.
    expect(wall?.getAttribute("title") ?? "").toContain("step time");
    expect(wall?.getAttribute("title") ?? "").toContain("setup");
  });

  it("labels a still-running step's elapsed the same way", () => {
    const { container } = render(<StepMeterEl meter={meter({ generating: true, elapsedMs: 5_000 })} />);
    const gen = container.querySelector(".gen");
    expect(gen?.getAttribute("data-hint")).toBe("step");
    expect(gen?.getAttribute("title") ?? "").toContain("step time");
  });

  it("keeps the badge's own text unchanged, so the frozen graph goldens still hold", () => {
    // `tests/parity/lib/extract-graph.js` reads `.mn-step-meter`'s
    // textContent. A visible label that entered the text would rebaseline
    // `goldens/mission-graph-canvas.txt` and `-timeline.txt` — the pane
    // labels in the session lens already establish the CSS-generated form as
    // this project's answer to that.
    const { container } = render(<StepMeterEl meter={meter({ wallMs: 3_000, tokens: 1200, turns: 2 })} />);
    expect(container.querySelector(".mn-step-meter")?.textContent).toBe("0:031.2k tok2 turns");
  });
});
