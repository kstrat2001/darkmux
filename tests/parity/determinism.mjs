#!/usr/bin/env bun
// Determinism check — runs the extraction spec twice back-to-back and
// asserts every golden is byte-identical between runs. If it isn't, some
// source of nondeterminism (unfrozen time, an unstable sort, a live poll
// tick that snuck in before extraction) survived the freeze — find it before
// trusting anything this harness produces.

import { execSync } from "node:child_process";
import { readdirSync, readFileSync, cpSync, rmSync, mkdirSync, existsSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR, PARITY_DIR } from "./lib/paths.js";

const SNAPSHOT_DIR = path.join(PARITY_DIR, ".determinism-run1");

function runExtract(label) {
  console.log(`--- extraction run (${label}) ---`);
  execSync("bunx playwright test extract.spec.ts --reporter=list", { cwd: PARITY_DIR, stdio: "inherit" });
}

function snapshotGoldens(destDir) {
  rmSync(destDir, { recursive: true, force: true });
  mkdirSync(destDir, { recursive: true });
  cpSync(GOLDENS_DIR, destDir, { recursive: true });
}

function main() {
  if (!existsSync(GOLDENS_DIR) || readdirSync(GOLDENS_DIR).length === 0) {
    console.error("determinism: goldens/ is empty — run `bun run extract` at least once first (this script re-runs it twice on top).");
    process.exit(1);
  }

  runExtract("1 of 2");
  snapshotGoldens(SNAPSHOT_DIR);

  runExtract("2 of 2");

  const run1Files = readdirSync(SNAPSHOT_DIR).sort();
  const run2Files = readdirSync(GOLDENS_DIR).sort();
  let ok = true;

  if (JSON.stringify(run1Files) !== JSON.stringify(run2Files)) {
    ok = false;
    console.error(`determinism: FILE SET CHANGED between runs.\n  run1: ${JSON.stringify(run1Files)}\n  run2: ${JSON.stringify(run2Files)}`);
  }

  for (const f of run1Files) {
    if (!run2Files.includes(f)) continue; // already reported above
    const a = readFileSync(path.join(SNAPSHOT_DIR, f));
    const b = readFileSync(path.join(GOLDENS_DIR, f));
    if (!a.equals(b)) {
      ok = false;
      console.error(`determinism: MISMATCH in ${f} (run1 ${a.length}B vs run2 ${b.length}B)`);
      // A short unified-ish diff for the transcript: first differing line.
      const la = a.toString("utf8").split("\n");
      const lb = b.toString("utf8").split("\n");
      for (let i = 0; i < Math.max(la.length, lb.length); i++) {
        if (la[i] !== lb[i]) {
          console.error(`  first differing line ${i + 1}:\n  run1: ${JSON.stringify(la[i] ?? "<eof>")}\n  run2: ${JSON.stringify(lb[i] ?? "<eof>")}`);
          break;
        }
      }
    }
  }

  rmSync(SNAPSHOT_DIR, { recursive: true, force: true });

  if (!ok) {
    console.error("\ndeterminism FAILED — goldens differ between two back-to-back runs. Find the nondeterminism (unfrozen time, unstable sort, a live poll that fired) before trusting this harness.");
    process.exit(1);
  }
  console.log(`\ndeterminism OK — ${run1Files.length} golden files byte-identical across two runs.`);
}

main();
