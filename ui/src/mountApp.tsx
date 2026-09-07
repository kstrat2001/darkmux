import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { App } from "./App";
import { createQueryClient } from "./lib/queryClient";
import { BootErrorBoundary } from "./components/BootErrorBoundary";
import { describeBootError } from "./lib/bootError";

/**
 * (#1709) The real entry-point logic, extracted from `main.tsx` so it is a
 * plain function this suite (`mountApp.test.tsx`) can call directly instead
 * of relying on a module's import-time side effects, which vitest can't
 * easily trigger or reset between cases. `main.tsx` itself is now just
 * `mountApp();`.
 *
 * Two independent mechanisms cover the boot path, because they cover
 * DIFFERENT failure states and neither can stand in for the other:
 *
 * 1. **`BootErrorBoundary`, wrapped around everything below it.** A real
 *    React error boundary — it catches a synchronous throw during render
 *    ANYWHERE in the tree it wraps: `App` itself, `Masthead`, `NavChrome`,
 *    `MachineDrawer`/`PhoneDrawer`, `EventLogColumn`, or (because it sits
 *    OUTSIDE `QueryClientProvider` here) even a throw from that provider's
 *    own render. `LensErrorBoundary` already covers a throw inside one
 *    lens; this is everything that boundary structurally cannot see.
 *
 * 2. **`window.onerror` / `unhandledrejection`, registered before anything
 *    else in this function runs.** These cover what NO React error boundary
 *    can, by React's own design: a throw between this line and
 *    `createRoot(...).render()` (e.g. `createQueryClient()` itself throwing,
 *    or — handled directly below rather than left to this net — `#root`
 *    missing from `index.html`), an error in an event handler, and a throw
 *    from a LATER tick after the app already mounted once (an unguarded
 *    `async` effect, a stream-record handler, a poll) — exactly the "page
 *    sits there silently frozen" case #1709 called out. A render-phase
 *    throw that `BootErrorBoundary` already caught never reaches these
 *    listeners; React stops it at the boundary and neither an "error" event
 *    nor a rejection is generated for it.
 *
 * **What neither mechanism covers, stated plainly** (measured in a real
 * browser, 2026-09-07): a failure BEFORE this function is ever called. These
 * listeners are the first statements in `mountApp`, but `mountApp` itself is
 * the LAST thing in the module graph — every `import` above (`App` and
 * transitively every lens, reactflow, the stylesheet) is evaluated first. A
 * module-level throw during that import, a bundle with a syntax error, a
 * blocked or failed script load, or a CSP denial all happen with no listener
 * registered yet. The last line of defence for those is static markup inside
 * `#root` in `index.html`, which React replaces on a successful mount and
 * which therefore remains on screen when the bundle never runs. Neither this
 * file nor `index.html` can do better than that alone; that is the honest
 * boundary of this guard.
 *
 * Painting is deliberately NOT routed through React for cases (1)-adjacent
 * uses of `window.onerror`: `paintBootError` below builds plain DOM nodes
 * by hand. A global safety net that itself depends on React still being
 * able to mount is not a safety net — if the reason `window.onerror` fired
 * is that React itself is broken, a `createRoot(...).render()` call from
 * inside the handler could fail the same way. Raw DOM has no such
 * dependency. `BootErrorScreen`'s JSX and this function's DOM-building
 * render the SAME content (both go through `describeBootError`, which owns
 * every sentence so neither renderer can drift from the other) but are
 * kept in sync by hand, not by sharing a renderer.
 *
 * The error is never swallowed on either path: `componentDidCatch`
 * (`BootErrorBoundary`) and `paintBootError` below both `console.error` the
 * real error before painting anything — and `paintBootError` logs BEFORE
 * every one of its early returns, so a suppressed or deduplicated error is
 * still in the console with its original object and stack.
 */
export function mountApp(): void {
  window.addEventListener("error", (event) => {
    // A cross-origin script error carries NO information: the browser sets
    // `error` to null and `message` to the literal string "Script error." —
    // its own privacy behavior for a resource served without CORS headers,
    // which in practice means a browser extension's injected script far more
    // often than anything of ours. Read `event.error` DIRECTLY to detect
    // that; an earlier version coalesced (`event.error ?? event.message`)
    // before testing for null, so the informationless case always arrived as
    // the non-null string "Script error." and the suppression never fired —
    // any extension throwing on the page painted a full-page "darkmux failed
    // to start" over a working viewer. Proven in a real browser before this
    // fix; `mountApp.test.tsx` pins it.
    if (event.error == null) {
      console.error("[darkmux] ignoring an errored script we have no information about", event.message);
      return;
    }
    paintBootError(event.error, "script error");
  });
  window.addEventListener("unhandledrejection", (event: PromiseRejectionEvent) => {
    paintBootError(event.reason, "unhandled promise rejection");
  });

  const rootEl = document.getElementById("root");
  if (!rootEl) {
    // Known, anticipated failure — paint directly rather than throwing and
    // hoping the listener above catches it. (It would: an uncaught throw at
    // module scope becomes exactly the "error" event that listener handles.
    // Calling `paintBootError` here directly is simpler to test and skips
    // one throw/catch round trip for a case we already know about.)
    paintBootError(new Error("darkmux: #root element missing from index.html"), "boot");
    return;
  }

  const queryClient = createQueryClient();

  createRoot(rootEl).render(
    <StrictMode>
      <BootErrorBoundary>
        <QueryClientProvider client={queryClient}>
          <App />
        </QueryClientProvider>
      </BootErrorBoundary>
    </StrictMode>,
  );
}

/**
 * The raw-DOM twin of `BootErrorScreen` — see this file's own doc for why
 * the global-error path can't go through React. Builds every text node via
 * `textContent`, never `innerHTML`, so an error message that happens to
 * contain HTML-looking text (a stack trace with `<anonymous>`, say) can
 * never be interpreted as markup — the same discipline `no-danger.test.ts`
 * enforces for the rest of this app.
 */
function paintBootError(error: unknown, context: string): void {
  // Always log FIRST, before any early return below — an operator with
  // devtools open must get the real error object and stack for every error,
  // including the ones this function then deliberately declines to paint.
  console.error(`[darkmux] boot error (${context})`, error);

  // Checked against the live DOM, not a module-level flag: a cascade of
  // global errors (a second, unrelated throw while the first overlay is
  // already showing) must not stack copies of it, but the check still has
  // to reflect reality if the overlay is ever removed (a reload, a dismiss,
  // a test resetting `document.body`) rather than remembering a stale
  // "already painted" bit forever.
  if (document.querySelector(".bootcrash-overlay")) return;

  const rootEl = document.getElementById("root");
  // Read from the live DOM for the same reason as the check above: whether a
  // working app is sitting behind this overlay is an OBSERVATION, not a flag
  // somebody has to remember to set. `createRoot(...).render()` is async in
  // React 18, so a "we called render()" boolean would claim a mount that may
  // not have committed — and would keep claiming it after a later unmount.
  // Children in `#root` means there is something to go back to.
  const appIsLive = !!rootEl && rootEl.children.length > 0;

  const { title, message, stack, buildLine, hint } = describeBootError(error, context, { appIsLive });

  const overlay = document.createElement("div");
  overlay.className = "bootcrash-overlay";

  const card = document.createElement("div");
  card.className = "bootcrash";
  card.setAttribute("role", "alert");
  overlay.appendChild(card);

  const titleEl = document.createElement("div");
  titleEl.className = "bootcrash__title";
  titleEl.textContent = title;
  card.appendChild(titleEl);

  const msgEl = document.createElement("div");
  msgEl.className = "bootcrash__msg";
  msgEl.textContent = message;
  card.appendChild(msgEl);

  if (buildLine) {
    const buildEl = document.createElement("div");
    buildEl.className = "bootcrash__build";
    buildEl.textContent = buildLine;
    card.appendChild(buildEl);
  }

  if (stack) {
    const stackEl = document.createElement("pre");
    stackEl.className = "bootcrash__stack";
    // Focusable and labelled so the stack is reachable by keyboard and
    // announced as a region: a `<pre>` alone is a dead end for anyone
    // driving this surface without a mouse, and the stack is the single
    // most useful thing on the card to be able to reach and copy.
    stackEl.tabIndex = 0;
    stackEl.setAttribute("role", "region");
    stackEl.setAttribute("aria-label", "error stack trace");
    stackEl.textContent = stack;
    card.appendChild(stackEl);
  }

  const hintEl = document.createElement("div");
  hintEl.className = "bootcrash__hint";
  hintEl.textContent = hint;
  card.appendChild(hintEl);

  const actions = document.createElement("div");
  actions.className = "bootcrash__actions";
  card.appendChild(actions);

  const reloadBtn = document.createElement("button");
  reloadBtn.type = "button";
  reloadBtn.className = "pcbtn";
  reloadBtn.textContent = "reload";
  reloadBtn.addEventListener("click", () => window.location.reload());
  actions.appendChild(reloadBtn);

  // The previously focused element, captured before the overlay steals
  // focus, so a dismiss puts the operator back where they were.
  const previouslyFocused = document.activeElement as HTMLElement | null;

  if (appIsLive) {
    // A later-tick throw over a MOUNTED app is not a boot failure, and
    // "reload" is not an acceptable only-exit for it: the app underneath is
    // still rendering and still holds the operator's state (an in-progress
    // filter, a scroll position, an opened drawer), all of which a reload
    // throws away. Measured in a real browser: after an async throw the tree
    // is intact and a typed filter value survives underneath. So the loud
    // surface still appears — never a silent failure, which is the whole
    // point of #1709 — but it is escapable without losing that work.
    const dismissBtn = document.createElement("button");
    dismissBtn.type = "button";
    dismissBtn.className = "pcbtn";
    dismissBtn.textContent = "dismiss";
    dismissBtn.addEventListener("click", () => {
      overlay.remove();
      releaseBackground(rootEl);
      previouslyFocused?.focus?.();
    });
    actions.appendChild(dismissBtn);
  }

  document.body.appendChild(overlay);

  // Focus lands INSIDE the surface, and the app behind it stops being
  // reachable. Without this the overlay is visual-only: measured in a real
  // browser, focus stayed on the input the operator had been typing in and
  // Tab walked 36 controls hidden behind the overlay, so a keyboard or
  // screen-reader operator was driving an app they could not see. `inert`
  // removes the subtree from focus order and from the accessibility tree in
  // one attribute; `aria-hidden` covers assistive tech predating it.
  const firstAction = actions.querySelector("button");
  firstAction?.focus();
  if (rootEl) {
    rootEl.setAttribute("inert", "");
    rootEl.setAttribute("aria-hidden", "true");
  }
}

function releaseBackground(rootEl: HTMLElement | null): void {
  rootEl?.removeAttribute("inert");
  rootEl?.removeAttribute("aria-hidden");
}
