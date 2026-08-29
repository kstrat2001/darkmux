import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { render, screen } from "@testing-library/react";
import {
  Meter,
  angleForPct,
  avgMaxTicks,
  compactMeterProps,
  fmtPct,
  simpleBand,
  meterBandLevel,
  bandLevelClass,
  DEFAULT_WARN_AT,
  DEFAULT_CRITICAL_AT,
  MEM_WARN_AT,
  MEM_CRITICAL_AT,
} from "./Meter";

// ── low-level Meter: the shared core (needle, arc/bands, redline, ticks,
//    gradient, children slot) — the pieces every caller, VRAM included,
//    draws through. VRAM's own byte-identical DOM is proven separately by
//    `MachineHealthRegion.test.tsx`, which never changed; these tests
//    exercise the SAME component from its generic surface. ──────────────

describe("Meter — the shared dial core", () => {
  it("draws a band, needle, and hub when given a reading", () => {
    const { container } = render(
      <Meter
        wrapperClassName="mm-gauge"
        ariaLabel="test"
        bands={[{ className: "mm-gauge-fill-compact", stroke: "green", lengthPct: 62 }]}
        needleAngleDeg={angleForPct(62)}
      />,
    );
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.getAttribute("stroke-dasharray")).toBe("62 100");
    expect(container.querySelector(".mm-gauge-needle")).not.toBeNull();
    expect(container.querySelector(".mm-gauge-hub")).not.toBeNull();
  });

  it("omits a band entirely when lengthPct is 0 and alwaysRender is not set", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[{ className: "mm-gauge-fill-compact", stroke: "green", lengthPct: 0 }]} />,
    );
    expect(container.querySelector(".mm-gauge-fill-compact")).toBeNull();
  });

  it("alwaysRender keeps a band in the DOM even at lengthPct 0 — VRAM's darkmux band", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[{ className: "mm-gauge-val", stroke: "url(#x)", lengthPct: 0, alwaysRender: true }]} />,
    );
    expect(container.querySelector(".mm-gauge-val")).not.toBeNull();
  });

  it("draws no needle when needleAngleDeg is omitted — absence, never a zero-angle needle", () => {
    const { container } = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} />);
    expect(container.querySelector(".mm-gauge-needle")).toBeNull();
    // The hub still draws — it is not a reading, just the pivot.
    expect(container.querySelector(".mm-gauge-hub")).not.toBeNull();
  });

  it("a hatched band carries its precomputed dasharray and NO stroke attribute", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[{ className: "mm-gauge-growth", lengthPct: 50, hatchedDasharray: "2 1 2 1 50 100", alwaysRender: true }]} />,
    );
    const growth = container.querySelector(".mm-gauge-growth")!;
    expect(growth.getAttribute("stroke-dasharray")).toBe("2 1 2 1 50 100");
    expect(growth.hasAttribute("stroke")).toBe(false);
  });

  it("draws ticks with a rotate transform, and a label only when label + position are all given", () => {
    const { container } = render(
      <Meter
        wrapperClassName="mm-gauge"
        ariaLabel="test"
        bands={[]}
        ticks={[
          { pct: 50, label: "50", labelX: 10, labelY: 20 },
          { pct: 75, className: "mm-gauge-tick mm-gauge-tick-max" },
        ]}
      />,
    );
    const ticks = container.querySelectorAll("line.mm-gauge-tick, line.mm-gauge-tick.mm-gauge-tick-max");
    expect(ticks.length).toBe(2);
    expect(ticks[0].getAttribute("transform")).toContain("rotate(90");
    expect(container.querySelectorAll(".mm-gauge-scale-label").length).toBe(1);
    expect(container.querySelector(".mm-gauge-scale-label")!.textContent).toBe("50");
  });

  it("draws a redline only when the prop is present, and toggles .lit off `lit`", () => {
    const lit = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} redline={{ lit: true }} />);
    expect(lit.container.querySelector(".mm-gauge-redline.lit")).not.toBeNull();
    const unlit = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} redline={{ lit: false }} />);
    expect(unlit.container.querySelector(".mm-gauge-redline")).not.toBeNull();
    expect(unlit.container.querySelector(".mm-gauge-redline.lit")).toBeNull();
    const none = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} />);
    expect(none.container.querySelector(".mm-gauge-redline")).toBeNull();
  });

  it("renders the gradient defs only when a gradient prop is given", () => {
    const withGrad = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} gradient={{ id: "g1", stops: [{ offset: 0, color: "red" }] }} />,
    );
    expect(withGrad.container.querySelector("linearGradient#g1")).not.toBeNull();
    const without = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} />);
    expect(without.container.querySelector("linearGradient")).toBeNull();
  });

  it("renders children inside the svg, after the hub — VRAM's odometer slot", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]}>
        <text className="mm-gauge-readout-label">MACHINE USED</text>
      </Meter>,
    );
    expect(container.querySelector("svg text.mm-gauge-readout-label")).not.toBeNull();
  });

  it("scales via width/height while keeping the same viewBox — the 'one geometry, two sizes' claim", () => {
    const big = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} />);
    const small = render(<Meter wrapperClassName="mm-gauge mm-gauge--compact" ariaLabel="test" bands={[]} width={112} height={79} />);
    const bigSvg = big.container.querySelector("svg")!;
    const smallSvg = small.container.querySelector("svg")!;
    expect(bigSvg.getAttribute("width")).toBe("300");
    expect(smallSvg.getAttribute("width")).toBe("112");
    expect(bigSvg.getAttribute("viewBox")).toBe(smallSvg.getAttribute("viewBox"));
  });
});

// ── the shared "numerals" default — the compact CPU/GPU/MEM readout,
//    genuinely part of THIS component's own core rather than duplicated
//    at each call site. ──────────────────────────────────────────────────

describe("Meter — the default numeral readout", () => {
  it("renders now big, avg/max together underneath, rounded (#2107 phone feedback: two short lines, not one long one)", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} label="CPU" numerals={{ now: 62.4, avg: 41.6, max: 88.2 }} />,
    );
    expect(container.querySelector(".meter-now")!.textContent).toBe("62%");
    expect(container.querySelector(".meter-avgmax")!.textContent?.replace(/\s+/g, " ").trim()).toBe("42% avg · 88% max");
    expect(screen.getByText("CPU")).toBeInTheDocument();
  });

  it("shows an em-dash, not zero, when a reading is null", () => {
    const { container } = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} numerals={{ now: null, avg: null, max: null }} />);
    expect(container.querySelector(".meter-now")!.textContent).toBe("—");
    expect(container.querySelector(".meter-avgmax")!.textContent?.replace(/\s+/g, " ").trim()).toBe("— avg · — max");
  });

  it("renders no numeral row at all when `numerals` is omitted — VRAM's own case", () => {
    const { container } = render(<Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} />);
    expect(container.querySelector(".meter-caption")).toBeNull();
    expect(container.querySelector(".meter-label")).toBeNull();
  });
});

// ── (#2108, operator finding — typography pass) the caption row + nowrap
//    window line. The gap between the arc and its caption was ~50px
//    (label stacked ABOVE a separate numerals block); label + the current
//    value now share ONE row directly under the gauge, and the avg/max
//    line below it must never wrap at three-across on a 390px phone. ─────

describe("Meter — caption row + nowrap window line (#2108)", () => {
  it("label and the current value render inside the SAME row element, directly under the gauge", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} label="CPU" numerals={{ now: 17, avg: 25, max: 51 }} />,
    );
    const caption = container.querySelector(".meter-caption")!;
    expect(caption).not.toBeNull();
    const label = caption.querySelector(".meter-label")!;
    const value = caption.querySelector(".meter-now")!;
    expect(label).not.toBeNull();
    expect(value).not.toBeNull();
    // Both are children of the SAME row — not siblings split across two
    // separate wrapper elements the way the pre-#2108 layout had them.
    expect(label.parentElement).toBe(caption);
    expect(value.parentElement).toBe(caption);
    expect(label.textContent).toBe("CPU");
    expect(value.textContent).toBe("17%");
  });

  it("a label-only caller (no numerals — VRAM's own case elsewhere) still gets the caption row", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} label="Super" />,
    );
    const caption = container.querySelector(".meter-caption")!;
    expect(caption).not.toBeNull();
    expect(caption.querySelector(".meter-label")!.textContent).toBe("Super");
    expect(caption.querySelector(".meter-now")).toBeNull();
  });

  it("`hideAvgMax` suppresses the avg/max line even though `numerals` is present (CPU clusters have no window concept)", () => {
    const { container } = render(
      <Meter
        wrapperClassName="mm-gauge"
        ariaLabel="test"
        bands={[]}
        label="Super"
        numerals={{ now: 46, avg: null, max: null }}
        hideAvgMax
      />,
    );
    expect(container.querySelector(".meter-caption .meter-now")!.textContent).toBe("46%");
    expect(container.querySelector(".meter-avgmax")).toBeNull();
  });

  it("omitting `hideAvgMax` keeps the avg/max line exactly as before, including the all-null case", () => {
    const { container } = render(
      <Meter wrapperClassName="mm-gauge" ariaLabel="test" bands={[]} numerals={{ now: null, avg: null, max: null }} />,
    );
    expect(container.querySelector(".meter-avgmax")!.textContent?.replace(/\s+/g, " ").trim()).toBe("— avg · — max");
  });

  // The stylesheet itself, not a jsdom-computed style — jsdom's test
  // environment never loads `styles.css` as a real stylesheet, so
  // `getComputedStyle` can't answer "does this rule say nowrap". Reading
  // the source is the actual verifiable claim: the `.meter-avgmax` rule
  // block genuinely specifies `white-space: nowrap` and tabular numerals,
  // not just that the class name is present in the DOM.
  it("the `.meter-avgmax` stylesheet rule specifies white-space: nowrap and tabular-nums", () => {
    const cssPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "../styles.css");
    const css = readFileSync(cssPath, "utf-8");
    const match = css.match(/\.meter-avgmax\s*\{([^}]*)\}/);
    expect(match).not.toBeNull();
    const rule = match![1];
    expect(rule).toMatch(/white-space:\s*nowrap/);
    expect(rule).toMatch(/font-variant-numeric:\s*tabular-nums/);
  });
});

// ── the compact-meter helper functions — pure, so the translation from a
//    plain {now, avg, high} reading into Meter's low-level props is
//    tested without rendering anything. ───────────────────────────────

describe("angleForPct / simpleBand / avgMaxTicks / compactMeterProps", () => {
  it("angleForPct clamps into the 0-100 sweep before scaling by 1.8", () => {
    expect(angleForPct(50)).toBe(90);
    expect(angleForPct(150)).toBe(180);
    expect(angleForPct(-10)).toBe(0);
  });

  it("simpleBand yields no band at all for a null reading", () => {
    expect(simpleBand("c", "s", null)).toEqual([]);
  });

  it("simpleBand clamps an out-of-range reading into 0-100", () => {
    expect(simpleBand("c", "s", 150)[0].lengthPct).toBe(100);
    expect(simpleBand("c", "s", -10)[0].lengthPct).toBe(0);
  });

  it("avgMaxTicks omits whichever of avg/max is null", () => {
    expect(avgMaxTicks(null, 80)).toEqual([{ pct: 80, className: "mm-gauge-tick mm-gauge-tick-max" }]);
    expect(avgMaxTicks(40, null)).toEqual([{ pct: 40, className: "mm-gauge-tick mm-gauge-tick-avg" }]);
    expect(avgMaxTicks(null, null)).toEqual([]);
  });

  it("compactMeterProps bundles bands/ticks/needle/numerals/label from one reading", () => {
    const props = compactMeterProps("GPU", "mm-gauge-fill-compact", "green", { now: 90, avg: 60, high: 95 });
    expect(props.label).toBe("GPU");
    expect(props.bands).toEqual([{ className: "mm-gauge-fill-compact", stroke: "green", lengthPct: 90, banded: true }]);
    expect(props.ticks).toEqual([
      { pct: 60, className: "mm-gauge-tick mm-gauge-tick-avg" },
      { pct: 95, className: "mm-gauge-tick mm-gauge-tick-max" },
    ]);
    expect(props.needleAngleDeg).toBe(angleForPct(90));
    expect(props.numerals).toEqual({ now: 90, avg: 60, max: 95 });
  });

  it("compactMeterProps draws no needle and no fill band for a never-sampled metric", () => {
    const props = compactMeterProps("MEM", "mm-gauge-fill-compact", "green", { now: null, avg: null, high: null });
    expect(props.bands).toEqual([]);
    expect(props.needleAngleDeg).toBeUndefined();
    expect(props.numerals).toEqual({ now: null, avg: null, max: null });
  });
});

describe("fmtPct", () => {
  it("rounds to the nearest integer with a trailing %", () => {
    expect(fmtPct(66.6)).toBe("67%");
    expect(fmtPct(66.4)).toBe("66%");
  });

  it("renders an em-dash for null rather than coercing to 0", () => {
    expect(fmtPct(null)).toBe("—");
  });
});

// ── (#2122) band-threshold coloring — the fill AND the caption both key
//    off the SAME reading, so a loaded GPU or CPU stops looking identical
//    to an idle one. Fixture renders at 79/80/95/100 per the issue's own
//    verification ask. ─────────────────────────────────────────────────

describe("meterBandLevel / bandLevelClass — the pure threshold lookup", () => {
  it("stays quiet below warnAt, including a null (unmeasured) reading", () => {
    expect(meterBandLevel(79, DEFAULT_WARN_AT, DEFAULT_CRITICAL_AT)).toBe("quiet");
    expect(meterBandLevel(null, DEFAULT_WARN_AT, DEFAULT_CRITICAL_AT)).toBe("quiet");
    expect(bandLevelClass("quiet")).toBe("");
  });

  it("crosses into warn AT the threshold (>=, matching the thermal pill's own wording)", () => {
    expect(meterBandLevel(80, DEFAULT_WARN_AT, DEFAULT_CRITICAL_AT)).toBe("warn");
    expect(bandLevelClass("warn")).toBe("mm-band-warn");
  });

  it("crosses into critical AT the threshold, and stays critical past 100", () => {
    expect(meterBandLevel(95, DEFAULT_WARN_AT, DEFAULT_CRITICAL_AT)).toBe("critical");
    expect(meterBandLevel(100, DEFAULT_WARN_AT, DEFAULT_CRITICAL_AT)).toBe("critical");
    expect(bandLevelClass("critical")).toBe("mm-band-critical");
  });

  it("MEM's own gentler pair reads 90/97, not the 80/95 default — no double-alarm with the pressure ledger", () => {
    expect(meterBandLevel(90, MEM_WARN_AT, MEM_CRITICAL_AT)).toBe("warn");
    expect(meterBandLevel(89, MEM_WARN_AT, MEM_CRITICAL_AT)).toBe("quiet");
    expect(meterBandLevel(97, MEM_WARN_AT, MEM_CRITICAL_AT)).toBe("critical");
    expect(meterBandLevel(96, MEM_WARN_AT, MEM_CRITICAL_AT)).toBe("warn");
  });
});

describe("Meter — band-colored fill + caption (#2122)", () => {
  function renderAt(now: number) {
    return render(
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        ariaLabel="GPU"
        label="GPU"
        bands={simpleBand("mm-gauge-fill-compact", "var(--accent, var(--good))", now)}
        numerals={{ now, avg: null, max: null }}
        hideAvgMax
        needleAngleDeg={angleForPct(now)}
      />,
    );
  }

  it("79% — quiet: no band-level class on the fill or the caption", () => {
    const { container } = renderAt(79);
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.getAttribute("class")).not.toMatch(/mm-band-/);
    expect(container.querySelector(".meter-now")!.getAttribute("class")).not.toMatch(/mm-band-/);
  });

  it("80% — warn: the fill and the caption both pick up .mm-band-warn", () => {
    const { container } = renderAt(80);
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.classList.contains("mm-band-warn")).toBe(true);
    expect(container.querySelector(".meter-now")!.classList.contains("mm-band-warn")).toBe(true);
  });

  it("95% — critical: the fill and the caption both pick up .mm-band-critical", () => {
    const { container } = renderAt(95);
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.classList.contains("mm-band-critical")).toBe(true);
    expect(container.querySelector(".meter-now")!.classList.contains("mm-band-critical")).toBe(true);
  });

  it("100% — still critical, not some fourth unstyled band", () => {
    const { container } = renderAt(100);
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.classList.contains("mm-band-critical")).toBe(true);
    expect(container.querySelector(".meter-now")!.classList.contains("mm-band-critical")).toBe(true);
  });

  it("a caller passing warnAt/criticalAt overrides the 80/95 default (MEM's own 90/97)", () => {
    const { container } = render(
      <Meter
        wrapperClassName="mm-gauge mm-gauge--compact"
        ariaLabel="MEM"
        label="MEM"
        bands={simpleBand("mm-gauge-fill-compact", "var(--accent, var(--good))", 92)}
        numerals={{ now: 92, avg: null, max: null }}
        hideAvgMax
        warnAt={MEM_WARN_AT}
        criticalAt={MEM_CRITICAL_AT}
      />,
    );
    // 92% is above MEM's own warnAt (90) but below its own criticalAt
    // (97) — warn, not critical, and NOT quiet (which the 80/95 default
    // alone wouldn't distinguish from the default's own warn band).
    const band = container.querySelector(".mm-gauge-fill-compact")!;
    expect(band.classList.contains("mm-band-warn")).toBe(true);
    expect(band.classList.contains("mm-band-critical")).toBe(false);
  });

  it("a non-banded band (VRAM's own bands) never picks up a band-level class regardless of length", () => {
    const { container } = render(
      <Meter
        wrapperClassName="mm-gauge"
        ariaLabel="VRAM"
        bands={[{ className: "mm-gauge-val", stroke: "url(#g)", lengthPct: 100, alwaysRender: true }]}
      />,
    );
    const band = container.querySelector(".mm-gauge-val")!;
    expect(band.getAttribute("class")).not.toMatch(/mm-band-/);
  });
});
