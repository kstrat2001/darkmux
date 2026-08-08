/**
 * Console-lens-owned panel metadata — a port of `viewer.html`'s `PANELS`
 * array (tab labels) and `MANUAL_PANELS` set. The ROUTING allowlist itself
 * (`PANEL_IDS`, the closed id set `parseRoute` validates a deep-link
 * `panel=` param against) lives in `lib/route.ts`, next to `RUNS_KINDS` —
 * that's the hash-grammar's business. This module is what the console lens
 * needs to actually RENDER the picker and decide manual-vs-auto behavior.
 * Every entry's `id` is typed as the `PanelId` union from `lib/route.ts`
 * (not re-declared here), so a typo or a dropped/renamed id fails to
 * typecheck against the routing allowlist rather than silently drifting —
 * the drift guard TypeScript already gives for free.
 */
import type { PanelId } from "../../lib/route";

export interface PanelDef {
  id: PanelId;
  label: string;
}

/** viewer.html: `const PANELS = [...]`. Order is the tab order. */
export const PANELS: PanelDef[] = [
  { id: "mission-status", label: "mission status" },
  { id: "mission-status-all", label: "all missions" },
  { id: "machine-status", label: "machine" },
  { id: "flow-status", label: "flow" },
  { id: "role-list", label: "roles" },
  { id: "config-list", label: "config" },
  { id: "lab-fixture-list", label: "fixtures" },
  { id: "doctor", label: "doctor" },
];

export const DEFAULT_PANEL_ID: PanelId = "mission-status";

/** viewer.html: `const MANUAL_PANELS = new Set(["doctor"])`. Panels the
 * daemon marks `auto_refresh: false` — they PROBE the machine, so nothing
 * but an explicit user action may run them (#1286 — "the observer must not
 * join the observed"). Selecting the tab must NEVER auto-fetch; only the
 * panel's own "run"/"re-run" button may. */
export const MANUAL_PANELS: ReadonlySet<PanelId> = new Set(["doctor"]);

export function isManualPanel(id: PanelId): boolean {
  return MANUAL_PANELS.has(id);
}

/** viewer.html: `function panelCols()`. Asks the daemon for a render width
 * that matches the space the panel actually has, so `mission status` sheds
 * its optional columns on a phone instead of overflowing — see that
 * function's own extensive comment in viewer.html for the 7.2px-advance-
 * width/24px-padding derivation and the #1613 floor story. The clamp
 * (36..=200) MUST agree with the daemon's own `clamp_cols`
 * (`crates/darkmux-serve/src/panel.rs`), or the negotiation lies.
 *
 * NOT load-bearing for parity: the parity harness's mocked `/panel/:id`
 * routes match on pathname only (see `tests/parity/lib/mock-routes.js`) and
 * ignore the `cols` query param entirely, always returning the fixture's
 * recorded content — so whatever this computes never changes a golden. It's
 * ported anyway because a real daemon DOES read `cols` (see panel.rs's
 * `clamp_cols`), and asking for the wrong width is a real behavior, not a
 * test-only one. */
export function panelCols(panelOutEl: Element | null): number {
  const px = panelOutEl ? panelOutEl.clientWidth : window.innerWidth;
  return Math.max(36, Math.min(200, Math.floor((px - 24) / 7.2)));
}
