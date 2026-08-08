#!/usr/bin/env bun
// Standalone tripwire — the hard gate, independent of record.mjs's own
// inline check. record.mjs refuses to WRITE a fixture with a residual
// sentinel hit; this script re-scans everything that's actually COMMITTED
// under corpus/ and goldens/ (the files git would ship), so a hand-edited
// fixture, a stale file from a prior run, or a golden that happened to
// quote raw corpus text all get the same scrutiny. Exit code 2 on any hit
// (mirrors `darkmux flow integrity-check`'s tamper-signal convention), so
// this composes into a CI/pre-commit gate later without changing shape.

import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { scanForSentinels } from "./lib/sanitize.mjs";
import { CORPUS_DIR, GOLDENS_DIR } from "./lib/paths.js";

function walkFiles(dir) {
  let out = [];
  let entries;
  try {
    entries = readdirSync(dir);
  } catch (_) {
    return out;
  }
  for (const name of entries) {
    const p = path.join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) out = out.concat(walkFiles(p));
    else out.push(p);
  }
  return out;
}

function main() {
  const targets = [...walkFiles(CORPUS_DIR), ...walkFiles(GOLDENS_DIR)];
  if (!targets.length) {
    console.error("tripwire: no files found under corpus/ or goldens/ — nothing to scan. Run `bun run record` (and `bun run extract`) first.");
    process.exit(2);
  }
  let anyHit = false;
  for (const f of targets) {
    const text = readFileSync(f, "utf8");
    const hits = scanForSentinels(text);
    if (hits.length) {
      anyHit = true;
      console.error(`TRIPWIRE HIT: ${path.relative(process.cwd(), f)}`);
      for (const h of hits) console.error(`  "${h.sentinel}" x${h.count}`);
    }
  }
  if (anyHit) {
    console.error("");
    console.error("tripwire FAILED — sensitive identifiers survive in committed fixture/golden text. Fix the sanitizer, do not hand-edit the fixture.");
    process.exit(2);
  }
  console.log(`tripwire OK — scanned ${targets.length} files under corpus/ + goldens/, zero sentinel hits.`);
}

main();
