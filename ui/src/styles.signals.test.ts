/**
 * (#1973) CSS-SOURCE assertions for the signal row's flex contract.
 *
 * **What this can and cannot prove.** jsdom does no layout — it resolves no
 * flexbox, so a rendered-DOM test CANNOT catch the defect these guard. The
 * bug they exist for was measured in a real browser: at a 390px row,
 * `.signal__detail` resolved to 0px wide and 2235px tall (146 characters,
 * one per line) because `.signal__fix` was `flex: 0 0 auto` and measured
 * 397px, taking the whole line. 1013 unit tests were green while it shipped,
 * and it was reported from a phone.
 *
 * So these read the STYLESHEET and pin the specific declarations that make
 * the row survive a narrow viewport. That is a weaker guard than measuring
 * layout — it verifies the rule is present, not that the layout is correct —
 * and it is the strongest guard available in this test environment. The real
 * check is a browser at a phone width; this is the fast tripwire that stops
 * the exact regression from returning unnoticed.
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const css = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

/** The declaration block for a selector, or "" if absent. */
function block(selector: string): string {
  const i = css.indexOf(selector + " {");
  if (i === -1) return "";
  return css.slice(i, css.indexOf("}", i));
}

describe("signal row flex contract (#1973)", () => {
  it("the row WRAPS — a single line is what let one child starve the other", () => {
    expect(block(".session-run .signals .signal__row")).toMatch(/flex-wrap:\s*wrap/);
  });

  it("the detail has a real flex-basis, not `auto` — nothing else stops it resolving to zero", () => {
    const b = block(".session-run .signals .signal__detail");
    expect(b).toMatch(/flex:\s*1\s+1\s+\d+px/);
    expect(b).toMatch(/min-width:\s*0/);
  });

  it("the fix may SHRINK — `flex: 0 0 auto` on it was the direct cause", () => {
    const b = block(".session-run .signals .signal__fix");
    expect(b).not.toMatch(/flex:\s*0\s+0\s+auto/);
    expect(b).toMatch(/flex:\s*1\s+1/);
  });

  it("both text children can break unbreakable tokens — `qwen3.6-35b-a3b-turboquant-mlx` would otherwise push the page sideways", () => {
    expect(block(".session-run .signals .signal__detail")).toMatch(/overflow-wrap:\s*anywhere/);
    expect(block(".session-run .signals .signal__fix")).toMatch(/overflow-wrap:\s*anywhere/);
  });
});
