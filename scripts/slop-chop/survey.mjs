// slop-chop survey (#2206, #2212) — stage A of the pipeline: find compound
// boolean conditions in TypeScript/JavaScript via the AST, decompose each
// into its distinct operands, and compute the truth table of the ORIGINAL
// expression over those operands. No model is involved at this stage; this
// is the "wide findings" half, and it is mechanical on purpose (#2212: the
// tokens belong further down the pipeline, where work is bounded and gated).
//
// Rescued from a session scratchpad prototype (2026-08-31/09-01), where it
// produced the numbers quoted in #2206 (197 prefilter hits → 244 AST sites →
// 46 qualifying in darkmux `ui/src`). Now importable AND runnable:
//
//   node scripts/slop-chop/survey.mjs <file.ts> [...more] [--out sites.json] [--strip <prefix>]
//   EMIT_LIST=/tmp/candidates.jsonl node scripts/slop-chop/survey.mjs ui/src/**/*.ts
//
// `typescript` is resolved from ui/'s own node_modules (the one place this
// repo already pins it), from THIS file's location, so the script works from
// any cwd and on any checkout — the prototype hard-coded an absolute path.
import fs from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const require = createRequire(new URL("../../ui/package.json", import.meta.url));
const ts = require("typescript");

const AND = ts.SyntaxKind.AmpersandAmpersandToken;
const OR = ts.SyntaxKind.BarBarToken;

/** Flatten a boolean expression into {op:'and'|'or', kids} | {op:'not', kid} | {op:'leaf', text}. */
export function decompose(node) {
  if (ts.isParenthesizedExpression(node)) return decompose(node.expression);
  if (ts.isBinaryExpression(node) && (node.operatorToken.kind === AND || node.operatorToken.kind === OR)) {
    const op = node.operatorToken.kind === AND ? "and" : "or";
    const l = decompose(node.left);
    const r = decompose(node.right);
    const kids = [];
    for (const k of [l, r]) (k.op === op ? kids.push(...k.kids) : kids.push(k));
    return { op, kids };
  }
  if (ts.isPrefixUnaryExpression(node) && node.operator === ts.SyntaxKind.ExclamationToken) {
    return { op: "not", kid: decompose(node.operand) };
  }
  return { op: "leaf", text: node.getText().replace(/\s+/g, " ").trim(), node };
}

/** Distinct leaf texts, in first-seen order. */
export function leaves(tree) {
  const acc = [];
  const walk = (t) => (t.op === "leaf" ? acc.push(t.text) : t.op === "not" ? walk(t.kid) : t.kids.forEach(walk));
  walk(tree);
  return [...new Set(acc)];
}

/** The leaf NODES (one per distinct text, first seen), for node-based classification. */
export function leafNodes(tree) {
  const seen = new Map();
  const walk = (t) => {
    if (t.op === "leaf") {
      if (!seen.has(t.text)) seen.set(t.text, t.node);
    } else if (t.op === "not") walk(t.kid);
    else t.kids.forEach(walk);
  };
  walk(tree);
  return [...seen.values()];
}

export function evalTree(t, env) {
  if (t.op === "leaf") return !!env[t.text];
  if (t.op === "not") return !evalTree(t.kid, env);
  return t.op === "and" ? t.kids.every((k) => evalTree(k, env)) : t.kids.some((k) => evalTree(k, env));
}

/**
 * Truth table over the distinct leaves: row m sets leaf i to bit i of m, so
 * row 0 is all-false and the last row is all-true. Returned as a "0"/"1"
 * string of length 2^n. Two sites with the same n and the same table are
 * provably equivalent as boolean functions of their operands.
 */
export function truthTable(tree, ls = leaves(tree)) {
  const rows = [];
  for (let m = 0; m < 1 << ls.length; m++) {
    const env = {};
    ls.forEach((l, i) => (env[l] = !!(m & (1 << i))));
    rows.push(evalTree(tree, env) ? 1 : 0);
  }
  return rows.join("");
}

// Purity is about SIDE EFFECTS, not about being a call. Three buckets:
//  - mutating: a real assignment (not ==/===/!=/<=/>=/=>), await, ++/--
//  - unknown : calls a function we cannot vouch for
//  - pure    : property access, comparisons, and known-pure builtins
// The `=` arm excludes a following `=` (equality) AND `>` (an arrow function
// is a value, not an assignment) — the prototype missed the arrow case and
// classified `a => b` as mutating; the test pins it.
const MUTATES = /(^|[^=!<>+\-*/%&|^])=(?![=>])|await |\+\+|--/;
const PURE_CALL = /^(Array\.isArray|Number\.is\w+|Object\.(keys|values|entries)|Math\.\w+|String|Number|Boolean|JSON\.stringify)\($/;

export function classify(leaf) {
  if (MUTATES.test(leaf)) return "mutating";
  const calls = leaf.match(/[\w.]+\s*\(/g) || [];
  if (calls.some((c) => !PURE_CALL.test(c.replace(/\s+/g, "")))) return "unknown";
  return "pure";
}

const worst = (a, b) =>
  a === "mutating" || b === "mutating" ? "mutating" : a === "unknown" || b === "unknown" ? "unknown" : "pure";

/**
 * Classify a leaf by its AST NODE, not its text. The text regex above is kept
 * for callers that only have a string, but it cannot see compound assignment
 * (`x += 1`, `x ||= 1` — the `+`/`|` before `=` fell in the regex's own
 * exclusion class and read as PURE, the unsafe direction) and it mis-reads
 * `=` or `--` inside string literals as mutation (the conservative
 * direction). The node knows: an assignment operator token, an `await`, a
 * `++`/`--`, or a call are structural facts. (Review finding on #2266.)
 */
export function classifyNode(node) {
  let cls = "pure";
  const bump = (c) => (cls = worst(cls, c));
  const walk = (n) => {
    if (ts.isBinaryExpression(n) && isAssignmentOperator(n.operatorToken.kind)) bump("mutating");
    else if (ts.isAwaitExpression(n)) bump("mutating");
    else if (
      (ts.isPrefixUnaryExpression(n) || ts.isPostfixUnaryExpression(n)) &&
      (n.operator === ts.SyntaxKind.PlusPlusToken || n.operator === ts.SyntaxKind.MinusMinusToken)
    )
      bump("mutating");
    else if (ts.isCallExpression(n) || ts.isNewExpression(n)) {
      const callee = n.expression.getText().replace(/\s+/g, "");
      if (!PURE_CALL.test(callee + "(")) bump("unknown");
    }
    ts.forEachChild(n, walk);
  };
  walk(node);
  return cls;
}

function isAssignmentOperator(kind) {
  return kind >= ts.SyntaxKind.FirstAssignment && kind <= ts.SyntaxKind.LastAssignment;
}

/**
 * Scan one source text for condition sites: `if`/`while`/`do` tests and
 * ternary tests whose top-level operator is `&&` or `||` with 2+ distinct
 * operands. (Qualification for the rule — 3+ operands — is the caller's
 * filter; see `qualifying()`.) `label` is what goes in `file`.
 */
export function scanSource(label, text, { tsx = /x$/.test(label) } = {}) {
  const src = ts.createSourceFile(label, text, ts.ScriptTarget.Latest, true, tsx ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const sites = [];
  const visit = (n) => {
    let cond = null;
    if (ts.isIfStatement(n)) cond = n.expression;
    else if (ts.isConditionalExpression(n)) cond = n.condition;
    else if (ts.isWhileStatement(n) || ts.isDoStatement(n)) cond = n.expression;
    if (cond) {
      const tree = decompose(cond);
      if (tree.op === "and" || tree.op === "or") {
        const ls = leaves(tree);
        if (ls.length >= 2) {
          // `text` is the FULL condition — the oracle re-parses it as the
          // expression, so it must never be cut. `preview` is the display
          // form. (The prototype stored only a 240-char slice, which was
          // harmless as a column and became a data bug the moment the oracle
          // read it back: a cut inside a string literal still parses under
          // TypeScript's tolerant parser and yields a plausible wrong table.)
          const full = cond.getText().replace(/\s+/g, " ");
          sites.push({
            file: label,
            line: src.getLineAndCharacterOfPosition(cond.getStart()).line + 1,
            n: ls.length,
            ops: ls,
            cls: leafNodes(tree).map(classifyNode).reduce(worst, "pure"),
            text: full,
            preview: full.length > 240 ? full.slice(0, 237) + "..." : full,
            table: truthTable(tree, ls),
          });
        }
      }
    }
    ts.forEachChild(n, visit);
  };
  visit(src);
  return sites;
}

export function scanFile(path, { strip = "" } = {}) {
  const label = strip && path.startsWith(strip) ? path.slice(strip.length) : path;
  return scanSource(label, fs.readFileSync(path, "utf8"), { tsx: /x$/.test(path) });
}

/** The rule's own bar: 3+ distinct operands and nothing that mutates. */
export function qualifying(sites) {
  return sites.filter((s) => s.n >= 3 && s.cls !== "mutating");
}

/** Provably-equivalent clusters among non-mutating sites: same n, same table. */
export function clusters(sites) {
  const byKey = new Map();
  for (const s of sites.filter((x) => x.cls !== "mutating")) {
    const k = `${s.n}:${s.table}`;
    if (!byKey.has(k)) byKey.set(k, []);
    byKey.get(k).push(s);
  }
  return [...byKey.entries()].filter(([, v]) => v.length > 1).sort((a, b) => b[1].length - a[1].length);
}

function main(argv) {
  const files = [];
  let out = null;
  let strip = "";
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--out") out = argv[++i];
    else if (argv[i] === "--strip") strip = argv[++i];
    else files.push(argv[i]);
  }
  if (files.length === 0) {
    console.error("usage: survey.mjs <file.ts> [...] [--out sites.json] [--strip <prefix>]");
    process.exit(2);
  }
  const sites = files.flatMap((f) => scanFile(f, { strip })).sort((a, b) => b.n - a.n);
  const q = qualifying(sites);
  console.log(
    `SITES: ${sites.length}   pure: ${sites.filter((s) => s.cls === "pure").length}  unknown-call: ${sites.filter((s) => s.cls === "unknown").length}  mutating(skip): ${sites.filter((s) => s.cls === "mutating").length}   qualifying(n>=3): ${q.length}`,
  );
  console.log(`by operand count: ${[2, 3, 4, 5].map((k) => `${k}→${sites.filter((s) => s.n === k).length}`).join("  ")}  (>5: ${sites.filter((s) => s.n > 5).length})`);
  console.log("\n--- top sites by operand count ---");
  for (const s of sites.slice(0, 8)) {
    console.log(`${s.file}:${s.line}  n=${s.n} ${s.cls.padEnd(8)} table=${s.table.length <= 16 ? s.table : s.table.slice(0, 16) + "…"}\n    ${s.preview}`);
  }
  const cl = clusters(sites);
  console.log(`\n--- provably-equivalent clusters (non-mutating sites): ${cl.length} ---`);
  for (const [k, v] of cl.slice(0, 5)) {
    console.log(`  n=${k.split(":")[0]} table=${k.split(":")[1].slice(0, 12)} × ${v.length}:  ${v.slice(0, 3).map((s) => `${s.file}:${s.line}`).join("  ")}`);
  }
  const rows = q.map((s, i) => ({ i: i + 1, ...s }));
  if (out) {
    fs.writeFileSync(out, JSON.stringify(rows, null, 2));
    console.error(`wrote ${rows.length} qualifying sites to ${out}`);
  }
  if (process.env.EMIT_LIST) {
    fs.writeFileSync(process.env.EMIT_LIST, rows.map((r) => JSON.stringify(r)).join("\n"));
    console.error(`wrote ${rows.length} candidates to ${process.env.EMIT_LIST}`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main(process.argv.slice(2));
