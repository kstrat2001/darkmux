/**
 * (U2-3) The stylesheet's own colour hygiene, as a ratchet.
 *
 * Two findings, both structural rather than visual:
 *
 *   1. `var(--panel, #12141c)` and `var(--line, #232838)` referenced two
 *      custom properties this sheet NEVER DEFINED, so the fallback was the
 *      only value that ever rendered. A fallback that is always taken is a
 *      literal wearing a token's name — and to anyone grepping for `--panel`
 *      it reads as evidence the token exists.
 *   2. 120 colour literals lived outside the token block, including a second
 *      success green (`#5af0a3` beside `--good`), two extra ambers, and
 *      three panel shades repeated 11, 6 and 4 times.
 *
 * The second assertion is a RATCHET, deliberately: it pins a ceiling, not a
 * target. Adding a literal fails; removing one passes and then the number
 * comes down with it. A hard "zero literals" rule would be dishonest — the
 * ANSI terminal palette (`.panelout .a-fg*`) is a fixed 16-colour spec that
 * is not this app's semantics, and the translucent overlay washes
 * (`rgba(255,255,255,.02)`, `rgba(0,0,0,.45)`) are scrims rather than
 * palette entries.
 *
 * Reading the sheet as TEXT rather than through a DOM: jsdom parses no
 * stylesheet, and the questions here ("is this name ever defined?", "how
 * many literals are there?") are questions about the source, not about a
 * render.
 */
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const CSS = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

/** The sheet with every comment blanked (newlines kept, so line numbers in a
 * failure message still point at the real line). Comments carry issue
 * numbers like `#2108`, which are indistinguishable from a hex colour to any
 * regex that does not do this first. */
const CODE = CSS.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, " "));

/** Every custom property this sheet DEFINES, wherever it defines it — the
 * two `:root` blocks and the scoped ones (`.missionlens` declares its own
 * `--ml-*` set). A reference is legitimate if any of them could supply it. */
function definedTokens(): Set<string> {
  return new Set([...CODE.matchAll(/(^|[;{])\s*(--[a-zA-Z0-9-]+)\s*:/g)].map((m) => m[2]));
}

/** Custom properties SET FROM JAVASCRIPT rather than declared in this sheet
 * — `App.tsx` measures the sticky chrome and the masthead and writes their
 * heights as inline properties (`el.style.setProperty("--chrome-h", ...)`).
 * They are legitimately absent from the CSS, and their `var()` fallbacks are
 * the pre-measurement value, which is the one case where a fallback is
 * load-bearing. */
const JS_SET_TOKENS = new Set(["--chrome-h", "--masthead-h"]);

function tokenRefs(): Array<{ name: string; line: number; hasFallback: boolean }> {
  const out: Array<{ name: string; line: number; hasFallback: boolean }> = [];
  for (const m of CODE.matchAll(/var\(\s*(--[a-zA-Z0-9-]+)\s*(,)?/g)) {
    out.push({ name: m[1], line: CODE.slice(0, m.index).split("\n").length, hasFallback: !!m[2] });
  }
  return out;
}

/** Colour literals outside every `:root` block — hex or `rgb()`/`rgba()`.
 * The token block itself is where literals BELONG. */
function literalsOutsideTokenBlock(): Array<{ value: string; line: number }> {
  const roots: Array<[number, number]> = [];
  for (const m of CODE.matchAll(/^:root\s*\{/gm)) {
    let i = m.index! + m[0].length;
    let depth = 1;
    while (depth > 0 && i < CODE.length) {
      if (CODE[i] === "{") depth++;
      else if (CODE[i] === "}") depth--;
      i++;
    }
    roots.push([m.index!, i]);
  }
  const inRoot = (p: number) => roots.some(([a, b]) => p >= a && p < b);
  const out: Array<{ value: string; line: number }> = [];
  for (const m of CODE.matchAll(/#[0-9a-fA-F]{3,8}\b|rgba?\([^)]*\)/g)) {
    if (inRoot(m.index!)) continue;
    out.push({ value: m[0].toLowerCase(), line: CODE.slice(0, m.index).split("\n").length });
  }
  return out;
}

describe("(U2-3) styles.css colour tokens", () => {
  it("never references a custom property the sheet does not define", () => {
    const defined = definedTokens();
    const dangling = tokenRefs().filter((r) => !defined.has(r.name) && !JS_SET_TOKENS.has(r.name));
    expect(
      dangling.map((r) => `${r.name} (line ${r.line})`),
      "a var() naming an undefined property renders its fallback forever — the token is fiction",
    ).toEqual([]);
  });

  it("keeps no fallback on a token that IS defined — a dead fallback hides which value is live", () => {
    // `var(--good, #4ade80)` looked defensive and was noise: `--good` has
    // been defined the whole time, so the literal beside it could drift from
    // the token without anything rendering differently.
    const defined = definedTokens();
    const deadFallbacks = tokenRefs().filter((r) => r.hasFallback && defined.has(r.name) && !JS_SET_TOKENS.has(r.name));
    expect(deadFallbacks.map((r) => `${r.name} (line ${r.line})`)).toEqual([]);
  });

  it("defines no token in terms of ITSELF — a cycle silently invalidates every use", () => {
    // Found by a pixel diff, not by this file's earlier assertions, which is
    // why it is here now. A bulk literal->token sweep rewrote the `:root`
    // DEFINITION too, leaving `--good: var(--good)`. A self-referential
    // custom property is "invalid at computed-value time": every
    // `var(--good)` in the sheet then resolved to `unset`, so green text
    // inherited `--fg` and green backgrounds fell through to `--bg` — on
    // five lenses at once, with no error anywhere and every unit test green.
    const cycles: string[] = [];
    for (const m of CODE.matchAll(/(--[a-zA-Z0-9-]+)\s*:\s*([^;{}]*);/g)) {
      if (new RegExp(`var\\(\\s*${m[1]}\\b`).test(m[2])) cycles.push(`${m[1]}: ${m[2].trim()}`);
    }
    expect(cycles, "a token defined as itself renders nothing, everywhere it is used").toEqual([]);
  });

  // The ratchet. Lower it whenever a promotion pass lands; never raise it.
  const LITERAL_CEILING = 72;

  it(`holds at most ${LITERAL_CEILING} colour literals outside the token block`, () => {
    const lits = literalsOutsideTokenBlock();
    expect(
      lits.length,
      `${lits.length} literals; the ceiling is ${LITERAL_CEILING}. New colours belong in :root. ` +
        `Newest offenders: ${JSON.stringify(lits.slice(-5))}`,
    ).toBeLessThanOrEqual(LITERAL_CEILING);
  });

  it("has no DUPLICATE of a semantic colour the token block already names", () => {
    // The specific finding: `#5af0a3` (9x) beside `--good: #4ade80`, and
    // `#ffb86b` (6x) beside `--warn: #f0b429` — two vocabularies for one
    // concept, which is how a status colour ends up meaning different things
    // on two lenses. The ANSI palette keeps its own literals (a fixed
    // terminal spec, not this app's semantics), so this names the retired
    // values rather than sweeping every green.
    const retired = ["#5af0a3", "#ffb86b", "#131826", "#1c1c22", "#1a2030", "#4ade80"];
    const lits = literalsOutsideTokenBlock();
    const ansiLines = new Set<number>();
    {
      let inAnsi = false;
      CODE.split("\n").forEach((line, i) => {
        if (/^\s*\.panelout \.a-(fg|bg)\d/.test(line)) inAnsi = true;
        if (inAnsi) ansiLines.add(i + 1);
        if (inAnsi && line.trim().endsWith("}")) inAnsi = false;
      });
    }
    const offenders = lits.filter((l) => retired.includes(l.value) && !ansiLines.has(l.line));
    expect(
      offenders.map((l) => `${l.value} (line ${l.line})`),
      "a retired duplicate came back — use the token it was mapped onto",
    ).toEqual([]);
  });
});
