// (#2915) ONE definition of the utility jobs in the viewer.
//
// A utility job kind is the Rust enum `darkmux_crew::usage::UtilityJobKind`,
// generated into `types/generated/UtilityJobKind.ts`, and spelled in this app
// only by `lib/utilityJobs.ts` (`UTILITY_JOB`, checked against the generated
// union by `satisfies`). Anything else keys a job through that map, so a
// renamed or added variant is a type error or a generic indicator, never a
// string that silently stops matching. Same for the two record actions the
// module owns (`utility.start`, `utility.error`).
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
import { LIVE_UTILITY_END_ACTION, UTILITY_ERROR_ACTION, UTILITY_JOB, UTILITY_START_ACTION } from "./utilityJobs";

const SRC_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const DEFINITION = path.join("lib", "utilityJobs.ts");

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
    // the event log's `compaction` category, and `source === "compaction"`
    [path.join("lib", "eventFilters.ts")]: 4,
    // `telemetry.compaction`'s `source`
    [path.join("lib", "flow.ts")]: 1,
    [path.join("lenses", "session", "sessionRun.ts")]: 1,
    // `CALL_KIND.compaction` (the generated `CallKind`)
    [path.join("lib", "usageRecords.ts")]: 1,
  },
};

describe("(#2915) utility job kinds are spelled once", () => {
  const files = sourceFiles(SRC_DIR).map((f) => ({ rel: f.slice(SRC_DIR.length + 1), text: readFileSync(f, "utf8") }));

  test("the scan sees the app's source, and the definition itself", () => {
    expect(files.length).toBeGreaterThan(50);
    const def = files.find((f) => f.rel === DEFINITION);
    expect(def, "lib/utilityJobs.ts must be scanned").toBeTruthy();
    for (const word of [...Object.values(UTILITY_JOB), UTILITY_START_ACTION, UTILITY_ERROR_ACTION, LIVE_UTILITY_END_ACTION]) {
      expect(literalCount(def!.text, word), `${word} is defined in ${DEFINITION}`).toBeGreaterThan(0);
    }
  });

  for (const word of [...Object.values(UTILITY_JOB), UTILITY_START_ACTION, UTILITY_ERROR_ACTION, LIVE_UTILITY_END_ACTION]) {
    test(`no literal "${word}" outside ${DEFINITION}, beyond other vocabularies' pinned uses`, () => {
      const allowed = OTHER_VOCABULARY[word] ?? {};
      const found: string[] = [];
      for (const f of files) {
        if (f.rel === DEFINITION) continue;
        const n = literalCount(f.text, word);
        if (n !== (allowed[f.rel] ?? 0)) found.push(`${f.rel}: ${n} (pinned ${allowed[f.rel] ?? 0})`);
      }
      expect(found, `key the job through UTILITY_JOB / the action constants in ${DEFINITION}`).toEqual([]);
    });
  }

  test("the counter sees a literal (red-proof on a fixture)", () => {
    expect(literalCount(`const x = "radio_routing";\n// "radio_routing" in prose\n`, UTILITY_JOB.radio_routing)).toBe(1);
  });
});
