// (#2915) ONE definition of the utility jobs in the viewer.
//
// A utility job kind is the Rust enum `darkmux_crew::usage::UtilityJobKind`,
// generated into `types/generated/UtilityJobKind.ts`, and spelled in this app
// only by `lib/utilityJobs.ts` (`UTILITY_JOB`, checked against the generated
// union by `satisfies`). Anything else keys a job through that map, so a
// renamed or added variant is a type error or a generic indicator, never a
// string that silently stops matching. The utility record actions
// (`utility.start`, `utility.error`, and the live channel's `utility.end`)
// are spelled once too, in `lib/ingest.ts`'s `ACTION`, the viewer's one
// boundary for record strings.
//
// This scans the app's own non-test source for a quoted literal of any job
// kind or action. `"compaction"` is also a word in three OTHER vocabularies
// (the event log's category, the `telemetry.compaction` record's `source`,
// and `CallKind`); those existing uses are pinned below by file and count, so
// a new one anywhere fails here and has to say which vocabulary it is.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { describe, expect, test } from "vitest";
import { UTILITY_JOB } from "./utilityJobs";
import { ACTION, tagText } from "./ingest";

const SRC_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const JOB_DEFINITION = path.join("lib", "utilityJobs.ts");
const ACTION_DEFINITION = path.join("lib", "ingest.ts");

/** Each spelled-once word, with the one file allowed to spell it. */
const WORDS: [string, string][] = [
  ...Object.values(UTILITY_JOB).map((w): [string, string] => [w, JOB_DEFINITION]),
  ...[ACTION.UtilityStart, ACTION.UtilityError, ACTION.UtilityEnd].map((w): [string, string] => [tagText(w), ACTION_DEFINITION]),
];

function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      if (entry === "types" || entry === "node_modules") continue;
      out.push(...sourceFiles(full));
    } else if (/\.(ts|tsx)$/.test(entry) && !/\.test\.(ts|tsx)$/.test(entry)) {
      out.push(full);
    }
  }
  return out;
}

/** Quoted occurrences of `word` on code lines (comment lines are prose). */
function literalCount(text: string, word: string): number {
  const re = new RegExp(`["'\`]${word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}["'\`]`, "g");
  let n = 0;
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (t.startsWith("//") || t.startsWith("/*") || t.startsWith("*")) continue;
    n += (line.match(re) ?? []).length;
  }
  return n;
}

/** Existing uses of the word `compaction` in OTHER vocabularies. */
const OTHER_VOCABULARY: Record<string, Record<string, number>> = {
  [UTILITY_JOB.compaction]: {
    // the event log's `compaction` category and its activity label
    [path.join("lib", "eventFilters.ts")]: 3,
    // `telemetry.compaction`'s `source`, spelled once, in `SOURCE`
    [path.join("lib", "ingest.ts")]: 1,
    // `CALL_KIND.compaction` (the generated `CallKind`)
    [path.join("lib", "usageRecords.ts")]: 1,
  },
};

describe("(#2915) utility job kinds are spelled once", () => {
  const files = sourceFiles(SRC_DIR).map((f) => ({ rel: f.slice(SRC_DIR.length + 1), text: readFileSync(f, "utf8") }));

  test("the scan sees the app's source, and each definition itself", () => {
    expect(files.length).toBeGreaterThan(50);
    for (const [word, definition] of WORDS) {
      const def = files.find((f) => f.rel === definition);
      expect(def, `${definition} must be scanned`).toBeTruthy();
      expect(literalCount(def!.text, word), `${word} is defined in ${definition}`).toBeGreaterThan(0);
    }
  });

  for (const [word, definition] of WORDS) {
    test(`no literal "${word}" outside ${definition}, beyond other vocabularies' pinned uses`, () => {
      const allowed = OTHER_VOCABULARY[word] ?? {};
      const found: string[] = [];
      for (const f of files) {
        if (f.rel === definition) continue;
        const n = literalCount(f.text, word);
        if (n !== (allowed[f.rel] ?? 0)) found.push(`${f.rel}: ${n} (pinned ${allowed[f.rel] ?? 0})`);
      }
      expect(found, `key it through the constant in ${definition}`).toEqual([]);
    });
  }

  test("the counter sees a literal (red-proof on a fixture)", () => {
    expect(literalCount(`const x = "radio_routing";\n// "radio_routing" in prose\n`, UTILITY_JOB.radio_routing)).toBe(1);
  });
});
