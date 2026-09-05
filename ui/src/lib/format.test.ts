import { describe, it, expect } from "vitest";
import { MISSING, clkrange, fmtElapsed, memBytes, memPct, memStateCls, reclaimableNote } from "./format";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

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

// (operator, 2026-09-01) These two moved here from `timeline.test.ts`. They
// were always `clkrange` tests — they just used the fleet header as their
// vehicle, and that header dropped its range. `clkrange` is still live
// (`replayMeta.ts` renders it), so the coverage moves rather than dies:
// deleting it with the header would have left the day-boundary branch
// untested while a real consumer still depended on it.
describe("clkrange — day-boundary branch", () => {
  it("a span crossing a calendar day prefixes each end with its short date", () => {
    const tMax = Date.now();
    const range = clkrange(tMax - 1440 * 60000, tMax);
    // A real 24h span crosses a day boundary in EVERY timezone — assert the
    // SHAPE, not a literal, so this red-proves against `clkrange` silently
    // collapsing to the bare form rather than encoding one machine's TZ.
    expect(range).toMatch(/–/);
    expect(range.split("–")[0]).toMatch(/^[A-Za-z]{3} \d{1,2} \d{2}:\d{2}:\d{2}$/);
  });

  it("a same-day span stays bare HH:MM:SS–HH:MM:SS", () => {
    // Anchored at local noon so a 1h window cannot straddle local midnight in
    // any real timezone — a TZ-safe way to force the SAME-day branch.
    const noon = new Date();
    noon.setHours(12, 0, 0, 0);
    const anchor = noon.getTime();
    const range = clkrange(anchor - 60 * 60000, anchor);
    expect(range.split("–")[0]).not.toMatch(/[A-Za-z]/);
  });
});

/** (U3-7/U5-2) Moved here from `lenses/mission/graph.test.ts` with the
 * function itself: there is ONE duration formatter now, and it does not
 * belong to the mission graph. The hour case is the whole reason the other
 * one (`fmtDuration`, no rollover) is gone — `lenses/session/sessionRun.ts`
 * formatted a dispatch's wall clock with it, and a dispatch over an hour is
 * ordinary. The sub-hour cases are the parity contract: every golden
 * recorded against `fmtDuration` must still read identically. */
describe("fmtElapsed — the one duration formatter", () => {
  it("renders m:ss, and rolls over to h:mm:ss past an hour", () => {
    expect(fmtElapsed(0)).toBe("0:00");
    expect(fmtElapsed(65_000)).toBe("1:05");
    expect(fmtElapsed(3_661_000)).toBe("1:01:01");
    // The measured shape of the bug: 75 minutes read as "75:23".
    expect(fmtElapsed(4_523_000)).toBe("1:15:23");
  });

  it("matches the retired fmtDuration below an hour, so no parity golden moves", () => {
    const retired = (ms: number) => {
      const c = Math.max(0, ms);
      return Math.floor(c / 60000) + ":" + String(Math.floor(c / 1000) % 60).padStart(2, "0");
    };
    for (const ms of [0, 1, 999, 1000, 59_999, 60_000, 61_500, 599_000, 3_599_999]) {
      expect(fmtElapsed(ms)).toBe(retired(ms));
    }
    // …and diverges only where the old one was wrong.
    expect(fmtElapsed(3_600_000)).not.toBe(retired(3_600_000));
  });

  it("never renders a negative duration", () => {
    expect(fmtElapsed(-5_000)).toBe("0:00");
  });

  it("(C4) renders the missing-value dash for a duration it cannot compute, not a confident 0:00", () => {
    // `NaN` is reachable in production: `sessionRun.ts`'s `runWallMs` falls
    // back to `T(close.ts) - startTs`, and an unparsable `ts` subtracts to
    // NaN. Rendering that as "0:00" asserts "this run took no time", which
    // is a lie shaped like data — the same failure the rest of this lens
    // uses "—" to avoid (TURNS, TOKENS IN and CTX all already do).
    expect(fmtElapsed(NaN)).toBe(MISSING);
    expect(fmtElapsed(Infinity)).toBe(MISSING);
    expect(fmtElapsed(-Infinity)).toBe(MISSING);
    // `0` is a real, computable duration and keeps reading as one.
    expect(fmtElapsed(0)).toBe("0:00");
  });

  it("(C5) matches the retired fmtDuration across the WHOLE sub-hour range, not nine samples", () => {
    // The nine-sample version could not have caught a rollover boundary it
    // did not happen to name. This sweeps every 17ms — coprime with 1000
    // and 60000, so it lands inside every second and every minute.
    const retired = (ms: number) => Math.floor(ms / 60000) + ":" + String(Math.floor(ms / 1000) % 60).padStart(2, "0");
    for (let ms = 0; ms < 3_600_000; ms += 17) {
      if (fmtElapsed(ms) !== retired(ms)) throw new Error(`fmtElapsed(${ms}) = ${fmtElapsed(ms)}, retired = ${retired(ms)}`);
    }
  });
});

// ── (fix-loop 4) how many relative-age formatters does this app have? ──
//
// `MachineHealthRegion.tsx` deleted a local `relSecondsAgo` in favor of
// `relAgoFrom`, and the comment justifying that deletion asserted a fact
// about the rest of the app ("the SAME formatter the fleet strip and the run
// list already render their ages with") that was FALSE for the run list. A
// prose claim about how many of a thing exist rots the moment somebody adds
// one, and nothing goes red.
//
// So the claim is a test. THREE surfaces format an age, each for a stated
// reason, and a fourth has to argue with this file to arrive:
//   `lib/format.ts::relAgoFrom`            the shared one — ms epochs, and
//                                          the only one with a "just now"
//                                          bucket
//   `lenses/runs/format.ts::runsAgo`       the run list — SECOND-resolution
//                                          timestamps, no "just now"; a
//                                          3s-old run reads "3s ago", so it
//                                          is not a byte-identical fold
//   `components/RecordView.tsx::relTime`   the record panel — parses an ISO
//                                          string, returns `null` (not text)
//                                          when it cannot
//
// The detector is the bucket ladder itself, the one thing every such
// formatter must contain to be one: a `}m ago`/`}h ago`/`}d ago` template
// tail. Copying an existing formatter — the actual failure mode — carries
// the ladder along with it, so the copy is counted.
describe("age formatters — three surfaces, and a fourth cannot arrive quietly", () => {
  const AGE_LADDER = /\}[mhd] ago`/;
  const SRC_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

  /** Every non-test source file under `ui/src`, as a `src`-relative path.
   * Same walk `src/no-danger.test.ts` uses for the same reason — the
   * invariant is about the SOURCE TREE, so it has to read the tree. */
  function sourceFiles(dir: string = SRC_DIR, prefix = ""): string[] {
    const out: string[] = [];
    for (const entry of readdirSync(dir)) {
      const full = path.join(dir, entry);
      if (statSync(full).isDirectory()) {
        if (entry === "types" || entry === "node_modules") continue;
        out.push(...sourceFiles(full, prefix ? `${prefix}/${entry}` : entry));
        continue;
      }
      if (/\.(ts|tsx)$/.test(entry) && !/\.test\.(ts|tsx)$/.test(entry)) {
        out.push(prefix ? `${prefix}/${entry}` : entry);
      }
    }
    return out.sort();
  }

  const HOMES = ["components/RecordView.tsx", "lenses/runs/format.ts", "lib/format.ts"];

  it("exactly three source files declare a relative-age formatter", () => {
    const files = sourceFiles();
    // The inverted case FIRST: prove the walk actually reached the source
    // tree. A walk that found nothing would satisfy any "no fourth formatter"
    // phrasing forever, and read exactly as green as this does.
    expect(files.length).toBeGreaterThan(50);
    expect(files).toContain("lib/format.ts");

    const found = files.filter((f) => AGE_LADDER.test(readFileSync(path.join(SRC_DIR, f), "utf8")));
    expect(found).toEqual(HOMES);
  });

  it("and none of the three has quietly grown a SECOND one", () => {
    for (const f of HOMES) {
      const ladders = (readFileSync(path.join(SRC_DIR, f), "utf8").match(/\}d ago`/g) ?? []).length;
      // The filename rides in the assertion so a failure names the file
      // rather than reporting `2 !== 1` with no address.
      expect(`${f}: ${ladders}`).toBe(`${f}: 1`);
    }
  });
});
