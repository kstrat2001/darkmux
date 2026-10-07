import type { RunKind } from "../types/generated/RunKind";

/**
 * A run's KIND (mission, dispatch, lab) as an icon. Every place that shows a
 * run's kind uses this one component.
 *
 * (operator, 2026-10-07) The kind used to be a bordered, colored chip in the
 * status chip's own style, so a green "LAB" beside a green "COMPLETE" read as
 * the same class of fact. A kind is not a state: it is drawn here in a
 * different style on purpose, monochrome (`currentColor`, the dim token), with
 * no chip background and no status color. The kind's name rides in the
 * accessible name and the tooltip ("lab run").
 *
 * An inline SVG contributes no text, so text-based goldens cannot see it (see
 * `ActivityIcon.tsx`'s module doc). Tests assert on `data-run-kind` instead.
 *
 * The box keeps the old chip's height (`.kindicon` in styles.css), so a row is
 * no taller or shorter than it was.
 */

// Paths from Lucide 0.469.0 (https://lucide.dev): flask-conical, rocket, send. ISC, notice in ui/vendor-licenses/LICENSE-lucide.
const GLYPH: Record<RunKind, readonly string[]> = {
  lab: ["M14 2v6a2 2 0 0 0 .245.96l5.51 10.08A2 2 0 0 1 18 22H6a2 2 0 0 1-1.755-2.96l5.51-10.08A2 2 0 0 0 10 8V2", "M6.453 15h11.094", "M8.5 2h7"],
  // A rocket, after darkmux's NASA mission vocabulary.
  mission: [
    "M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z",
    "m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z",
    "M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0",
    "M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5",
  ],
  dispatch: ["M14.536 21.686a.5.5 0 0 0 .937-.024l6.5-19a.496.496 0 0 0-.635-.635l-19 6.5a.5.5 0 0 0-.024.937l7.93 3.18a2 2 0 0 1 1.112 1.11z", "m21.854 2.147-10.94 10.939"],
};

export function RunKindIcon({ kind }: { kind: RunKind }) {
  const label = `${kind} run`;
  const paths = GLYPH[kind] as readonly string[] | undefined;
  return (
    <span className="kindicon" data-run-kind={kind} role="img" aria-label={label} title={label}>
      {paths ? (
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
          {paths.map((d) => (
            <path key={d} d={d} />
          ))}
        </svg>
      ) : (
        // A kind a newer daemon sends that this build has no glyph for: its
        // word, in the same slot, rather than an empty box.
        kind
      )}
    </span>
  );
}
