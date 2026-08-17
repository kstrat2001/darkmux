import { useEffect, useRef, useState } from "react";
import {
  computeGaugeGeometry,
  computeBandGeometry,
  hatchedSegmentDash,
  deriveLamps,
  digitCells,
  gaugeFaceCaption,
  gaugeRampStops,
  sevenSegmentPolygons,
  isSevenSegDot,
  SEVEN_SEG_CELL,
  SEVEN_SEG_GHOST,
  gaugeRampSwatch,
  gaugeValueParts,
  groupResidencyRows,
  isEstimatedRow,
  isOverLimit,
  rowStateDiffers,
  isUtilityTierRow,
  memStateCls,
  modelKvLine,
  odometerTiles,
  perModelScale,
  redlineLit,
  type ResidencyRowView,
} from "./machineGauge";
import { memBytes, reclaimableNote } from "../../lib/format";
import { attributionLine, DAEMON_UNREACHABLE_MESSAGE, LOADING_MESSAGE, limitDescription, notLocalMessage, overPriceHint, stampLine, STALE_BANNER_TEXT } from "./memoryLedgerLines";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

/**
 * (#1806 Stage 2/3 — the machine-lens redesign, `docs/design/machine-lens/proposal.md` in the design
 * packet at the top of this repo's scratch workspace) The health region's
 * hierarchy + level-3 treatment: a bezel-less semicircle hero (the machine
 * gauge, `<Gauge>`), a tell-tale lamp row (`<LampRow>`), odometer digit
 * cells for the pressure instruments (`<Odometer>`), and model rows grouped
 * darkmux-first with ghost/NEW residency states (`<ModelRow>`) — replacing
 * Stage 1's flat `.memcard`/`.membar` ledger (formerly `MemLedgerCards.tsx`;
 * this file's rename to `MachineHealthRegion.tsx` matches what it now
 * builds — a health REGION, not a stack of legacy-shaped cards).
 *
 * All the number-crunching lives in `machineGauge.ts` (pure, unit-tested
 * without a DOM); this file is markup + the honesty rules that markup has
 * to physically enforce:
 *
 * - **Absence, never zero.** `<Gauge>` draws NO commit tick when
 *   `commitPct` is `null` (no models, or Σ priced potential is 0); a model
 *   row draws NO `.mm-row-pot` layer when its OWN `potential_bytes` is
 *   `null` (unpriced) — the same rule Stage 1 pinned (in the component this one replaced),
 *   carried forward rather than re-derived.
 * - **Color is never the only channel.** Every severity-colored element
 *   (the gauge fill, a row's current bar, a lamp) also carries the state
 *   WORD somewhere adjacent, and a hostile/unrecognized state string
 *   degrades through `memStateCls()` to the neutral "unknown" class before
 *   it ever reaches a `className` — never landed raw.
 * - **The redline keys on one server field.** `redlineLit()` /
 *   `gaugeFaceCaption()` read `machine.state` only; `isOverLimit()` is the
 *   one piece of client arithmetic docs/design/machine-lens/proposal.md sanctions (the server's own
 *   over-limit rule applied to two server numbers) and never substitutes
 *   for the server's verdict.
 * - **Stale keeps the last good reading, visibly marked.** `resourcesErrored`
 *   drives the banner + a `saturate(.3) opacity(.72)` desaturation filter on
 *   the whole ledger — it never blanks the numbers (see #1812; the caller,
 *   `MachineLens.tsx`, is what actually keeps the last-good payload across
 *   an errored poll — this component just renders whatever it's handed).
 */

// ── The gauge (SVG semicircle hero) ─────────────────────────────────────

const CX = 120;
const CY = 120;
const R = 86;
const HALF_ARC_D = `M 34 120 A ${R} ${R} 0 0 1 206 120`;

/** The arc ramp's gradient id. One gauge renders per page, so a fixed id is
 * safe; it is named rather than generated so the same string can be asserted
 * in tests and found in the built artifact. */
const RAMP_ID = "mm-gauge-ramp";


/** Where a tick label sits, in the arc's own local geometry — one fixed
 * layout per quarter-tick index (0/25/50/75/100%), matching `level3.html`'s
 * hand-placed label coordinates (a computed trig placement would drift off
 * legible positions at these font sizes; five fixed slots is simpler and
 * exactly matches the canonical mockup). */
const TICK_LABEL_XY: [number, number][] = [
  [20, 136],
  [43, 46],
  [120, 16],
  [197, 46],
  [220, 136],
];

/** The center readout's odometer geometry, in the SVG's own units. The digit
 * cells are laid out by hand rather than by `textAnchor` because the operator's
 * nit was precisely that the old single centered `<text>` was NOT centered: it
 * centered the whole run — number AND unit — so the number itself always sat
 * left of the hub by half the width of " GB". Here the CELLS are centered on
 * `CX` and the unit hangs off the right edge, the way a real odometer's unit
 * plate does, so the figure the eye actually reads is the thing that is
 * centered. `.` gets a narrow cell — a decimal point in a full-width digit cell
 * reads as a blank slot. */
const ODO_CELL_W = 17;
const ODO_DOT_W = 8;
const ODO_GAP = 2.5;
const ODO_TOP = 133;
const ODO_H = 23;
const ODO_BASELINE = ODO_TOP + 16.5;

function odoLayout(chars: string[]): { cells: { ch: string; x: number; w: number }[]; width: number } {
  const widths = chars.map((c) => (c === "." ? ODO_DOT_W : ODO_CELL_W));
  const width = widths.reduce((a, b) => a + b, 0) + ODO_GAP * Math.max(0, widths.length - 1);
  let x = CX - width / 2;
  const cells = chars.map((ch, i) => {
    const cell = { ch, x, w: widths[i] };
    x += widths[i] + ODO_GAP;
    return cell;
  });
  return { cells, width };
}

function Gauge({ resources, stale }: { resources: MachineResources; stale: boolean }) {
  const geo = computeGaugeGeometry(resources);
  const pressureRed = !!resources.pressure?.red;
  const overLimit = isOverLimit(resources.machine.current_bytes, resources.limit_bytes);
  const lit = redlineLit(resources.machine.state) && !stale;
  // The readout shows what the NEEDLE points at — the machine's used memory —
  // not darkmux's share. Those were different subjects on one instrument
  // until the operator caught it: a needle at ~82% beside a readout of
  // 36.8 GiB. darkmux's own figure is named in the caption below, beside the
  // inner ring it belongs to.
  const centerVal = gaugeValueParts(resources.pool?.used_bytes ?? resources.machine.current_bytes);
  const faceCaption = gaugeFaceCaption(resources.machine.state, pressureRed, overLimit);
  // The fill's hue answers "how full", NOT "what did the arbiter decide" —
  // see `gaugeFillSeverity`'s own doc for why that separation is load-bearing.
  const band = computeBandGeometry(resources);
  // Hue follows the MACHINE's fill now, not darkmux's share — the ring it
  // colours is the machine's.

  const odo = odoLayout(digitCells(centerVal.num));

  const committed = gaugeValueParts(resources.machine.potential_bytes);
  // The "% full" clause is gated on a READABLE current, not rendered
  // unconditionally: `memPct(null, scale)` is 0 by design (it exists to never
  // hand a caller NaN), so an unreadable `current_bytes` would otherwise have
  // the screen reader announce "0% full" for the same payload the odometer
  // honestly renders as a single "—" cell. Absence is never zero — including
  // in the channel a sighted reader can't check.
  const scaleVal = gaugeValueParts(geo.scale);
  // The aria narrative describes the SAME band a sighted reader sees, in the
  // same stacked order. Two bugs lived here until #1821's review: the
  // percentage was computed from `geo.pct` — darkmux's share — while the
  // figure beside it was the machine's, so a screen reader heard "87.7 GiB in
  // use (29% full)"; and it still cited "the dashed tick" months after that
  // tick was deleted. The needle-vs-readout defect had survived in the one
  // channel nobody looks at.
  const usedVal = gaugeValueParts(resources.pool?.used_bytes);
  const darkmuxVal = gaugeValueParts(resources.machine.current_bytes);
  const hasUsed = resources.pool?.used_bytes != null;
  const ariaLabel = [
    hasUsed
      ? `Machine memory: ${usedVal.num} ${usedVal.unit} used of the ${scaleVal.num} ${scaleVal.unit} ${geo.scaleWord.toLowerCase()} (${Math.round(band.usedPct)}% full).`
      : `Machine memory: usage unreadable, of the ${scaleVal.num} ${scaleVal.unit} ${geo.scaleWord.toLowerCase()}.`,
    resources.machine.current_bytes != null ? `Of that, darkmux holds ${darkmuxVal.num} ${darkmuxVal.unit}.` : "",
    band.growth.lengthPct > 0
      ? `Committed ${committed.num} ${committed.unit}${resources.machine.unpriced_models ? ` plus ${resources.machine.unpriced_models} unpriced model(s)` : ""}${resources.machine.estimated_models ? ` (${resources.machine.estimated_models} estimated)` : ""}, shown as the hatched extension beyond the needle.`
      : "",
    `State ${resources.machine.state || "unknown"}.`,
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <div className="mm-gauge">
      <svg width="300" height="212" viewBox="0 0 240 170" role="img" aria-label={ariaLabel}>
        {/* The colour ramp lives across the arc's SWEEP, not in any figure
            about the machine — laid across the arc's bounding box in user
            space so it is independent of how much of the arc is filled. */}
        <defs>
          <linearGradient id={RAMP_ID} gradientUnits="userSpaceOnUse" x1={CX - R} y1={0} x2={CX + R} y2={0}>
            {gaugeRampStops().map((s) => (
              <stop key={s.offset} offset={s.offset} stopColor={s.color} />
            ))}
          </linearGradient>
        </defs>
        <path className="mm-gauge-track" d={HALF_ARC_D} fill="none" strokeWidth={11} pathLength={100} />
        {/* ONE STACKED BAND, in scale order: darkmux from 0, everything
            else on top of it ending at the needle, then darkmux's committed
            growth beyond. Stacking is what restores ADDITIVITY — this page's
            question is "will it fit", which is a sum, and two concentric
            rings could show every part but never the total. It also makes
            `other` visible at last: as a span between darkmux's end and the
            needle, its derivedness is self-evident, where as an undrawn gap
            between two radii it was simply missing. */}
        <path
          className="mm-gauge-val"
          stroke={`url(#${RAMP_ID})`}
          d={HALF_ARC_D}
          fill="none"
          strokeWidth={11}
          pathLength={100}
          strokeDasharray={`${band.darkmux.lengthPct} 100`}
        />
        {band.other.lengthPct > 0 && (
          <path
            className="mm-gauge-other"
            stroke={`url(#${RAMP_ID})`}
            d={HALF_ARC_D}
            fill="none"
            strokeWidth={11}
            pathLength={100}
            strokeDasharray={`${band.other.lengthPct} 100`}
            strokeDashoffset={-band.other.startPct}
          />
        )}
        {band.growth.lengthPct > 0 && (
          <path
            className="mm-gauge-growth"
            d={HALF_ARC_D}
            fill="none"
            strokeWidth={11}
            pathLength={100}
            strokeDasharray={hatchedSegmentDash(band.growth.startPct, band.growth.lengthPct)}
          />
        )}
        {geo.ticks.map((t) => (
          <line key={t.pct} className="mm-gauge-tick" x1={30} y1={CY} x2={38} y2={CY} transform={`rotate(${t.pct * 1.8} ${CX} ${CY})`} />
        ))}
        {geo.ticks.map((t, i) => (
          <text key={t.pct} className="mm-gauge-scale-label" x={TICK_LABEL_XY[i][0]} y={TICK_LABEL_XY[i][1]} textAnchor="middle">
            {t.label}
          </text>
        ))}
        <text className="mm-gauge-scale-word" x={220} y={146} textAnchor="middle">
          {geo.scaleWord}
        </text>
        <path
          className={`mm-gauge-redline${lit ? " lit" : ""}`}
          d={HALF_ARC_D}
          fill="none"
          strokeWidth={11}
          pathLength={100}
          strokeDasharray="2.5 100"
          strokeDashoffset="-97.5"
        />
        {/* The needle is deliberately UNCOLORED by state. It used to carry
            `is-${stateCls}`, which on a real machine means `is-unknown` — a dim
            grey needle over a dim grey fill, permanently (provenance finding
            1). Position is the needle's whole job; the fill beside it now
            carries the how-full channel and the lamps carry the verdict, so a
            third, permanently-grey encoding of the same question is subtraction
            rather than information. */}
        <line className="mm-gauge-needle" x1={CX} y1={CY} x2={42} y2={CY} transform={`rotate(${band.needleAngleDeg} ${CX} ${CY})`} />
        <circle className="mm-gauge-hub" cx={CX} cy={CY} r={5} />
        <g className={`mm-gauge-center-val${lit ? " lit" : ""}`}>
          {/* Seven-segment, drawn as polygons in the SAME cell geometry the
              boxed odometer used, so the figure still centres on the hub and
              the unit still sits where it sat. `currentColor` keeps colour
              with the CSS (`.mm-gauge-center-val`) rather than moving it into
              the component — the glyph form is what changed here, not the
              palette. */}
          {odo.cells.map((c, i) =>
            isSevenSegDot(c.ch) ? (
              <circle key={i} cx={c.x + c.w / 2} cy={ODO_TOP + ODO_H - 3.5} r={1.7} fill="currentColor" />
            ) : (
              <g key={i} transform={`translate(${c.x} ${ODO_TOP}) scale(${c.w / SEVEN_SEG_CELL.w} ${ODO_H / SEVEN_SEG_CELL.h})`}>
                {sevenSegmentPolygons(c.ch).map((sg, j) => (
                  <polygon key={j} points={sg.points} fill="currentColor" opacity={sg.lit ? 1 : SEVEN_SEG_GHOST} />
                ))}
              </g>
            ),
          )}
          <text className="mm-gauge-center-unit" x={CX + odo.width / 2 + 5} y={ODO_BASELINE} textAnchor="start">
            {centerVal.unit}
          </text>
        </g>
        {/* Rendered only when there IS a reason — see `gaugeFaceCaption`.
            The slot exists to name which disjunct put the machine in Red,
            beneath the reading where the eye already is; in every other
            state it used to say `IN USE`, restating the one thing a needle
            over a 0→LIMIT scale cannot fail to communicate. */}
        {/* The readout's own subject label. `IN USE` was deleted as noise when
            the dial had ONE subject and the caption restated the obvious.
            With a machine ring and a darkmux ring on one face, naming which
            one the big number belongs to is no longer restatement — it is the
            difference between two readings. */}
        <text className="mm-gauge-readout-label" x={CX} y={164} textAnchor="middle">
          MACHINE USED
        </text>
        {faceCaption && (
          <text className="mm-gauge-center-caption" x={CX} y={176} textAnchor="middle">
            {faceCaption}
          </text>
        )}
      </svg>
    </div>
  );
}

/**
 * The dial's legend. Three bands share one face — the machine's used memory,
 * darkmux's share inside it, and darkmux's committed growth beyond it — and
 * until this existed nothing named any of them. The operator, looking at the
 * finished rings: "a new user will not know what this means."
 *
 * Each entry pairs a SWATCH DRAWN IN THE BAND'S OWN TREATMENT with its figure,
 * so the mapping is visual rather than positional — a reader matches the
 * hatching, not a sentence describing where to look. "Everything else" is
 * deliberately absent: it is the gap between the rings, a derived quantity
 * (`used - darkmux`), and giving it a swatch would present arithmetic as a
 * measured band.
 */
function GaugeLegend({ resources, band }: { resources: MachineResources; band: ReturnType<typeof computeBandGeometry> }) {
  const growth = band.growth.lengthPct > 0;
  const other = resources.pool?.used_bytes != null && resources.machine.current_bytes != null
    ? Math.max(0, Number(resources.pool.used_bytes) - Number(resources.machine.current_bytes))
    : null;
  const projected = resources.pool?.used_bytes != null && growth
    ? Number(resources.pool.used_bytes) + Math.max(0, Number(resources.machine.potential_bytes ?? 0) - Number(resources.machine.current_bytes ?? 0))
    : null;
  return (
    <div className="mm-legend">
      <span className="mm-legend-item">
        <span className="mm-legend-sw" style={{ background: gaugeRampSwatch(0, band.darkmux.lengthPct) }} /> darkmux <b>{memBytes(resources.machine.current_bytes)}</b>
      </span>
      {other != null && (
        <span className="mm-legend-item">
          <span className="mm-legend-sw is-other" style={{ background: gaugeRampSwatch(band.other.startPct, band.usedPct) }} /> other <b>{memBytes(other)}</b>
        </span>
      )}
      {growth && (
        <span className="mm-legend-item">
          {/* Label first, exactly like `darkmux` and `other` above — the
              figure trailing its label is the row's grammar, and this entry
              had it inverted. */}
          <span className="mm-legend-sw is-growth" /> committed +
          <b>{memBytes(Math.max(0, Number(resources.machine.potential_bytes ?? 0) - Number(resources.machine.current_bytes ?? 0)))}</b>
          {projected != null ? <> → <b>{memBytes(projected)}</b></> : null}
        </span>
      )}
    </div>
  );
}


// ── Tell-tale lamp row ───────────────────────────────────────────────────

function LampRow({
  resources,
  resourcesErrored,
  residencyChanged,
}: {
  resources: MachineResources;
  resourcesErrored: boolean;
  residencyChanged: boolean;
}) {
  // #1821: the WARN lamp counts `warn` + `error` severity messages ONLY —
  // an `info` disclosure (the #1819 estimate note) must not light it.
  const alarmMessagesCount = Array.isArray(resources.messages)
    ? resources.messages.filter((m) => m.severity === "warn" || m.severity === "error").length
    : 0;
  const lamps = deriveLamps({
    state: resources.machine.state,
    pressureRed: !!resources.pressure?.red,
    overLimit: isOverLimit(resources.machine.current_bytes, resources.limit_bytes),
    unprivedCount: Number(resources.machine.unpriced_models) || 0,
    alarmMessagesCount,
    resourcesErrored,
    residencyChanged,
  });
  return (
    <div className="mm-lamps">
      {lamps.map((l) => (
        <span key={l.key} className={`mm-lamp${l.lit ? ` is-lit-${l.severity}` : ""}`} title={l.title}>
          {l.word}
        </span>
      ))}
    </div>
  );
}

// ── Odometer tiles ───────────────────────────────────────────────────────

/**
 * The three pressure instruments. Each tile's explanatory note is revealed
 * ON DEMAND behind an `(i)` toggle rather than rendered permanently.
 *
 * The notes used to be a third, always-on line of 8.5px `#4a5162` text under
 * every tile — the operator's read, and it is right: "very small and dim
 * text at the very bottom that adds an extra row under the meters. no one
 * will read all this." Each note says something true and worth knowing
 * exactly ONCE (which figure can trigger red; that swap and compressor are
 * high-water marks; that the compressor is macOS's, not darkmux's
 * compactor), and then it is permanent furniture — the same defect that got
 * the `darkmux/utility` card deleted, one row down.
 *
 * Deliberately a `<button>` that TOGGLES, not a `title` tooltip: this
 * dashboard is read over the tailnet on a phone, where hover does not exist
 * and a `title` is simply invisible. Tap and hover both work on a button,
 * it is keyboard-reachable, and `aria-expanded` states the relationship for
 * a screen reader. The revealed note is the SAME text either way — no
 * desktop-only knowledge.
 */
function Odometer({ resources }: { resources: MachineResources }) {
  const tiles = odometerTiles(resources.pressure);
  const [openLabel, setOpenLabel] = useState<string | null>(null);
  const rowRef = useRef<HTMLDivElement>(null);

  // A popover has to be dismissible by the two gestures every popover
  // supports, or it is just a div that will not go away: Escape, and a click
  // anywhere outside it. Both listeners exist ONLY while one is open — an
  // idle machine page registers nothing (the observer-must-not-perturb rule
  // applies to the client too, and this component re-renders on every 5s
  // poll).
  useEffect(() => {
    if (openLabel == null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpenLabel(null);
    };
    const onDown = (e: MouseEvent) => {
      if (!rowRef.current?.contains(e.target as Node)) setOpenLabel(null);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("mousedown", onDown);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("mousedown", onDown);
    };
  }, [openLabel]);

  return (
    <div className="mm-odorow" ref={rowRef}>
      {tiles.map((t) => {
        const open = openLabel === t.label;
        return (
          <div className="mm-odo" key={t.label}>
            <span className="mm-odo-cells">
              {t.digits.map((d, i) =>
                isSevenSegDot(d) ? (
                  <span className="mm-odo-dot" key={i} />
                ) : (
                  <svg
                    className="mm-odo-seg"
                    key={i}
                    viewBox={`0 0 ${SEVEN_SEG_CELL.w} ${SEVEN_SEG_CELL.h}`}
                    aria-hidden="true"
                  >
                    {sevenSegmentPolygons(d).map((sg, j) => (
                      <polygon key={j} points={sg.points} fill="currentColor" opacity={sg.lit ? 1 : SEVEN_SEG_GHOST} />
                    ))}
                  </svg>
                ),
              )}
              {/* The figure stays available to assistive tech as TEXT — the
                  glyphs above are decorative shapes and a screen reader would
                  otherwise read nothing at all where a number used to be. */}
              <span className="mm-sr-only">{t.digits.join("")}</span>
            </span>
            <span className="mm-odo-unit">{t.unit}</span>
            <div className="mm-odo-k">
              {t.label}{" "}
              <button
                type="button"
                className={`mm-odo-i${open ? " is-open" : ""}`}
                aria-expanded={open}
                aria-label={`What is ${t.label}?`}
                onClick={() => setOpenLabel(open ? null : t.label)}
              >
                i
              </button>
            </div>
            {open && (
              <div className="mm-odo-n" role="note">
                {t.note}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

// ── Model rows ───────────────────────────────────────────────────────────

function relSecondsAgo(nowMs: number, thenMs: number): string {
  const s = Math.max(0, Math.round((nowMs - thenMs) / 1000));
  return `${s}s ago`;
}

function ModelRow({
  row,
  scale,
  nowMs,
  utilityModelId,
  machineState,
}: {
  row: ResidencyRowView;
  scale: number;
  nowMs: number;
  utilityModelId: string | null;
  machineState: string | null | undefined;
}) {
  const m: MachineResourcesModel = row.model;
  const isGhost = row.status === "ghost";
  const isNew = row.status === "new";
  const isUtility = isUtilityTierRow(m.identifier, m.model_key, utilityModelId);
  const stateCls = memStateCls(m.state);
  const pot = m.potential_bytes != null ? Number(m.potential_bytes) : null;
  const cur = m.current_bytes != null ? Number(m.current_bytes) : null;
  const potPct = pot != null && scale ? Math.max(0, Math.min(100, (pot / scale) * 100)) : null;
  const curPct = cur != null && scale ? Math.max(0, Math.min(100, (cur / scale) * 100)) : null;

  const rowCls = ["mm-row", isGhost ? "is-ghost" : "", isNew ? "is-new" : ""].filter(Boolean).join(" ");

  return (
    <div className={rowCls}>
      <div className="mm-row-top">
        <span className="mm-row-name">{m.identifier || m.model_key}</span>
        {/* Identity marker, not a health verdict — deliberately NO severity
            class (green/amber/red): "which resident is the internal tier" is
            identity, and this page spends color only on verified health.
            This badge is ALL that remains of the `darkmux/utility` card that
            used to sit on this page; see `isUtilityTierRow`'s doc for why
            the card was config rather than machine state, and why matching
            on the id alone is correct by construction. The title carries the
            gloss the card used to spend three lines on. */}
        {!isGhost && isUtility && (
          <span className="mm-row-chip is-identity" title="darkmux's internal small-model tier — handles compaction · mission-compile · estimate · scribe">
            utility
          </span>
        )}
        {isGhost && <span className="mm-row-chip is-warn">DEPARTED · last seen {new Date(row.lastSeenMs).toLocaleTimeString([], { hour12: false })}</span>}
        {isNew && <span className="mm-row-chip is-new">NEW · first seen {relSecondsAgo(nowMs, row.firstSeenMs ?? row.lastSeenMs)}</span>}
        {!isGhost && pot == null && <span className="mm-row-chip is-warn">UNPRICED · potential unknown</span>}
        {/* #1819: a row IS priced (pot != null) but by the size-based
            fallback, not a measurement — neither a severity verdict (it
            doesn't say whether the machine is healthy) nor an identity
            marker (it doesn't say WHAT this resident is), so it gets
            neither `.is-state` nor `.is-identity`'s visual language; see
            `.mm-row-chip.is-estimated` in styles.css for the third axis
            this establishes. The title states the assumption the figure
            rests on (dense attention — every layer holds a KV cache —
            which over-reserves hybrid-attention models but UNDER-reserves
            pre-GQA multi-head ones — stated rather than implied, per the
            #1819 merge gate). */}
        {!isGhost && isEstimatedRow(m) && (
          <span
            className="mm-row-chip is-estimated"
            title="priced by size-based estimate, not measurement — neither a readable config.json nor a readable GGUF header (a corrupt or truncated download, an ambiguous multi-file directory, or a weights format neither reader understands). Assumes DENSE attention at a size-tiered rate, set at or above every modern GQA architecture in its size class. Over-reserves hybrid-attention models; under-reserves pre-GQA multi-head models such as Llama-2-13B (#1819, #1820)."
          >
            ESTIMATED
          </span>
        )}
        {/* Renders ONLY when this row disagrees with the machine's verdict —
            see `rowStateDiffers`. A row that agrees is one machine-level
            fact stamped once per row; a row that disagrees (a materialized
            model under machine-amber, or an unpriceable one) is the only
            place that fact exists. On a healthy or uniformly-unknown
            machine, no row renders this at all. */}
        {!isGhost && rowStateDiffers(m.state, machineState) && (
          <span className={`mm-row-chip is-state is-${stateCls}`}>{(m.state || "unknown").toUpperCase()}</span>
        )}
      </div>
      <div
        className="mm-row-track-wrap"
        role="img"
        aria-label={
          isGhost
            ? `${m.identifier || m.model_key}: no longer resident; last observed ${memBytes(cur)} current`
            : pot == null
              ? `${m.identifier || m.model_key}: ${memBytes(cur)} current; unpriced — no committed extent can be computed`
              : `${m.identifier || m.model_key}: ${memBytes(cur)} current of ${memBytes(pot)}${isEstimatedRow(m) ? " estimated" : ""} committed; state ${m.state || "unknown"}`
        }
      >
        <div className={`mm-row-track${isGhost ? " is-empty" : ""}`}>
          {!isGhost && potPct != null && <div className="mm-row-pot" style={{ width: `${potPct.toFixed(2)}%` }} />}
          {!isGhost && curPct != null && (
            <>
              <div className={`mm-row-cur is-${stateCls}`} style={{ width: `${curPct.toFixed(2)}%` }} />
              <span className="mm-row-val" style={{ left: `${curPct.toFixed(2)}%` }}>
                {gaugeValueParts(cur).num}
              </span>
            </>
          )}
        </div>
      </div>
      {isGhost ? (
        <div className="mm-row-kv">
          no longer resident — last observed current <b>{memBytes(cur)}</b> · row retires after the next successful poll
        </div>
      ) : (
        <div className="mm-row-kv">{modelKvLine(m)}</div>
      )}
      {!isGhost && pot == null && (
        <div className="mm-hint">
          ↳ unpriceable: no readable arch facts or catalog size — machine committed total undercounts by this model's commitment
        </div>
      )}
      {!isGhost && isEstimatedRow(m) && (
        <div className="mm-hint">
          ↳ estimated: no readable config.json and no readable GGUF header — priced from catalog size + a size-tiered dense-attention KV rate (every layer assumed to hold a KV cache). Set at or above every modern GQA architecture in its size class; it over-reserves hybrid-attention models, and under-reserves pre-GQA multi-head models like Llama-2-13B
        </div>
      )}
      {/* #1854 — the row this is ABOUT carries the fact (which resident, by
          how much, what the projection now counts); the machine caption one
          altitude up carries only the consequence. Neither repeats the
          other's sentence. A ghost row is excluded like every other hint
          here: its figures are a last observation, not a live claim. */}
      {!isGhost && overPriceHint(m) && <div className="mm-hint">↳ {overPriceHint(m)}</div>}
      {!isGhost && (m as { shrink_hint?: string }).shrink_hint && <div className="mm-hint">↳ {(m as { shrink_hint?: string }).shrink_hint}</div>}
    </div>
  );
}

function ModelRows({
  rows,
  nowMs,
  utilityModelId,
  machineState,
}: {
  rows: ResidencyRowView[];
  nowMs: number;
  utilityModelId: string | null;
  machineState: string | null | undefined;
}) {
  const groups = groupResidencyRows(rows);
  // Shared scale (`perModelScale`'s own doc) — the largest figure across
  // every rendered row, GHOSTS INCLUDED (a departed model's last-observed
  // figures still count for the one poll its row survives, so the shared
  // track doesn't visibly rescale out from under it).
  const scale = perModelScale(rows.map((r) => r.model));
  if (groups.length === 0) {
    return <div className="none">no models loaded.</div>;
  }
  return (
    <>
      {groups.map((g) => (
        <div key={g.key}>
          <div className="mm-grouphdr">
            {g.label} · {g.rows.filter((r) => r.status !== "ghost").length} RESIDENT
            {g.rows.some((r) => r.status === "ghost") ? ` (+${g.rows.filter((r) => r.status === "ghost").length} DEPARTED)` : ""}
          </div>
          {g.rows.map((r) => (
            <ModelRow key={r.identifier} row={r} scale={scale} nowMs={nowMs} utilityModelId={utilityModelId} machineState={machineState} />
          ))}
        </div>
      ))}
    </>
  );
}

// ── Top-level region ─────────────────────────────────────────────────────

export interface HealthRegionProps {
  isLocalMach: boolean;
  machineName: string;
  resources: MachineResources | null; // the last GOOD payload — see #1812
  resourcesErrored: boolean; // the LATEST poll failed (may still have `resources` from an earlier one)
  residencyRows?: ResidencyRowView[];
  residencyChanged?: boolean;
  nowMs?: number;
  /** The RESIDENT utility-tier model's id, or `null` — threaded explicitly
   * from `MachineLens.tsx`'s own `utilityView()` call rather than this
   * component reaching for `specs` itself (this region never otherwise
   * touches `/machine/specs`). `null` covers every non-resident state
   * (not configured, not reported, registered-but-not-loaded) uniformly —
   * see `isUtilityTierRow`'s doc for why the caller pre-filters to just
   * the resident case. */
  utilityModelId?: string | null;
}

export function MachineHealthRegion({
  isLocalMach,
  machineName,
  resources,
  resourcesErrored,
  residencyRows = [],
  residencyChanged = false,
  nowMs = Date.now(),
  utilityModelId = null,
}: HealthRegionProps) {
  if (!isLocalMach) {
    return (
      <div className="memcard">
        <div className="memhdr">
          <div className="memname">residency / RAM</div>
        </div>
        <div className="none">{notLocalMessage(machineName)}</div>
      </div>
    );
  }
  if (resourcesErrored && !resources) {
    return <div className="none">{DAEMON_UNREACHABLE_MESSAGE}</div>;
  }
  if (!resources) {
    return <div className="none">{LOADING_MESSAGE}</div>;
  }

  const b = resources;
  // Computed ONCE for the hero — an earlier cut called this three times per
  // render, on a component that re-renders every 5s poll.
  const bandGeo = computeBandGeometry(resources);
  const stale = resourcesErrored; // a poll failed, but we still have a last-good payload (#1812)
  // #1821: replaces `warnings: string[]` — each entry now carries a
  // severity, rendered below (an `info` disclosure must not look like a
  // `warn`/`error`).
  const messages = Array.isArray(b.messages) ? b.messages : [];

  return (
    <>
      {stale && (
        <div className="mm-stalebanner">
          {STALE_BANNER_TEXT} — snapshot {relSecondsAgo(nowMs, b.generated_at_ms)}
        </div>
      )}

      <div className={`mm-hero${stale ? " is-stale" : ""}`}>
        <div className="mm-heroline">
          <div className="mm-semi">
            <Gauge resources={b} stale={stale} />
            <GaugeLegend resources={b} band={bandGeo} />
          </div>
          <div>
            <LampRow resources={b} resourcesErrored={resourcesErrored} residencyChanged={residencyChanged} />
            <Odometer resources={b} />
          </div>
        </div>
      </div>

      {/* (#1811) The pool figure no longer needs the reconciling ` (128 GiB)`
          parenthetical that used to sit here: `memBytes` is binary now, so
          `hw.memsize` reads `128.00 GiB` on this row and `128 GB` in the stage
          header — the SAME NUMBER, which is what the finding was about. What
          survives is a unit-suffix mismatch (`GB` on a figure `specOf` has
          always computed in binary), and relabelling that one token is gated on
          retiring the machine stage's last byte-exact parity tie to legacy.
          Operator call, still not a drive-by. */}
      <div className="mm-kv mm-kv--machine">
        {/* #1821 (operator-approved naming): this row used to read
            `pool free <memBytes(pool.available_bytes)>` — truly-free pages,
            sitting a few inches from a "% free" pressure tile that measured
            something else entirely (82% margin vs 30.8% truly-free, same
            instant). `used` and `available` now name what they actually
            are; `available` (the colloquial "how much is left" —
            free + inactive + speculative) is the headline figure in the
            slot `pool free` used to occupy. Truly-free pages (`free_bytes`)
            stay in the payload but are deliberately NOT given prime space
            here — two figures both reading as "how much is left" was the
            defect being fixed, not something to preserve under a new name. */}
        limit source <b>{limitDescription(b.limit_source)}</b> · pool <b>{memBytes(b.pool?.capacity_bytes)}</b>{" "}
        · used <b>{memBytes(b.pool?.used_bytes)}</b> · available <b>{memBytes(b.pool?.available_bytes)}</b>
        {reclaimableNote(b.pool?.available_bytes, b.pool?.free_bytes)}{" "}
        · unpriced{" "}
        <b>{Number(b.machine.unpriced_models) || 0} model{Number(b.machine.unpriced_models) === 1 ? "" : "s"}</b>
        {/* #1819: the same row that already discloses the genuinely-unpriced
            count discloses the ESTIMATED count too — a different fact
            (counted, but via a labeled guess, not a measurement), stated
            beside it rather than folded into the same number. Omitted
            entirely when zero, matching the unpriced clause's own
            always-present-but-usually-zero shape being the one exception
            worth keeping (unpriced is a structural row; estimated only
            earns its place on the page when it's actually true). */}
        {Number(b.machine.estimated_models) > 0 && (
          <>
            {" "}
            · estimated{" "}
            <b>
              {b.machine.estimated_models} model{b.machine.estimated_models === 1 ? "" : "s"}
            </b>
          </>
        )}
      </div>

      {/* The machine's OWN shrink hint (distinct from a per-model one — an
          `amber` "Σ potential > limit" verdict names a shrink target at the
          MACHINE level, `model_ledger.rs::shrink_hint`). Stage 1 rendered
          this as `.memhint` under the old flat card; Stage 2/3 has no card
          left to hang it under, so it renders here, right after the detail
          row it's a footnote to. */}
      {(b.machine as { shrink_hint?: string }).shrink_hint && (
        <div className="mm-hint">↳ {(b.machine as { shrink_hint?: string }).shrink_hint}</div>
      )}

      <div className={stale ? "is-stale" : ""}>
        <ModelRows rows={residencyRows} nowMs={nowMs} utilityModelId={utilityModelId} machineState={b.machine.state} />
      </div>

      {/* #1821: `messages` replaces `warnings` — each entry carries a
          severity, and an `info` disclosure (the #1819 estimate note) must
          NOT render with the same alarm treatment as a `warn`/`error`.
          `.memmsg-*` in styles.css keys color+icon off the severity; a
          plain `.memwarn` uniformly-amber treatment is exactly the defect
          this replaces. */}
      {messages.length > 0 && (
        <div className="memcard">
          <div className="memhdr">
            <div className="memname">messages</div>
          </div>
          {messages.map((m, i) => (
            <div className={`memmsg memmsg-${m.severity}`} key={i}>
              {m.severity === "error" ? "✕" : m.severity === "warn" ? "⚠" : "ℹ"} {m.text}
            </div>
          ))}
        </div>
      )}

      <div className="memfoot">{attributionLine(b)}</div>
      <div className="memfoot" id="memstamp">
        snapshot {relSecondsAgo(nowMs, b.generated_at_ms)} · {stampLine(b)}
      </div>
    </>
  );
}
