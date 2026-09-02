// node --test scripts/slop-chop/*.test.mjs   (runs in CI's ui job; needs ui/node_modules)
import test from "node:test";
import assert from "node:assert/strict";
import { scanSource, qualifying, classify, clusters } from "./survey.mjs";
// (review on #2266) node-based classification + full-text oracle contract
import { oracleFor, oracles } from "./oracle.mjs";

const src = `
function f(status, daysOverdue, balance, x, a, b, c, tf, nf, cf) {
  // 3 operands, one domain idea: the rule's positive case
  if (status === "active" && daysOverdue < 30 && balance > 0) return 1;
  // mixed && / || — also qualifies on the operator-mix bar
  if (a && (b || c)) return 2;
  // a null guard chain: passes the SURVEY (it is a 3-operand && chain) —
  // the rule's no_match prose excludes it at JUDGMENT, not here
  if (x && x.y && x.y.z) return 3;
  // a default chain selects a value; it is still an || site to the survey
  const d = a || b || "default";
  // 2 operands: a site, but not qualifying
  if (a && b) return 4;
  // an impure operand
  if (a && fetch(b)) return 5;
  // a mutating operand
  if (a && (x = 1)) return 6;
  // ternary test
  const t = tf && nf && cf ? 1 : 0;
  return d + t;
}
`;

test("survey finds every &&/|| condition site with 2+ distinct operands", () => {
  const sites = scanSource("fixture.ts", src);
  const texts = sites.map((s) => s.text);
  assert.ok(texts.includes('status === "active" && daysOverdue < 30 && balance > 0'));
  assert.ok(texts.includes("a && (b || c)"));
  assert.ok(texts.includes("x && x.y && x.y.z"));
  assert.ok(texts.includes("a && b"));
  assert.ok(texts.includes("tf && nf && cf"), "ternary tests are sites too");
  // `const d = a || b || "default"` is an initializer, not a condition site
  assert.ok(!texts.some((t) => t.includes('"default"')));
});

test("qualifying applies the rule's bar: 3+ operands and nothing mutating", () => {
  const sites = scanSource("fixture.ts", src);
  const q = qualifying(sites).map((s) => s.text);
  assert.ok(q.includes('status === "active" && daysOverdue < 30 && balance > 0'));
  assert.ok(q.includes("a && (b || c)"));
  assert.ok(q.includes("x && x.y && x.y.z"), "null guards pass the survey by design; no_match handles them");
  assert.ok(!q.includes("a && b"), "2 operands does not qualify");
  assert.ok(!q.includes("a && (x = 1)"), "a mutating operand never qualifies");
});

test("classify: pure / unknown call / mutating", () => {
  assert.equal(classify("Array.isArray(a)"), "pure");
  assert.equal(classify("x.y.z"), "pure");
  assert.equal(classify("fetch(b)"), "unknown");
  assert.equal(classify("(x = 1)"), "mutating");
  assert.equal(classify("a === b"), "pure", "comparison is not assignment");
  assert.equal(classify("a => b"), "pure", "arrow is not assignment");
});

test("oracle: truth table is computed from the original expression, bit i = operand i", () => {
  // leaves in first-seen order: a, b, c ; row m: a=bit0, b=bit1, c=bit2
  // a && (b || c): rows 0..7 → 0,0,0,1,0,1,0,1
  const o = oracleFor("a && (b || c)");
  assert.deepEqual(o.ops, ["a", "b", "c"]);
  assert.equal(o.table, "00010101");
  // a null guard chain is a plain conjunction
  assert.equal(oracleFor("x && x.y && x.y.z").table, "00000001");
  // negation is honored
  assert.equal(oracleFor("!a && b").table, "0010");
});

test("oracle flags a survey/oracle operand disagreement instead of hiding it", () => {
  const [ok] = oracles([{ i: 1, file: "f", line: 1, ops: ["a", "b"], text: "a && b" }]);
  assert.equal(ok.mismatch, false);
  const [bad] = oracles([{ i: 2, file: "f", line: 2, ops: ["a", "zzz"], text: "a && b" }]);
  assert.equal(bad.mismatch, true);
});

test("clusters group provably-equivalent sites by (n, table)", () => {
  const sites = scanSource("fixture.ts", "if (p && q && r) {}\nif (u && v && w) {}\nif (u || v || w) {}\n");
  const cl = clusters(sites);
  assert.equal(cl.length, 1, "the two 3-way conjunctions cluster; the disjunction does not");
  assert.equal(cl[0][1].length, 2);
});

test("site classification is AST-based: compound assignment is mutating, string contents are not", () => {
  const src = `
function g(x, a, s) {
  if (a && (x += 1) && s) return 1;      // compound assignment: mutating
  if (a && (x ||= 1) && s) return 2;     // logical assignment: mutating
  if (a && s === "a=b" && x) return 3;   // '=' inside a string: NOT mutation
  if (a && s === \`\${a}--\${x}\` && x) return 4; // '--' inside a template: NOT mutation
  if (a && i++ && x) return 5;           // increment: mutating
}
`;
  const by = Object.fromEntries(scanSource("fixture.ts", src).map((st) => [st.line, st.cls]));
  assert.equal(by[3], "mutating", "x += 1");
  assert.equal(by[4], "mutating", "x ||= 1");
  assert.equal(by[5], "pure", "a string containing '=' is pure");
  assert.equal(by[6], "pure", "a template containing '--' is pure");
  assert.equal(by[7], "mutating", "i++");
  // the text-only classifier is still exported and still misses compound assignment —
  // pinned so nobody reaches for it as the site classifier again
  assert.equal(classify("(x += 1)"), "pure");
});

test("survey keeps the FULL condition text; preview is the display cut; the oracle round-trips a long site", () => {
  const long = Array.from({ length: 12 }, (_, i) => `veryLongOperandName${i}.someProperty === "expectedValue${i}"`).join(" && ");
  assert.ok(long.length > 240, `fixture must exceed the old 240-char cut (got ${long.length})`);
  const [site] = scanSource("fixture.ts", `if (${long}) {}`);
  assert.equal(site.text, long, "text is never truncated");
  assert.ok(site.preview.length <= 240 && site.preview.endsWith("..."), "preview is the display cut");
  assert.equal(site.n, 12);
  const [o] = oracles([site]);
  assert.equal(o.mismatch, false, "the oracle re-parses the FULL text and agrees with the survey");
  assert.equal(o.table.length, 1 << 12);
});

test("a // line comment inside a multi-line condition survives the survey→oracle round-trip", () => {
  const src = "function h(a, b, c) {\n  if (a && // note\n      b && c) return 1;\n}\n";
  const [site] = scanSource("fixture.ts", src);
  assert.deepEqual(site.ops, ["a", "b", "c"]);
  assert.ok(site.text.includes("\n"), "text keeps the newline that ends the line comment");
  assert.ok(!site.preview.includes("\n"), "preview is one line");
  const [o] = oracles([site]);
  assert.equal(o.mismatch, false);
  assert.equal(o.table, "00000001");
});

test("delete and yield are mutating; a tagged template is an unknown call", () => {
  const src = "function* k(o, a, x, tag) {\n  if (a && delete o.k && x) return 1;\n  if (a && (yield x) && a) return 2;\n  if (a && tag`t` && x) return 3;\n}\n";
  const by = Object.fromEntries(scanSource("fixture.ts", src).map((st) => [st.line, st.cls]));
  assert.equal(by[2], "mutating", "delete o.k");
  assert.equal(by[3], "mutating", "yield");
  assert.equal(by[4], "unknown", "tag`t`");
});
