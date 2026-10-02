import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { FILTER_DIMS, type FilterDim } from "../../lib/runsFilterQuery";
import { MACHINE_NOT_FOUND_LABEL } from "../../lib/machineKey";
import { DIM_LABEL, runsCountText, selectionText, type FacetView } from "./runFilters";

/** (#2925) The runs board's filter bar: one pill per dimension, a popover
 * checklist per pill, and the summary row (count, one chip per active value,
 * Clear all). Thin on top of `runFilters.ts`, which owns every rule about
 * what a count or a selection means. */

/** A popover lists a search box once it has more values than this. */
const SEARCH_ABOVE = 7;

/** Selecting a value in a multi-select dimension. */
function toggled(selected: string[], value: string): string[] {
  return selected.includes(value) ? selected.filter((v) => v !== value) : [...selected, value];
}

function onActivate(fn: () => void) {
  return (e: KeyboardEvent<HTMLElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      fn();
    }
  };
}

export function FilterBar({
  facets,
  rendered,
  shown,
  total,
  onChange,
  onClearAll,
}: {
  facets: FacetView[];
  /** Rows on screen (the newest page unless the list is expanded). */
  rendered: number;
  /** Runs the filters keep, and the runs before any dimension filter. */
  shown: number;
  total: number;
  /** Replace one dimension's selection. */
  onChange: (dim: FilterDim, values: string[]) => void;
  onClearAll: () => void;
}) {
  const [open, setOpen] = useState<FilterDim | null>(null);
  const pills = useRef(new Map<FilterDim, HTMLButtonElement>());
  const openFacet = facets.find((f) => f.dim === open);
  const close = () => {
    const dim = open;
    setOpen(null);
    if (dim) pills.current.get(dim)?.focus();
  };
  return (
    <div className="fbarwrap">
      <div className="fbar" role="toolbar" aria-label="Filter runs">
        {facets.map((f) => (
          <FilterPill
            key={f.dim}
            facet={f}
            open={open === f.dim}
            setRef={(el) => (el ? pills.current.set(f.dim, el) : pills.current.delete(f.dim))}
            onToggle={() => setOpen(open === f.dim ? null : f.dim)}
          />
        ))}
      </div>
      {openFacet && (
        <FilterPopover
          key={openFacet.dim}
          facet={openFacet}
          anchor={pills.current.get(openFacet.dim) ?? null}
          onChange={(values) => onChange(openFacet.dim, values)}
          onClose={close}
        />
      )}
      <FilterSummary facets={facets} rendered={rendered} shown={shown} total={total} onChange={onChange} onClearAll={onClearAll} />
    </div>
  );
}

function FilterPill({ facet, open, setRef, onToggle }: { facet: FacetView; open: boolean; setRef: (el: HTMLButtonElement | null) => void; onToggle: () => void }) {
  const text = selectionText(facet);
  const idle = !facet.narrows;
  return (
    <button
      type="button"
      ref={setRef}
      className={`fpill${text ? " on" : ""}${idle ? " idle" : ""}`}
      data-dim={facet.dim}
      aria-haspopup="dialog"
      aria-expanded={open}
      title={idle ? `every run in view has the same ${DIM_LABEL[facet.dim].toLowerCase()}` : undefined}
      onClick={onToggle}
    >
      {DIM_LABEL[facet.dim]}
      {text && <span className="fpillv">{text}</span>}
    </button>
  );
}

function FilterPopover({ facet, anchor, onChange, onClose }: { facet: FacetView; anchor: HTMLElement | null; onChange: (values: string[]) => void; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const ref = useRef<HTMLDivElement>(null);
  const isTime = facet.dim === "time";
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    ref.current?.querySelector<HTMLElement>("input")?.focus();
    return () => document.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const rect = anchor?.getBoundingClientRect();
  const style = rect ? { top: rect.bottom + 4, left: Math.max(8, Math.min(rect.left, window.innerWidth - 280)) } : undefined;
  const q = query.trim().toLowerCase();
  const visible = facet.choices.filter((c) => c.label.toLowerCase().includes(q));
  return createPortal(
    <>
      <div className="fscrim" onClick={onClose} />
      <div ref={ref} className="fpop" role="dialog" aria-label={`${DIM_LABEL[facet.dim]} filter`} style={style}>
        <div className="fpoptitle">{DIM_LABEL[facet.dim]}</div>
        {!isTime && facet.choices.length > SEARCH_ABOVE && (
          <input className="fsearch" type="search" placeholder={`search ${DIM_LABEL[facet.dim].toLowerCase()}`} aria-label="search values" value={query} onChange={(e) => setQuery(e.target.value)} />
        )}
        <ul className="flist">
          {isTime && (
            <li>
              <label className="fopt">
                <input type="radio" name="ftime" checked={facet.selected.length === 0} onChange={() => onChange([])} />
                <span className="foptl">Any time</span>
              </label>
            </li>
          )}
          {visible.map((c) => (
            <li key={c.value}>
              <label className={`fopt${c.count === 0 ? " zero" : ""}`}>
                {isTime ? (
                  <input type="radio" name="ftime" checked={facet.selected.includes(c.value)} onChange={() => onChange([c.value])} />
                ) : (
                  <input type="checkbox" checked={facet.selected.includes(c.value)} onChange={() => onChange(toggled(facet.selected, c.value))} />
                )}
                <span className="foptl">{c.label}</span>
                <span className="foptn">{c.count}</span>
              </label>
              {!isTime && (
                <button type="button" className="fonly" onClick={() => onChange([c.value])}>
                  only
                </button>
              )}
            </li>
          ))}
          {visible.length === 0 && <li className="none">no matching values</li>}
        </ul>
        <div className="ffoot">
          <button type="button" className="fbtn" onClick={() => onChange([])}>
            Clear
          </button>
          <button type="button" className="fbtn primary" onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    </>,
    document.body,
  );
}

/** A chip reads "Machine studio"; a machine the hash names but no card does
 * reads just "machine not found". */
function chipText(f: FacetView, value: string): string {
  const label = f.choices.find((c) => c.value === value)?.label ?? value;
  return label === MACHINE_NOT_FOUND_LABEL ? label : `${DIM_LABEL[f.dim]} ${label}`;
}

function FilterSummary({ facets, rendered, shown, total, onChange, onClearAll }: { facets: FacetView[]; rendered: number; shown: number; total: number; onChange: (dim: FilterDim, values: string[]) => void; onClearAll: () => void }) {
  const active = FILTER_DIMS.flatMap((dim) => {
    const f = facets.find((x) => x.dim === dim);
    return f ? f.selected.map((value) => ({ f, value })) : [];
  });
  return (
    <div className="fsummary">
      <span className="fcount">
        {runsCountText({ rendered, matching: shown, total })}
      </span>
      {active.map(({ f, value }) => (
        <span
          key={`${f.dim}:${value}`}
          className="runchip on fchip"
          data-act={f.dim === "machine" ? "clearmachine" : "clearfilter"}
          role="button"
          tabIndex={0}
          title={`remove ${chipText(f, value)}`}
          onClick={() => onChange(f.dim, f.selected.filter((v) => v !== value))}
          onKeyDown={onActivate(() => onChange(f.dim, f.selected.filter((v) => v !== value)))}
        >
          {chipText(f, value)} ✕
        </span>
      ))}
      {active.length > 0 && (
        <button type="button" className="fclear" onClick={onClearAll}>
          Clear all
        </button>
      )}
    </div>
  );
}
