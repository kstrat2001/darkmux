import {
  computeGaugeGeometry,
  deriveLamps,
  gaugeFaceCaption,
  gaugeValueParts,
  groupResidencyRows,
  isOverLimit,
  memStateCls,
  modelKvLine,
  odometerTiles,
  perModelScale,
  redlineLit,
  type ResidencyRowView,
} from "./machineGauge";
import { memBytes } from "../../lib/format";
import { attributionLine, DAEMON_UNREACHABLE_MESSAGE, LOADING_MESSAGE, limitDescription, notLocalMessage, stampLine, STALE_BANNER_TEXT } from "./memoryLedgerLines";
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

function Gauge({ resources, stale }: { resources: MachineResources; stale: boolean }) {
  const geo = computeGaugeGeometry(resources);
  const pressureRed = !!resources.pressure?.red;
  const overLimit = isOverLimit(resources.machine.current_bytes, resources.limit_bytes);
  const lit = redlineLit(resources.machine.state) && !stale;
  const stateCls = memStateCls(resources.machine.state);
  const centerVal = gaugeValueParts(resources.machine.current_bytes);
  const faceCaption = gaugeFaceCaption(resources.machine.state, pressureRed, overLimit);

  const ariaLabel = `Machine memory: ${centerVal.num} ${centerVal.unit} in use of the ${gaugeValueParts(geo.scale).num} ${gaugeValueParts(geo.scale).unit} ${geo.scaleWord.toLowerCase()}. ${
    geo.commitPct != null ? `Committed ${gaugeValueParts(resources.machine.potential_bytes).num} GB${resources.machine.unpriced_models ? ` plus ${resources.machine.unpriced_models} unpriced model(s)` : ""}, marked by the dashed tick. ` : ""
  }State ${resources.machine.state || "unknown"}.`;

  return (
    <div className="mm-gauge">
      <svg width="300" height="212" viewBox="0 0 240 170" role="img" aria-label={ariaLabel}>
        <path className="mm-gauge-track" d={HALF_ARC_D} fill="none" strokeWidth={11} pathLength={100} />
        <path
          className={`mm-gauge-val is-${stateCls}`}
          d={HALF_ARC_D}
          fill="none"
          strokeWidth={11}
          pathLength={100}
          strokeDasharray={`${geo.pct} 100`}
        />
        {geo.commitAngleDeg != null && (
          <line
            className={`mm-gauge-commit${geo.overcommitted ? " over" : ""}`}
            x1={26}
            y1={CY}
            x2={44}
            y2={CY}
            transform={`rotate(${geo.commitAngleDeg} ${CX} ${CY})`}
          />
        )}
        {geo.ticks.map((t) => (
          <line
            key={t.pct}
            className="mm-gauge-tick"
            x1={30}
            y1={CY}
            x2={38}
            y2={CY}
            transform={`rotate(${t.pct * 1.8} ${CX} ${CY})`}
          />
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
        <line
          className={`mm-gauge-needle is-${stateCls}`}
          x1={CX}
          y1={CY}
          x2={52}
          y2={CY}
          transform={`rotate(${geo.needleAngleDeg} ${CX} ${CY})`}
        />
        <circle className="mm-gauge-hub" cx={CX} cy={CY} r={5} />
        <text className={`mm-gauge-center-val${lit ? " lit" : ""}`} x={CX} y={150} textAnchor="middle">
          {centerVal.num} <tspan className="mm-gauge-center-unit">{centerVal.unit}</tspan>
        </text>
        <text className="mm-gauge-center-caption" x={CX} y={164} textAnchor="middle">
          {faceCaption}
        </text>
      </svg>
    </div>
  );
}

function GaugeCaption({ resources }: { resources: MachineResources }) {
  const stateCls = memStateCls(resources.machine.state);
  const stateText = (resources.machine.state || "unknown").toUpperCase();
  const unpriced = Number(resources.machine.unpriced_models) || 0;
  const committed = memBytes(resources.machine.potential_bytes);
  return (
    <div className="mm-gcap">
      <b>machine total</b> <span className={`mm-chip is-${stateCls}`}>{stateText}</span> · ╌ committed {committed}
      {unpriced ? ` (+${unpriced} unpriced)` : ""}
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
  const lamps = deriveLamps({
    state: resources.machine.state,
    pressureRed: !!resources.pressure?.red,
    overLimit: isOverLimit(resources.machine.current_bytes, resources.limit_bytes),
    unprivedCount: Number(resources.machine.unpriced_models) || 0,
    warningsCount: Array.isArray(resources.warnings) ? resources.warnings.length : 0,
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

function Odometer({ resources }: { resources: MachineResources }) {
  const tiles = odometerTiles(resources.pressure);
  return (
    <div className="mm-odorow">
      {tiles.map((t) => (
        <div className="mm-odo" key={t.label}>
          <span className="mm-odo-cells">
            {t.digits.map((d, i) => (
              <span className="mm-odo-c" key={i}>
                {d}
              </span>
            ))}
          </span>
          <span className="mm-odo-unit">{t.unit}</span>
          <div className="mm-odo-k">{t.label}</div>
          <div className="mm-odo-n">{t.note}</div>
        </div>
      ))}
    </div>
  );
}

// ── Model rows ───────────────────────────────────────────────────────────

function relSecondsAgo(nowMs: number, thenMs: number): string {
  const s = Math.max(0, Math.round((nowMs - thenMs) / 1000));
  return `${s}s ago`;
}

function ModelRow({ row, scale, nowMs }: { row: ResidencyRowView; scale: number; nowMs: number }) {
  const m: MachineResourcesModel = row.model;
  const isGhost = row.status === "ghost";
  const isNew = row.status === "new";
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
        {isGhost && <span className="mm-row-chip is-warn">DEPARTED · last seen {new Date(row.lastSeenMs).toLocaleTimeString([], { hour12: false })}</span>}
        {isNew && <span className="mm-row-chip is-new">NEW · first seen {relSecondsAgo(nowMs, row.firstSeenMs ?? row.lastSeenMs)}</span>}
        {!isGhost && pot == null && <span className="mm-row-chip is-warn">UNPRICED · potential unknown</span>}
        {!isGhost && <span className={`mm-row-chip is-state is-${stateCls}`}>{(m.state || "unknown").toUpperCase()}</span>}
      </div>
      <div
        className="mm-row-track-wrap"
        role="img"
        aria-label={
          isGhost
            ? `${m.identifier || m.model_key}: no longer resident; last observed ${memBytes(cur)} current`
            : pot == null
              ? `${m.identifier || m.model_key}: ${memBytes(cur)} current; unpriced — no committed extent can be computed`
              : `${m.identifier || m.model_key}: ${memBytes(cur)} current of ${memBytes(pot)} committed; state ${m.state || "unknown"}`
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
      {!isGhost && (m as { shrink_hint?: string }).shrink_hint && <div className="mm-hint">↳ {(m as { shrink_hint?: string }).shrink_hint}</div>}
    </div>
  );
}

function ModelRows({ rows, nowMs }: { rows: ResidencyRowView[]; nowMs: number }) {
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
            <ModelRow key={r.identifier} row={r} scale={scale} nowMs={nowMs} />
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
}

export function MachineHealthRegion({
  isLocalMach,
  machineName,
  resources,
  resourcesErrored,
  residencyRows = [],
  residencyChanged = false,
  nowMs = Date.now(),
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
  const stale = resourcesErrored; // a poll failed, but we still have a last-good payload (#1812)
  const warnings = Array.isArray(b.warnings) ? b.warnings : [];

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
            <GaugeCaption resources={b} />
          </div>
          <div>
            <LampRow resources={b} resourcesErrored={resourcesErrored} residencyChanged={residencyChanged} />
            <Odometer resources={b} />
          </div>
        </div>
      </div>

      <div className="mm-kv mm-kv--machine">
        limit source <b>{limitDescription(b.limit_source)}</b> · pool <b>{memBytes(b.pool?.capacity_bytes)}</b> · pool free{" "}
        <b>{memBytes(b.pool?.available_bytes)}</b> · unpriced <b>{Number(b.machine.unpriced_models) || 0} model{Number(b.machine.unpriced_models) === 1 ? "" : "s"}</b>
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
        <ModelRows rows={residencyRows} nowMs={nowMs} />
      </div>

      {warnings.length > 0 && (
        <div className="memcard">
          <div className="memhdr">
            <div className="memname">warnings</div>
          </div>
          {warnings.map((w, i) => (
            <div className="memwarn" key={i}>
              ⚠ {w}
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
