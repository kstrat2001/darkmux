import { clamp } from "./shared";
export function a(x: number) {
  return clamp(x, 0, 1, false);
}
