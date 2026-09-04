// (#2310 P4c-2b fixture) shared-symbol-callers: a shared, exported
// function whose signature changed — three callers live elsewhere in the
// tree (not part of the diff; what a real `search` step would enumerate).
export function clamp(value: number, min: number, max: number, strict: boolean): number {
  if (value < min) return min;
  if (value > max) return max;
  return value;
}
