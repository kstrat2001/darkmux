/**
 * The fallback for a `#lens=` value `parseRoute` doesn't recognize — the
 * ONLY route kind that reaches this component (`App.tsx`'s `renderRoute`
 * switch: every named `Route` kind — `fleet`/`runs`/`machine`/`console`/
 * `session`/`mission`/`playback` — has its own real lens; only `unknown`
 * falls through here, always with `label="unrecognized"`). Per the
 * overnight runbook's render-sanity contract: an unrecognized route must be
 * VISIBLE, never silent. `hash` (when present) shows the exact raw hash the
 * operator arrived with, so a broken bookmark is debuggable at a glance
 * instead of just "nothing happened".
 *
 * (#1920) The note used to say "This view still lives in the legacy
 * viewer — see `GET /`." That was true when this component was written
 * (Packet 1–3, an actively-porting app with a real legacy fallback still
 * serving `/`) and became false the moment `viewer.html` was deleted
 * (#1865): `GET /` now serves this SAME React app, `id="root"`, not a
 * different page with different capabilities. So every operator who landed
 * here — a case-mismatched `lens=`, a typo — was told to go to the page
 * they were already on, where nothing different was waiting. The fix names
 * what actually happened (this build doesn't recognize the hash) and what
 * the operator can actually do about it: the lens tabs `NavChrome` renders
 * directly above this component, on every route including this one, are a
 * real, working way out — never a dead end with no visible next step.
 * (`#lens=fleet` — a plausible typed-or-shared guess — also stopped
 * dead-ending here in the same packet: it's a real, explicit route now,
 * see `route.ts`'s own doc.)
 */
export function LensPlaceholder({ label, hash }: { label: string; hash?: string }) {
  return (
    <div className="lens-placeholder" data-state="not-ported" role="status">
      <p className="lens-placeholder__title">Lens not ported yet: {label}</p>
      {hash !== undefined && <p className="lens-placeholder__hash">hash: #{hash || "(empty)"}</p>}
      <p className="lens-placeholder__note">
        This build doesn't recognize that hash — pick a lens from the tabs above, or clear the hash to return to
        the live fleet view.
      </p>
    </div>
  );
}
