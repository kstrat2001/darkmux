// ts-function-key.mjs: a stable name for the TypeScript function at a source
// position, for scripts/complexity-ratchet.py. The ratchet keys a function by
// name, never by where it sits, so an edit above it does not move it and a new
// function cannot inherit another's baseline.
//
//   echo '[{"file": "/abs/a.tsx", "line": 12, "column": 5}]' | node scripts/ts-function-key.mjs
//   -> ["Outer.inner"]
//
// Line and column are ESLint's (1-based), pointing into the function's head.
// The name is the chain of named enclosing functions and classes, then the
// function's own name:
//   * a declaration, method, accessor or named function expression: its name;
//   * a function assigned to something: `const X = () => ...`, `x: () => ...`,
//     `this.x = () => ...`, seen through a call it is passed to, so
//     `const rows = useMemo(() => ...)` is `rows`;
//   * otherwise a callback: `<map callback>#2`, the second `.map` callback
//     inside its nearest named enclosing function, counted in source order.
//     Never a file-wide ordinal: a callback added in another function does
//     not renumber this one.
//
// `typescript` is resolved from ui/'s node_modules, as the slop-chop scripts do.
import fs from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(new URL("../ui/package.json", import.meta.url));
const ts = require("typescript");

const isFunction = (n) =>
  ts.isFunctionDeclaration(n) || ts.isFunctionExpression(n) || ts.isArrowFunction(n) ||
  ts.isMethodDeclaration(n) || ts.isGetAccessorDeclaration(n) || ts.isSetAccessorDeclaration(n) ||
  ts.isConstructorDeclaration(n);

const isWrapper = (p, child) =>
  ts.isParenthesizedExpression(p) || ts.isAsExpression(p) || ts.isSatisfiesExpression(p) ||
  ts.isNonNullExpression(p) || (ts.isCallExpression(p) && p.arguments.includes(child));

/** The name a function or class is bound to, or null when it has none. */
function ownName(n) {
  if (ts.isConstructorDeclaration(n)) return "constructor";
  if (n.name && (ts.isIdentifier(n.name) || ts.isPrivateIdentifier(n.name) || ts.isStringLiteral(n.name))) return n.name.text;
  let child = n;
  let p = n.parent;
  while (p && isWrapper(p, child)) {
    child = p;
    p = p.parent;
  }
  if (!p) return null;
  if (ts.isVariableDeclaration(p) && ts.isIdentifier(p.name)) return p.name.text;
  if ((ts.isPropertyAssignment(p) || ts.isPropertyDeclaration(p)) && p.initializer === child) return p.name.getText();
  if (ts.isBinaryExpression(p) && p.operatorToken.kind === ts.SyntaxKind.EqualsToken && p.right === child) return p.left.getText();
  if (ts.isExportAssignment(p)) return "default";
  return null;
}

/** `<map callback>` for a function passed to `.map(...)`, `<callback>`
 *  when it is not a call argument at all. */
function callbackLabel(n) {
  const p = n.parent;
  if (p && ts.isCallExpression(p) && p.arguments.includes(n)) {
    const callee = ts.isPropertyAccessExpression(p.expression) ? p.expression.name.text : p.expression.getText();
    return `<${callee} callback>`;
  }
  return "<callback>";
}

const isScope = (n) => isFunction(n) || ts.isClassDeclaration(n) || ts.isClassExpression(n);

/** The nearest enclosing node that has a name of its own, or the file. */
function namedParent(n) {
  for (let p = n.parent; p; p = p.parent) if (isScope(p) && ownName(p)) return p;
  return n.getSourceFile();
}

function qualified(n) {
  const names = [];
  for (let p = namedParent(n); !ts.isSourceFile(p); p = namedParent(p)) names.unshift(ownName(p));
  return names;
}

/** `label#k`: k counts the unnamed functions with this label whose nearest
 *  named parent is `n`'s, in source order. */
function callbackKey(n) {
  const scope = namedParent(n);
  const label = callbackLabel(n);
  let k = 0;
  let found = 0;
  const visit = (x) => {
    if (found) return;
    if (isFunction(x) && !ownName(x) && namedParent(x) === scope && callbackLabel(x) === label) {
      k += 1;
      if (x === n) found = k;
    }
    ts.forEachChild(x, visit);
  };
  visit(scope);
  return `${label}#${found}`;
}

/** The innermost function whose span holds `pos`. */
function functionAt(sf, pos) {
  let best = null;
  const visit = (x) => {
    if (pos < x.getStart(sf) || pos >= x.getEnd()) return;
    if (isFunction(x)) best = x;
    ts.forEachChild(x, visit);
  };
  visit(sf);
  return best;
}

export function keyAt(sf, line, column) {
  const pos = sf.getPositionOfLineAndCharacter(line - 1, column - 1);
  const fn = functionAt(sf, pos);
  if (!fn) throw new Error(`no function at ${sf.fileName}:${line}:${column}`);
  const own = ownName(fn) ?? callbackKey(fn);
  return [...qualified(fn), own].join(".");
}

export function parse(file, text) {
  const kind = file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS;
  return ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, kind);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const wanted = JSON.parse(fs.readFileSync(0, "utf8"));
  const files = new Map();
  const keys = wanted.map(({ file, line, column }) => {
    if (!files.has(file)) files.set(file, parse(file, fs.readFileSync(file, "utf8")));
    return keyAt(files.get(file), line, column);
  });
  process.stdout.write(JSON.stringify(keys));
}
