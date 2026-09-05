import { clamp } from "./shared";
export function c(x: number) {
  return clamp(x, -1, 1, false);
}
