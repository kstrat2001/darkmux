/**
 * ONE status-to-color table (operator, 2026-10-07: "the color of the status
 * should be a one time status setting so it should not even have a chance of
 * being different across views").
 *
 * A status word maps to a kind once (`WorkStatus.tsx`'s `workStatusKind`), and
 * a kind maps to a color once, here: `[data-status-kind="<kind>"]` sets
 * `--status-color`. The work-status chip, the fleet timeline's bar and the fleet
 * card's lamp read it and name no color of their own. Before this, the bar had
 * its own table keyed by status (`.sbar.running` amber where the chip is teal;
 * `degraded` and `escalated` bars dim where the chip is the warn color).
 *
 * Read from the STYLESHEET: jsdom resolves no cascade or custom properties.
 */
import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import type { WorkStatusKind } from "./components/WorkStatus";

const css = readFileSync(path.join(path.dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

/** Every rule as [selector, body]. */
const rules: [string, string][] = [...css.replace(/\/\*[\s\S]*?\*\//g, "").matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => [m[1].trim(), m[2]]);

const KINDS: readonly WorkStatusKind[] = ["running", "done", "error", "degraded", "stopped", "idle", "unknown"];
const RUN_STATUSES = ["planned", "running", "complete", "degraded", "error", "escalated", "abandoned", "unparseable"];

const sets = (body: string, prop: string) => new RegExp(`(^|;|\\s)${prop}\\s*:`).test(body);

describe("one status-to-color table", () => {
  it("gives every kind its color in exactly one rule", () => {
    for (const k of KINDS) {
      const owners = rules.filter(([sel, body]) => sel.split(",").some((s) => s.trim() === `[data-status-kind="${k}"]`) && sets(body, "--status-color"));
      expect(owners.length, `${k}: ${JSON.stringify(owners)}`).toBe(1);
    }
  });

  it("the chip and the timeline bar take their color from it, and no rule colors either by status", () => {
    const colorOf = (sel: string) => rules.filter(([s]) => s === sel).map(([, b]) => b).join(";");
    expect(colorOf(".wstatus")).toMatch(/color:\s*var\(--status-color\)/);
    expect(colorOf(".sbar")).toMatch(/background(-color)?:\s*var\(--status-color\)/);
    const offenders = rules.filter(
      ([sel, body]) =>
        (sets(body, "color") || sets(body, "background") || sets(body, "background-color") || sets(body, "border-color") || sets(body, "box-shadow")) &&
        sel.split(",").some((s) => {
          const t = s.trim();
          return RUN_STATUSES.some((st) => t === `.sbar.${st}`) || KINDS.some((k) => t === `.wstatus.is-${k}`);
        }),
    );
    expect(offenders, JSON.stringify(offenders)).toEqual([]);
  });

  it("the fleet card's lit lamp is the running color from the same table", () => {
    const stat = rules.filter(([s]) => s === ".mach .stat").map(([, b]) => b).join(";");
    expect(stat).toMatch(/--lit:\s*var\(--status-running\)/);
    expect(rules.some(([s, b]) => s.includes(':root') && /--status-running:/.test(b))).toBe(true);
  });
});
