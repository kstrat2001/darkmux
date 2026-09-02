// slop-chop oracle (#2206, #2207) — stage B: for each surveyed site, recompute
// the truth table FROM THE ORIGINAL EXPRESSION TEXT, so the expected values a
// later gate checks an extraction against were never authored by the model
// that proposes the extraction. That independence is the whole point of the
// oracle; #2207's characterization-test rule is built on it.
//
//   node scripts/slop-chop/oracle.mjs --sites sites.json --out oracles.json
import fs from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { decompose, leaves, truthTable } from "./survey.mjs";

const require = createRequire(new URL("../../ui/package.json", import.meta.url));
const ts = require("typescript");

/** Parse a condition's text back into an expression and return its oracle. */
export function oracleFor(text) {
  const src = ts.createSourceFile("expr.ts", `(${text});`, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const stmt = src.statements[0];
  if (!stmt || !ts.isExpressionStatement(stmt)) throw new Error(`not an expression: ${text}`);
  const tree = decompose(stmt.expression);
  const ops = leaves(tree);
  return { ops, table: truthTable(tree, ops) };
}

/**
 * Oracles for every site. A site whose recomputed operands disagree with the
 * survey's is flagged `mismatch: true` rather than silently overwritten —
 * the survey and the oracle are two independent readings of the same text
 * and a disagreement is a finding about the tooling, not about the code.
 */
export function oracles(sites) {
  return sites.map((s) => {
    const o = oracleFor(s.text);
    const mismatch = s.ops && (o.ops.length !== s.ops.length || o.ops.some((x, i) => x !== s.ops[i]));
    return { i: s.i, file: s.file, line: s.line, ops: o.ops, table: o.table, mismatch: !!mismatch };
  });
}

function main(argv) {
  let sitesPath = null;
  let out = null;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--sites") sitesPath = argv[++i];
    else if (argv[i] === "--out") out = argv[++i];
  }
  if (!sitesPath) {
    console.error("usage: oracle.mjs --sites sites.json [--out oracles.json]");
    process.exit(2);
  }
  const sites = JSON.parse(fs.readFileSync(sitesPath, "utf8"));
  const result = oracles(sites);
  const mismatches = result.filter((r) => r.mismatch).length;
  if (out) fs.writeFileSync(out, JSON.stringify(result, null, 2));
  console.log(`oracles: ${result.length}  mismatches: ${mismatches}${out ? `  → ${out}` : ""}`);
  if (mismatches) process.exit(1);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main(process.argv.slice(2));
