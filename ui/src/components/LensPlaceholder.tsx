/**
 * The page for a hash `parseRoute` doesn't recognize: the ONLY route kind that
 * reaches this component (`App.tsx`'s `renderRoute` switch gives every named
 * `Route` kind its own lens). An unrecognized route must be VISIBLE, never
 * silent: `hash` shows the exact raw hash the operator arrived with, so a
 * broken bookmark is debuggable at a glance. That includes a retired link
 * spelling (`#session=`, `#lens=lab`, `uid=`, `panel=mission-status-all`): the
 * viewer keeps no aliases, so an old link lands here rather than being
 * silently rewritten.
 *
 * The lens tabs `NavChrome` renders directly above this component, on every
 * route including this one, are the way out — never a dead end with no visible
 * next step.
 */
export function LensPlaceholder({ hash }: { hash?: string }) {
  return (
    <div className="lens-placeholder" data-state="not-ported" role="status">
      <p className="lens-placeholder__title">Unknown route</p>
      {hash !== undefined && <p className="lens-placeholder__hash">hash: #{hash || "(empty)"}</p>}
      <p className="lens-placeholder__note">
        This build has no route for that hash. Old link spellings are not redirected: pick a lens from the tabs
        above, or clear the hash to return to the live fleet view.
      </p>
    </div>
  );
}
