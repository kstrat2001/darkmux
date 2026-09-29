import { describe, expect, it } from "vitest";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

/**
 * The viewer imports only GENERATED types for daemon data: every response type
 * a `fetchJson<T>` names comes from `types/generated/` (generated from the
 * server's own Rust types, `bun run types:regen`), or is a plain composition of
 * generated types over a committed static document. This is the guard that
 * keeps a hand-written copy of a route's shape from coming back.
 */
const SRC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function sourceFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const full = path.join(dir, name);
    if (name === "generated") return [];
    if (statSync(full).isDirectory()) return sourceFiles(full);
    return /\.(ts|tsx)$/.test(name) && !/\.test\.tsx?$/.test(name) ? [full] : [];
  });
}

/** `fetchJson<...>(` generics, with their bracketed argument balanced. */
function fetchGenerics(text: string): string[] {
  const out: string[] = [];
  for (const m of text.matchAll(/fetchJson<([^(]*?)>\(/g)) out.push(m[1]);
  return out;
}

/** `.json() as T` casts: a call that reads a response body without `fetchJson`. */
function jsonCasts(text: string): string[] {
  return [...text.matchAll(/\.json\(\)\)?\s+as\s+([^;\n]+)/g)].map((m) => m[1]);
}

/** The identifiers a generic mentions that are not TypeScript built-ins. */
function namedTypes(generic: string): string[] {
  const builtins = new Set(["unknown", "Record", "string", "T"]);
  return [...generic.matchAll(/[A-Z][A-Za-z0-9]*/g)].map((m) => m[0]).filter((n) => !builtins.has(n));
}

describe("daemon data is typed by generated types only", () => {
  it("no hand-written types module exists", () => {
    expect(() => statSync(path.join(SRC, "types", "handwritten.ts"))).toThrow();
  });

  it("every type a fetchJson<...> or .json() as T names is imported from types/generated", () => {
    const offenders: string[] = [];
    let seen = 0;
    for (const file of sourceFiles(SRC)) {
      const text = readFileSync(file, "utf8");
      for (const generic of [...fetchGenerics(text), ...jsonCasts(text)]) {
        seen += 1;
        for (const name of namedTypes(generic)) {
          if (!new RegExp(`import type \\{[^}]*\\b${name}\\b[^}]*\\} from "[./]*types/generated/`).test(text)) {
            offenders.push(`${file.slice(SRC.length + 1)}: ${generic} names ${name}, which is not a generated type`);
          }
        }
      }
    }
    expect(seen, "the scan must find the fetchJson call sites, or it proves nothing").toBeGreaterThan(20);
    expect(offenders).toEqual([]);
  });
});
