import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import type { ComponentProps } from "react";
import { Scrubber } from "./Scrubber";
import { clkhm, clkrange } from "../../lib/format";
import { fmtElapsed } from "../../lib/format";

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

  // (#1869 code review) `.mm-sr-only` uses `clip`, which keeps the node in
  // the accessibility tree AND in `innerText` — a visually-hidden `<label>`
  // here would announce "playback position" TWICE (once as loose row text
  // a screen reader hits walking the DOM, once as the slider's own
  // computed name) and leak an extra line into the parity golden.
  // `aria-label` is this port's established pattern for naming an input
  // without a rendered node (`EventLogColumn.tsx`'s search box).
  it("names the range via aria-label, not a visually-hidden <label> that would still enter innerText", () => {
    renderScrubber();
    const slider = screen.getByRole("slider", { name: /playback position/i });
    expect(slider).toHaveAttribute("aria-label", "playback position");
    expect(document.querySelector("label")).toBeNull();
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
  it("the play/pause button's icon and its name flip together", () => {
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
      />,
    );
    const playBtn = screen.getByRole("button", { name: "play" });
    expect(playBtn.querySelector("svg path")?.getAttribute("d")).toBe("M4 2l10 6-10 6z"); // the play triangle

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
      />,
    );
    const pauseBtn = screen.getByRole("button", { name: "pause" });
    expect(pauseBtn.querySelector("svg path")?.getAttribute("d")).toBe("M3 2h4v12H3zM9 2h4v12H9z"); // the pause bars
    expect(screen.queryByRole("button", { name: "play" })).not.toBeInTheDocument();
  });

  // (#2120, operator finding — "almost none of it is meaningful") The
  // rec-count readout is gone; the range input is the progress indicator
  // now. The clock reads bare when the caller passes no label (a phone
  // route, or a desktop route with nothing derivable — see `App.tsx`'s own
  // `playbackMissionLabel` doc).
  it("the clock readout names only the playhead time when no label is given", () => {
    renderScrubber({ t: TMIN });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(clkhm(TMIN));
  });

  it("appends the label after the clock, separated by a middot, when one is given", () => {
    renderScrubber({ t: TMIN, label: "Review · nameof recency" });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${clkhm(TMIN)} · Review · nameof recency`);
  });

  // (#2346) A dispatch/mission focus reports its own elapsed time — the
  // SAME `fmtElapsed` the run detail's WALL CLOCK tile uses — beside the
  // time of day, so the two clocks agree by construction. Day focus passes
  // no `elapsedMs` at all: elapsed-since-the-day's-first-record is
  // meaningless, so only the time of day renders (the two tests above).
  it("shows the focus's elapsed time before the time of day when elapsedMs is given", () => {
    const t = TMIN + 6_888_000; // 1:54:48
    renderScrubber({ t, elapsedMs: 6_888_000 });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${fmtElapsed(6_888_000)} · ${clkhm(t)}`);
  });

  it("still appends a label after the clock when both elapsedMs and label are given", () => {
    const t = TMIN + 6_888_000;
    renderScrubber({ t, elapsedMs: 6_888_000, label: "coder" });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(`${fmtElapsed(6_888_000)} · ${clkhm(t)} · coder`);
  });

  it("omits the elapsed segment entirely when elapsedMs is null (day focus)", () => {
    renderScrubber({ t: TMIN, elapsedMs: null });
    expect(screen.getByTestId("scrubber-clock").textContent).toBe(clkhm(TMIN));
  });

  // (#1869 code review) A zero-span day pins the thumb at the RIGHT edge
  // (100), not the left (0) — legacy's own rule (`span > 0 ? Math.round(...)
  // : 100`, viewer.html:2618, named at #1640). A previous version of this
  // test pinned the WRONG value (0): with `tMin === tMax`, `t - tMin` is 0,
  // and the pre-fix code floored `span` to 1 before dividing, so `raw`
  // silently computed to 0 instead of following legacy's explicit `else
  // 100` branch — thumb hard left while the day's whole span read as
  // scrubbed already. Never divides by zero either way; only the RENDERED
  // VALUE was wrong.
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
      />,
    );
    expect(screen.getByRole("slider")).toHaveValue("100");
  });
});
