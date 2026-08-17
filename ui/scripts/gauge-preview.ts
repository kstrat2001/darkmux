// Throwaway design harness (#1835 rethink): renders the machine gauge across a
// spread of fill levels and ownership splits, so the arc ramp can be judged in
// more situations than a live machine happens to produce. Imports the REAL
// `gaugeRampStops`/`gaugeFillColor`/`computeBandGeometry`, so the sheet cannot
// drift from what the page draws — a preview built from a copy of the ramp
// would be judging something that is not shipping.
import { gaugeRampStops, gaugeFillColor, gaugeRampSwatch, computeBandGeometry } from "../src/lenses/machine/machineGauge";
import type { MachineResources } from "../src/types/handwritten";

const CX = 120, R = 86;
const HALF_ARC_D = `M 34 120 A ${R} ${R} 0 0 1 206 120`;
const GIB = 1024 ** 3;
const LIMIT_GIB = 128;
const LIMIT = LIMIT_GIB * GIB;

function resources(usedGiB: number, darkmuxGiB: number): MachineResources {
  const used = usedGiB * GIB, dm = darkmuxGiB * GIB;
  return {
    schema_version: "2.2", generated_at_ms: 1, gather_ms: 1,
    limit_bytes: LIMIT, limit_source: "physical_pool",
    pool: { capacity_bytes: LIMIT, used_bytes: used, available_bytes: LIMIT - used, free_bytes: 1 },
    pressure: { swap_used_bytes: 0, compressor_bytes: 0, margin_percent: 90, red: false },
    models: [],
    machine: { potential_bytes: dm, unpriced_models: 0, estimated_models: 0, current_bytes: dm, state: "green" },
    attribution: "per_process", messages: [], cache_ttl_ms: 2000,
  } as unknown as MachineResources;
}

// COSINE-spaced — correct for an arc under a horizontal gradient.
const arcStops = gaugeRampStops()
  .map((s) => `<stop offset="${s.offset}" stop-color="${s.color}"/>`).join("");

// LINEAR-spaced — correct for a straight bar. Same ramp, different geometry;
// using the arc's own stops here would bunch the colours toward the ends.
const barStops = Array.from({ length: 25 }, (_, i) => i / 24)
  .map((t) => `<stop offset="${t}" stop-color="${gaugeFillColor(t * 100)}"/>`).join("");

function gauge(usedGiB: number, darkmuxGiB: number, id: string): string {
  const b = computeBandGeometry(resources(usedGiB, darkmuxGiB));
  const otherGiB = usedGiB - darkmuxGiB;
  return `<figure class="card">
  <svg viewBox="0 0 240 152" role="img" aria-label="${usedGiB.toFixed(1)} GiB of ${LIMIT_GIB} GiB used">
    <defs><linearGradient id="${id}" gradientUnits="userSpaceOnUse" x1="${CX - R}" y1="0" x2="${CX + R}" y2="0">${arcStops}</linearGradient></defs>
    <path d="${HALF_ARC_D}" fill="none" stroke="#1a2030" stroke-width="11" pathLength="100"/>
    <path d="${HALF_ARC_D}" fill="none" stroke="url(#${id})" stroke-width="11" pathLength="100"
          stroke-dasharray="${b.darkmux.lengthPct} 100"/>
    <path d="${HALF_ARC_D}" fill="none" stroke="url(#${id})" stroke-width="11" pathLength="100" opacity="0.32"
          stroke-dasharray="${b.other.lengthPct} 100" stroke-dashoffset="${-b.other.startPct}"/>
    <line x1="120" y1="120" x2="44" y2="120" stroke="#e6e8ee" stroke-width="2" stroke-linecap="round"
          transform="rotate(${b.needleAngleDeg} 120 120)"/>
    <circle cx="120" cy="120" r="5" fill="#5b6478"/>
    <text x="120" y="146" text-anchor="middle" class="dial-fig">${usedGiB.toFixed(1)}<tspan class="dial-unit"> GiB</tspan></text>
  </svg>
  <figcaption>
    <div class="capline"><span class="pct">${b.usedPct.toFixed(0)}%</span> of ${LIMIT_GIB} GiB</div>
    <div class="sw">
      <span><i style="background:${gaugeRampSwatch(0, b.darkmux.lengthPct)}"></i>darkmux ${darkmuxGiB.toFixed(1)}</span>
      <span><i style="background:${gaugeRampSwatch(b.other.startPct, b.usedPct)};opacity:.32"></i>other ${otherGiB.toFixed(1)}</span>
    </div>
  </figcaption>
</figure>`;
}


// ── Seven-segment readout ────────────────────────────────────────────────
// Drawn as SVG polygons rather than an embedded font: no licensing, no
// webfont bytes, and it gets the detail that actually sells the look —
// UNLIT segments rendered faintly, the way a real LCD shows its whole
// character cell. A font can only draw the lit ones.
const SEG_ON: Record<string, string> = {
  "0": "abcdef", "1": "bc", "2": "abged", "3": "abgcd", "4": "fgbc",
  "5": "afgcd", "6": "afgedc", "7": "abc", "8": "abcdefg", "9": "abfgcd",
  "-": "g", " ": "",
};
const HBAR = (cy: number) => `12,${cy} 17,${cy - 5} 43,${cy - 5} 48,${cy} 43,${cy + 5} 17,${cy + 5}`;
const VBAR = (cx: number, y1: number, y2: number) =>
  `${cx},${y1} ${cx + 5},${y1 + 5} ${cx + 5},${y2 - 5} ${cx},${y2} ${cx - 5},${y2 - 5} ${cx - 5},${y1 + 5}`;
const SEG_D: Record<string, string> = {
  a: HBAR(6), g: HBAR(50), d: HBAR(94),
  f: VBAR(6, 12, 44), b: VBAR(54, 12, 44),
  e: VBAR(6, 56, 88), c: VBAR(54, 56, 88),
};

function sevenSeg(text: string, color: string, ghost = 0.08): string {
  const cells = text.split("").map((ch) => {
    if (ch === ".") return `<span class="seg-dot" style="background:${color}"></span>`;
    const on = SEG_ON[ch] ?? "";
    const segs = Object.entries(SEG_D).map(([k, d]) =>
      `<polygon points="${d}" fill="${color}" opacity="${on.includes(k) ? 1 : ghost}"/>`).join("");
    return `<svg class="seg-cell" viewBox="0 0 60 100">${segs}</svg>`;
  }).join("");
  return `<span class="seg">${cells}</span>`;
}

function odometer(text: string): string {
  return `<span class="odo">${text.split("").map((c) => `<span class="odo-c">${c}</span>`).join("")}</span>`;
}

function plain(text: string): string {
  return `<span class="plainfig">${text}</span>`;
}


const GHOSTS: [number, string][] = [
  [0.08, "8% — full character cell"],
  [0.045, "4.5% — barely there"],
  [0.02, "2% — a texture, not a shape"],
  [0, "0% — lit segments only"],
];

const READOUTS: [string, string, string][] = [
  ["110.6", "GiB", "machine used"],
  ["92", "% margin", "margin"],
  ["6.87", "GiB", "swap used"],
  ["4.30", "GiB", "compressor"],
];

const sweep = [4, 16, 32, 48, 64, 80, 96, 110, 122, 128];
const splits: [number, number][] = [[64, 6], [64, 32], [64, 58], [110, 12], [110, 55], [110, 100]];
const ticks = [0, 32, 64, 96, 128];

console.log(`<title>Machine Gauge Ramp</title>
<style>
  /* Deliberately single-theme. This sheet is a replica of a dark instrument
     panel and its entire job is judging colours against the ground the viewer
     actually paints them on; rendering it light would defeat the purpose.
     Every colour is stated explicitly so the page holds on either host. */
  :root {
    --bg: #0b0e14; --panel: #121722; --border: #232838;
    --fg: #e6e8ee; --dim: #8b93a7; --accent: #4fd1c5;
    --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, monospace;
    --sans: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 28px 22px 64px; background: var(--bg); color: var(--fg);
    font-family: var(--mono); font-size: 13px; line-height: 1.55;
    font-variant-numeric: tabular-nums;
  }
  .wrap { max-width: 1080px; margin: 0 auto; display: flex; flex-direction: column; gap: 40px; }
  header { display: flex; flex-direction: column; gap: 10px; }
  .brand { font-size: 15px; font-weight: 700; letter-spacing: 0.04em; color: var(--accent); }
  h1 { font-size: 21px; margin: 0; font-weight: 600; text-wrap: balance; letter-spacing: -0.01em; }
  p.note {
    font-family: var(--sans); font-size: 13.5px; color: var(--dim);
    max-width: 66ch; margin: 0;
  }
  p.note code { font-family: var(--mono); font-size: 12.5px; color: var(--fg); }
  section { display: flex; flex-direction: column; gap: 14px; }
  h2 {
    font-size: 11px; letter-spacing: 0.14em; text-transform: uppercase;
    color: var(--dim); font-weight: 600; margin: 0;
    padding-bottom: 9px; border-bottom: 1px solid var(--border);
  }
  .grid { display: grid; gap: 14px; grid-template-columns: repeat(auto-fill, minmax(212px, 1fr)); }
  .card {
    margin: 0; background: var(--panel); border: 1px solid var(--border);
    padding: 10px 10px 12px; display: flex; flex-direction: column; gap: 6px;
  }
  .card svg { width: 100%; height: auto; display: block; }
  .dial-fig { fill: var(--fg); font-family: var(--mono); font-size: 17px; font-weight: 600; }
  .dial-unit { fill: var(--dim); font-size: 10px; }
  figcaption { display: flex; flex-direction: column; gap: 5px; font-size: 10.5px; color: var(--dim); }
  .capline { text-align: center; }
  .pct { color: var(--fg); font-weight: 600; }
  .sw { display: flex; flex-direction: column; gap: 3px; }
  .sw span { display: flex; align-items: center; gap: 6px; }
  .sw i { width: 16px; height: 7px; flex: none; display: inline-block; }
  .refbar { display: flex; flex-direction: column; gap: 0; }
  .refbar svg { width: 100%; height: auto; display: block; }
  .reftick { fill: var(--dim); font-family: var(--mono); font-size: 9px; }
  /* Readout comparison */
  .rowset { display: flex; flex-direction: column; gap: 12px; }
  .rrow { display: grid; grid-template-columns: 1fr; gap: 10px; }
  @media (min-width: 720px) { .rrow { grid-template-columns: repeat(3, 1fr); } }
  .rcell { background: var(--panel); border: 1px solid var(--border); padding: 12px 14px;
           display: flex; flex-direction: column; gap: 8px; align-items: center; }
  .rlabel { font-size: 9.5px; letter-spacing: 0.14em; text-transform: uppercase; color: var(--dim); }
  .runit { font-size: 10px; color: var(--dim); letter-spacing: 0.08em; }
  .figrow { display: flex; align-items: baseline; gap: 7px; min-height: 40px; }
  .seg { display: inline-flex; align-items: flex-end; gap: 3px; }
  .seg-cell { width: 22px; height: 37px; display: block; }
  .seg-dot { width: 6px; height: 6px; border-radius: 50%; display: inline-block; margin: 0 1px 3px; }
  .odo { display: inline-flex; gap: 3px; }
  .odo-c { min-width: 22px; height: 37px; display: grid; place-items: center; font-size: 24px;
           font-weight: 600; background: #1a2030; border: 1px solid var(--border); }
  .plainfig { font-size: 30px; font-weight: 600; letter-spacing: -0.02em; }
  .small .seg-cell { width: 13px; height: 22px; }
  .small .odo-c { min-width: 14px; height: 22px; font-size: 14px; }
  .small .plainfig { font-size: 18px; }
  .small .figrow { min-height: 24px; }
  .ghostgrid { display: grid; gap: 12px; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); }
  .gcell { background: var(--panel); border: 1px solid var(--border); padding: 14px 12px;
           display: flex; flex-direction: column; gap: 10px; align-items: center; }
  .gcell .figrow { min-height: 40px; }
  .gsmall { display: flex; align-items: baseline; gap: 6px; }
  .glabel { font-size: 9.5px; letter-spacing: 0.1em; text-transform: uppercase; color: var(--dim);
            text-align: center; }
  .gsmall .seg-cell { width: 13px; height: 22px; }


</style>
<div class="wrap">
  <header>
    <div class="brand">darkmux</div>
    <h1>Machine gauge — the ramp lives on the arc</h1>
    <p class="note">Every dial below is drawn by the shipped
      <code>gaugeRampStops()</code> and <code>computeBandGeometry()</code>, so this sheet
      cannot drift from the page. The ramp is fixed to the dial — green at 0, amber at
      mid-scale, red at the limit — and is identical on every machine and every poll.
      The filled band only reveals its own slice of it, so nothing here is a verdict.</p>
  </header>

  <section>
    <h2>The ramp, and where its stops land</h2>
    <div class="refbar">
      <svg viewBox="0 0 480 46" role="img" aria-label="Colour ramp from green at 0 to red at 128 GiB">
        <defs><linearGradient id="bar" x1="0" y1="0" x2="1" y2="0">${barStops}</linearGradient></defs>
        <rect x="8" y="6" width="464" height="14" fill="url(#bar)"/>
        ${ticks.map((t) => {
          const x = 8 + (t / LIMIT_GIB) * 464;
          return `<line x1="${x}" y1="20" x2="${x}" y2="26" stroke="#8b93a7" stroke-width="1"/>
                  <text x="${x}" y="38" text-anchor="middle" class="reftick">${t}</text>`;
        }).join("")}
      </svg>
    </div>
    <p class="note">Shown straight here, the ramp's stops are evenly spaced. On the arc they
      are <em>not</em>: a horizontal gradient interpolates along X while the arc advances by
      angle, related by <code>x = cx − r·cos(pct·π)</code>. Each arc stop sits at
      <code>(1 − cos(pct·π)) / 2</code>, which is why amber lands exactly on the 64 tick
      below rather than drifting off it. Check any dial against its own tick marks.</p>
  </section>


  <section>
    <h2>Readout style · three candidates</h2>
    <p class="note">Boxed cells read as a mechanical odometer — a counter, and mostly a
      museum piece. Seven-segment reads as an instrument, which is what sits above it on
      this page, and it is still the vernacular of every microwave and dashboard. Plain
      figures make no period reference at all. Hero size first, then the size the pressure
      tiles actually render at, because that is where seven-segment either survives or
      doesn't.</p>
    <div class="rowset">
      ${READOUTS.map(([fig, unit, label], i) => `
      <div class="rrow${i === 0 ? "" : " small"}">
        <div class="rcell"><div class="figrow">${odometer(fig)}<span class="runit">${unit}</span></div>
          <div class="rlabel">${label} · odometer</div></div>
        <div class="rcell"><div class="figrow">${sevenSeg(fig, "#4fd1c5")}<span class="runit">${unit}</span></div>
          <div class="rlabel">${label} · seven-segment</div></div>
        <div class="rcell"><div class="figrow">${plain(fig)}<span class="runit">${unit}</span></div>
          <div class="rlabel">${label} · plain</div></div>
      </div>`).join("")}
    </div>
  </section>


  <section>
    <h2>Seven-segment · how much of the unlit cell to show</h2>
    <p class="note">A real display shows its whole character cell, lit or not — that ghosting
      is what separates a display from a typeface. But it is also the part that turns to
      visual noise at small sizes. Each card shows the hero figure and, beneath it, the same
      treatment at pressure-tile size, which is where the decision actually bites.</p>
    <div class="ghostgrid">
      ${GHOSTS.map(([g, label]) => `
      <div class="gcell">
        <div class="figrow">${sevenSeg("110.6", "#4fd1c5", g)}</div>
        <div class="gsmall">${sevenSeg("92", "#4fd1c5", g)}<span class="runit">% margin</span></div>
        <div class="glabel">${label}</div>
      </div>`).join("")}
    </div>
  </section>

  <section>
    <h2>Fill sweep · darkmux holding a quarter</h2>
    <div class="grid">${sweep.map((g, i) => gauge(g, g * 0.25, `r${i}`)).join("")}</div>
  </section>

  <section>
    <h2>Same fill, different ownership split</h2>
    <p class="note">The open question. With hue carried by position rather than ownership,
      darkmux's band and everyone else's are separated only by 32% dimming — plus the fact
      that darkmux always starts at 0, so it always begins green while <em>other</em> stacks
      into the hot end. These six are where that either holds up or doesn't.</p>
    <div class="grid">${splits.map(([u, d], i) => gauge(u, d, `s${i}`)).join("")}</div>
  </section>
</div>`);
