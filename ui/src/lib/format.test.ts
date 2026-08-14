import { describe, it, expect } from "vitest";
import { memPct, memStateCls, poolGiBNote } from "./format";

// `memStateCls` was ported (format.ts) ahead of its first consumer; #1806
// Stage 1, then Stage 2/3's `MachineHealthRegion.tsx`, is that consumer —
// it decides BOTH a chip's severity class AND a bar layer's fill class, so a
// wrong mapping here is a security-adjacent bug (a hostile state string
// landing raw in a class attribute), not just a cosmetic one. Covered here
// directly since it now sits on that path.
describe("memStateCls", () => {
  it("passes green/amber/red through unchanged", () => {
    expect(memStateCls("green")).toBe("green");
    expect(memStateCls("amber")).toBe("amber");
    expect(memStateCls("red")).toBe("red");
  });

  it("normalizes the real-world 'unknown' state to itself", () => {
    expect(memStateCls("unknown")).toBe("unknown");
  });

  // The inverted case: anything NOT in the known vocabulary — missing,
  // mistyped, or hostile — degrades to "unknown" rather than being passed
  // through into a class attribute. This is the guard
  // `viewer-machine.spec.js`'s XSS payload exercises live (a state string
  // shaped like `red" onmouseover=...`).
  it("degrades an unrecognized string to unknown, never passing it through", () => {
    expect(memStateCls("yellow")).toBe("unknown");
    expect(memStateCls(`red" onmouseover=window.__xss=1 x="`)).toBe("unknown");
  });

  it("degrades null/undefined to unknown", () => {
    expect(memStateCls(null)).toBe("unknown");
    expect(memStateCls(undefined)).toBe("unknown");
  });
});

// `memPct()` sizes every bar layer — #1806 Stage 1's new consumer, carried
// into Stage 2/3's `MachineHealthRegion.tsx` (the gauge's commit tick and
// each model row's `.mm-row-pot`/`.mm-row-cur`).
describe("memPct", () => {
  it("computes a plain percentage of part against scale", () => {
    expect(memPct(25, 100)).toBe(25);
    expect(memPct(50, 200)).toBe(25);
  });

  it("clamps above 100 — current can exceed potential/scale in a real ledger", () => {
    expect(memPct(150, 100)).toBe(100);
  });

  it("clamps below 0 — never a negative width", () => {
    expect(memPct(-10, 100)).toBe(0);
  });

  // The inverted case this function exists to serve: `part == null` (the
  // unpriced-model case, docs/design/machine-lens/provenance.md) returns 0 rather than NaN — but
  // callers gate on `pot != null`/`cur != null` before rendering the layer
  // at all, so this value is a safety net, not something a real unpriced
  // bar ever paints.
  it("returns 0, not NaN, when part is null or undefined", () => {
    expect(memPct(null, 100)).toBe(0);
    expect(memPct(undefined, 100)).toBe(0);
  });

  it("returns 0, not Infinity/NaN, when scale is 0", () => {
    expect(memPct(50, 0)).toBe(0);
  });
});

/**
 * (#1811) `hw.memsize` is rendered twice on the machine page in two byte
 * conventions — the stage header's binary `128 GB` and the ledger's decimal
 * `137.44 GB` — and a reader comparing them sees two quantities rather than
 * one number. `poolGiBNote` is the parenthetical that reconciles them at the
 * point of comparison.
 *
 * The suppression cases are the interesting half: a parenthetical that
 * restates the number beside it is noise, and noise on this page costs more
 * than it looks like, because every figure here is load-bearing.
 */
describe("poolGiBNote", () => {
  it("names the binary equivalent when the two conventions genuinely differ", () => {
    // 137438953472 = exactly 128 GiB = 137.44 GB. The operator's own machine.
    expect(poolGiBNote(137438953472)).toBe(" (128 GiB)");
  });

  it("stays silent when both conventions round to the same integer — the note would only restate", () => {
    // 2e9 bytes: 2 GB decimal, 2 GiB rounded. Nothing to reconcile.
    expect(poolGiBNote(2_000_000_000)).toBe("");
  });

  it("stays silent below a gigabyte, where a rounded GiB figure would be misleading", () => {
    expect(poolGiBNote(500_000_000)).toBe("");
  });

  it("stays silent rather than guessing on an unreadable byte count", () => {
    expect(poolGiBNote(null)).toBe("");
    expect(poolGiBNote(undefined)).toBe("");
    expect(poolGiBNote(Number.NaN)).toBe("");
  });
});
