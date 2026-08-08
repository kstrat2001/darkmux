#!/usr/bin/env bun
// Verify goldens/ WITHOUT mutating it — the fix for a QA finding (post-0a
// review): `bun run check` used to run `extract.spec.ts` directly, which
// WRITES goldens/ as a side effect. That means `check` could never detect
// drift — it regenerates the very thing it's supposed to be checking, then
// reports green against its own freshly-written output. QA proved this by
// hand-corrupting a golden and watching `check` stay green (extract
// silently overwrote the corruption before anything downstream could see
// it).
//
// This script separates the two concerns:
//   - `bun run rebaseline` (== `bun run extract`) explicitly regenerates
//     goldens/ from a fresh Playwright run — a deliberate, visible act, the
//     only path that's supposed to change what's in goldens/.
//   - `bun run verify` (this script) snapshots the CURRENT goldens/ before
//     running extraction, lets extraction overwrite goldens/, diffs the
//     result against the snapshot, and — critically — RESTORES the snapshot
//     if anything differs, so a failed verify never leaves goldens/
//     silently migrated. On success it's a no-op from the caller's
//     perspective (fresh extraction already matched what was there).
//
// `bun run check` calls `verify`, not `extract` — that's the actual fix.

import { execSync } from "node:child_process";
import { readdirSync, readFileSync, cpSync, rmSync, mkdirSync, existsSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR, PARITY_DIR } from "./lib/paths.js";

const SNAPSHOT_DIR = path.join(PARITY_DIR, ".verify-snapshot");

function snapshot(destDir, srcDir) {
  rmSync(destDir, { recursive: true, force: true });
  mkdirSync(destDir, { recursive: true });
  if (existsSync(srcDir)) cpSync(srcDir, destDir, { recursive: true });
}

function main() {
  if (!existsSync(GOLDENS_DIR) || readdirSync(GOLDENS_DIR).length === 0) {
    console.error("verify: goldens/ is empty — nothing to verify against. Run `bun run rebaseline` first to establish a baseline.");
    process.exit(1);
  }

  console.log("--- snapshotting current goldens/ (the baseline being verified) ---");
  snapshot(SNAPSHOT_DIR, GOLDENS_DIR);

  console.log("--- running extraction (writes goldens/ — compared against the snapshot below, then restored on any diff) ---");
  execSync("bunx playwright test extract.spec.ts --reporter=list", { cwd: PARITY_DIR, stdio: "inherit" });

  const before = readdirSync(SNAPSHOT_DIR).sort();
  const after = readdirSync(GOLDENS_DIR).sort();
  const diffs = [];

  if (JSON.stringify(before) !== JSON.stringify(after)) {
    diffs.push(`FILE SET CHANGED — before: ${JSON.stringify(before)}  after: ${JSON.stringify(after)}`);
  }
  for (const f of before) {
    if (!after.includes(f)) continue; // already reported above
    const a = readFileSync(path.join(SNAPSHOT_DIR, f));
    const b = readFileSync(path.join(GOLDENS_DIR, f));
    if (!a.equals(b)) {
      const la = a.toString("utf8").split("\n");
      const lb = b.toString("utf8").split("\n");
      let firstDiffLine = null;
      for (let i = 0; i < Math.max(la.length, lb.length); i++) {
        if (la[i] !== lb[i]) {
          firstDiffLine = `line ${i + 1}:\n    committed: ${JSON.stringify(la[i] ?? "<eof>")}\n    extracted: ${JSON.stringify(lb[i] ?? "<eof>")}`;
          break;
        }
      }
      diffs.push(`${f} differs (committed ${a.length}B vs freshly-extracted ${b.length}B)${firstDiffLine ? "\n    " + firstDiffLine : ""}`);
    }
  }

  if (diffs.length) {
    console.error("");
    console.error(`verify FAILED — freshly-extracted goldens differ from what's currently in goldens/:\n`);
    for (const d of diffs) console.error(`  - ${d}`);
    console.error("");
    console.error("Restoring goldens/ to its pre-verify state (verify never leaves a silent mutation behind).");
    rmSync(GOLDENS_DIR, { recursive: true, force: true });
    cpSync(SNAPSHOT_DIR, GOLDENS_DIR, { recursive: true });
    rmSync(SNAPSHOT_DIR, { recursive: true, force: true });
    console.error("If this drift is INTENTIONAL (the legacy viewer's recorded behavior genuinely changed), run `bun run rebaseline` and commit the new goldens/ deliberately.");
    process.exit(1);
  }

  rmSync(SNAPSHOT_DIR, { recursive: true, force: true });
  console.log(`\nverify OK — extraction reproduced all ${after.length} goldens byte-for-byte; goldens/ is unchanged.`);
}

main();
