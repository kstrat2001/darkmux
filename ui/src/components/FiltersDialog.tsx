import type { Facets, FilterState } from "../lib/eventFilters";
import { MODEL_ACTIVITIES } from "../lib/eventFilters";
import { Dialog } from "./Dialog";

const GROUPS: { title: string; key: keyof Facets }[] = [
  { title: "activity", key: "act" },
  { title: "category", key: "cat" },
  { title: "tier", key: "tier" },
  { title: "telemetry source", key: "src" },
];

/**
 * `renderFilters()` (viewer.html:2862-2866) — the checkbox-per-facet grid
 * (activity/category/tier/telemetry source), plus the modal's own search
 * field and its two quick actions ("model only" — `onlyModelActivity()`,
 * viewer.html:2869-2873 — and "clear all" — `clearFilters()`,
 * viewer.html:2877). Wired to the SAME `records`/`FilterState` the event log
 * itself filters by (`EventLogColumn.tsx` owns the state; this renders the
 * controls for it) — legacy's `#fsearch`/`#logq` split, one `state.filters.q`.
 *
 * This was the named, deliberate cut in `EventLogColumn.tsx`'s own module
 * doc ("The filter MODAL is a named, deliberate cut, not a half-build") —
 * the real thing, now that the shared dialog machinery exists to hold it.
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
  onSetQuery,
  onOnlyModel,
  onClearAll,
}: {
  facets: Facets;
  filters: FilterState;
  onToggle: (key: keyof Facets, value: string) => void;
  onSetQuery: (q: string) => void;
  onOnlyModel: () => void;
  onClearAll: () => void;
}) {
  return (
    <Dialog id="modalbg" titleId="filters-title" title="filter events">
      <div id="filterbody">
        {GROUPS.map(({ title, key }) => (
          <div className="dialog__fgroup" key={key}>
            <h4>{title}</h4>
            {facets[key].map((value) => (
              <label key={value}>
                <input type="checkbox" checked={filters[key].has(value)} onChange={() => onToggle(key, value)} />
                {value}
              </label>
            ))}
          </div>
        ))}
      </div>
      <div className="dialog__filterfoot">
        <input
          id="fsearch"
          placeholder="search text…"
          value={filters.q}
          onChange={(e) => onSetQuery(e.target.value)}
        />
        <button type="button" className="dialog__clr" title="show only reasoning, tool calls, and turns" onClick={onOnlyModel}>
          model only
        </button>
        <button type="button" className="dialog__clr" onClick={onClearAll}>
          clear all
        </button>
      </div>
    </Dialog>
  );
}

/** `onlyModelActivity()` — viewer.html:2869-2873. Narrows the activity facet
 * to exactly `MODEL_ACTIVITIES` (intersected with what's actually present in
 * `facets.act`, matching legacy's `FACTS.filter(a=>keep.has(a))`). */
export function onlyModelFacet(facets: Facets): Set<string> {
  return new Set(facets.act.filter((a) => MODEL_ACTIVITIES.has(a)));
}
