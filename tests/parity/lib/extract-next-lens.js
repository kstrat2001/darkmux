// The new-UI (`/next`, React + TanStack Query) half of the parity harness.
// Deliberately THIN: `waitSettled`/`installFrozenClock`/`normalize` are
// imported verbatim from `extract-lens.js` (none of those three are
// legacy-DOM-specific — they operate on any `page`/any selector), and
// `installCorpusRoutes`/`installBlankRoutes` from `mock-routes.js` work
// UNCHANGED here too, because the React app fetches the exact same endpoint
// paths (`/runs`, `/lab/runs`, ...) the legacy viewer does — no new
// interception table to maintain in parallel with the old one, so the two
// harnesses cannot silently drift apart on WHAT gets mocked.
//
// The one genuinely new piece is `extractStageOnlyText`, and its scoping
// decision is the one deviation from `extractLensText`'s four-region extract
// this file makes on purpose (not by omission):
//
// `viewer.html`'s four extracted regions (`#crumb`/`#meta`/`#logscope`/
// `#stage`) are NOT all lens-owned. `#meta` (the fleet-status badge line) and
// `#logscope` (the event-log scope label) are GLOBAL APP CHROME, populated
// by `setBadges()`/boot()'s live-tail wiring — every lens shares the same
// badge and scope label, none of them own it. `#crumb` is similar but with a
// twist: the scaffold's `App.tsx` (Packet 1) already gave EVERY lens a
// non-empty crumb (`"darkmux · <lens label>"`), a deliberate, documented,
// cross-lens UX decision made before this packet existed — matching
// `viewer.html`'s literal (mostly-empty) `#crumb` text byte-for-byte here
// would mean fighting Packet 1's own scaffold convention, not doing runs-lens
// work. None of the three exist as portable concepts in `/next` today: no
// packet owns global chrome yet, and inventing it here would be scope creep
// into every other lens packet's territory.
//
// So parity is scoped to `#stage` — the actual per-lens rendering target in
// BOTH apps (`viewer.html`'s every `render*()` writes there; `/next`'s
// `<main id="stage">` in `App.tsx` wraps whatever the routed lens component
// renders) — compared against the golden's own `=== stage ===` section only.
// This is the one named, justified normalization this harness applies;
// everything within it is still byte-equality, no fuzzy matching.
const { regionText, waitSettled, installFrozenClock, normalize } = require("./extract-lens.js");
const { installCorpusRoutes, installBlankRoutes, loadMeta } = require("./mock-routes.js");

async function extractStageOnlyText(page) {
  const stage = await regionText(page, "stage");
  return `=== stage ===\n${normalize(stage || "(empty)")}\n`;
}

/** (Packet 4) The catalog-panel analog of `extractStageOnlyText` above —
 * `#catpanel` is global chrome mounted as a SIBLING of `#stage` in BOTH apps
 * (see `CatalogPanel.tsx`'s own module doc), not something `#stage`-only
 * scoping would ever see, so it gets its own extractor + its own golden
 * section rather than being folded into the stage comparison. */
async function extractCatalogOnlyText(page) {
  const catalog = await regionText(page, "catpanel");
  return `=== catalog ===\n${normalize(catalog || "(empty)")}\n`;
}

/** Slice the `=== catalog ===` section out of a full legacy golden — the
 * `extractCatalogOnlyText` counterpart to `stageSectionOf` below. */
function catalogSectionOf(fullGoldenText) {
  const marker = "=== catalog ===\n";
  const idx = fullGoldenText.indexOf(marker);
  if (idx === -1) throw new Error(`catalogSectionOf: no "${marker.trim()}" marker found in golden text`);
  return fullGoldenText.slice(idx);
}

/** Slice just the `=== stage ===` section out of a FULL legacy golden (the
 * four-region file `extractLensText` writes) — so this harness compares
 * against the SAME committed golden file the legacy extractor produces,
 * rather than needing a second stage-only golden fork. Keeps exactly one
 * golden per lens/state as the source of truth for both apps. */
function stageSectionOf(fullGoldenText) {
  const marker = "=== stage ===\n";
  const idx = fullGoldenText.indexOf(marker);
  if (idx === -1) throw new Error(`stageSectionOf: no "${marker.trim()}" marker found in golden text`);
  return fullGoldenText.slice(idx);
}

module.exports = {
  extractStageOnlyText,
  stageSectionOf,
  extractCatalogOnlyText,
  catalogSectionOf,
  waitSettled,
  installFrozenClock,
  installCorpusRoutes,
  installBlankRoutes,
  loadMeta,
  normalize,
};
