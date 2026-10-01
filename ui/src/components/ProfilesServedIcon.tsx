/*
 * The `layers` icon paths below are from Lucide (https://lucide.dev), copied here rather than
 * added as a dependency. ISC License, Copyright (c) 2026 Lucide Icons and Contributors; the full
 * notice is in ui/vendor-licenses/LICENSE-lucide and ships with the built viewer.
 */

/** A machine serves `count` profiles to peers: the number of distinct profiles in its registry
 * that its allow-list grants to at least one peer (its card's `serves_profiles`). A fact about
 * that machine alone, so it reads the same from every server: never which peer may use them,
 * and never a relationship with the machine serving the viewer. A stack of layers and the
 * count, in the card's name row beside the radio icon; the tooltip (and the accessible name)
 * says what it means. */
export function ProfilesServedIcon({ machine, count }: { machine: string; count: number }) {
  const title = `${machine} serves ${count} ${count === 1 ? "profile" : "profiles"} to peers it allows.`;
  return (
    <span className="profiles-served" data-testid="profiles-served" role="img" title={title} aria-label={title}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
        <path d="M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z" />
        <path d="M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12" />
        <path d="M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17" />
      </svg>
      <span className="profiles-served__n" aria-hidden="true">
        {count}
      </span>
    </span>
  );
}
