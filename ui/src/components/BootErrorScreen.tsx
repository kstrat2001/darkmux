import { useEffect, useRef } from "react";
import { describeBootError } from "../lib/bootError";

/**
 * (#1709) The boot-failure surface. Same visual idiom as `LensErrorBoundary`'s
 * `.lenscrash` fallback (same tokens, same "loud and plain, not decorated"
 * posture) but full-page: there is no "rest of the app" left to sit beside
 * when boot itself fails, unlike a single crashed lens.
 *
 * Rendered from TWO places that share this content but not this mechanism:
 * `BootErrorBoundary` mounts it as a normal React fallback (a render throw
 * caught by React); `mountApp.tsx`'s `window.onerror`/`unhandledrejection`
 * listeners paint the SAME content via raw DOM instead (see that file's own
 * doc for why a global safety net must not depend on React still working).
 * Keep this component's JSX and `paintBootError`'s DOM-building in sync by
 * hand if either changes — `describeBootError` owns every sentence, so the
 * words themselves cannot drift between the two.
 *
 * No `dismiss` here, unlike the raw-DOM twin, and the difference is real
 * rather than an oversight: this component renders because a boundary
 * REPLACED the tree that threw, so there is nothing behind it to dismiss
 * back to. The twin offers dismiss only when it finds a live app still
 * mounted in `#root`.
 */
export function BootErrorScreen({ error, context }: { error: unknown; context: string }) {
  const { title, message, stack, buildLine, hint } = describeBootError(error, context);
  const reloadRef = useRef<HTMLButtonElement>(null);

  // Focus lands inside the surface rather than staying wherever it was in
  // the tree React just unmounted, which would leave a keyboard operator
  // with focus on `body` and no indication of where they are.
  useEffect(() => {
    reloadRef.current?.focus();
  }, []);

  return (
    <div className="bootcrash-overlay">
      <div className="bootcrash" role="alert">
        <div className="bootcrash__title">{title}</div>
        <div className="bootcrash__msg">{message}</div>
        {buildLine ? <div className="bootcrash__build">{buildLine}</div> : null}
        {stack ? (
          <pre className="bootcrash__stack" tabIndex={0} role="region" aria-label="error stack trace">
            {stack}
          </pre>
        ) : null}
        <div className="bootcrash__hint">{hint}</div>
        <div className="bootcrash__actions">
          <button ref={reloadRef} type="button" className="pcbtn" onClick={() => window.location.reload()}>
            reload
          </button>
        </div>
      </div>
    </div>
  );
}
