/**
 * (#2890) The scope's center: a rule that re-centers a lone unit must not
 * reach the TOOLS center.
 *
 * Idle shows a unit with no number, so a rule moves that unit to the circle's
 * center. The TOOLS center has no number either, and the first version of
 * that rule matched it too: being more specific than the tools caption's own
 * rule, it pulled "writing · 3 s" up onto the tool's glyph, on every tool.
 *
 * Read as text because jsdom lays nothing out; the geometry itself was
 * measured in a browser when this was fixed.
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const css = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

/** Every selector in the sheet that re-centers a `.token-scope-u` on its own. */
function loneUnitSelectors(): string[] {
  const out: string[] = [];
  for (const m of css.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    const body = m[2];
    if (!/top:\s*50%/.test(body) || !/translateY\(-50%\)/.test(body)) continue;
    for (const sel of m[1].split(",").map((s) => s.replace(/\/\*[\s\S]*?\*\//g, "").trim())) {
      if (sel.includes(".token-scope-u")) out.push(sel);
    }
  }
  return out;
}

describe("the scope center's lone-unit rule", () => {
  it("exists (idle's unit sits at the center)", () => {
    expect(loneUnitSelectors().length).toBeGreaterThan(0);
  });

  it("never applies to an icon center, so a tool's caption stays under its glyph", () => {
    for (const sel of loneUnitSelectors()) {
      expect(sel, sel).toContain(":not(.token-scope-center--icon)");
    }
  });
});
