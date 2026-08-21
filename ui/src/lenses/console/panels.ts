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
 *
 * (#1911) This module ALSO carries `PANEL_OPTS` — the client twin of
 * `crates/darkmux-serve/src/panel.rs`'s declared option space
 * (`PanelOpt`/`PanelOptValue`/`panel_spec`'s `opts`). The server owns the
 * WIRE contract (an unknown name/value is a 400); this table exists so the
 * client can render an opts bar, compose the pending-command preview, and
 * build a cache/hash key WITHOUT a round trip — the two tables are pinned
 * against each other by `panels.test.ts`'s own drift guard, the same way
 * `PANELS` is pinned against `PANEL_IDS` above. This table never lets a
 * client-supplied VALUE reach argv either: `PanelOptValue.argv` here is
 * always one of the server's own literal fragments, copied verbatim — the
 * client only ever selects among them by `(name, value)`, same trust
 * mechanism as the server's own struct-shape legality proof (see
 * `panel.rs`'s module doc, "Opts: a declared option space, not an open
 * one").
 */
import type { PanelId } from "../../lib/route";

export interface PanelDef {
  id: PanelId;
  label: string;
}

/** One legal value of a [[PanelOpt]] and the literal argv fragment it
 * contributes when selected — the client twin of `panel.rs`'s
 * `PanelOptValue`. `argv` is always either empty (the default, always
 * `values[0]`) or flag-shaped, mirroring the server's own
 * `every_default_value_has_empty_argv`/`every_opt_value_argv_is_flag_shaped`
 * guards (pinned here by `panels.test.ts`, not re-asserted structurally —
 * there is no consumer here that would silently misbehave if a future edit
 * broke the convention the way the server's compose/cache-key logic
 * would). */
export interface PanelOptValue {
  readonly value: string;
  readonly argv: readonly string[];
}

/** One declared, closed option group for a panel — e.g. `--kind` on
 * `run-list` (#1911). The client twin of `panel.rs`'s `PanelOpt`. `name` is
 * the query-param key (`opt.<name>`) AND, by convention, matches the flag
 * it drives — the opts bar's group label is literally `--${name}`. */
export interface PanelOpt {
  readonly name: string;
  /** Legal values. `values[0]` IS the default and carries empty argv, so
   * "nothing selected" and "explicitly picked the default" resolve
   * identically — see [[resolveOpts]]. */
  readonly values: readonly PanelOptValue[];
}

/** The `--all` toggle shared by `mission-status` and `run-list` — the
 * client twin of `panel.rs`'s `ALL_OPT` const, same name and shape. */
const ALL_OPT: PanelOpt = {
  name: "all",
  values: [
    { value: "recent", argv: [] },
    { value: "all", argv: ["--all"] },
  ],
};

/** `run list`'s `--kind` filter — the client twin of `panel.rs`'s
 * `RUN_LIST_KIND_OPT`. Values are deliberately the same strings as
 * `RUNS_KINDS` (`lib/route.ts`) and the Rust-side `RunKindArg` — that pair
 * is pinned by `run_list::run_kind_arg_vocabulary_matches_the_ui_runs_kinds_twin`
 * (`src/run_list.rs`); this file's own drift guard (`panels.test.ts`) pins
 * THIS table against `panel.rs`'s `RUN_LIST_KIND_OPT` directly, closing the
 * third leg of the same triangle. */
const RUN_LIST_KIND_OPT: PanelOpt = {
  name: "kind",
  values: [
    { value: "all", argv: [] },
    { value: "mission", argv: ["--kind", "mission"] },
    { value: "dispatch", argv: ["--kind", "dispatch"] },
    { value: "lab", argv: ["--kind", "lab"] },
  ],
};

/** One panel's base argv plus its declared option space — the client twin
 * of `panel.rs`'s `PanelSpec` (minus `auto_refresh`/`cache_ttl`, which
 * `MANUAL_PANELS` below already covers and the client never needs to
 * re-declare a TTL for). */
export interface PanelOptsEntry {
  readonly argv: readonly string[];
  readonly opts: readonly PanelOpt[];
}

/** The client twin of `panel.rs`'s `panel_spec` match — one entry per BASE
 * verb (#1911: this table counts base verbs, not variants; a verb's
 * declared `opts` is where its variant space lives). `mission-status-all`
 * is NOT an entry here — it resolves through `PANEL_ALIASES`
 * (`lib/route.ts`) to `mission-status` with `all` forced, mirroring the
 * server's own `resolve_alias` split (kept OUT of `panel_spec`'s match for
 * the identical reason: its old argv would fail a "flags are opts, ids are
 * verbs" guard if it were entered directly). */
export const PANEL_OPTS: Record<PanelId, PanelOptsEntry> = {
  "mission-status": { argv: ["mission", "status"], opts: [ALL_OPT] },
  "role-list": { argv: ["role", "list"], opts: [] },
  "machine-status": { argv: ["machine", "status"], opts: [] },
  "config-list": { argv: ["config", "list"], opts: [] },
  "flow-status": { argv: ["flow", "status"], opts: [] },
  "lab-fixture-list": { argv: ["lab", "fixture", "list"], opts: [] },
  // (#1911) The CLI twin of the RUNS lens's union — see `src/run_list.rs`'s
  // own module doc.
  "run-list": { argv: ["run", "list"], opts: [RUN_LIST_KIND_OPT, ALL_OPT] },
  doctor: { argv: ["doctor"], opts: [] },
};

export function panelArgv(id: PanelId): readonly string[] {
  return PANEL_OPTS[id].argv;
}

export function panelOptGroups(id: PanelId): readonly PanelOpt[] {
  return PANEL_OPTS[id].opts;
}

/** One resolved `(name, value)` opt selection, in the panel's own
 * DECLARATION order — never caller-supplied order — with its argv fragment
 * and whether it is the opt's default. The client twin of `panel.rs`'s
 * `ResolvedOpt`/`resolve_opts`, with ONE deliberate difference: the server
 * 400s on an unknown name/value (the wire boundary), while this function
 * silently DEFAULTS an unknown name/value — "invalid or irrelevant params
 * drop silently to default, matching the existing posture where an
 * unrecognized `panel` parses the same as absent" (#1911's own hash-grammar
 * spec). The client never has an "reject the request" move to make; a
 * degraded-to-default variant is the honest client-side equivalent. */
export interface ResolvedOpt {
  name: string;
  value: string;
  argv: readonly string[];
  isDefault: boolean;
}

export function resolveOpts(id: PanelId, requested?: Readonly<Record<string, string>>): ResolvedOpt[] {
  const opts = panelOptGroups(id);
  return opts.map((opt) => {
    const def = opt.values[0];
    const raw = requested?.[opt.name];
    const picked = opt.values.find((v) => v.value === raw) ?? def;
    return { name: opt.name, value: picked.value, argv: picked.argv, isDefault: picked.value === def.value };
  });
}

/** Compose the final argv: the panel's own base argv followed by each
 * resolved opt's fragment, in DECLARATION order — the client twin of
 * `panel.rs`'s `compose_argv`. Used for the pending-command preview
 * (`NotLoadedChrome`) rather than `id.replace(/-/g," ")`'s old guess. */
export function composeArgv(id: PanelId, requested?: Readonly<Record<string, string>>): string[] {
  const out = [...panelArgv(id)];
  for (const r of resolveOpts(id, requested)) out.push(...r.argv);
  return out;
}

/** Sorted, NON-DEFAULT `[name, value]` pairs — the client twin of
 * `panel.rs`'s `variant_key`'s own pair-building half. This is the ONE
 * canonicalization both `queryKeys.panel` and `hashSync.canonicalHash` call
 * (#1911's own instruction: "one implementation, so the two tiers cannot
 * disagree"), so a cache-key/hash-write drift between them is structurally
 * impossible rather than merely undesired. */
export function canonicalOptPairs(id: PanelId, requested?: Readonly<Record<string, string>>): [string, string][] {
  const pairs = resolveOpts(id, requested)
    .filter((r) => !r.isDefault)
    .map((r): [string, string] => [r.name, r.value]);
  pairs.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return pairs;
}

/** Filter `raw` down to only the entries `id`'s own opts table recognizes —
 * a known opt NAME with a value on that opt's own closed list. Everything
 * else (an unknown name, or a known name with a value outside the closed
 * set) is dropped, never passed through — the client-side twin of the
 * server's 400, except a client can't refuse a request that already
 * happened; it degrades the unrecognized piece to "as if absent" instead
 * (#1911's own hash-grammar spec: "invalid or irrelevant params drop
 * silently to default"). Used by `lib/route.ts::parseRoute` to build
 * `Route`'s `console.opts` field from the hash's raw `opt.*` params. */
export function sanitizeOptParams(id: PanelId, raw: Readonly<Record<string, string>>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const opt of panelOptGroups(id)) {
    const v = raw[opt.name];
    if (v !== undefined && opt.values.some((pv) => pv.value === v)) out[opt.name] = v;
  }
  return out;
}

/** The canonical cache-key string — the client twin of `panel.rs`'s
 * `variant_key`: base id alone when every opt is at its default (so "no
 * selection" and "explicitly picked the default" are ONE cache entry, same
 * server-side guarantee), else `id?name=value&name=value` sorted by name. */
export function variantKey(id: PanelId, requested?: Readonly<Record<string, string>>): string {
  const pairs = canonicalOptPairs(id, requested);
  if (pairs.length === 0) return id;
  return `${id}?${pairs.map(([n, v]) => `${n}=${v}`).join("&")}`;
}

/** viewer.html: `const PANELS = [...]`. Order is the tab order.
 *
 * (#1904) These are exactly the CLI-backed panels — the drift guard in
 * `panels.test.ts` pins `PANELS.map(p => p.id)` to `PANEL_IDS` from
 * `lib/route.ts`, which is itself the twin of the Rust-side allowlist
 * (`crates/darkmux-serve/src/panel.rs::PANEL_IDS`, hard-capped at 8 by its
 * own doctrine assertion). The console lens's DEFAULT landing view (the
 * "activity" tab, and its "all activity" escape hatch) is NOT one of these
 * — it is a client-rendered, `/runs`-fed view with no CLI command or argv
 * behind it, so it deliberately stays OUT of this list and out of
 * `PanelId`/`PANEL_IDS` entirely. `ConsolePanel.tsx` owns that pair as its
 * own local `ConsoleSelection` union, widening `PanelId` rather than
 * pretending a client-only view is a CLI panel.
 *
 * (#1911) `all missions` is gone (folded into `mission-status`'s own `all`
 * opt — see `PANEL_ALIASES` in `lib/route.ts`), and `run list` joins right
 * before `doctor`, the one position change this packet makes to an
 * otherwise-unchanged tab order. Every label is now DERIVED from the same
 * argv `PANEL_OPTS` carries (`panelArgv(id).join(" ")`) rather than a
 * separately hand-picked string, so "labels read as the command they run"
 * (#1911's own instruction) can't drift from the pending-command preview
 * that uses the exact same argv. */
export const PANELS: PanelDef[] = (
  ["mission-status", "machine-status", "flow-status", "role-list", "config-list", "lab-fixture-list", "run-list", "doctor"] as const
).map((id) => ({ id, label: panelArgv(id).join(" ") }));

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
