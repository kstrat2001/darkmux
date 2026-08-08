/**
 * Hash-route parser — a straight TypeScript port of `viewer.html`'s own
 * `catalogQuery`/`labQuery`/`consoleQuery`/`machineQuery` functions (see that
 * file's own comments around line 3210 onward). The grammar and its
 * precedence order are PRESERVED VERBATIM — the CLI prints these deep links
 * (`panel_deep_link`, `mission status`'s reconcile commands, etc.) and every
 * bookmark/phone-shortcut minted against the legacy viewer must keep
 * resolving once `/next` takes over.
 *
 * Precedence when the hash carries multiple intents (same order `boot()`
 * checks them in): `lens=runs`/`lens=lab` > `lens=machine` > `lens=console` >
 * `mission=`/`session=` > default fleet. A `lens` value this build doesn't
 * recognize (not `runs`/`lab`/`machine`/`console`, and no bare
 * `mission=`/`session=` present either) falls through to `unknown` — a
 * visible "lens not ported yet" placeholder naming the raw hash, never a
 * blank page (the overnight runbook's render-sanity contract).
 */

export const RUNS_KINDS = ["all", "mission", "dispatch", "lab"] as const;
export type RunsKind = (typeof RUNS_KINDS)[number];

export type Route =
  | { kind: "fleet" }
  | { kind: "runs"; runsKind: RunsKind; run: string | null }
  | { kind: "machine" }
  | { kind: "console"; panelId: string }
  | { kind: "session"; sessionId: string }
  /** `#mission=<id>` is a FULL NAVIGATION in the legacy viewer
   * (`location.href = "/mission/<id>/graph"`) — a separate asset with its own
   * vendored React Flow bundle. Out of scope for this packet (see
   * `tests/parity/README.md`'s lens inventory); the router recognizes the
   * hash shape and reports it distinctly from `unknown` so a lens packet can
   * wire the redirect without re-deriving the grammar. */
  | { kind: "mission-redirect"; missionId: string }
  | { kind: "unknown"; hash: string };

function hashParams(): URLSearchParams {
  return new URLSearchParams((location.hash || "").replace(/^#/, ""));
}

/** Parse the CURRENT `location.hash` into a [[Route]]. Pure function of
 * `location.hash` (and, matching the legacy grammar, `location.search` as a
 * fallback source for the same param names) — call it fresh on every
 * `hashchange`, never cache across navigations. */
export function parseRoute(): Route {
  const search = new URLSearchParams(location.search);
  const hash = hashParams();
  const get = (name: string): string =>
    (search.get(name) || hash.get(name) || "").trim();

  const lens = get("lens");

  if (lens === "runs" || lens === "lab") {
    const rawKind = (get("kind") || (lens === "lab" ? "lab" : "all")).toLowerCase();
    const runsKind = (RUNS_KINDS as readonly string[]).includes(rawKind)
      ? (rawKind as RunsKind)
      : "all";
    const run = search.has("run") ? search.get("run") : hash.has("run") ? hash.get("run") : null;
    return { kind: "runs", runsKind, run: run === null ? null : run.trim() };
  }

  if (lens === "machine") {
    return { kind: "machine" };
  }

  if (lens === "console") {
    return { kind: "console", panelId: get("panel") };
  }

  const mission = get("mission");
  if (mission) {
    return { kind: "mission-redirect", missionId: mission };
  }

  const session = get("session");
  if (session) {
    return { kind: "session", sessionId: session };
  }

  const raw = (location.hash || "").replace(/^#/, "");
  if (lens) {
    // A `lens=` value this build doesn't recognize (typo, a future lens the
    // legacy viewer grew after this port, etc.) — name it rather than
    // silently falling back to fleet, which would hide a broken bookmark.
    return { kind: "unknown", hash: raw };
  }
  if (!raw) {
    return { kind: "fleet" };
  }
  // A bare hash that parses as neither a recognized `lens=` nor a bare-date
  // playback pin (out of scope for `/next` — playback lives at `/play/:date`,
  // not a hash on this route) is unrecognized too.
  if (/^\d{4}-\d{2}-\d{2}$/.test(raw)) {
    return { kind: "unknown", hash: raw };
  }
  return { kind: "fleet" };
}
