import { clamp } from "./shared";
export function b(x: number) {
  return clamp(x, 0, 100, false);
}
