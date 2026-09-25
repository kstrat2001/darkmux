import { describe, it, expect } from "vitest";
import {
  SCOPE_SPEED,
  advanceMorph,
  createMorph,
  scopeStateOf,
  scopeTargets,
  settleMorph,
  stateTone,
  toolIconKind,
  type ScopeParams,
} from "./scopeMorph";
import type { Rgb } from "./scopeTone";

const GREEN: Rgb = [125, 255, 160];
const BLUE: Rgb = [122, 162, 255];
const RED: Rgb = [248, 113, 113];

/** Step a morph forward `seconds` at a steady 60 fps. Time is ONLY `dt`:
 *  no wall clock anywhere, so the distance travelled is a parameter of the
 *  test, not of when the suite runs. */
function run(m: ReturnType<typeof createMorph>, state: Parameters<typeof advanceMorph>[1], rate: number, rgb: Rgb, seconds: number): ScopeParams {
  const dt = 1 / 60;
  let p = advanceMorph(m, state, rate, rgb, dt);
  for (let t = dt; t < seconds; t += dt) p = advanceMorph(m, state, rate, rgb, dt);
  return p;
}

describe("scopeTargets: each state is a set of targets on ONE trace (#2890)", () => {
  it("GEN drives the wave from the rate, full-bright ring, nothing else lit", () => {
    const t = scopeTargets("generating", 0, 180, GREEN);
    expect(t).toMatchObject({ ring: 1, wave: 180, comet: 0, inward: 0, breath: 0, fuzz: 0, sx: 1, sy: 1, ember: 0, rscale: 1 });
    expect([t.r, t.g, t.b]).toEqual(GREEN);
  });

  it("only GEN (and FINISHED's average echo, tested below) carries a rate; every other state's wave target is 0", () => {
    for (const s of ["prompt", "tools", "rest", "stalled", "nosignal", "idle"] as const) {
      expect(scopeTargets(s, 0, 180, GREEN).wave).toBe(0);
    }
  });

  it("PROMPT dims the ring and lights the inward rings", () => {
    expect(scopeTargets("prompt", 0, 0, GREEN)).toMatchObject({ ring: 0.35, inward: 1, comet: 0 });
  });

  it("TOOLS dims the ring and lights the comet", () => {
    expect(scopeTargets("tools", 0, 0, BLUE)).toMatchObject({ ring: 0.3, comet: 1, inward: 0 });
  });

  it("REST breathes on a slightly smaller ring", () => {
    expect(scopeTargets("rest", 0, 0, GREEN)).toMatchObject({ ring: 0.8, breath: 1, rscale: 0.92 });
  });

  it("NO SIGNAL turns the ring off and the static on", () => {
    expect(scopeTargets("nosignal", 0, 0, GREEN)).toMatchObject({ ring: 0, fuzz: 1 });
  });

  it("FINISHED is a calm dimmed ring, no motion layers", () => {
    expect(scopeTargets("finished", 0, 0, GREEN)).toMatchObject({ ring: 0.55, rscale: 0.96, comet: 0, inward: 0, breath: 0, fuzz: 0, ember: 0 });
  });

  it("STALL collapses in three phases: squash to a line, shrink to a dot, then an ember", () => {
    expect(scopeTargets("stalled", 0.1, 0, RED)).toMatchObject({ sy: 0.02, sx: 1, ember: 0 });
    expect(scopeTargets("stalled", 0.45, 0, RED)).toMatchObject({ sy: 0.02, sx: 0.02, ember: 0 });
    expect(scopeTargets("stalled", 0.8, 0, RED)).toMatchObject({ sy: 0.02, sx: 0.02, ember: 1, ring: 0 });
  });

  it("writes into a caller-supplied object instead of allocating one per frame", () => {
    const out = scopeTargets("prompt", 0, 0, GREEN);
    const again = scopeTargets("tools", 0, 0, BLUE, out);
    expect(again).toBe(out);
    expect(out.comet).toBe(1);
    expect(out.inward).toBe(0);
  });
});

describe("advanceMorph: states glide, never pop (#2890)", () => {
  it("the first frame adopts the targets (a page load does not animate in from nothing)", () => {
    const m = createMorph();
    const p = advanceMorph(m, "tools", 0, BLUE, 1 / 60);
    expect(p.comet).toBe(1);
    expect(p.ring).toBeCloseTo(0.3);
  });

  it("a state change moves part of the way on the next frame, not all of it", () => {
    const m = createMorph();
    run(m, "prompt", 0, GREEN, 1);
    const p = advanceMorph(m, "tools", 0, BLUE, 1 / 60);
    expect(p.comet).toBeGreaterThan(0);
    expect(p.comet).toBeLessThan(0.2);
    expect(p.inward).toBeGreaterThan(0.8);
  });

  it("the approach is exponential in dt: k = 1 - exp(-speed * dt)", () => {
    const m = createMorph();
    run(m, "prompt", 0, GREEN, 1);
    const dt = 0.05;
    const p = advanceMorph(m, "tools", 0, BLUE, dt);
    expect(p.comet).toBeCloseTo(1 - Math.exp(-SCOPE_SPEED.comet * dt), 6);
  });

  it("settles on the new state's targets within about half a second", () => {
    const m = createMorph();
    run(m, "prompt", 0, GREEN, 1);
    const p = run(m, "tools", 0, BLUE, 0.6);
    expect(p.comet).toBeGreaterThan(0.9);
    expect(p.inward).toBeLessThan(0.1);
    expect(p.b).toBeGreaterThan(245);
  });

  it("color glides too", () => {
    const m = createMorph();
    run(m, "generating", 100, GREEN, 1);
    const p = advanceMorph(m, "tools", 0, BLUE, 1 / 60);
    expect(p.g).toBeLessThan(255);
    expect(p.g).toBeGreaterThan(BLUE[1]);
  });

  it("the CRT collapse runs faster than the rest: sy nearly flat after 0.25 s of STALL", () => {
    const m = createMorph();
    run(m, "generating", 100, GREEN, 1);
    const p = run(m, "stalled", 0, RED, 0.25);
    expect(p.sy).toBeLessThan(0.05);
    expect(p.sx).toBeGreaterThan(0.9);
  });

  it("a held STALL ends as a dot with an ember", () => {
    const m = createMorph();
    run(m, "generating", 100, GREEN, 1);
    const p = run(m, "stalled", 0, RED, 2);
    expect(p.sx).toBeLessThan(0.05);
    expect(p.ember).toBeGreaterThan(0.9);
    expect(p.ring).toBeLessThan(0.05);
  });

  it("leaving STALL powers the tube back on: it stays a line first, then opens", () => {
    const m = createMorph();
    run(m, "stalled", 0, RED, 2);
    const early = run(m, "prompt", 0, GREEN, 0.2);
    expect(early.sy).toBeLessThan(0.05);
    expect(early.sx).toBeGreaterThan(0.9);
    const later = run(m, "prompt", 0, GREEN, 0.6);
    expect(later.sy).toBeGreaterThan(0.9);
  });

  it("an ordinary state change does not squash the tube", () => {
    const m = createMorph();
    run(m, "prompt", 0, GREEN, 1);
    const p = run(m, "tools", 0, BLUE, 0.1);
    expect(p.sy).toBeCloseTo(1);
  });

  it("the ember's own clock advances with dt only", () => {
    const m = createMorph();
    run(m, "prompt", 0, GREEN, 1.5);
    expect(m.clock).toBeCloseTo(1.5, 1);
  });

  it("settleMorph jumps straight to the settled look (reduced motion), a stall included", () => {
    const m = createMorph();
    const p = settleMorph(m, "stalled", 0, RED);
    expect(p).toMatchObject({ sx: 0.02, sy: 0.02, ember: 1, ring: 0 });
  });
});

describe("toolIconKind: a glowing icon per tool, a gear for anything else (#2890)", () => {
  it("maps the runtime's tool names", () => {
    expect(toolIconKind("read")).toBe("read");
    expect(toolIconKind("edit")).toBe("edit");
    expect(toolIconKind("write")).toBe("write");
    expect(toolIconKind("bash")).toBe("bash");
    expect(toolIconKind("search")).toBe("search");
  });

  it("falls back to the gear for any other tool, an unknown one, or none", () => {
    expect(toolIconKind("create_finding")).toBe("other");
    expect(toolIconKind("create_mod")).toBe("other");
    expect(toolIconKind("")).toBe("other");
    expect(toolIconKind(null)).toBe("other");
    expect(toolIconKind(undefined)).toBe("other");
  });

  it("is not fooled by case or padding", () => {
    expect(toolIconKind(" Read ")).toBe("read");
  });
});

describe("scopeStateOf: one state from the props every caller already passes", () => {
  it("a live state maps to itself", () => {
    for (const s of ["generating", "prompt", "tools", "rest", "stalled"] as const) {
      expect(scopeStateOf({ state: s })).toBe(s);
    }
  });

  it("no live state is no signal when the connection says so, otherwise idle", () => {
    expect(scopeStateOf({ state: null, noSignal: true })).toBe("nosignal");
    expect(scopeStateOf({ state: null })).toBe("idle");
  });

  it("a finished run is finished whatever the last live reading was", () => {
    expect(scopeStateOf({ state: "stalled", finished: true })).toBe("finished");
  });
});

describe("stateTone: the color each scope state draws in", () => {
  it("live states keep their lamp's tone; finished and idle are phosphor; no signal is its own gray", () => {
    expect(stateTone("tools")).toBe("tools");
    expect(stateTone("finished")).toBe("none");
    expect(stateTone("idle")).toBe("none");
    expect(stateTone("nosignal")).toBe("nosignal");
  });
});

describe("the finished scope echoes its average (#2890 operator review)", () => {
  it("draws the wave the average rate would draw, at a slow tempo", () => {
    const t = scopeTargets("finished", 0, 192, GREEN);
    expect(t.wave).toBe(192);
    expect(t.tempo).toBeLessThan(0.5);
  });
  it("keeps every live state at full tempo", () => {
    for (const s of ["generating", "prompt", "tools", "rest", "stalled", "nosignal"] as const) {
      expect(scopeTargets(s, 0, 180, GREEN).tempo).toBe(1);
    }
  });
  it("leaves an idle scope (no average) flat", () => {
    expect(scopeTargets("idle", 0, 0, GREEN).wave).toBe(0);
  });
});

describe("(#2889) TOOLS while the model WRITES the call", () => {
  it("is the same TOOLS look (color, comet, ring) plus a writing cue", () => {
    const running = scopeTargets("tools", 0, 0, BLUE);
    const writing = scopeTargets("tools", 0, 0, BLUE, undefined, true);
    expect(writing).toMatchObject({ ring: running.ring, comet: running.comet, r: running.r, g: running.g, b: running.b, scribe: 1 });
    expect(running.scribe).toBe(0);
  });

  it("the cue is TOOLS-only: no other state lights it, even if asked", () => {
    for (const s of ["generating", "prompt", "rest", "stalled", "nosignal", "finished", "idle"] as const) {
      expect(scopeTargets(s, 0, 0, GREEN, undefined, true).scribe).toBe(0);
    }
  });

  it("writing to running glides the cue out rather than dropping it", () => {
    const m = createMorph();
    const dt = 1 / 60;
    for (let t = 0; t < 1; t += dt) advanceMorph(m, "tools", 0, BLUE, dt, true);
    expect(m.p!.scribe).toBeGreaterThan(0.95);
    const p = advanceMorph(m, "tools", 0, BLUE, dt, false);
    expect(p.scribe).toBeGreaterThan(0.5);
    expect(p.scribe).toBeLessThan(1);
  });

  it("the reduced-motion frame settles on the cue too", () => {
    expect(settleMorph(createMorph(), "tools", 0, BLUE, true).scribe).toBe(1);
  });
});

describe("(#2890) the thinking tint", () => {
  it("is on only in GEN while thinking", () => {
    const rgb: [number, number, number] = [80, 220, 140];
    expect(scopeTargets("generating", 0, 60, rgb, undefined, false, true).iris).toBe(1);
    expect(scopeTargets("generating", 0, 60, rgb, undefined, false, false).iris).toBe(0);
    expect(scopeTargets("tools", 0, 0, rgb, undefined, false, true).iris).toBe(0);
  });
});

describe("(#2890) idle breathes like rest, without rest's dot", () => {
  it("idle breathes with no drift; rest breathes and drifts", () => {
    const rgb: [number, number, number] = [120, 130, 125];
    const idle = scopeTargets("idle", 0, 0, rgb);
    expect(idle.breath).toBeGreaterThan(0);
    expect(idle.drift).toBe(0);
    const rest = scopeTargets("rest", 0, 0, rgb);
    expect(rest.breath).toBe(1);
    expect(rest.drift).toBe(1);
  });
});

