// (#1868 QA finding) A real, project-wide replacement for the coverage the
// retired standalone mission-graph.html page's own inline scan provided
// (`crates/darkmux-serve/src/lib_tests.rs`'s `mission_graph_html_serves_the_page`,
// deleted along with that page). That coverage could NOT simply move to a
// scan of the built `next.html`: `next.html` is a SINGLE bundled document
// (react + react-dom + reactflow + this app's own code, inlined by
// `vite-plugin-singlefile`), and React's own runtime legitimately contains
// the literal string `dangerouslySetInnerHTML` dozens of times — it has to
// reference the prop name to validate/guard against misuse. A whole-bundle
// scan for that string always fails, on React's OWN code, regardless of
// whether this app's code ever uses it. See
// `viewer_has_no_inline_event_handlers`'s own doc comment in `lib_tests.rs`
// for the same finding stated from the Rust side.
//
// The fix: scan SOURCE, before bundling, where this app's own files are
// cleanly distinguishable from the framework's. React's `createElement`
// escapes text content by construction; `dangerouslySetInnerHTML` is the
// one prop that opts back OUT of that guarantee, so this app's own source
// must never set it.
//
// Comment lines are excluded on purpose: `ansi.tsx`'s own module doc
// mentions the identifier in prose ("there is no `dangerouslySetInnerHTML`
// anywhere in this module") to STATE the property this test now verifies —
// a naive whole-file `includes()` would flag that true statement as a
// violation. A line is treated as a comment when its trimmed text starts
// with `//`, `/*`, or `*` (this codebase's two comment shapes throughout
// `ui/src`), which is precise enough here: real JSX/object-literal usage of
// `dangerouslySetInnerHTML` is never comment-only in a codebase without a
// linter enforcing prettier/eslint comment styles that would break this
// heuristic — verified against every current match (`ansi.tsx` alone) below.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { describe, expect, test } from "vitest";

const SRC_DIR = path.dirname(fileURLToPath(import.meta.url));

function collectSourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    const stat = statSync(full);
    if (stat.isDirectory()) {
      if (entry === "types" || entry === "node_modules") continue; // generated/vendored, not this app's own hand-written code
      out.push(...collectSourceFiles(full));
      continue;
    }
    if (/\.(ts|tsx)$/.test(entry) && !/\.test\.(ts|tsx)$/.test(entry)) {
      out.push(full);
    }
  }
  return out;
}

function isCommentLine(line: string): boolean {
  const t = line.trim();
  return t.startsWith("//") || t.startsWith("/*") || t.startsWith("*");
}

describe("no-danger", () => {
  test("no ui/src source file sets dangerouslySetInnerHTML outside a comment", () => {
    const files = collectSourceFiles(SRC_DIR);
    expect(files.length).toBeGreaterThan(50); // sanity: the walk actually found the tree, not an empty/wrong dir

    const violations: string[] = [];
    for (const file of files) {
      const lines = readFileSync(file, "utf8").split("\n");
      lines.forEach((line, i) => {
        if (line.includes("dangerouslySetInnerHTML") && !isCommentLine(line)) {
          violations.push(`${file}:${i + 1}: ${line.trim()}`);
        }
      });
    }

    expect(
      violations,
      `dangerouslySetInnerHTML found outside a comment — React's escape-by-construction guarantee is bypassed:\n${violations.join("\n")}`
    ).toEqual([]);
  });
});
