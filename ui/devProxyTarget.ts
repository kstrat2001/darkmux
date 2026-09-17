// (#2782) Where `bun run dev`'s API proxy points — see `vite.config.ts`.
//
// Split out of that file for ONE reason: to be testable. The target used to
// be the literal `http://127.0.0.1:8765` inline in the config, and #2765 —
// which made the daemon's address configurable and routed every Rust client
// through `config_access::serve_port`/`serve_bind` — could not reach it,
// because this is a TypeScript build-tool config and cannot call the Rust
// resolver. On a machine that set `serve.port`, the dev server proxied every
// API call into a dead port: the viewer loaded and rendered nothing, which
// presents as "the dev environment is broken" rather than "the proxy points
// at the wrong place". Same silent-and-healthy shape #2765 exists to remove,
// one layer out from anything Rust can fix.
//
// **The resolution order here is deliberately NARROWER than the Rust side's,
// and that is stated rather than implied.** This honours the ENV TIER ONLY:
//
//     env(DARKMUX_SERVE_BIND / DARKMUX_SERVE_PORT) > built-in default
//
// The Rust resolver's middle tier — `config.json` — is SKIPPED. Reading it
// here would mean teaching a build-tool config where `DARKMUX_HOME` resolves
// and how that file is shaped, i.e. a second implementation of the
// precedence rule #2765 deliberately put in exactly one place. A second,
// quietly-different resolution order is a worse trap than a narrower one
// that says so. So when the port lives in `config.json` (the documented
// mechanism), export it for the dev server too:
//
//     DARKMUX_SERVE_PORT=$(darkmux config get serve.port) bun run dev
//
// `darkmux doctor`'s `serve address` row prints what the daemon actually
// resolved, and is the answer whenever the two disagree.
//
// Lives under `src/`'s sibling rather than inside it because it is build
// tooling, never shipped: nothing reachable from `main.tsx` imports it, so
// it cannot reach the singlefile bundle. `vitest.config.ts`'s `include`
// carries a root-level `*.test.ts` entry so its test still runs.

/// The built-in default, matching `SERVE_BIND_DEFAULT` / `SERVE_PORT_DEFAULT`
/// on the Rust side.
export const DEFAULT_BIND = "127.0.0.1";
export const DEFAULT_PORT = "8765";

/**
 * The proxy target URL for the dev server, given an environment.
 *
 * Takes `env` explicitly rather than reading `process.env` so the table of
 * cases is assertable without mutating the test process's own environment.
 */
export function devProxyTarget(env: Record<string, string | undefined>): string {
  const bind = env.DARKMUX_SERVE_BIND?.trim() || DEFAULT_BIND;
  const port = env.DARKMUX_SERVE_PORT?.trim() || DEFAULT_PORT;

  // A wildcard is a bind directive, never a destination — collapse it to
  // loopback, which the daemon is by definition also listening on. Same rule
  // as `config_access::format_client_addr`, and the same reason: connecting
  // to `0.0.0.0` is unspecified-to-hostile across platforms.
  const isWildcard = bind === "0.0.0.0" || bind === "::";
  const host = isWildcard ? DEFAULT_BIND : bind;

  // Bracket a bare IPv6 literal so the URL parses (`::1` -> `[::1]`).
  // Anything already bracketed, an IPv4 literal, or a hostname is used
  // as-is. A colon is the tell: no IPv4 literal or hostname contains one
  // here, because a port is never part of `DARKMUX_SERVE_BIND`.
  const hostPart = host.includes(":") && !host.startsWith("[") ? `[${host}]` : host;

  return `http://${hostPart}:${port}`;
}
