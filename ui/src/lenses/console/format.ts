/** Pure helper functions for the console lens's chrome line — ported from
 * `viewer.html`'s `function panelAgeLabel(body, fetchedAt)`. Split out from
 * `ConsolePanel.tsx` for the same reason `lenses/runs/format.ts` is split
 * from `RunsBoard.tsx`: pure functions are unit-testable without a DOM. */
import type { PanelResponse } from "../../types/handwritten";

export interface PanelAgeLabel {
  hhmmss: string;
  served: string;
  stale: boolean;
  ageSec: number;
}

/** viewer.html: `function panelAgeLabel(body, fetchedAt)`. `fetchedAt` is
 * WHEN THIS CLIENT fetched (`Date.now()` at the time — react-query's
 * `dataUpdatedAt` is the direct analog of legacy's `st.fetchedAt`); the
 * daemon reports whether IT served a cached body via `age_ms`, so both are
 * stated rather than implied (#1286's "cadence is a recorded knob, never
 * adaptive-silent"). */
export function panelAgeLabel(body: PanelResponse, fetchedAt: number): PanelAgeLabel {
  const age = Date.now() - (fetchedAt || Date.now());
  const ttl = body.cache_ttl_ms || 0;
  const captured = new Date(body.captured_ts_ms || Date.now());
  const hhmmss = captured.toTimeString().slice(0, 8);
  const served = body.age_ms ? " · daemon cache " + Math.round(body.age_ms / 1000) + "s" : "";
  const stale = ttl > 0 && age > ttl * 3;
  return { hhmmss, served, stale, ageSec: Math.round(age / 1000) };
}
