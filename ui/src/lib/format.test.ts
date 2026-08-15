import { describe, it, expect } from "vitest";
import { memBytes, memPct, memStateCls, reclaimableNote } from "./format";

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
 * (#1811) `memBytes` is **binary**, and these are the tests that pin it there.
 *
 * The bug this closes: `hw.memsize` used to render as the header's `128 GB`
 * and the ledger's `137.44 GB`, and a reader comparing the two saw two
 * quantities rather than one number — on the single screen whose job is
 * telling them how much room they have. The predecessor of this block tested a
 * reconciling parenthetical (` (128 GiB)`) bolted beside the decimal figure;
 * the operator's call was to fix the units instead, so the parenthetical and
 * its tests are gone and the conversion itself is asserted here.
 *
 * The exact-boundary cases matter more than they look: an off-by-one in the
 * threshold (`>` for `>=`, or a decimal 1e9 left behind in one arm) silently
 * relabels a whole magnitude, and every figure on that page is load-bearing.
 */
describe("memBytes", () => {
  it("renders hw.memsize as the power of two the machine is actually sold as", () => {
    // 137438953472 = exactly 128 GiB — the operator's own machine, and the
    // figure that used to read "137.44 GB".
    expect(memBytes(137438953472)).toBe("128.00 GiB");
  });

  it("uses binary divisors, not decimal, at every magnitude", () => {
    expect(memBytes(32378306560)).toBe("30.15 GiB"); // decimal would say 32.38 GB
    expect(memBytes(5 * 1073741824)).toBe("5.00 GiB");
    expect(memBytes(700 * 1048576)).toBe("700 MiB"); // decimal would say 734 MB
    expect(memBytes(4 * 1024)).toBe("4 KiB");
  });

  it("switches magnitude exactly AT each binary boundary, never one byte off", () => {
    expect(memBytes(1073741824)).toBe("1.00 GiB");
    expect(memBytes(1073741823)).toBe("1024 MiB");
    expect(memBytes(1048576)).toBe("1 MiB");
    expect(memBytes(1048575)).toBe("1024 KiB");
    expect(memBytes(1024)).toBe("1 KiB");
    expect(memBytes(1023)).toBe("1023 B");
  });

  it("says so rather than guessing on an unreadable byte count", () => {
    expect(memBytes(null)).toBe("—");
    expect(memBytes(undefined)).toBe("—");
    expect(memBytes(Number.NaN)).toBe("—");
  });
});

/**
 * `used` and `available` deliberately overlap — `used` counts inactive pages
 * as app memory, `available` counts them as reclaimable — so side by side they
 * summed to 152.78 GiB on a 128 GiB machine (#1821). Two correct numbers, one
 * impossible impression. This parenthetical names the overlap.
 */
describe("reclaimableNote", () => {
  it("names the overlap between available and free — the pages counted twice", () => {
    // Live figures the day this shipped: available 76.82, free 41.37 GiB.
    const G = 1073741824;
    expect(reclaimableNote(76.82 * G, 41.37 * G)).toBe(" (35.45 GiB reclaimable)");
  });

  it("stays silent when there is nothing reclaimable to explain", () => {
    const G = 1073741824;
    expect(reclaimableNote(10 * G, 10 * G)).toBe("");
    // ...and never renders a negative, whatever the probe reports.
    expect(reclaimableNote(5 * G, 9 * G)).toBe("");
  });

  it("stays silent rather than guessing on an unreadable figure", () => {
    expect(reclaimableNote(null, 1)).toBe("");
    expect(reclaimableNote(1, null)).toBe("");
    expect(reclaimableNote(Number.NaN, 1)).toBe("");
  });
});
