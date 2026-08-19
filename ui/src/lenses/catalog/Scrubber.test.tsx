import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import type { ComponentProps } from "react";
import { Scrubber } from "./Scrubber";
import { clkhm, clkrange } from "../../lib/format";

const TMIN = Date.parse("2026-08-07T00:00:00.000Z");
const TMAX = Date.parse("2026-08-07T02:00:00.000Z");

function renderScrubber(overrides: Partial<ComponentProps<typeof Scrubber>> = {}) {
  const onScrub = vi.fn();
  const onRewind = vi.fn();
  const onTogglePlay = vi.fn();
  const onCycleSpeed = vi.fn();
  render(
    <Scrubber
      t={TMIN}
      tMin={TMIN}
      tMax={TMAX}
      playing={false}
      speed={1}
      onScrub={onScrub}
      onRewind={onRewind}
      onTogglePlay={onTogglePlay}
      onCycleSpeed={onCycleSpeed}
      visibleCount={1}
      totalCount={4}
      {...overrides}
    />,
  );
  return { onScrub, onRewind, onTogglePlay, onCycleSpeed };
}

describe("Scrubber", () => {
  it("renders a range at the position implied by t within [tMin, tMax]", () => {
    renderScrubber({ t: TMIN + (TMAX - TMIN) / 2 });
    expect(screen.getByRole("slider")).toHaveValue("50");
  });

  it("the range has a real accessible name and an aria-valuetext naming the playhead clock time", () => {
    const t = TMIN + (TMAX - TMIN) / 4;
    renderScrubber({ t });
    const slider = screen.getByRole("slider", { name: /playback position/i });
    expect(slider).toHaveAttribute("aria-valuetext", `${clkhm(t)} of ${clkrange(TMIN, TMAX)}`);
  });

  it("dragging the range calls onScrub with the timestamp that percentage maps to", () => {
    const { onScrub } = renderScrubber();
    fireEvent.change(screen.getByRole("slider"), { target: { value: "25" } });
    expect(onScrub).toHaveBeenCalledWith(TMIN + (TMAX - TMIN) * 0.25);
  });

  it("rewind and play/pause/speed are real, separately-labeled buttons", () => {
    const { onRewind, onTogglePlay, onCycleSpeed } = renderScrubber();
    fireEvent.click(screen.getByRole("button", { name: "jump to start" }));
    expect(onRewind).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "play" }));
    expect(onTogglePlay).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: /playback speed/i }));
    expect(onCycleSpeed).toHaveBeenCalledTimes(1);
  });

  // (#1067) The glyph carries visual state; title/aria-label carry meaning —
  // and must flip TOGETHER with the glyph, not drift from it.
  it("the play/pause button's glyph and its name flip together", () => {
    const { rerender } = render(
      <Scrubber
        t={TMIN}
        tMin={TMIN}
        tMax={TMAX}
        playing={false}
        speed={1}
        onScrub={() => {}}
        onRewind={() => {}}
        onTogglePlay={() => {}}
        onCycleSpeed={() => {}}
        visibleCount={1}
        totalCount={4}
      />,
    );
    const playBtn = screen.getByRole("button", { name: "play" });
    expect(playBtn.textContent).toBe("▶");

    rerender(
      <Scrubber
        t={TMIN}
        tMin={TMIN}
        tMax={TMAX}
        playing
        speed={1}
        onScrub={() => {}}
        onRewind={() => {}}
        onTogglePlay={() => {}}
        onCycleSpeed={() => {}}
        visibleCount={1}
        totalCount={4}
      />,
    );
    const pauseBtn = screen.getByRole("button", { name: "pause" });
    expect(pauseBtn.textContent).toBe("⏸");
    expect(screen.queryByRole("button", { name: "play" })).not.toBeInTheDocument();
  });

  it("the clock readout names the playhead time and the visible/total record count", () => {
    renderScrubber({ t: TMIN, visibleCount: 2, totalCount: 9 });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${clkhm(TMIN)} · 2/9 rec`);
  });

  // (#1869 code review) A zero-span day pins the thumb at the RIGHT edge
  // (100), not the left (0) — legacy's own rule (`span > 0 ? Math.round(...)
  // : 100`, viewer.html:2618, named at #1640). A previous version of this
  // test pinned the WRONG value (0): with `tMin === tMax`, `t - tMin` is 0,
  // and the pre-fix code floored `span` to 1 before dividing, so `raw`
  // silently computed to 0 instead of following legacy's explicit `else
  // 100` branch — thumb hard left while the clock reads "1/1 rec" and the
  // hero shows the whole day. Never divides by zero either way; only the
  // RENDERED VALUE was wrong.
  it("a zero-span day (tMin === tMax) pins the thumb at the end, not the start (#1640)", () => {
    render(
      <Scrubber
        t={TMIN}
        tMin={TMIN}
        tMax={TMIN}
        playing={false}
        speed={1}
        onScrub={() => {}}
        onRewind={() => {}}
        onTogglePlay={() => {}}
        onCycleSpeed={() => {}}
        visibleCount={1}
        totalCount={1}
      />,
    );
    expect(screen.getByRole("slider")).toHaveValue("100");
  });
});
