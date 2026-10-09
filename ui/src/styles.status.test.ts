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

const KINDS: readonly WorkStatusKind[] = ["running", "done", "error", "degraded", "stopped", "aborted", "idle", "unknown"];
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

  /** A `:root` custom property's value, following `var(--x)` to a literal. */
  const rootValue = (name: string): string => {
    const body = rules.filter(([sel]) => sel === ":root").map(([, b]) => b).join(";");
    const m = new RegExp(`(?:^|;|\\s)${name}\\s*:\\s*([^;]+)`).exec(body);
    expect(m, `${name} is defined on :root`).not.toBeNull();
    const v = m![1].trim();
    const ref = /^var\((--[\w-]+)\)$/.exec(v);
    return ref ? rootValue(ref[1]) : v.toLowerCase();
  };

  // (operator, 2026-10-07) "Aborted" (a person stopped it) and "degraded" (a
  // caution) shared one amber. Aborted has its own neutral tone, which must not
  // read as idle/dim or as running teal either.
  it("gives aborted its own color: not degraded's amber, not dim, not running, error or complete", () => {
    const aborted = rootValue("--status-aborted");
    expect(aborted).toMatch(/^#[0-9a-f]{6}$/);
    for (const other of ["--status-degraded", "--status-idle", "--status-running", "--status-error", "--status-done", "--status-stopped"]) {
      expect(aborted, `--status-aborted vs ${other}`).not.toBe(rootValue(other));
    }
    expect(rootValue("--status-degraded"), "degraded keeps the caution amber").toBe(rootValue("--warn"));
  });

  // The mission lens had its own node-status color table (`--ml-run` amber for
  // running, `--dim` for abandoned). Its status-keyed rules take their color
  // from the one table now, as the chip and the bar do.
  it("the mission lens colors its nodes, steps and timeline from the same table", () => {
    expect(css.replace(/\/\*[\s\S]*?\*\//g, ""), "no rule names the lens's retired amber").not.toMatch(/--ml-run|var\(--run\)/);
    const STATUS_CLASS = /\.(?:mnode|phasegroup|steprow|tlphase|tltask)\.s-(?:running|complete|error|degraded|abandoned|waiting)\b/;
    const PALETTE = /var\(--(?:good|bad|warn|dim|accent|run|ml-run|good-soft|severe)\)/;
    const offenders = rules.filter(([sel, body]) => STATUS_CLASS.test(sel) && PALETTE.test(body));
    expect(offenders, JSON.stringify(offenders)).toEqual([]);
  });

  // A `WorkStatus` call site adds a layout class and never a second color
  // (`WorkStatus.tsx`'s own doc). The mission timeline's phase chip
  // (`.tlph-tag`) set its text dim, so an ABORTED or DEGRADED chip read gray
  // text in a colored border there and nowhere else.
  it("no chip layout class colors the chip", () => {
    const LAYOUT = /\.(?:tlph-tag|pg-tag|mstatus|labbadge|pill)\b/;
    const offenders = rules.filter(
      ([sel, body]) => LAYOUT.test(sel) && !/::?(?:before|after)/.test(sel) && (sets(body, "color") || sets(body, "border-color") || sets(body, "background")),
    );
    expect(offenders, JSON.stringify(offenders)).toEqual([]);
  });
});
