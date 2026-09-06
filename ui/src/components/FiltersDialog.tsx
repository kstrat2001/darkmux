import { useEffect, useRef } from "react";
import type { Facets, FilterState } from "../lib/eventFilters";
import { DEFAULT_ACTIVITIES, groupActivitiesBySections } from "../lib/eventFilters";
import { Dialog } from "./Dialog";

const OTHER_GROUPS: { title: string; key: keyof Facets }[] = [
  { title: "category", key: "cat" },
  { title: "tier", key: "tier" },
  { title: "telemetry source", key: "src" },
];

/**
 * `renderFilters()` (viewer.html:2862-2866) — the checkbox-per-facet grid
 * (activity/category/tier/telemetry source) plus the modal's own search
 * field. Wired to the SAME `records`/`FilterState` the event log itself
 * filters by (`EventLogColumn.tsx` owns the state; this renders the
 * controls for it) — legacy's `#fsearch`/`#logq` split, one `state.filters.q`.
 *
 * (operator, 2026-09-06) Legacy's own "model only" / "clear all" quick
 * actions — and this port's `onlyModelActivity()`/`clearFilters()`
 * equivalents — are GONE. On a phone the search field and those two buttons
 * sat in a footer BELOW ~40 checkboxes, so reaching either meant scrolling
 * past everything first. They're replaced by two changes: the search field
 * moves to the TOP of the panel (full width, own row, 16px — the iOS
 * zoom-on-focus rule), and the activity facet is grouped into named
 * sections (MODEL/DISPATCH/MISSION/MACHINE/OTHER —
 * `groupActivitiesBySections`, `lib/eventFilters.ts`) whose header IS a
 * tri-state toggle for everything in that section. Every other facet
 * (category/tier/telemetry source) gets the same header-toggle treatment
 * over its one flat group, so the affordance is uniform across the panel.
 *
 * **Legacy's per-checkbox `data-act="filter" data-k data-arg` attributes
 * (viewer.html:2864) are deliberately NOT carried over.** The repo's rule is
 * to preserve legacy's `data-act` hooks so e2e coverage transfers rather
 * than being re-authored — but those three were legacy's DELEGATED-EVENT
 * plumbing (one body-level listener reading `data-k`/`data-arg` off the
 * clicked node), not test hooks. React binds each checkbox's handler
 * directly, so they would be dead attributes. Checked before dropping them:
 * no spec in `tests/e2e` or `tests/parity` targets them. The hook that IS a
 * test hook — `data-act="filters"`, the trigger that opens this dialog —
 * is preserved, and is what the specs actually use.
 */
export function FiltersDialog({
  facets,
  filters,
  onToggle,
  onToggleMany,
  onSetQuery,
}: {
  facets: Facets;
  filters: FilterState;
  onToggle: (key: keyof Facets, value: string) => void;
  onToggleMany: (key: keyof Facets, values: string[], on: boolean) => void;
  onSetQuery: (q: string) => void;
}) {
  return (
    // (#2116) `className="dialog--filters"` — the activity facet alone can
    // run to ~40 checkboxes on a busy day (facets are computed from the
    // day's own records), which the shared 380px `.dialog` box turns into
    // a skinny scrolling column. `dialog--filters` (styles.css) widens
    // ONLY this dialog to `min(90vw, 720px)`; About and Machine info,
    // which share the plain `.dialog` class, are untouched.
    <Dialog id="modalbg" titleId="filters-title" title="filter events" className="dialog--filters">
      <FiltersBody facets={facets} filters={filters} onToggle={onToggle} onToggleMany={onToggleMany} onSetQuery={onSetQuery} />
    </Dialog>
  );
}

/** A tri-state header checkbox controlling every value in `values` at once
 * — checked when all are on, unchecked when none are, `indeterminate`
 * (set imperatively via ref, the only way to express that state on a real
 * `<input type="checkbox">`) when it's a mix. Click behavior (operator
 * spec): all on → turn all off; anything else (all off OR mixed) → turn
 * all on. */
function SectionHeader({
  label,
  values,
  selected,
  onToggleMany,
  facetKey,
}: {
  label: string;
  values: string[];
  selected: Set<string>;
  onToggleMany: (key: keyof Facets, values: string[], on: boolean) => void;
  facetKey: keyof Facets;
}) {
  const ref = useRef<HTMLInputElement | null>(null);
  const onCount = values.filter((v) => selected.has(v)).length;
  const allOn = values.length > 0 && onCount === values.length;
  const allOff = onCount === 0;
  useEffect(() => {
    if (ref.current) ref.current.indeterminate = !allOn && !allOff;
  }, [allOn, allOff]);
  return (
    <label className="dialog__sectionhead">
      <input
        ref={ref}
        type="checkbox"
        checked={allOn}
        aria-label={`${label}: ${onCount} of ${values.length} on`}
        onChange={() => onToggleMany(facetKey, values, !allOn)}
      />
      <h4>{label}</h4>
    </label>
  );
}

/** (operator, 2026-09-01) The dialog's CONTENTS, split out so a phone can
 *  render them inline in the events pane instead of stacking a modal over a
 *  small screen. Desktop keeps the modal untouched — `#modalbg` is a named
 *  e2e surface (`viewer-keyboard.spec.js` drives Escape and focus-restore
 *  through it), so the dialog path had to stay byte-identical rather than be
 *  reshaped around the phone.
 *
 * (operator, 2026-09-06) Layout is now: search field FIRST (full width, own
 * row), then the activity facet as named sections, then the remaining
 * facets — each with its own header toggle. There is no more footer; the
 * old "model only" / "clear all" buttons and the props that drove them
 * (`onOnlyModel`/`onClearAll`) are gone (see this module's own top doc).
 */
export function FiltersBody({
  facets,
  filters,
  onToggle,
  onToggleMany,
  onSetQuery,
}: {
  facets: Facets;
  filters: FilterState;
  onToggle: (key: keyof Facets, value: string) => void;
  onToggleMany: (key: keyof Facets, values: string[], on: boolean) => void;
  onSetQuery: (q: string) => void;
}) {
  const activitySections = groupActivitiesBySections(facets.act);
  return (
    <>
      <input
        id="fsearch"
        className="dialog__fsearch"
        type="search"
        placeholder="search text…"
        value={filters.q}
        onChange={(e) => onSetQuery(e.target.value)}
      />
      <div id="filterbody">
        {activitySections.map((section) => (
          <div className="dialog__fgroup dialog__fsection" key={section.title}>
            <SectionHeader
              label={section.title.toLowerCase()}
              values={section.values}
              selected={filters.act}
              onToggleMany={onToggleMany}
              facetKey="act"
            />
            <div className="dialog__fgroup--activity">
              {section.values.map((value) => (
                <label key={value}>
                  <input type="checkbox" checked={filters.act.has(value)} onChange={() => onToggle("act", value)} />
                  {value}
                </label>
              ))}
            </div>
          </div>
        ))}
        {OTHER_GROUPS.map(({ title, key }) =>
          facets[key].length ? (
            <div className="dialog__fgroup" key={key}>
              <SectionHeader label={title} values={facets[key]} selected={filters[key]} onToggleMany={onToggleMany} facetKey={key} />
              {facets[key].map((value) => (
                <label key={value}>
                  <input type="checkbox" checked={filters[key].has(value)} onChange={() => onToggle(key, value)} />
                  {value}
                </label>
              ))}
            </div>
          ) : null,
        )}
      </div>
    </>
  );
}

/** `onlyModelActivity()` — viewer.html:2869-2873. Narrows the activity facet
 * to exactly `DEFAULT_ACTIVITIES` (intersected with what's actually present
 * in `facets.act`, matching legacy's `FACTS.filter(a=>keep.has(a))`).
 *
 * (operator, 2026-09-06) The "model only" BUTTON that used to call this is
 * gone (see this module's own top doc — replaced by the MODEL section's own
 * header toggle, which is scoped to the section rather than a whole-facet
 * quick action). This pure vocabulary function survives because
 * `eventFilters.test.ts` exercises it directly against the production
 * activity → facet pipeline (`activityOf` → `computeFacets` →
 * `onlyModelFacet` → `matchesFilters`) as a regression guard on
 * `DEFAULT_ACTIVITIES` itself, independent of which UI control (if any)
 * currently drives it. */
export function onlyModelFacet(facets: Facets): Set<string> {
  return new Set(facets.act.filter((a) => DEFAULT_ACTIVITIES.has(a)));
}
