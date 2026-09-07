import { injectedMeta } from "./injectedMeta";

/**
 * (#1709) `boot()`/`main.tsx` had no guard: any render-time throw yielded a
 * silent black screen with no diagnosis path. This is the shared, pure
 * "what happened" logic behind the boot-failure surface — used by BOTH
 * `BootErrorBoundary` (a real React error boundary, for a throw caught
 * during the tree's render) and `mountApp`'s `window.onerror`/
 * `unhandledrejection` listeners (for everything a React boundary
 * structurally cannot see: a throw before `render()` is ever called, or one
 * from a later tick after the app already mounted once — a stream record
 * handler, a poll). Kept pure and JSX-free so both call sites, and their
 * very different rendering mechanisms (React vs a last-resort raw-DOM
 * fallback — see `mountApp.tsx`'s own doc for why that one deliberately does
 * NOT go through React), stay in sync on the actual content.
 */
export interface BootErrorContent {
  /** One line: what failed, and via which mechanism. */
  title: string;
  /** The error's own message — never invented, never generic. */
  message: string;
  /** The error's own stack, when the runtime gave it one. */
  stack: string | null;
  /**
   * The build the daemon is serving, in the SAME `v<semver> · flow schema
   * <n>` shape the masthead's version chip already uses (`Masthead.tsx`'s
   * `verTitle`) — read from the SAME injected meta (`injectedMeta`,
   * `crates/darkmux-serve/src/lib.rs`'s `inject_mode_meta`), not a second
   * source. `null` wherever nothing injected it: every test harness, and a
   * daemon-less static build.
   */
  buildLine: string | null;
  /**
   * The closing paragraph. Lives HERE rather than being written out at each
   * of the two render sites, because those two sites render by different
   * mechanisms (React JSX vs raw DOM) and a paragraph copy-pasted between
   * them is a drift hazard with nothing to catch it — the sentences said the
   * same thing only for as long as somebody remembered to edit both.
   */
  hint: string;
}

/** Whether a working app is still mounted BEHIND this surface. It changes
 * what is true, so it changes the words: a later-tick throw over a live
 * viewer is not a failure to start, and telling the operator "every lens is
 * unavailable" when every lens is in fact still rendering underneath is a
 * false statement they can see through the overlay. */
export interface BootErrorOptions {
  appIsLive?: boolean;
}

/** Normalizes whatever a `throw` or a promise rejection actually handed us
 * (almost always an `Error`, but JS allows throwing anything) into a
 * message worth showing plus a stack — ONLY when the original value was a
 * real `Error`. A stack synthesized by wrapping a bare string in `new
 * Error()` here would point at this function, not at the real throw site,
 * which is worse than no stack at all: it looks like evidence and isn't. */
function describe(value: unknown): { message: string; stack: string | null } {
  if (value instanceof Error) {
    return { message: String(value.message || value), stack: value.stack ?? null };
  }
  if (typeof value === "string") return { message: value, stack: null };
  try {
    return { message: JSON.stringify(value), stack: null };
  } catch {
    return { message: String(value), stack: null };
  }
}

export function describeBootError(
  error: unknown,
  context: string,
  options: BootErrorOptions = {},
): BootErrorContent {
  const { message, stack } = describe(error);
  const version = injectedMeta("darkmux-version");
  const schema = injectedMeta("darkmux-flow-schema");
  const buildLine = version ? `darkmux ${version}${schema ? ` · flow schema ${schema}` : ""}` : null;
  const appIsLive = options.appIsLive ?? false;
  return {
    title: appIsLive
      ? `darkmux hit an unexpected error — ${context}`
      : `darkmux failed to start — ${context}`,
    message,
    stack,
    buildLine,
    hint: appIsLive
      ? "The viewer is still running behind this notice — dismissing returns you to it with your " +
        "current view intact. Something failed outside the render path, so what you see underneath " +
        "may be stale. The error above is selectable to paste into a report; the full stack (and " +
        "this same message) is also in the browser console."
      : "The darkmux viewer itself failed to start — every lens is unavailable until this is fixed. " +
        "The error above is selectable to paste into a report; the full stack (and this same message) " +
        "is also in the browser console.",
  };
}
