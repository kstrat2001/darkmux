// node --test scripts/ts-function-key.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { keyAt, parse } from "./ts-function-key.mjs";

/** The key of the function whose head holds the marker `/*@*\/`. */
function keyOf(src) {
  const at = src.indexOf("/*@*/");
  const clean = src.replace("/*@*/", "");
  const sf = parse("a.tsx", clean);
  const { line, character } = sf.getLineAndCharacterOfPosition(at);
  return keyAt(sf, line + 1, character + 1);
}

test("a declaration, a method and an accessor are keyed by their name", () => {
  assert.equal(keyOf("export function /*@*/parse(x: string) { return x; }"), "parse");
  assert.equal(keyOf("class Store { /*@*/ingest(x: number) { return x; } }"), "Store.ingest");
  assert.equal(keyOf("class Store { get /*@*/size() { return 1; } }"), "Store.size");
});

test("an arrow is keyed by what it is bound to", () => {
  assert.equal(keyOf("export const Card = /*@*/(p: number) => p;"), "Card");
  assert.equal(keyOf("const o = { handle: /*@*/(x: number) => x };"), "handle");
  assert.equal(keyOf("function C() { const rows = useMemo(/*@*/() => 1, []); }"), "C.rows");
  assert.equal(keyOf("function C() { const f = /*@*/function () { return 1; }; }"), "C.f");
});

test("a callback is keyed by its enclosing named function and an ordinal within it", () => {
  const src = (marked) =>
    "function A() { xs.map(() => 1); }\n" +
    "function B() { xs.map(() => 1); xs.forEach(() => 2); xs.map(" + marked + ") ; }";
  assert.equal(keyOf(src("/*@*/() => 3")), "B.<map callback>#2");
  // A callback added in ANOTHER function does not renumber this one.
  const more = "function A() { xs.map(() => 1); xs.map(() => 0); }\n" +
    "function B() { xs.map(() => 1); xs.forEach(() => 2); xs.map(/*@*/() => 3) ; }";
  assert.equal(keyOf(more), "B.<map callback>#2");
  // A callback of another kind in the same function does not either.
  assert.equal(keyOf("function B() { xs.forEach(() => 0); xs.map(/*@*/() => 3); }"), "B.<map callback>#1");
});

test("a callback in JSX is keyed like any other", () => {
  const src = "export function List() { return <ul>{items.map(/*@*/(i) => <li>{i}</li>)}</ul>; }";
  assert.equal(keyOf(src), "List.<map callback>#1");
});
