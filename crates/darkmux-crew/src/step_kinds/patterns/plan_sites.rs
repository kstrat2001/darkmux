//! The generalized "prefilter hits over a source, window each hit, pack
//! windows into sizing-bounded units" procedure (#1352 Tier 2, extracted
//! for #2310 P4b).
//!
//! Extracted from the crawl planner (`darkmux-lab`'s `crawl::plan::
//! collect_site_units`). Per #1352's own framing: DESIGN.md ("Crawl as a
//! mission" / "Code review as a second config on the crawl's building
//! blocks") names the crawl's own control flow as "sources -> files ->
//! prefilter hits -> sites with a rule window -> units sized by
//! `max_sites_per_unit`/`max_est_tokens_per_unit`" and says the control flow
//! stays the same for a code review's diff-scoped planner — only the SOURCE
//! enumeration (a tree walk vs. a diff's hunks) differs. That is exactly
//! Tier 2's test: the procedure is new-and-reusable, the algorithm (which
//! files exist, what their content is, which lines are worth prefiltering)
//! is a caller-supplied strategy.
//!
//! What generalizes and what does not, named explicitly because it is easy
//! to get backwards: enumerating files, reading their content, and deciding
//! which lines are candidates worth testing is exactly [`SiteSource`]'s
//! job, and stays with the caller (`darkmux-lab`'s `crawl::plan::
//! TreeSource`/`DiffSource` — this crate has no `darkmux-lab` dependency and
//! never will, so a source impl cannot live here even for the crawl's own
//! use). Compiling a rule's prefilter into a matcher is ALSO the caller's:
//! this module takes an `is_hit: &dyn Fn(&str) -> bool` closure rather than
//! a regex list, so it never needs the `regex` crate as a dependency —
//! deciding "empty prefilter" behavior (zero units for a tree walk, "match
//! everything" for a diff rule declaring `prefilter: none`, per DESIGN.md
//! "A rule is a procedure") is therefore the caller's call too, made before
//! it ever calls [`plan_site_units`].
//!
//! What this module owns: merging overlapping/adjacent hit windows into
//! spans (`merge_windows`, moved here verbatim from `crawl::plan` — same
//! logic, same doc, unchanged behavior), packing spans into units under a
//! sites/tokens budget (unchanged from `collect_site_units`), and — new in
//! #2310 P4b — capping a single merged span's width via `max_span_lines`.
//! `crawl::plan`'s own `TreeSource` caller passes `None`, so its output is
//! byte-identical to before this module existed (a tree walk's prefilter
//! hits are sparse regex matches; a merged run has never needed a width
//! cap). A diff's "every hunk line is a candidate" mode (`prefilter: none`)
//! makes one hunk ONE contiguous run of hits by construction — with no cap
//! that run would always merge into a single, unbounded site, which
//! contradicts DESIGN.md's "a hunk with more lines than the window becomes
//! several sites" — so `Some(n)` is what makes that sentence true for a
//! diff-backed caller. `n` is the caller's choice (`crawl::plan::DiffSource`
//! documents its own).

use anyhow::Result;
use std::rc::Rc;

/// One file a [`SiteSource`] is willing to consider, plus the 1-indexed
/// line numbers within it worth testing as prefilter candidates.
pub struct SiteSourceFile {
    pub file: String,
    /// `Rc<String>` rather than an owned `String` so a source backed by an
    /// already-cached read (`crawl::plan::SourceFiles`, an `Rc<String>` per
    /// file, shared across every rule pass over that file) hands back a
    /// cheap handle instead of cloning the file's bytes a second time per
    /// rule — the module doc's "read once from disk" guarantee said
    /// nothing about the in-memory copy count, and a clone-per-rule would
    /// have quietly regressed it. (Not `Rc<str>`: converting an `Rc<String>`
    /// cache entry to `Rc<str>` would itself require a fresh allocation —
    /// there is no free unsizing coercion between the two — so this stays
    /// the type the cache already holds.)
    pub content: Rc<String>,
    /// 1-indexed line numbers to test — every line of the file for a tree
    /// walk, a diff's hunk lines for a diff. Always sorted ascending. A
    /// file with nothing worth testing is simply absent from
    /// [`SiteSource::files`]'s result, not present with an empty list.
    pub candidates: Vec<usize>,
}

/// Where [`plan_site_units`] gets its files and candidate lines from.
/// `TreeSource`/`DiffSource` are `darkmux-lab`'s own (`crawl::plan`) — see
/// the module doc for why an impl cannot live in this crate.
pub trait SiteSource {
    /// `&mut self` because a real source reads through a file-content
    /// cache it owns (`crawl::plan::SourceFiles`, shared across rule
    /// passes so a file two rules both match is only ever read from disk
    /// once) and records read failures as it goes — both are mutations,
    /// and a `&self` signature would force interior mutability onto every
    /// impl for no benefit (nothing here runs concurrently).
    fn files(&mut self) -> Result<Vec<SiteSourceFile>>;
}

/// One packed unit's worth of sites, before the caller assigns an id — ids
/// interleave across rules/sources in `crawl::plan::plan_with_params`'s own
/// sequence, which this function has no visibility into and no opinion on.
pub struct PlannedUnit {
    pub sites: Vec<PlannedSite>,
    pub est_tokens: usize,
}

/// Mirrors `crawl::plan::Site`'s fields exactly (this crate cannot depend
/// on that type — see the module doc); the caller converts 1:1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSite {
    pub file: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub hits: Vec<usize>,
}

pub struct SiteUnitParams {
    /// Lines of context on each side of a hit — `Rule::window_or_default()`.
    pub window: usize,
    pub max_sites_per_unit: usize,
    pub max_est_tokens_per_unit: usize,
    /// Cap a single merged span at this many lines, splitting a wider one
    /// into sequential chunks. `None` (the tree-walk default) never splits
    /// — see the module doc for why that keeps `TreeSource`'s output
    /// byte-identical to before this module existed.
    pub max_span_lines: Option<usize>,
}

/// rules × source → windows → units, over whatever `source` enumerates.
/// `is_hit` decides which of a file's candidate lines count as a hit — the
/// caller's own prefilter, already compiled (or `|_| true`, for a rule
/// declaring no prefilter at all against a source whose empty prefilter
/// means "match everything"). `est_tokens` is the caller's token-estimate
/// heuristic (`crawl::plan::estimate_tokens` — kept a callback rather than
/// moved crates, so the ONE canonical implementation doesn't have to leave
/// its documented home to be reused here).
///
/// Deciding "should I call this at all" (an empty prefilter against a
/// source that means "zero units", the crawl planner's existing behavior)
/// is the caller's — this function always runs `is_hit` over every
/// candidate and does not special-case emptiness.
pub fn plan_site_units(
    source: &mut dyn SiteSource,
    is_hit: &dyn Fn(&str) -> bool,
    params: &SiteUnitParams,
    est_tokens: &dyn Fn(&str) -> usize,
) -> Result<Vec<PlannedUnit>> {
    let mut batch: Vec<(PlannedSite, usize)> = Vec::new();
    for f in source.files()? {
        let lines: Vec<&str> = f.content.lines().collect();
        let total = lines.len();
        let mut hits: Vec<usize> = Vec::new();
        for &ln in &f.candidates {
            if ln == 0 || ln > total {
                continue; // a candidate line outside the file's own range is never a hit
            }
            if is_hit(lines[ln - 1]) {
                hits.push(ln);
            }
        }
        if hits.is_empty() {
            continue;
        }
        for (s, e, first, hit_list) in merge_windows(&hits, params.window, total) {
            for (cs, ce, cfirst, chits) in split_span(s, e, first, &hit_list, params.max_span_lines) {
                let tokens = est_tokens(&window_text(&lines, cs, ce));
                batch.push((
                    PlannedSite { file: f.file.clone(), line: cfirst, start: cs, end: ce, hits: chits },
                    tokens,
                ));
            }
        }
    }

    let mut out = Vec::new();
    let mut cur_sites: Vec<PlannedSite> = Vec::new();
    let mut cur_tokens = 0usize;
    for (site, tok) in batch {
        let would_exceed_count = cur_sites.len() + 1 > params.max_sites_per_unit;
        let would_exceed_tokens = !cur_sites.is_empty() && cur_tokens + tok > params.max_est_tokens_per_unit;
        if !cur_sites.is_empty() && (would_exceed_count || would_exceed_tokens) {
            out.push(PlannedUnit { sites: std::mem::take(&mut cur_sites), est_tokens: cur_tokens });
            cur_tokens = 0;
        }
        cur_sites.push(site);
        cur_tokens += tok;
    }
    if !cur_sites.is_empty() {
        out.push(PlannedUnit { sites: cur_sites, est_tokens: cur_tokens });
    }
    Ok(out)
}

/// Merge 1-indexed hit line numbers into `(start, end, first_hit_line,
/// every_hit_line)` spans using `window` lines of context on each side,
/// clamped to `[1, total_lines]`. `hits` must be sorted ascending.
/// Overlapping/adjacent spans merge into one, and every hit line that
/// merged into a span is carried forward, not just the first.
///
/// Same logic, same doc as `crawl::plan::merge_windows` (#2310 P4b), whose
/// `collect_site_units` caller now goes through this module instead —
/// `crawl::plan` keeps its OWN copy too, because `collect_edge_import_sites`
/// (edge-kind rules, out of scope for this packet — see #2310's own P4b
/// brief) still calls it directly and edge units are not part of the
/// pattern extracted here.
fn merge_windows(hits: &[usize], window: usize, total_lines: usize) -> Vec<(usize, usize, usize, Vec<usize>)> {
    let clamp_end = total_lines.max(1);
    let mut spans: Vec<(usize, usize, usize, Vec<usize>)> = Vec::new();
    for &h in hits {
        let s = h.saturating_sub(window).max(1);
        let e = (h + window).min(clamp_end);
        match spans.last_mut() {
            Some(last) if s <= last.1 + 1 => {
                last.1 = last.1.max(e);
                last.3.push(h);
            }
            _ => spans.push((s, e, h, vec![h])),
        }
    }
    spans
}

/// Split one merged span into sequential chunks of at most `cap` lines each
/// (#2310 P4b) — `None` returns the span unchanged (one element). A chunk's
/// `hits` is the subset of the original span's hits that fall inside it; a
/// chunk with none (possible only when a caller sets a cap narrower than
/// the gaps between its own hits — not reachable from a diff's "every line
/// is a hit" mode, where hits are contiguous) is dropped rather than
/// emitted as a hit-less site.
fn split_span(
    start: usize,
    end: usize,
    first: usize,
    hits: &[usize],
    cap: Option<usize>,
) -> Vec<(usize, usize, usize, Vec<usize>)> {
    let Some(cap) = cap.filter(|c| *c > 0 && end - start + 1 > *c) else {
        return vec![(start, end, first, hits.to_vec())];
    };
    let mut out = Vec::new();
    let mut s = start;
    while s <= end {
        let e = (s + cap - 1).min(end);
        let chunk_hits: Vec<usize> = hits.iter().copied().filter(|h| *h >= s && *h <= e).collect();
        if let Some(&chunk_first) = chunk_hits.first() {
            out.push((s, e, chunk_first, chunk_hits));
        }
        s = e + 1;
    }
    out
}

fn window_text(lines: &[&str], start: usize, end: usize) -> String {
    lines[start.saturating_sub(1)..end.min(lines.len())].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSource(Vec<SiteSourceFile>);
    impl SiteSource for FixedSource {
        fn files(&mut self) -> Result<Vec<SiteSourceFile>> {
            Ok(self.0.iter().map(|f| SiteSourceFile {
                file: f.file.clone(),
                content: f.content.clone(),
                candidates: f.candidates.clone(),
            }).collect())
        }
    }

    fn params(window: usize, max_sites: usize, max_tokens: usize, max_span_lines: Option<usize>) -> SiteUnitParams {
        SiteUnitParams { window, max_sites_per_unit: max_sites, max_est_tokens_per_unit: max_tokens, max_span_lines }
    }

    fn chars_est(s: &str) -> usize {
        s.chars().count().div_ceil(4)
    }

    #[test]
    fn a_hit_line_becomes_one_windowed_site() {
        let content: Rc<String> = Rc::new("a\nb\nHIT\nc\nd\n".to_string());
        let mut source = FixedSource(vec![SiteSourceFile { file: "f.ts".into(), content, candidates: (1..=5).collect() }]);
        let out = plan_site_units(&mut source, &|l| l == "HIT", &params(1, 40, 16_000, None), &chars_est).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sites.len(), 1);
        let site = &out[0].sites[0];
        assert_eq!((site.start, site.end, site.line, site.hits.clone()), (2, 4, 3, vec![3]));
    }

    #[test]
    fn no_hits_produces_no_units() {
        let content: Rc<String> = Rc::new("a\nb\nc\n".to_string());
        let mut source = FixedSource(vec![SiteSourceFile { file: "f.ts".into(), content, candidates: (1..=3).collect() }]);
        let out = plan_site_units(&mut source, &|_| false, &params(1, 40, 16_000, None), &chars_est).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn a_span_wider_than_the_cap_splits_into_several_sites() {
        // (#2310 P4b — mutation-kill for `split_span`) 10 contiguous hit
        // lines, cap 4: three chunks (4, 4, 2), never one.
        let content: Rc<String> = Rc::new((1..=10).map(|n| format!("l{n}")).collect::<Vec<_>>().join("\n") + "\n");
        let candidates: Vec<usize> = (1..=10).collect();
        let mut source = FixedSource(vec![SiteSourceFile { file: "f.ts".into(), content, candidates }]);
        let out = plan_site_units(&mut source, &|_| true, &params(0, 40, 1_000_000, Some(4)), &chars_est).unwrap();
        let sites: Vec<&PlannedSite> = out.iter().flat_map(|u| u.sites.iter()).collect();
        assert_eq!(sites.len(), 3, "{sites:?}");
        assert_eq!((sites[0].start, sites[0].end), (1, 4));
        assert_eq!((sites[1].start, sites[1].end), (5, 8));
        assert_eq!((sites[2].start, sites[2].end), (9, 10));
        // no line is lost across the split, and none is duplicated
        let mut every_hit: Vec<usize> = sites.iter().flat_map(|s| s.hits.clone()).collect();
        every_hit.sort_unstable();
        assert_eq!(every_hit, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn without_a_cap_a_wide_span_never_splits() {
        let content: Rc<String> = Rc::new((1..=10).map(|n| format!("l{n}")).collect::<Vec<_>>().join("\n") + "\n");
        let candidates: Vec<usize> = (1..=10).collect();
        let mut source = FixedSource(vec![SiteSourceFile { file: "f.ts".into(), content, candidates }]);
        let out = plan_site_units(&mut source, &|_| true, &params(0, 40, 1_000_000, None), &chars_est).unwrap();
        let sites: Vec<&PlannedSite> = out.iter().flat_map(|u| u.sites.iter()).collect();
        assert_eq!(sites.len(), 1, "no cap set: the whole run stays one span, {sites:?}");
        assert_eq!((sites[0].start, sites[0].end), (1, 10));
    }

    #[test]
    fn sizing_caps_split_units_the_same_way_collect_site_units_did() {
        let content: Rc<String> = Rc::new("a\nHIT1\nb\nHIT2\nc\nHIT3\n".to_string());
        let mut source = FixedSource(vec![SiteSourceFile { file: "f.ts".into(), content, candidates: (1..=6).collect() }]);
        // window 0 so each hit is its own 1-line span, cap of 1 site/unit
        // forces three separate units.
        let out = plan_site_units(&mut source, &|l| l.starts_with("HIT"), &params(0, 1, 1_000_000, None), &chars_est)
            .unwrap();
        assert_eq!(out.len(), 3, "{:?}", out.iter().map(|u| u.sites.len()).collect::<Vec<_>>());
    }
}
