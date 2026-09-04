// (#2310 P4c-2b fixture) the EXISTING enum `status.ts`'s new union
// overlaps — untouched by the diff, so it never becomes a planned site
// itself, only the thing a real `search` step would find.
export enum LogLevel {
  Debug,
  Info,
  Warn,
  Error,
}
