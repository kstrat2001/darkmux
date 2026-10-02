/**
 * Byte-for-byte ports of `viewer.html`'s formatting helpers (the machine
 * lens is the first consumer; future lenses should import from here rather
 * than re-deriving these). Each function is named at its legacy source line
 * so a drift audit can diff them directly.
 *
 * Deliberately NOT using CSS `text-transform: uppercase` to reproduce
 * legacy's `.memowner`/`.memstate`/`.rglbl` styling (see `lenses/machine/`'s
 * module docs) — the parity harness extracts `innerText`, which DOES honor
 * CSS text-transform in a real browser, but keying the extracted text on a
 * stylesheet rule the port is free to change (per the packet brief: "visual
 * styling may differ, text content must match") is fragile. These helpers
 * uppercase the STRING directly so the text is correct independent of CSS.
 */

/** THE duration formatter — `M:SS`, rolling over to `H:MM:SS` past an hour.
 * Floored, never negative, and {@link MISSING} for a non-finite input.
 *
 * (U3-7/U5-2) There were TWO: this one (`fmtElapsed`, from
 * `lenses/mission/graph.ts`, itself `mission-graph.html`'s own) and a
 * `fmtDuration` (`fmt()`) with NO hour rollover, which
 * rendered a 75-minute run as "75:23". `lenses/session/sessionRun.ts`
 * formatted a dispatch's WALL CLOCK with the latter, and a dispatch running
 * over an hour is ordinary (the operator's own #2346 run: 1h54m). One
 * formatter now, the one that stays correct past 60 minutes; identical
 * output below an hour, so every parity golden is unchanged.
 *
 * Kept HERE rather than in `graph.ts` because it is not mission-graph
 * vocabulary — the run detail, the scrubber clock and the step rows all
 * format the same concept. */
export const MISSING = "\u2014";

export function fmtElapsed(ms: number): string {
  // (C4) A duration that could not be COMPUTED is not a duration of zero.
  // `NaN` reaches here from production: `lenses/session/sessionRun.ts`'s
  // `runWallMs` is NaN when the terminal record has neither a `wall_ms` nor
  // a parsed time (`tMs === null`) — which used to render "0:00", asserting that a
  // run took no time. The rest of that same tile row (TURNS, TOKENS IN,
  // CTX) already renders "—" for an absent number; this joins it.
  // A NEGATIVE duration still clamps to 0:00: it is computable, just skewed
  // (a terminal timestamped before its own start), and the surrounding code
  // already treats that as zero rather than unknown.
  if (!Number.isFinite(ms)) return MISSING;
  const clamped = !ms || ms < 0 ? 0 : ms;
  const s = Math.floor(clamped / 1000);
  const m = Math.floor(s / 60);
  const hr = Math.floor(m / 60);
  const ss = String(s % 60).padStart(2, "0");
  if (hr > 0) return hr + ":" + String(m % 60).padStart(2, "0") + ":" + ss;
  return m + ":" + ss;
}

/** `clk()`. Time-of-day in the browser's local timezone
 * (the parity harness's Playwright context pins `timezoneId: 'UTC'`, same
 * as the legacy extraction, so both resolve identically under test). */
export function clk(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour12: false });
}

/** A record's time of day, from its parsed `tMs`: a record with no usable
 *  timestamp reads as a same-width placeholder rather than "Invalid Date". */
export function clkAt(t: number | null): string {
  return t === null ? "--:--:--" : clk(t);
}

/** `clkhm()`. `HH:MM` local, no seconds — the fleet
 * activity-timeline axis labels. */
export function clkhm(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}

/** `lday()`. Local DATE, no time. Ported with #1800's
 * replay meta line, the only surface that names a calendar day: a live view
 * says "LIVE" in the masthead pill (#2412), a replay states the actual date
 * its records came from. Same locale-dependence as `clk` above — the
 * harness pins the timezone for both sides. */
export function lday(t: number): string {
  return new Date(t).toLocaleDateString();
}

/** `sameDay()`. */
function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

/** `clkrange()` (#1530 dogfood). A time-only formatter
 * can't distinguish two instants exactly 24h apart, so a same-day range
 * stays bare `HH:MM:SS–HH:MM:SS`; a window straddling a day boundary
 * prefixes each end with its short date ("Aug 7 16:40:59–Aug 8 16:40:59"). */
export function clkrange(a: number, b: number): string {
  if (sameDay(a, b)) return `${clk(a)}–${clk(b)}`;
  const d = (t: number) => new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
  return `${d(a)} ${clk(a)}–${d(b)} ${clk(b)}`;
}

/** `relAgoFrom()`. Coarse past-only relative time.
 * `<5s` reads as "just now"; note this is NOT the same threshold as
 * `<60s` — 5-59s renders as "Ns ago", a real bucket the machine lens's own
 * "just now" row (`ref===t`) never hits but a future corpus could. */
export function relAgoFrom(ref: number, t: number): string {
  const d = ref - t;
  if (d < 0) return "";
  const s = Math.floor(d / 1000);
  if (s < 60) return s < 5 ? "just now" : `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/** `fmtN()`. Thousands-grouped integer. */
export function fmtN(n: number): string {
  return Math.round(n)
    .toString()
    .replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** `fmtC()` — compact token count (1.2M / 29.18k / 984).
 *
 * (#2842) The thousands arm keeps TWO decimals. It used to round to the
 * nearest thousand, which made the smallest distinguishable step 1,000
 * tokens: a tile read `7k` whether the call had generated 6,500 or 7,499.
 * Measured over 102 real values from one night of runs, that hid 256 tokens
 * on average and 497 at worst, per tile — enough that a reader comparing two
 * runs could not see a difference of several hundred tokens at all.
 *
 * Two decimals put the step at 10 tokens. One would put it at 100, which is
 * still coarse for the sub-10k counts these tiles usually show.
 *
 * The decimals are FIXED rather than trimmed (`7.00k`, not `7k`) so a column
 * of tiles stays aligned on the decimal point; these render in a grid where
 * ragged widths are harder to scan than a trailing zero.
 *
 * The millions arm keeps one decimal, none from ten million. It is reached
 * by fleet-wide totals, where a tenth of a million is already finer than any
 * decision made from it.
 *
 * (#2919) Each arm hands over where the arm below it would ROUND to its
 * unit, not where the unit's value begins. Two decimals of thousands round
 * 999,995 up to `1000.00k` — a figure that spells a million as a thousand,
 * and one column wider than anything else the arm prints — so the millions
 * arm takes over from {@link K_TO_M}, not from 1,000,000. This is the CLI's
 * rule: `tokens_cell` (`src/run_list.rs`, #2902) promotes at the same value,
 * and the two are meant to print one count identically. (The ten-million
 * step only drops the decimal, so `9.96M` rounding to `10.0M` is the widest
 * one-decimal form, not a unit change; the CLI prints the same.) The
 * thousands arm below needs no such constant: an integer count cannot round
 * to 1,000 from below, so `>= 1000` already IS that boundary.
 *
 * The rule itself lives in {@link compactThousands}, shared with the other
 * compact styles in the app; this function only picks the decimals.
 */
export function fmtC(n: number): string {
  if (n >= 1000) return compactThousands(n, FMT_C_STYLE);
  // Below 1000 the exact integer is shown, so there is nothing to round.
  return fmtN(n);
}

/** Two decimals of thousands; one of millions, none from ten million. */
const FMT_C_STYLE: CompactStyle = { k: 2, m: (n) => (n >= 1e7 ? 0 : 1) };

/** (#2919) The smallest count `fmtC` prints in millions: the first value the
 * thousands arm's two decimals would round up to `1000.00k`. The CLI
 * (`tokens_cell`, `src/run_list.rs`) hardcodes this number; here it is what
 * {@link compactThousands} derives for two decimals, and `format.test.ts`
 * pins the two to each other. */
export const K_TO_M = 999_995;

/** How a compact count spells its arms: decimals in the thousands arm and in
 * the millions arm, each either fixed or chosen per value (so a style can
 * print `1.5k` but `15k`, or `1.2M` but `12M`). Units are always `k` and
 * `M`; a count below 1000 never reaches this core, so how it prints is the
 * caller's own business (`fmtN`, `String`, `toLocaleString` all exist). */
export interface CompactStyle {
  k: number | ((n: number) => number);
  m: number | ((n: number) => number);
}

/** (#2919) THE compact-count core for a count of at least 1000: `fmtC`
 * (the tiles and the fleet hero), the mission graph's `fmtTok` (step rows,
 * node labels) and the event log's `fmtTok` / `compactCountLabel` all print
 * through here, so the one rule that matters is written once:
 *
 *   an arm hands over to the next where its OWN rounding would carry the
 *   unit — the thousands arm prints whatever `toFixed` gives it unless that
 *   reads `1000` or more, in which case the millions arm prints instead.
 *
 * That derives the boundary from the style rather than pinning a constant
 * per caller: 999,995 for two decimals, 999,950 for one, 999,500 for none.
 * Before this, every copy of the rule sat at `1,000,000` and each printed a
 * million as `1000.00k` / `1000.0k` / `1000k` just below it — and two had no
 * millions arm at all, so they would have printed `1000k` forever. */
export function compactThousands(n: number, style: CompactStyle): string {
  const kd = typeof style.k === "function" ? style.k(n) : style.k;
  const k = (n / 1000).toFixed(kd);
  if (Number(k) < 1000) return `${k}k`;
  const md = typeof style.m === "function" ? style.m(n) : style.m;
  return `${(n / 1e6).toFixed(md)}M`;
}

export const GIB = 1073741824; // 2³⁰
export const MIB = 1048576; // 2²⁰
export const KIB = 1024; // 2¹⁰

/** `memBytes()` — **binary** GiB/MiB/KiB (`bytes / 2³⁰`, two decimals for the
 * GiB arm).
 *
 * Legacy was decimal — `bytes / 1e9`, labeled "GB" — and
 * the port matched it byte-for-byte until #1811. The operator called it on the
 * live gauge: a machine that Apple, the box, and every operator on earth calls
 * "128 GB" was rendering its own ceiling as `137.44 GB`, and the one screen
 * whose entire job is telling you how much room you have was answering in units
 * nobody's machine is sold in. Powers of two are what a reader can check against
 * the hardware they bought.
 *
 * Binary throughout, labeled `GiB` so the unit is not itself the lie, is the
 * whole fix — and it retires the reconciling parenthetical this replaced
 * (`poolGiBNote`, ` (128 GiB)` beside a decimal `137.44 GB`), because there are
 * no longer two conventions on the page to reconcile.
 *
 * Residual, deliberately NOT taken here: the stage header's own RAM figure
 * (`specOf`, in `lenses/fleet/cards.ts`) has always been binary and has always
 * been labeled `GB`. It now agrees with this function NUMERICALLY (both say
 * 128), which is the confusion #1811 was actually about; only the unit suffix
 * still differs. Relabelling it is a one-token change gated on retiring the
 * machine stage's last byte-exact parity tie to legacy — an operator call, and
 * a separate one from the units decision made here. */
export function memBytes(b: number | null | undefined): string {
  if (b == null) return "—";
  const n = Number(b);
  if (!Number.isFinite(n)) return "—";
  if (n >= GIB) return (n / GIB).toFixed(2) + " GiB";
  if (n >= MIB) return Math.round(n / MIB) + " MiB";
  if (n >= KIB) return Math.round(n / KIB) + " KiB";
  return n + " B";
}

/** `memStateCls()`. Only green/amber/red pass through;
 * anything else (missing, unrecognized) normalizes to "unknown". Ported
 * ahead of its first consumer; #1806 Stage 1 (then Stage 2/3's
 * `MachineHealthRegion.tsx`, `machineGauge.ts`) is that consumer — see this
 * module's own doc for why the mapping normalizes a hostile/unrecognized
 * state string rather than passing it through into a class attribute. */
export function memStateCls(s: string | null | undefined): "green" | "amber" | "red" | "unknown" {
  return s === "green" || s === "amber" || s === "red" ? s : "unknown";
}

/** `memPct()`. Clamped 0-100 percent of `part`
 * against `scale`. Its one caller is `machineGauge.ts`'s
 * `computeGaugeGeometry` — the model ROWS inline their own clamp rather
 * than routing through here, so do not read this as their shared helper.
 * `part == null`
 * (the unpriced-model case — no committed extent to draw at all, see that
 * component's own doc) returns 0 rather than NaN; callers
 * still gate on `part != null` before rendering the layer at all, so this
 * value is never actually used for that case — it exists so the function is
 * total and never hands a caller `NaN%`. */
export function memPct(part: number | null | undefined, scale: number): number {
  if (part == null || !scale) return 0;
  return Math.max(0, Math.min(100, (Number(part) / scale) * 100));
}

/** ` (35.45 GiB reclaimable)` — the parenthetical that stops `used` and
 * `available` from reading as an addition.
 *
 * They deliberately OVERLAP. `used` is Activity-Monitor-style and counts
 * inactive pages as app memory; `available` counts those same pages as
 * reclaimable. Both are correct, and on a 128 GiB machine they summed to
 * 152.78 GiB when first shown side by side (#1821) — two right numbers making
 * an impossible impression, which is the same defect class as the two figures
 * that both called themselves "free" and differed by 51 points.
 *
 * `available - free` IS the overlap: inactive + speculative, the pages counted
 * in both. Naming it is what makes `available > capacity - used` legible
 * instead of looking like broken arithmetic.
 *
 * Returns `""` whenever it would not clarify — either figure unreadable, or a
 * non-positive difference (nothing reclaimable, so nothing to explain). */
export function reclaimableNote(availableBytes: number | null | undefined, freeBytes: number | null | undefined): string {
  if (availableBytes == null || freeBytes == null) return "";
  const a = Number(availableBytes);
  const f = Number(freeBytes);
  if (!Number.isFinite(a) || !Number.isFinite(f)) return "";
  const reclaimable = a - f;
  if (reclaimable <= 0) return "";
  return ` (${memBytes(reclaimable)} reclaimable)`;
}

/** (#2902 step 5) A span of seconds in words a line can hold: "23h 53m",
 *  then "12m", then "45s" under a minute. A day-long budget wait counts
 *  down in this form, in the tube and on the line, never as raw seconds.
 *  Minutes round UP (119 s is "2m", 23h 53m 59s is "23h 54m"): a
 *  countdown must never say less time is left than is. Under a minute the
 *  seconds are exact, so a rest's hand still ticks every second. */
export function compactDuration(secs: number): string {
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s}s`;
  const mins = Math.ceil(s / 60);
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

/** Strip a LEADING `darkmux:` namespace prefix; absent model renders as "". */
export function shortModel(m: string | null | undefined): string {
  return String(m || "").replace(/^darkmux:/, "");
}
