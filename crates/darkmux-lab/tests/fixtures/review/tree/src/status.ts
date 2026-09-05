// (#2310 P4c-2b fixture) union-vs-enum: a new string-literal union that
// overlaps the existing `LogLevel` enum in types.ts.
export type Level = "debug" | "info" | "warn" | "error";

export function describe(level: Level): string {
  return `level=${level}`;
}
