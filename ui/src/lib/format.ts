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

/** `fmt()` — viewer.html:962. Duration as `M:SS`, floored, never negative. */
export function fmtDuration(ms: number): string {
  const clamped = Math.max(0, ms);
  return Math.floor(clamped / 60000) + ":" + String(Math.floor(clamped / 1000) % 60).padStart(2, "0");
}

/** `clk()` — viewer.html:968. Time-of-day in the browser's local timezone
 * (the parity harness's Playwright context pins `timezoneId: 'UTC'`, same
 * as the legacy extraction, so both resolve identically under test). */
export function clk(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour12: false });
}

/** `clkhm()` — viewer.html:976. `HH:MM` local, no seconds — the fleet
 * activity-timeline axis labels. */
export function clkhm(t: number): string {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}

/** `lday()` — viewer.html:992. Local DATE, no time. Ported with #1800's
 * replay meta line, the only surface that names a calendar day: a live view
 * says "today" in the badge, a replay states the actual date its records
 * came from. Same locale-dependence as `clk` above — the harness pins the
 * timezone for both sides. */
export function lday(t: number): string {
  return new Date(t).toLocaleDateString();
}

/** `sameDay()` — viewer.html:982. */
function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

/** `clkrange()` — viewer.html:983-987 (#1530 dogfood). A time-only formatter
 * can't distinguish two instants exactly 24h apart, so a same-day range
 * stays bare `HH:MM:SS–HH:MM:SS`; a window straddling a day boundary
 * prefixes each end with its short date ("Aug 7 16:40:59–Aug 8 16:40:59"). */
export function clkrange(a: number, b: number): string {
  if (sameDay(a, b)) return `${clk(a)}–${clk(b)}`;
  const d = (t: number) => new Date(t).toLocaleDateString([], { month: "short", day: "numeric" });
  return `${d(a)} ${clk(a)}–${d(b)} ${clk(b)}`;
}

/** `relAgoFrom()` — viewer.html:987-989. Coarse past-only relative time.
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

/** `fmtN()` — viewer.html:1526. Thousands-grouped integer. */
export function fmtN(n: number): string {
  return Math.round(n)
    .toString()
    .replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** `fmtC()` — viewer.html:1528. Compact token count (1.2M / 45k / 984). */
export function fmtC(n: number): string {
  if (n >= 1e6) return (n / 1e6).toFixed(n >= 1e7 ? 0 : 1) + "M";
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return fmtN(n);
}

export const GIB = 1073741824; // 2³⁰
export const MIB = 1048576; // 2²⁰
export const KIB = 1024; // 2¹⁰

/** `memBytes()` — **binary** GiB/MiB/KiB (`bytes / 2³⁰`, two decimals for the
 * GiB arm).
 *
 * Legacy (`viewer.html:4853`) was decimal — `bytes / 1e9`, labeled "GB" — and
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

/** `memStateCls()` — viewer.html:4866. Only green/amber/red pass through;
 * anything else (missing, unrecognized) normalizes to "unknown". Ported
 * ahead of its first consumer; #1806 Stage 1 (then Stage 2/3's
 * `MachineHealthRegion.tsx`, `machineGauge.ts`) is that consumer — see this
 * module's own doc for why the mapping normalizes a hostile/unrecognized
 * state string rather than passing it through into a class attribute. */
export function memStateCls(s: string | null | undefined): "green" | "amber" | "red" | "unknown" {
  return s === "green" || s === "amber" || s === "red" ? s : "unknown";
}

/** `memPct()` — viewer.html:4939-4940. Clamped 0-100 percent of `part`
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
