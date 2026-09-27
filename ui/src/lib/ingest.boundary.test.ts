// The second net behind `lib/ingest.ts`'s boundary. The first is the type
// system: a record's action, level, category, stage and tier are opaque tags,
// so comparing one to a string literal, calling a string method on it, or
// testing it against a `string[]`/regex does not compile
// (`ingest.types.test.ts` pins that). This scan catches what the types cannot
// see: `Object.is`, a cast through `unknown`, `tagText(...)` (the one way to a
// tag's text) used for a decision rather than display, a literal `case` in a
// switch over a field, and a record's `ts` parsed outside ingest.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { describe, expect, it } from "vitest";

const SRC_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const BOUNDARY = path.join(SRC_DIR, "lib", "ingest.ts");

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      if (entry !== "types" && entry !== "node_modules") out.push(...sourceFiles(full));
    } else if (/\.(ts|tsx)$/.test(entry) && !/\.test\.(ts|tsx)$/.test(entry) && full !== BOUNDARY) {
      out.push(full);
    }
  }
  return out;
}

const isComment = (line: string) => /^\s*(\/\/|\/\*|\*)/.test(line);

const FIELD = String.raw`\.(?:action|level|category|stage|tier)\b`;
const LITERAL = String.raw`["'\x60]`;
const TEXT = String.raw`tagText\([^)]*\)`;
/** Each forbidden shape, with what it means. */
const RULES: { name: string; re: RegExp }[] = [
  { name: "a record field compared to a literal", re: new RegExp(String.raw`${FIELD}\s*[!=]==?\s*${LITERAL}`) },
  { name: "a literal compared to a record field", re: new RegExp(String.raw`${LITERAL}\s*[!=]==?\s*[\w.?]*${FIELD}`) },
  { name: "an action alias compared to a literal", re: new RegExp(String.raw`\baction\s*[!=]==?\s*${LITERAL}`) },
  { name: "a string method on a record field", re: new RegExp(String.raw`${FIELD}\??\.(?:startsWith|endsWith|includes|match|split|replace)\(`) },
  { name: "Object.is on a record field", re: new RegExp(String.raw`Object\.is\([^)]*${FIELD}`) },
  { name: "a record field cast out of its tag", re: new RegExp(String.raw`${FIELD}\)?\s+as\s+(?:unknown|string)\b`) },
  { name: "a record field against a literal list", re: new RegExp(String.raw`\[\s*${LITERAL}[^\]]*\]\s*\)?\s*\.(?:includes|has|indexOf)\([^)]*${FIELD}`) },
  { name: "a regex test on a record field", re: new RegExp(String.raw`\/[gimsuy]*\.test\([^)]*${FIELD}`) },
  { name: "a tag's text compared to a literal", re: new RegExp(String.raw`${TEXT}\s*[!=]==?\s*${LITERAL}|${LITERAL}\s*[!=]==?\s*${TEXT}`) },
  { name: "a string method on a tag's text", re: new RegExp(String.raw`${TEXT}\.(?:startsWith|endsWith|includes|match|split|replace)\(`) },
  { name: "a regex test on a tag's text", re: new RegExp(String.raw`\/[gimsuy]*\.test\(\s*${TEXT}`) },
  { name: "a record ts parsed outside ingest", re: /Date\.parse\([^)]*\bts\)/ },
  { name: "record ts strings ordered or sliced", re: /\.ts\s*[<>]=?\s*[\w.]*\.ts\b|\.ts\)?\.slice\(/ },
];

/** A `switch` over a record field, and the literal `case`s inside it. */
const SWITCH = new RegExp(String.raw`\bswitch\s*\([^)]*(?:${FIELD}|${TEXT})`);
const LITERAL_CASE = new RegExp(String.raw`\bcase\s+${LITERAL}`);

/** A local bound to a record field (`const a = r.action || ""`), whose later
 *  comparisons are the same thing as comparing the field. */
const ALIAS = new RegExp(String.raw`\b(?:const|let)\s+(\w+)\s*=\s*[\w.?]*${FIELD}`);

function violations(text: string): { line: number; rule: string; text: string }[] {
  const lines = text.split("\n");
  const aliases = lines.flatMap((l) => (isComment(l) ? [] : [ALIAS.exec(l)?.[1]].filter((a): a is string => !!a)));
  const rules = [
    ...RULES,
    ...aliases.map((a) => ({ name: `a field alias (${a}) compared to a literal`, re: new RegExp(String.raw`\b${a}\s*[!=]==?\s*${LITERAL}`) })),
  ];
  const out: { line: number; rule: string; text: string }[] = [];
  let inFieldSwitch = 0;
  lines.forEach((line, i) => {
    if (isComment(line)) return;
    for (const { name, re } of rules) if (re.test(line)) out.push({ line: i + 1, rule: name, text: line.trim() });
    if (SWITCH.test(line)) inFieldSwitch = 40;
    else if (inFieldSwitch > 0) {
      inFieldSwitch--;
      if (LITERAL_CASE.test(line)) out.push({ line: i + 1, rule: "a literal case in a switch over a record field", text: line.trim() });
    }
  });
  return out;
}

describe("the ingest boundary", () => {
  it("every rule catches the shape it names", () => {
    const planted = [
      `if (r.action === "dispatch.start") {}`,
      `if ("telemetry" === rec.category) {}`,
      `const hit = action !== "note";`,
      `r.action?.startsWith("mission.run")`,
      `const t = Date.parse(r.ts);`,
      `if (r.level == 'warn') {}`,
      `const a = r.action || "";`,
      `if (a === "dispatch.turn") {}`,
      `if (Object.is(r.action, "note")) {}`,
      `const s = r.stage as unknown as string;`,
      `if (["a", "b"].includes(r.action)) {}`,
      `if (/^step\\./.test(r.action)) {}`,
      `if (tagText(r.action) === "dispatch.start") {}`,
      `if (tagText(r.category).endsWith("metry")) {}`,
      `if (/x/.test(tagText(r.tier))) {}`,
      `switch (r.action) {`,
      `  case ACTION.DispatchStart: break;`,
      `  case "dispatch.turn": break;`,
      `}`,
      `rows.sort((a, b) => (a.ts < b.ts ? -1 : 1));`,
      `const day = String(records[0].ts).slice(0, 10);`,
    ].join("\n");
    expect([...new Set(violations(planted).map((v) => v.line))]).toEqual([1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 18, 20, 21]);
    expect(violations(`// r.action === "x" in prose\nif (r.action === ACTION.Note) {}`)).toEqual([]);
  });

  it("no viewer source outside lib/ingest.ts matches record strings", () => {
    const found = sourceFiles(SRC_DIR).flatMap((f) =>
      violations(readFileSync(f, "utf8")).map((v) => `${f.slice(SRC_DIR.length + 1)}:${v.line} ${v.rule}: ${v.text}`),
    );
    expect(found).toEqual([]);
  });
});
