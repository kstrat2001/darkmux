// node --test scripts/slop-chop/*.test.mjs   (runs in CI's ui job; needs ui/node_modules)
import test from "node:test";
import assert from "node:assert/strict";
import { scanSource, qualifying, classify, clusters } from "./survey.mjs";
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
