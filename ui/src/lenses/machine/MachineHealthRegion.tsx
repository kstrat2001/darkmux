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
import { memBytes, reclaimableNote, relAgoFrom } from "../../lib/format";
import { attributionLine, DAEMON_UNREACHABLE_MESSAGE, LOADING_MESSAGE, limitDescription, notLocalMessage, overPriceHint, stampLine, STALE_BANNER_TEXT } from "./memoryLedgerLines";
import { Meter, CX, type MeterBand, type MeterTick } from "../../components/Meter";
import { useIsMobile } from "../../hooks/useIsMobile";
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
//
// `CX`/`CY`/`R`/`HALF_ARC_D` now live in `../../components/Meter` — the
// shared dial geometry every caller (this one included) draws from. See
// that file's own doc for the extraction's full reasoning.

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
  // The band's color is not computed here at all: it comes from the arc
  // ramp (`gaugeRampStops`), which is fixed to the dial and identical on
  // every machine. Nothing about this machine's state can reach it, which is
  // the separation the old bucketed fill only approximated.
  const band = computeBandGeometry(resources);
  // Hue follows the MACHINE's fill now, not darkmux's share — the ring it
  // colors is the machine's.

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

  // (extraction packet) Every VRAM-specific decoration this dial draws,
  // handed to the shared `<Meter>` as data rather than inline JSX — see
  // that component's own doc for the full reasoning. The RENDER OUTPUT
  // this produces is byte-identical to the pre-extraction JSX; only where
  // the markup is now assembled moved.
  const bands: MeterBand[] = [
    // ONE STACKED BAND, in scale order: darkmux from 0, everything else on
    // top of it ending at the needle, then darkmux's committed growth
    // beyond. Stacking is what restores ADDITIVITY — this page's question
    // is "will it fit", which is a sum, and two concentric rings could show
    // every part but never the total. It also makes `other` visible at
    // last: as a span between darkmux's end and the needle, its derivedness
    // is self-evident, where as an undrawn gap between two radii it was
    // simply missing.
    { className: "mm-gauge-val", stroke: `url(#${RAMP_ID})`, lengthPct: band.darkmux.lengthPct, alwaysRender: true },
    { className: "mm-gauge-other", stroke: `url(#${RAMP_ID})`, lengthPct: band.other.lengthPct, startPct: band.other.startPct },
    { className: "mm-gauge-growth", lengthPct: band.growth.lengthPct, hatchedDasharray: hatchedSegmentDash(band.growth.startPct, band.growth.lengthPct) },
  ];
  const ticks: MeterTick[] = geo.ticks.map((t, i) => ({
    pct: t.pct,
    label: t.label,
    labelX: TICK_LABEL_XY[i][0],
    labelY: TICK_LABEL_XY[i][1],
  }));

  return (
    <Meter
      wrapperClassName="mm-gauge"
      ariaLabel={ariaLabel}
      gradient={{ id: RAMP_ID, stops: gaugeRampStops() }}
      bands={bands}
      ticks={ticks}
      scaleWord={geo.scaleWord}
      redline={{ lit }}
      needleAngleDeg={band.needleAngleDeg}
    >
      {/* Seven-segment, drawn as polygons in the SAME cell geometry the
          boxed odometer used, so the figure still centers on the hub and
          the unit still sits where it sat. `currentColor` keeps color
          with the CSS (`.mm-gauge-center-val`) rather than moving it into
          the component — the glyph form is what changed here, not the
          palette. */}
      <g className={`mm-gauge-center-val${lit ? " lit" : ""}`}>
        {odo.cells.map((c, i) =>
          isSevenSegDot(c.ch) ? (
            <circle
              key={i}
              className="mm-gauge-odo-cell"
              cx={c.x + c.w / 2}
              cy={ODO_TOP + ODO_H - 3.5}
              r={1.7}
              fill="currentColor"
            />
          ) : (
            <g
              key={i}
              className="mm-gauge-odo-cell"
              transform={`translate(${c.x} ${ODO_TOP}) scale(${c.w / SEVEN_SEG_CELL.w} ${ODO_H / SEVEN_SEG_CELL.h})`}
            >
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
    </Meter>
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
                  <span className="mm-odo-dot" key={i} aria-hidden="true" />
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

/* A local `relSecondsAgo()` (`round((now-then)/1000)` + "s ago", unbucketed)
 * lived here and fed all three of this region's ages. It rendered the demo
 * fixture's own footer as "snapshot 553169s ago" (operator, 2026-09-05): past
 * about a minute a raw second count stops answering "is this reading current?"
 * and starts asking the reader to do arithmetic.
 *
 * Deleted rather than fixed in place. `lib/format.ts`'s `relAgoFrom` is the
 * coarse past-only formatter (`just now` / `Ns` / `Nm` / `Nh` / `Nd`) this
 * app's OTHER `relAgoFrom` callers already render their ages with — the
 * notes dialog, the drawer's last-sample line and the fleet meta line — and
 * it covers every bucket this region needs, including the seconds bucket the
 * NEW chip lives in. Reusing it, rather than minting a fourth formatter, is
 * the point: two relative-time formatters on one page is two answers for one
 * instant waiting to happen.
 *
 * Stated exactly, because an earlier version of this comment said "the same
 * formatter the fleet strip and the run list already render their ages with"
 * and the run-list half was simply untrue. This app has THREE age formatters,
 * one per surface, and no more (pinned by `lib/format.test.ts`'s
 * "age formatters" count):
 *   1. `lib/format.ts::relAgoFrom`      — this region, the notes dialog,
 *                                          the machine drawer, `metaLine`
 *   2. `lenses/runs/format.ts::runsAgo` — the run list (SECOND-resolution
 *                                          timestamps, and no "just now"
 *                                          bucket: a 3s-old run reads "3s
 *                                          ago", so folding it into
 *                                          `relAgoFrom` would change what
 *                                          that list renders)
 *   3. `components/RecordView.tsx::relTime` — the record panel (parses an
 *                                          ISO string, returns `null` rather
 *                                          than text when it cannot) */

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
  const overHint = overPriceHint(m);
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
        {isNew && <span className="mm-row-chip is-new">NEW · first seen {relAgoFrom(nowMs, row.firstSeenMs ?? row.lastSeenMs)}</span>}
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
        // "row retires after the next successful poll" was cut (operator,
        // 2026-09-05): the DEPARTED chip above already carries a timestamp,
        // and how long a row lingers is the component's own bookkeeping, not
        // a fact about the machine.
        <div className="mm-row-kv">
          no longer resident — last observed current <b>{memBytes(cur)}</b>
        </div>
      ) : (
        <div className="mm-row-kv">{modelKvLine(m)}</div>
      )}
      {/* (operator finding, 2026-09-05) Both hints below were trimmed to the
          one fact the row does NOT already state. The chip beside the name
          already says UNPRICED / ESTIMATED, and its `title` carries the full
          caveat verbatim (which weights reader failed, which attention
          assumption the figure rests on, which architectures it over- and
          under-reserves). What survives here is the CONSEQUENCE — the part a
          reader cannot derive from the chip: that the machine total is short
          by this model, and that this row's potential is a guess rather than
          a measurement. The estimated hint was 52 words of the same content
          the chip title, the lamp title and the server's own `messages` entry
          each carry; the same sentence in four places is furniture. */}
      {!isGhost && pot == null && (
        <div className="mm-hint">↳ unpriceable — machine committed total undercounts by this model</div>
      )}
      {!isGhost && isEstimatedRow(m) && (
        <div className="mm-hint">↳ estimated: priced from catalog size at a dense-attention rate, not measured</div>
      )}
      {/* #1854 — the row this is ABOUT carries the fact (which resident, by
          how much, what the projection now counts); the machine caption one
          altitude up carries only the consequence. Neither repeats the
          other's sentence. A ghost row is excluded like every other hint
          here: its figures are a last observation, not a live claim. */}
      {!isGhost && overHint && <div className="mm-hint">↳ {overHint}</div>}
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
  /** (#2108, operator finding) Test-only override for the mobile/desktop
   * ledger-summary split below — production omits this and measures
   * `window.innerWidth` via `useIsMobile` (see that hook's own doc; the
   * SAME 768px breakpoint `MachineDrawer.tsx`/`PhoneDrawer.tsx` key their
   * own phone skin off). */
  isMobileOverride?: boolean;
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
  isMobileOverride,
}: HealthRegionProps) {
  const measuredIsMobile = useIsMobile();
  const isMobile = isMobileOverride ?? measuredIsMobile;
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
  // Two clocks, not one. `nowMs` is the READING browser's; `generated_at_ms`
  // is the daemon HOST's, and this lens is read off-box over the tailnet by
  // design (#1286 constraint 2 — the display renders off-machine). A reader
  // whose clock sits behind the host makes the delta negative, and
  // `relAgoFrom` renders a negative delta as the empty string: the footer
  // came out as a bare "snapshot" and the banner as "… — snapshot",
  // answering nothing at the exact moment the reader asked "is this current?".
  //
  // Clamped HERE rather than inside `relAgoFrom`, which is shared with
  // callers this fix has no measurement for. Skew is unknowable from inside
  // the browser, so the honest floor is the freshest the reading could be.
  const ageRef = Math.max(nowMs, b.generated_at_ms);

  return (
    <>
      {stale && (
        <div className="mm-stalebanner">
          {STALE_BANNER_TEXT} — snapshot {relAgoFrom(ageRef, b.generated_at_ms)}
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
      {/* (#2108, operator finding) A phone found this row's dotted inline
          form — "limit source physical pool · pool 128.00 GiB · used … ·
          available … (… reclaimable) · unpriced 0 models" — wrapping into
          four ragged lines with the ` · ` separators landing mid-line. On
          a narrow viewport (`useIsMobile`, the SAME 768px breakpoint the
          drawer keys its own phone skin off) this renders the identical
          FACTS as a definition-style list instead — one row per item,
          muted label left, bold value right — rather than one long
          wrapping run; desktop keeps the inline dotted form unchanged.
          Every string is computed ONCE (`unpricedValue`/`estimatedValue`/
          `reclaim` below) and reused by both branches, so the two forms
          can never drift apart on the actual numbers, only on layout. */}
      {(() => {
        const unpricedCount = Number(b.machine.unpriced_models) || 0;
        const estimatedCount = Number(b.machine.estimated_models) || 0;
        const reclaim = reclaimableNote(b.pool?.available_bytes, b.pool?.free_bytes);
        // A non-breaking space, not a plain one: a count severed from its
        // unit ("0" alone on one line, "models" on the next) is the same
        // defect as a label severed from its value (#2000) — see the
        // desktop branch's own historical note below for the wrapping
        // case this originally guarded.
        const unpricedValue = (
          <>
            {unpricedCount}&nbsp;model{unpricedCount === 1 ? "" : "s"}
          </>
        );
        const estimatedValue = (
          <>
            {estimatedCount}&nbsp;model{estimatedCount === 1 ? "" : "s"}
          </>
        );

        if (isMobile) {
          return (
            <div className="mm-kv mm-kv--machine mm-kv--machine-mobile" data-act="machine-detail-rows">
              <div className="mm-kv-row">
                <span className="mm-kv-row__label">limit source</span>
                <span className="mm-kv-row__value">{limitDescription(b.limit_source)}</span>
              </div>
              <div className="mm-kv-row">
                <span className="mm-kv-row__label">pool</span>
                <span className="mm-kv-row__value">{memBytes(b.pool?.capacity_bytes)}</span>
              </div>
              <div className="mm-kv-row">
                <span className="mm-kv-row__label">used</span>
                <span className="mm-kv-row__value">{memBytes(b.pool?.used_bytes)}</span>
              </div>
              <div className="mm-kv-row">
                <span className="mm-kv-row__label">available</span>
                <span className="mm-kv-row__value">{memBytes(b.pool?.available_bytes)}</span>
              </div>
              {/* The overlap parenthetical, as its OWN row under `available`
                  rather than folded onto that row's already-tight two-column
                  line — it is a secondary, explanatory fact, not a fourth
                  column. Trimmed of its wrapping parens/space (desktop's own
                  inline form keeps them, since there it reads as a trailing
                  parenthetical, not a standalone line). */}
              {reclaim && (
                <div className="mm-kv-row mm-kv-row--note">
                  {reclaim.replace(/^\s*\(|\)\s*$/g, "")}
                </div>
              )}
              <div className="mm-kv-row">
                <span className="mm-kv-row__label">unpriced</span>
                <span className="mm-kv-row__value">{unpricedValue}</span>
              </div>
              {estimatedCount > 0 && (
                <div className="mm-kv-row">
                  <span className="mm-kv-row__label">estimated</span>
                  <span className="mm-kv-row__value">{estimatedValue}</span>
                </div>
              )}
            </div>
          );
        }

        return (
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
            {reclaim}{" "}
            · unpriced{" "}
            {/* A non-breaking space, not a plain one: this k/v strip is a flat text
                run with no per-pair element, so the browser may break at ANY space
                in it — and at the wider type scale it chose the one INSIDE this
                value, rendering `unpriced 0` on one line and `models` alone on the
                next. A count severed from its unit is the same defect as a label
                severed from its value (#2000), just produced by inline wrapping
                rather than by a grid.

                Scoped to the counts rather than `white-space: nowrap` on every
                `<b>`: other values in this strip are phrases, not short tokens,
                and must stay breakable or they overflow a phone. */}
            <b>{unpricedValue}</b>
            {/* #1819: the same row that already discloses the genuinely-unpriced
                count discloses the ESTIMATED count too — a different fact
                (counted, but via a labeled guess, not a measurement), stated
                beside it rather than folded into the same number. Omitted
                entirely when zero, matching the unpriced clause's own
                always-present-but-usually-zero shape being the one exception
                worth keeping (unpriced is a structural row; estimated only
                earns its place on the page when it's actually true). */}
            {estimatedCount > 0 && (
              <>
                {" "}
                · estimated <b>{estimatedValue}</b>
              </>
            )}
          </div>
        );
      })()}

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

      {/* (operator finding, 2026-09-05: "is all the prose necessary?") The
          region's ONE disclosure. Freshness stays on screen — "how old is
          this reading" is asked at a glance, and it is three words.
          Everything else in this footer answers "where did the reading come
          from, and what did taking it cost": the attribution note (26 words
          on a real machine) and the observer-cost stamp (gather ms, server
          cache TTL, poll cadence). Those are asked once a month, and they
          were permanent furniture under every figure on the page.

          #1286 constraint 3 — "samplers stamp their own cost into the
          artifact" — is about the PAYLOAD and is untouched: `gather_ms` and
          `cache_ttl_ms` ride every `/machine/resources` response, and
          `#memstamp` still renders them verbatim, one tap away. Record
          exhaustively, display selectively.

          A `<details>` rather than a `title` for the same reason the
          odometer's note is a button and not a tooltip: this page is read
          on a phone over the tailnet, where hover does not exist. */}
      <div className="memfoot">snapshot {relAgoFrom(ageRef, b.generated_at_ms)}</div>
      <details className="mm-about">
        <summary>how this was measured</summary>
        <div className="memfoot">{attributionLine(b)}</div>
        <div className="memfoot" id="memstamp">
          {stampLine(b)}
        </div>
      </details>
    </>
  );
}
