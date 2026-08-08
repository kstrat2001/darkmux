/**
 * "Lens not ported yet" — the fallback every route this packet doesn't
 * implement renders instead of a blank page. Per the overnight runbook's
 * render-sanity contract: unknown/unimplemented routes must be VISIBLE, never
 * silent. `label` names what a lens packet needs to build; `hash` (when
 * present) shows the exact raw hash the operator arrived with, so a broken
 * bookmark is debuggable at a glance instead of just "nothing happened".
 */
export function LensPlaceholder({ label, hash }: { label: string; hash?: string }) {
  return (
    <div className="lens-placeholder" data-state="not-ported" role="status">
      <p className="lens-placeholder__title">Lens not ported yet: {label}</p>
      {hash !== undefined && <p className="lens-placeholder__hash">hash: #{hash || "(empty)"}</p>}
      <p className="lens-placeholder__note">
        This view still lives in the legacy viewer — see <code>GET /</code>.
      </p>
    </div>
  );
}
