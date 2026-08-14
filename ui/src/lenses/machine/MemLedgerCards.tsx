import { memPct, memStateCls } from "../../lib/format";
import {
  DAEMON_UNREACHABLE_MESSAGE,
  LOADING_MESSAGE,
  STALE_BANNER_TEXT,
  attributionLine,
  machineScale,
  machineTotalText,
  modelLines,
  notLocalMessage,
  perModelScale,
  pressureText,
  stampLine,
} from "./memoryLedgerLines";
import type { MachineResources, MachineResourcesModel } from "../../types/handwritten";

/**
 * (#1806 Stage 1) The `.machine-lens__health` region's structured render —
 * `.memcard` cards, `.membar` four-layer meters, `.memwarn` warnings,
 * `#memstamp` — replacing the flat classified-line-array render this region
 * used before (`Lines classify lines={healthLines(...)}`, since retired).
 * Class names are legacy's own (`crates/darkmux-serve/assets/viewer.html`
 * CSS 274-303, render 1870-1930/4946-4973) so #1806's three `test.fixme`'d
 * e2e specs (`tests/e2e/viewer-machine.spec.js`) un-fixme against this
 * structure without their own assertions changing.
 *
 * **The one constraint every component below is built around: `innerText`
 * stays byte-identical to the retired flat-line render.** Every TEXT
 * fragment here comes from `memoryLedgerLines.ts`'s granular exports, which
 * are straight extractions of what used to be individual `lines.push(...)`
 * calls — same strings, same order, nothing re-derived. That is what keeps
 * `tests/parity/goldens/machine.txt`/`machine-deeplink.txt` green: those
 * goldens compare `.machine-lens__health`'s `innerText` byte-for-byte
 * (`tests/parity/next-parity.spec.ts`'s `machineHealthChromeText`), and
 * `innerText` is LAYOUT-aware — it inserts a line break at every rendered
 * block-level box, so nesting text inside new wrapper elements is only free
 * of drift if every wrapper stays effectively block (see `styles.css`'s
 * machine-lens section for the `display:block;width:fit-content` — never
 * `inline-block` — rule this file's markup depends on) and every VISUAL-ONLY
 * addition (the four bar layers below) carries no text node of its own.
 *
 * `.pot`/`.cur`/`.lim`/`.poolline` are `position:absolute` and never render
 * a text child — verified against legacy's OWN captured golden, which
 * already nests these exact elements between text lines
 * (`renderMachineMemModel`/`memBar`, viewer.html:4946-4973) with zero
 * blank-line drift, so an empty absolutely-positioned layer sitting inside a
 * `.membar` block does not cost this port a phantom blank line either.
 *
 * Honesty rules this structure exists to carry (PROVENANCE.md, `#44`):
 * - **`unknown` is a real, designed state** — the machine's own snapshot is
 *   `unknown` whenever an unpriced resident exists, which is the NORMAL case
 *   here, not an edge case. `memStateCls()` maps it to a real CSS class
 *   (`unknown`), never an absent one, and the chip/bar keep rendering — dim,
 *   never blank.
 * - **An unpriced model has NO committed extent, not a zero-width one.**
 *   `MemBar` only renders `.pot` when `pot != null` — the same guard legacy's
 *   `memBar()` uses (viewer.html:4951) — so "I cannot price this" draws as
 *   an ABSENT layer, never a `width:0` lie.
 * - **Color is never the only channel.** `.membar .cur`'s amber/red/unknown
 *   variants carry a hatch/dot pattern in `styles.css` alongside the hue (the
 *   level-3 mockup's own recipe — `level1.html`'s `.m-cur` rules), and the
 *   state word is always ALSO printed in the `.memstate` chip — a reader who
 *   can't distinguish hue still gets the pattern and the word.
 */

function MemBar({
  cur,
  pot,
  scale,
  stateCls,
  limit,
  poolCap,
}: {
  cur: number | null;
  pot: number | null;
  scale: number;
  stateCls: "green" | "amber" | "red" | "unknown";
  limit?: number | null;
  poolCap?: number | null;
}) {
  return (
    <div className="membar">
      {pot != null && <span className="pot" style={{ width: `${memPct(pot, scale).toFixed(2)}%` }} />}
      {cur != null && <span className={`cur ${stateCls}`} style={{ width: `${memPct(cur, scale).toFixed(2)}%` }} />}
      {limit != null && <span className="lim" style={{ left: `${memPct(limit, scale).toFixed(2)}%` }} />}
      {poolCap != null && poolCap !== limit && <span className="poolline" style={{ left: `${memPct(poolCap, scale).toFixed(2)}%` }} />}
    </div>
  );
}

/** The machine-total card — viewer.html:1898-1903's `machineCard` template.
 * The bar's scale/limit/pool tick come straight from the SAME raw numbers
 * `machineTotalText`'s meta line already formats, so the bar and the text
 * beside it can never disagree about what `limit`/`pool` mean. */
function MachineTotalCard({ resources }: { resources: MachineResources }) {
  const { stateText, metaLine, hint } = machineTotalText(resources);
  const stateCls = memStateCls(resources.machine.state);
  const limit = resources.limit_bytes != null ? Number(resources.limit_bytes) : null;
  const poolCap = resources.pool && resources.pool.capacity_bytes != null ? Number(resources.pool.capacity_bytes) : null;
  const pot = resources.machine.potential_bytes != null ? Number(resources.machine.potential_bytes) : null;
  const cur = resources.machine.current_bytes != null ? Number(resources.machine.current_bytes) : null;
  const scale = machineScale(limit, poolCap, pot, cur);
  return (
    <div className={`memcard ${stateCls}`}>
      <div className="memhdr">
        <div className="memname">machine total</div>
        <div className={`memstate ${stateCls}`}>{stateText}</div>
      </div>
      <MemBar cur={cur} pot={pot} scale={scale} stateCls={stateCls} limit={limit} poolCap={poolCap} />
      <div className="memmeta">{metaLine}</div>
      {hint && <div className="memhint">↳ {hint}</div>}
    </div>
  );
}

/** One per-model card — `renderMachineMemModel()`, viewer.html:4963-4973.
 * `scale` is the shared `perModelScale()` figure so every model's bar reads
 * against the same track (legacy's `perScale`, viewer.html:1891) — a small
 * model's bar stays legible next to a large one instead of each maxing out
 * its own track. No `.lim`/`.poolline` here: legacy's own per-model call
 * passes `limit=null` (viewer.html:1905) — the limit/pool ticks are a
 * MACHINE-level reading, not a per-model one. */
function ModelCard({ m, scale }: { m: MachineResourcesModel; scale: number }) {
  const [identifier, owner, stateText, meta] = modelLines(m);
  // Read the RAW hint (no `↳ ` prefix) rather than `modelLines()`'s own
  // pre-formatted 5th element — that element already carries the prefix
  // baked in (see `modelLines()`'s own doc), and prefixing it again here
  // would print `↳ ↳ …`.
  const hint = (m as { shrink_hint?: string }).shrink_hint;
  const stateCls = memStateCls(m.state);
  const pot = m.potential_bytes != null ? Number(m.potential_bytes) : null;
  const cur = m.current_bytes != null ? Number(m.current_bytes) : null;
  return (
    <div className={`memcard ${stateCls}`}>
      <div className="memhdr">
        <div className="memname">{identifier}</div>
        <div className="memowner">{owner}</div>
        <div className={`memstate ${stateCls}`}>{stateText}</div>
      </div>
      <MemBar cur={cur} pot={pot} scale={scale} stateCls={stateCls} />
      <div className="memmeta">{meta}</div>
      {hint && <div className="memhint">↳ {hint}</div>}
    </div>
  );
}

export interface HealthRegionProps {
  isLocalMach: boolean;
  machineName: string;
  resources: MachineResources | null; // MACHINE_MEM
  resourcesErrored: boolean; // MACHINE_MEM_ERR
}

/** The residency/RAM health-region body — the `healthBody` branch chain,
 * viewer.html:1835-1900, ported to structure instead of a flat line array
 * (see this file's own module doc). Rendered inside
 * `MachineLens.tsx`'s `.machine-lens__health` wrapper, which keeps owning
 * the `data-state` marker. */
export function MachineHealthRegion({ isLocalMach, machineName, resources, resourcesErrored }: HealthRegionProps) {
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
  const models = Array.isArray(b.models) ? b.models : [];
  const scale = perModelScale(models);
  const { red, swapText, compressorText, freeText } = pressureText(b.pressure);
  const warnings = Array.isArray(b.warnings) ? b.warnings : [];

  return (
    <>
      {resourcesErrored && <div className="memwarn">{STALE_BANNER_TEXT}</div>}

      <MachineTotalCard resources={b} />

      {models.length ? (
        models.map((m) => <ModelCard key={m.identifier || m.model_key} m={m} scale={scale} />)
      ) : (
        <div className="none">no models loaded.</div>
      )}

      <div className={`memcard${red ? " red" : ""}`}>
        <div className="memhdr">
          <div className="memname">pressure</div>
          {red && <div className="memstate red">RED</div>}
        </div>
        {/* Flat, not re-paired — see `pressureText()`'s own doc for why k/v
            pairing is out of Stage 1's scope. Reuses the existing
            `mline--label`/`mline--meta` classes (`styles.css`) rather than
            inventing new ones for the same look. */}
        <div className="mline--label">swap used</div>
        <div className="mline--meta">{swapText}</div>
        <div className="mline--label">compressor</div>
        <div className="mline--meta">{compressorText}</div>
        <div className="mline--label">memory free</div>
        <div className="mline--meta">{freeText}</div>
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
        {stampLine(b)}
      </div>
    </>
  );
}
