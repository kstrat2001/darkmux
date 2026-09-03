//! Crawl planning (#1959) — turns a MATERIALIZED workspace (a resolved
//! `darkmux_crew::workspace_spec::Materialized`, checked out sources +
//! rules) into a deterministic `Plan` of work units with token estimates.
//! NO model dispatch happens here; this is the mechanical, free-to-compute
//! half of the crawler (prefilters, globs, the npm range check) that the
//! launcher's (`src/crawl_launch.rs`) dispatch loop consumes.

use darkmux_crew::workspace_spec::{glob, Materialized, MaterializedSource};
use darkmux_crew::rules::{Rule, RuleKind};
use crate::crawl::semver::{prerelease_tag, range_admits};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const PLAN_SCHEMA_VERSION: &str = "1.1";

/// `ceil(chars / 4)` — the ONE place this project-wide token-estimate
/// heuristic lives. Deliberately crude: real tokenization is model-specific
/// and not worth doing at plan time; good enough to size a unit against a
/// chunk budget.
pub const CHARS_PER_TOKEN: usize = 4;

pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// Files at or under this size are read; larger files are skipped (recorded
/// in `totals.skipped`) rather than silently truncated.
pub const MAX_FILE_BYTES: u64 = 512 * 1024;

/// `site`/`edge` grouping caps — see `crate::crawl::plan`'s module docs and
/// the packet-1 spec: "at most 40 sites or ~16,000 estimated tokens,
/// whichever first". These are the DEFAULTS `PlanParams::default()`
/// resolves to; an operator overrides either via `--param
/// max_sites_per_unit=`/`--param max_est_tokens_per_unit=` on `mission
/// launch crawl` (#2190) — see `src/crawl_launch.rs`.
pub const MAX_SITES_PER_UNIT: usize = 40;
pub const MAX_SITE_TOKENS_PER_UNIT: usize = 16_000;
/// Edge units cap at 80 sites (spilling into more units beyond that).
/// Not yet operator-settable (#2190 scoped to `site`-kind units only —
/// the mechanism observed live was a `site` unit's read batch; edge units
/// stalling the same way is untested).
pub const MAX_EDGE_SITES_PER_UNIT: usize = 80;

/// (#2190) Operator-controllable unit-sizing knobs for `site`-kind units —
/// how many sites (count and/or estimated tokens) `collect_site_units`
/// packs into one unit before starting a new one, "whichever binds
/// first" (unchanged semantics from the hard-coded constants this
/// replaces). `PlanParams::default()` resolves to exactly
/// `MAX_SITES_PER_UNIT`/`MAX_SITE_TOKENS_PER_UNIT`, so `plan()` (which
/// always uses the default) produces byte-identical plans to before this
/// existed — `plan_with_params` is the only way to override either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanParams {
    pub max_sites_per_unit: usize,
    pub max_est_tokens_per_unit: usize,
}

impl Default for PlanParams {
    fn default() -> Self {
        Self { max_sites_per_unit: MAX_SITES_PER_UNIT, max_est_tokens_per_unit: MAX_SITE_TOKENS_PER_UNIT }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /// Every 1-indexed hit line that merged into this span, in ascending
    /// order — `line` is always `hits[0]`. Overlapping/adjacent windows
    /// merge into one site (see `merge_windows`), which used to discard
    /// every hit but the first; a reviewer (or the model) reading `sites`
    /// alone had no way to tell "one hit, wide window" from "several hits,
    /// merged span" (#1959 finding 9). Additive field — every existing
    /// reader of `line`/`start`/`end` is unaffected.
    pub hits: Vec<usize>,
}

/// A read unit's file list entry: a whole file (serializes as a bare
/// string) or a line range within one oversized file (serializes as an
/// object). `untagged` gives exactly the two JSON shapes the packet-1
/// contract specifies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ReadFileEntry {
    Whole(String),
    Range { file: String, start: usize, end: usize },
}

/// One work unit. Internally tagged on `kind` — serializes as a flat
/// object, e.g. `{"kind":"site","id":"u-0001",...}`, matching the
/// packet-1 plan.json contract exactly.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Unit {
    Site {
        id: String,
        rule: String,
        source: String,
        sites: Vec<Site>,
        est_tokens: usize,
    },
    Read {
        id: String,
        rules: Vec<String>,
        source: String,
        files: Vec<ReadFileEntry>,
        est_tokens: usize,
    },
    Edge {
        id: String,
        rule: String,
        source: String,
        library: String,
        package: String,
        pinned: String,
        library_version: String,
        range_admits: bool,
        sites: Vec<Site>,
        /// The library source's tree root the surface below was resolved
        /// against — lets a downstream dispatch open the exact files
        /// `library_surface` names without re-deriving the path (#1959
        /// finding 4).
        library_tree: PathBuf,
        /// The library's package.json entry points (`main`, `module`,
        /// `types`/`typings`, every string reachable under `exports`) plus
        /// any top-level `CHANGELOG*` file, relative to `library_tree`,
        /// filtered to paths that actually exist and deduplicated. This —
        /// not the library's whole tree — is what the model is asked to
        /// read alongside the consumer's import sites; see the
        /// `stale-consumer` rule prose for why the `no_match`/`why_hint`
        /// wording is scoped to exactly these files.
        library_surface: Vec<String>,
        /// Set when `library_surface` came back empty — names why, so an
        /// operator reading the plan doesn't mistake "found nothing" for
        /// "didn't look".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        est_tokens: usize,
    },
}

impl Unit {
    pub fn id(&self) -> &str {
        match self {
            Unit::Site { id, .. } | Unit::Read { id, .. } | Unit::Edge { id, .. } => id,
        }
    }
    pub fn est_tokens(&self) -> usize {
        match self {
            Unit::Site { est_tokens, .. }
            | Unit::Read { est_tokens, .. }
            | Unit::Edge { est_tokens, .. } => *est_tokens,
        }
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PlanSource {
    pub id: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub tree: PathBuf,
    /// Total regular files found under this source's tree (before any
    /// rule's glob filtering) — the CLI table's `files_walked` column.
    pub files_walked: usize,
    /// Files the workspace spec's include/exclude left out of this source —
    /// out of scope, counted, never listed (#1959).
    #[serde(default)]
    pub out_of_scope: usize,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SkippedEntry {
    pub reason: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Every manifest edge this plan attempted, whether or not it produced a
/// unit — the ledger that shows an edge WAS checked even when its range
/// admits the library version (or the check was inconclusive).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct EdgeLedgerEntry {
    pub consumer: String,
    pub library: String,
    pub package: String,
    pub pinned: Option<String>,
    pub library_version: Option<String>,
    /// `Some(false)` -> a unit was produced. `Some(true)` -> the range
    /// admits the library version, no unit. `None` -> unsupported range
    /// syntax, an unresolved package/version, or an unsupported ecosystem —
    /// see `note`.
    pub range_admits: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct RuleTotal {
    pub units: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sites: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    /// True when at least one of this rule's `read` units is SHARED with
    /// another active read rule (the single combined read pass groups
    /// files by their exact ruleset — see `collect_read_units` — so two
    /// rules matching the same file share its unit, and thus its
    /// `est_tokens`). Surfaced so the CLI table's per-rule sums don't read
    /// as double-counting the plan's own `totals.est_tokens` (#1959
    /// finding 17). Always `false` for `site`/`edge` rules.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
    pub est_tokens: usize,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
pub struct Totals {
    pub units: usize,
    pub est_tokens: usize,
    pub by_rule: BTreeMap<String, RuleTotal>,
    pub skipped: Vec<SkippedEntry>,
    pub edges: Vec<EdgeLedgerEntry>,
}

/// `Deserialize` added in #1959 packet 2 (this struct and every type it
/// contains — `PlanSource`/`SkippedEntry`/`EdgeLedgerEntry`/`RuleTotal`/
/// `Totals`) for `darkmux mission launch crawl --param plan=<plan.json>`:
/// the launcher loads a plan a prior `darkmux crawl plan` run already
/// wrote, rather than re-planning, and verifies each source's `sha` still
/// matches the resolved tree before dispatching anything. No wire-format
/// change — every field this crate already writes reads back unchanged.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Plan {
    pub schema_version: String,
    pub workspace: String,
    pub planned_at: String,
    pub sources: Vec<PlanSource>,
    pub units: Vec<Unit>,
    pub totals: Totals,
    /// (#2298, plan schema 1.1) The rule ids this plan was cut for — one
    /// entry when a `crawl.plan` step planned a single rule, the whole set
    /// when the launcher planned them together. Lenient on read: a 1.0 plan
    /// has none recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// (#2298, plan schema 1.1) The sizing knobs the units were packed
    /// under, so a plan is self-describing for later comparison. Lenient on
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<PlanParamsRecord>,
}

/// (#2298) The serialized form of [`PlanParams`] a plan records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PlanParamsRecord {
    pub max_sites_per_unit: usize,
    pub max_est_tokens_per_unit: usize,
}

impl From<PlanParams> for PlanParamsRecord {
    fn from(p: PlanParams) -> Self {
        Self { max_sites_per_unit: p.max_sites_per_unit, max_est_tokens_per_unit: p.max_est_tokens_per_unit }
    }
}

/// Per-source file cache: the sorted relative-path listing (computed once)
/// plus a lazily-populated, read-once content cache shared across every
/// rule pass over this source, so a file matched by two rules (e.g.
/// `swallowed-error` + `doc-contradicts-code`, both `**/*.ts`) is only ever
/// read from disk once (cost-check requirement at ~200K-line scale).
struct SourceFiles {
    tree: PathBuf,
    all: Vec<String>,
    content: HashMap<String, Option<Rc<String>>>,
}

impl SourceFiles {
    /// (#1959) `all` is the ALREADY spec-filtered, already-sorted file
    /// list `workspace_spec::materialize` computed for this source
    /// (`Materialized.files[source_id]`) — this no longer walks the tree
    /// itself. A rule's own `applies_to`/`exclude` (`matching`, below)
    /// filters ON TOP of that: a file must pass both the spec's
    /// include/exclude AND the rule's own to be read.
    fn new(tree: &Path, all: Vec<String>) -> Self {
        Self {
            tree: tree.to_path_buf(),
            all,
            content: HashMap::new(),
        }
    }

    fn matching(&self, applies_to: &[String], exclude: &[String]) -> Vec<String> {
        self.all
            .iter()
            .filter(|f| glob::applies(applies_to, exclude, f))
            .cloned()
            .collect()
    }

    fn get(&mut self, rel: &str, skipped: &mut Vec<SkippedEntry>, source_id: &str) -> Option<Rc<String>> {
        if let Some(v) = self.content.get(rel) {
            return v.clone();
        }
        let full = self.tree.join(rel);
        let result = match fs::metadata(&full) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                skipped.push(SkippedEntry {
                    reason: format!("exceeds {MAX_FILE_BYTES} bytes ({} bytes)", meta.len()),
                    file: rel.to_string(),
                    source: Some(source_id.to_string()),
                });
                None
            }
            Ok(_) => match fs::read(&full) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => Some(Rc::new(s)),
                    Err(_) => {
                        skipped.push(SkippedEntry {
                            reason: "not valid UTF-8".to_string(),
                            file: rel.to_string(),
                            source: Some(source_id.to_string()),
                        });
                        None
                    }
                },
                Err(e) => {
                    skipped.push(SkippedEntry {
                        reason: format!("read error: {e}"),
                        file: rel.to_string(),
                        source: Some(source_id.to_string()),
                    });
                    None
                }
            },
            Err(e) => {
                skipped.push(SkippedEntry {
                    reason: format!("stat error: {e}"),
                    file: rel.to_string(),
                    source: Some(source_id.to_string()),
                });
                None
            }
        };
        self.content.insert(rel.to_string(), result.clone());
        result
    }
}

/// Merge 1-indexed hit line numbers into `(start, end, first_hit_line,
/// every_hit_line)` spans using `window` lines of context on each side,
/// clamped to `[1, total_lines]`. `hits` must be sorted ascending (callers
/// build it that way via a single forward line scan). Overlapping/adjacent
/// spans merge into one — "overlapping windows in the same file count
/// once" — and every hit line that merged into a span is carried forward
/// (`Site::hits`, #1959 finding 9) rather than only the first.
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

fn window_text(lines: &[&str], start: usize, end: usize) -> String {
    lines[start.saturating_sub(1)..end.min(lines.len())].join("\n")
}

fn next_unit_id(seq: &mut usize) -> String {
    let id = format!("u-{seq:04}");
    *seq += 1;
    id
}

/// Plan a materialized workspace. Deterministic for a fixed set of trees:
/// files are pre-walked once (by `workspace_spec::materialize`) and
/// sorted; unit ids are assigned sequentially in a fixed pass order
/// (every `site` rule, then the combined `read` pass per source, then
/// every `edge` rule).
///
/// (#1959) Takes a `Materialized` (spec + git resolution + the spec's own
/// include/exclude already applied) instead of a `CorpusManifest` +
/// separately-resolved sources — the crawl module's `manifest.rs`/
/// `sources.rs` retired in favor of the generic `workspace_spec` module.
/// A rule's own `applies_to`/`exclude` filters ON TOP of the spec's
/// include/exclude (`SourceFiles::matching`, unchanged) — a file must
/// pass both.
pub fn plan(materialized: &Materialized, rules: &[Rule]) -> Result<Plan> {
    plan_with_params(materialized, rules, PlanParams::default())
}

/// (#2190) Same as `plan`, with operator-overridable `site`-unit sizing.
/// `plan(m, r)` is exactly `plan_with_params(m, r, PlanParams::default())`
/// — the two are byte-identical for a fixed input, which is the
/// regression this split exists to make checkable.
pub fn plan_with_params(materialized: &Materialized, rules: &[Rule], params: PlanParams) -> Result<Plan> {
    // (#1959) Seed `skipped` from what `workspace_spec::materialize`
    // already found (symlinks, files excluded by the spec's own
    // include/exclude) — the file-size/read-error skips below are still
    // this module's own concern (materialize never reads file CONTENTS).
    let mut skipped: Vec<SkippedEntry> = materialized
        .skipped
        .iter()
        .map(|sf| SkippedEntry {
            reason: sf.reason.clone(),
            file: sf.relative_path.clone(),
            source: Some(sf.source_id.clone()),
        })
        .collect();
    let mut files_by_id: BTreeMap<String, SourceFiles> = BTreeMap::new();
    for s in &materialized.sources {
        let files = materialized.files.get(&s.id).cloned().unwrap_or_default();
        files_by_id.insert(s.id.clone(), SourceFiles::new(&s.tree, files));
    }

    let mut units: Vec<Unit> = Vec::new();
    let mut unit_seq: usize = 1;

    // --- site rules ---
    for rule in rules.iter().filter(|r| r.kind == RuleKind::Site) {
        for source in &materialized.sources {
            let files = files_by_id.get_mut(&source.id).expect("source resolved");
            units.extend(collect_site_units(rule, source, files, &mut skipped, &mut unit_seq, &params)?);
        }
    }

    // --- read pass (all read rules share one pass per source) ---
    let read_rules: Vec<&Rule> = rules.iter().filter(|r| r.kind == RuleKind::Read).collect();
    if !read_rules.is_empty() {
        for source in &materialized.sources {
            let files = files_by_id.get_mut(&source.id).expect("source resolved");
            units.extend(collect_read_units(&read_rules, source, files, &mut skipped, &mut unit_seq)?);
        }
    }

    // --- edge rules ---
    let mut edge_ledger: Vec<EdgeLedgerEntry> = Vec::new();
    let sources_by_id: BTreeMap<String, &MaterializedSource> =
        materialized.sources.iter().map(|s| (s.id.clone(), s)).collect();
    for rule in rules.iter().filter(|r| r.kind == RuleKind::Edge) {
        for edge in &materialized.edges {
            let (Some(_consumer), Some(_library)) =
                (sources_by_id.get(&edge.consumer), sources_by_id.get(&edge.library))
            else {
                // Manifest validation already guarantees both ids are
                // DECLARED sources; this guards the (should-be-impossible)
                // case a caller passed a `sources` slice narrower than the
                // manifest's own source list.
                continue;
            };
            let library = sources_by_id[&edge.library];

            let ecosystem = rule.edge.as_ref().map(|e| e.ecosystem.as_str()).unwrap_or("npm");
            if ecosystem != "npm" {
                edge_ledger.push(EdgeLedgerEntry {
                    consumer: edge.consumer.clone(),
                    library: edge.library.clone(),
                    package: edge.package.clone(),
                    pinned: None,
                    library_version: None,
                    range_admits: None,
                    note: Some(format!(
                        "ecosystem '{ecosystem}' not supported by rule '{}' — only the npm ecosystem is supported",
                        rule.id
                    )),
                });
                continue;
            }

            let consumer_pkg = read_package_json(&sources_by_id[&edge.consumer].tree);
            let library_pkg = read_package_json(&library.tree);
            let pinned = consumer_pkg.as_ref().ok().and_then(|v| pinned_range(v, &edge.package));
            let lib_version = library_pkg.as_ref().ok().and_then(package_version);

            let (admits, mut note) = match (&pinned, &lib_version) {
                (Some(p), Some(v)) => match range_admits(p, v) {
                    Some(b) => (Some(b), None),
                    None => (None, Some(format!("unsupported range syntax '{p}' — skipped"))),
                },
                (None, _) => (
                    None,
                    Some(format!(
                        "package '{}' not found in {}'s dependencies/devDependencies/peerDependencies/optionalDependencies",
                        edge.package, edge.consumer
                    )),
                ),
                (_, None) => (
                    None,
                    Some(format!("no `version` found in {}'s package.json", edge.library)),
                ),
            };
            // #1959 finding 11: a prerelease library version is treated as
            // its release-line core version for the range check (the same
            // normalization `semver::parse_semver` already applies) — note
            // it explicitly rather than letting that normalization happen
            // silently. Only overrides a still-empty note (an
            // unsupported-range/not-found note already explains itself).
            if note.is_none() {
                if let Some(v) = &lib_version {
                    if let Some(tag) = prerelease_tag(v) {
                        note = Some(format!("prerelease `{tag}` ignored for the range check"));
                    }
                }
            }

            edge_ledger.push(EdgeLedgerEntry {
                consumer: edge.consumer.clone(),
                library: edge.library.clone(),
                package: edge.package.clone(),
                pinned: pinned.clone(),
                library_version: lib_version.clone(),
                range_admits: admits,
                note,
            });

            if admits != Some(false) {
                continue;
            }

            // #1959 finding 4: the model reads the library's entry surface
            // (package.json's main/module/types/typings/exports + any
            // top-level CHANGELOG*), not its whole tree — resolved once per
            // edge and reused across every batch of this edge's units.
            // Reuses the library source's own file cache (`SourceFiles::get`)
            // so a surface file already read by another rule pass isn't
            // read from disk twice, same reasoning as every other read
            // path in this module.
            let (surface_rels, surface_note) = resolve_library_surface(&library.tree, &mut skipped, &edge.library);

            if surface_rels.is_empty() {
                // #1959 second-round CONSIDER 5: nothing resolved for the
                // model to compare the consumer's usage against — emitting
                // a unit here would just burn a dispatch confirming
                // "nothing to compare". The edge still reads as CHECKED
                // (the ledger entry pushed above), just with the note
                // extended to name why no unit followed. Appends onto
                // whatever note the entry already carries (e.g. the
                // prerelease-tag note above) rather than overwriting it.
                if let Some(entry) = edge_ledger.last_mut() {
                    const NO_UNIT: &str = "no unit emitted: nothing to compare against";
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(existing) = entry.note.take() {
                        parts.push(existing);
                    }
                    if let Some(sn) = &surface_note {
                        parts.push(sn.clone());
                    }
                    parts.push(NO_UNIT.to_string());
                    entry.note = Some(parts.join("; "));
                }
                continue;
            }

            let mut surface_tokens = 0usize;
            {
                let lib_files = files_by_id.get_mut(&edge.library).expect("library resolved");
                for rel in &surface_rels {
                    if let Some(text) = lib_files.get(rel, &mut skipped, &edge.library) {
                        surface_tokens += estimate_tokens(&text);
                    }
                }
            }

            let files = files_by_id.get_mut(&edge.consumer).expect("consumer resolved");
            let hits = collect_edge_import_sites(rule, &edge.package, files, &mut skipped, &edge.consumer)?;
            for batch in hits.chunks(MAX_EDGE_SITES_PER_UNIT) {
                let est_tokens: usize = batch.iter().map(|(_, tok)| *tok).sum::<usize>() + surface_tokens;
                units.push(Unit::Edge {
                    id: next_unit_id(&mut unit_seq),
                    rule: rule.id.clone(),
                    source: edge.consumer.clone(),
                    library: edge.library.clone(),
                    package: edge.package.clone(),
                    pinned: pinned.clone().unwrap_or_default(),
                    library_version: lib_version.clone().unwrap_or_default(),
                    range_admits: false,
                    sites: batch.iter().map(|(s, _)| s.clone()).collect(),
                    library_tree: library.tree.clone(),
                    library_surface: surface_rels.clone(),
                    note: surface_note.clone(),
                    est_tokens,
                });
            }
        }
    }

    let plan_sources: Vec<PlanSource> = materialized
        .sources
        .iter()
        .map(|s| PlanSource {
            id: s.id.clone(),
            sha: s.sha.clone(),
            git_ref: s.git_ref.clone(),
            tree: s.tree.clone(),
            files_walked: files_by_id.get(&s.id).map(|f| f.all.len()).unwrap_or(0),
            out_of_scope: materialized.out_of_scope.get(&s.id).copied().unwrap_or(0),
        })
        .collect();

    let totals = compute_totals(rules, &units, skipped, edge_ledger);

    Ok(Plan {
        schema_version: PLAN_SCHEMA_VERSION.to_string(),
        workspace: materialized.name.clone(),
        planned_at: darkmux_flow::ts_utc_now(),
        sources: plan_sources,
        units,
        totals,
        rules: rules.iter().map(|r| r.id.clone()).collect(),
        params: Some(params.into()),
    })
}

fn compute_totals(
    rules: &[Rule],
    units: &[Unit],
    skipped: Vec<SkippedEntry>,
    edges: Vec<EdgeLedgerEntry>,
) -> Totals {
    let mut by_rule: BTreeMap<String, RuleTotal> = BTreeMap::new();
    // #1959 finding 8: seed every resolved rule id at zero FIRST, so "this
    // rule ran and matched nothing" is a visible `units: 0` row rather than
    // an absent one indistinguishable from "this rule never ran".
    for rule in rules {
        by_rule.entry(rule.id.clone()).or_default();
    }
    for unit in units {
        match unit {
            Unit::Site { rule, sites, est_tokens, .. } => {
                let t = by_rule.entry(rule.clone()).or_default();
                t.units += 1;
                *t.sites.get_or_insert(0) += sites.len();
                t.est_tokens += est_tokens;
            }
            Unit::Read { rules, files, est_tokens, .. } => {
                // #1959 finding 17: a read unit shared by more than one
                // active read rule (the combined read pass groups files by
                // their exact ruleset) contributes its `est_tokens` to
                // EVERY rule sharing it — flag those rules so the CLI table
                // can say so, rather than let the per-rule sums silently
                // outrun `totals.est_tokens`.
                let shared = rules.len() > 1;
                for rid in rules {
                    let t = by_rule.entry(rid.clone()).or_default();
                    t.units += 1;
                    *t.files.get_or_insert(0) += files.len();
                    t.est_tokens += est_tokens;
                    t.shared = t.shared || shared;
                }
            }
            Unit::Edge { rule, sites, est_tokens, .. } => {
                let t = by_rule.entry(rule.clone()).or_default();
                t.units += 1;
                *t.sites.get_or_insert(0) += sites.len();
                t.est_tokens += est_tokens;
            }
        }
    }
    Totals {
        units: units.len(),
        est_tokens: units.iter().map(|u| u.est_tokens()).sum(),
        by_rule,
        skipped,
        edges,
    }
}

fn collect_site_units(
    rule: &Rule,
    source: &MaterializedSource,
    files: &mut SourceFiles,
    skipped: &mut Vec<SkippedEntry>,
    unit_seq: &mut usize,
    params: &PlanParams,
) -> Result<Vec<Unit>> {
    if rule.prefilter.is_empty() {
        return Ok(Vec::new());
    }
    let regexes: Vec<Regex> = rule
        .prefilter
        .iter()
        .map(|p| Regex::new(p).with_context(|| format!("compiling prefilter for rule '{}': {p}", rule.id)))
        .collect::<Result<_>>()?;
    let window = rule.window_or_default();
    let matching = files.matching(&rule.applies_to, &rule.exclude);

    let mut batch: Vec<(Site, usize)> = Vec::new();
    for rel in matching {
        let Some(text) = files.get(&rel, skipped, &source.id) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let mut hits = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if regexes.iter().any(|re| re.is_match(line)) {
                hits.push(i + 1);
            }
        }
        if hits.is_empty() {
            continue;
        }
        for (s, e, first, hit_list) in merge_windows(&hits, window, total) {
            let tokens = estimate_tokens(&window_text(&lines, s, e));
            batch.push((Site { file: rel.clone(), line: first, start: s, end: e, hits: hit_list }, tokens));
        }
    }

    let mut out = Vec::new();
    let mut cur_sites: Vec<Site> = Vec::new();
    let mut cur_tokens = 0usize;
    for (site, tok) in batch {
        let would_exceed_count = cur_sites.len() + 1 > params.max_sites_per_unit;
        let would_exceed_tokens = !cur_sites.is_empty() && cur_tokens + tok > params.max_est_tokens_per_unit;
        if !cur_sites.is_empty() && (would_exceed_count || would_exceed_tokens) {
            out.push(Unit::Site {
                id: next_unit_id(unit_seq),
                rule: rule.id.clone(),
                source: source.id.clone(),
                sites: std::mem::take(&mut cur_sites),
                est_tokens: cur_tokens,
            });
            cur_tokens = 0;
        }
        cur_sites.push(site);
        cur_tokens += tok;
    }
    if !cur_sites.is_empty() {
        out.push(Unit::Site {
            id: next_unit_id(unit_seq),
            rule: rule.id.clone(),
            source: source.id.clone(),
            sites: cur_sites,
            est_tokens: cur_tokens,
        });
    }
    Ok(out)
}

fn collect_read_units(
    read_rules: &[&Rule],
    source: &MaterializedSource,
    files: &mut SourceFiles,
    skipped: &mut Vec<SkippedEntry>,
    unit_seq: &mut usize,
) -> Result<Vec<Unit>> {
    // file -> sorted rule ids that apply. Partitioning by the EXACT ruleset
    // (rather than any-match) keeps a read unit's `rules` field accurate
    // for every file it contains: two files matched by different subsets
    // of the active read rules never end up sharing one unit's `rules`
    // array, which would misattribute a rule to a file it doesn't apply to.
    let mut file_ruleset: HashMap<String, Vec<String>> = HashMap::new();
    for rel in &files.all {
        let mut rs: Vec<String> = read_rules
            .iter()
            .filter(|r| glob::applies(&r.applies_to, &r.exclude, rel))
            .map(|r| r.id.clone())
            .collect();
        if rs.is_empty() {
            continue;
        }
        rs.sort();
        file_ruleset.insert(rel.clone(), rs);
    }

    let mut partitions: BTreeMap<Vec<String>, Vec<String>> = BTreeMap::new();
    for rel in &files.all {
        if let Some(rs) = file_ruleset.get(rel) {
            partitions.entry(rs.clone()).or_default().push(rel.clone());
        }
    }

    let mut out = Vec::new();
    for (ruleset, rel_files) in &partitions {
        let chunk_tokens = ruleset
            .iter()
            .filter_map(|rid| read_rules.iter().find(|r| &r.id == rid))
            .map(|r| r.chunk_tokens_or_default())
            .min()
            .unwrap_or(darkmux_crew::rules::DEFAULT_READ_CHUNK_TOKENS);

        let mut cur_files: Vec<ReadFileEntry> = Vec::new();
        let mut cur_tokens = 0usize;

        for rel in rel_files {
            let Some(text) = files.get(rel, skipped, &source.id) else { continue };
            let file_tokens = estimate_tokens(&text);

            if file_tokens > chunk_tokens {
                flush_read_unit(&mut cur_files, &mut cur_tokens, &mut out, unit_seq, ruleset, &source.id);
                for (s, e, range_text) in split_into_ranges(&text, chunk_tokens) {
                    out.push(Unit::Read {
                        id: next_unit_id(unit_seq),
                        rules: ruleset.clone(),
                        source: source.id.clone(),
                        files: vec![ReadFileEntry::Range { file: rel.clone(), start: s, end: e }],
                        est_tokens: estimate_tokens(&range_text),
                    });
                }
                continue;
            }

            if !cur_files.is_empty() && cur_tokens + file_tokens > chunk_tokens {
                flush_read_unit(&mut cur_files, &mut cur_tokens, &mut out, unit_seq, ruleset, &source.id);
            }
            cur_files.push(ReadFileEntry::Whole(rel.clone()));
            cur_tokens += file_tokens;
        }
        flush_read_unit(&mut cur_files, &mut cur_tokens, &mut out, unit_seq, ruleset, &source.id);
    }

    Ok(out)
}

fn flush_read_unit(
    cur_files: &mut Vec<ReadFileEntry>,
    cur_tokens: &mut usize,
    out: &mut Vec<Unit>,
    unit_seq: &mut usize,
    ruleset: &[String],
    source_id: &str,
) {
    if cur_files.is_empty() {
        return;
    }
    out.push(Unit::Read {
        id: next_unit_id(unit_seq),
        rules: ruleset.to_vec(),
        source: source_id.to_string(),
        files: std::mem::take(cur_files),
        est_tokens: *cur_tokens,
    });
    *cur_tokens = 0;
}

/// Split an oversized file's text into 1-indexed inclusive `(start, end,
/// text)` line ranges, each bounded by `chunk_tokens` (never slicing a
/// line in half). Always makes forward progress even when a single line
/// alone exceeds the budget (that line becomes its own range).
fn split_into_ranges(text: &str, chunk_tokens: usize) -> Vec<(usize, usize, String)> {
    let budget_chars = chunk_tokens.saturating_mul(CHARS_PER_TOKEN).max(1);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0usize; // 0-indexed
    let mut idx = 0usize;
    let mut char_count = 0usize;
    while idx < lines.len() {
        let line_chars = lines[idx].chars().count() + 1; // +1 for the line break
        if char_count > 0 && char_count + line_chars > budget_chars {
            out.push((start + 1, idx, lines[start..idx].join("\n")));
            start = idx;
            char_count = 0;
        }
        char_count += line_chars;
        idx += 1;
    }
    if start < lines.len() {
        out.push((start + 1, lines.len(), lines[start..].join("\n")));
    }
    out
}

fn collect_edge_import_sites(
    rule: &Rule,
    package: &str,
    files: &mut SourceFiles,
    skipped: &mut Vec<SkippedEntry>,
    source_id: &str,
) -> Result<Vec<(Site, usize)>> {
    let esc = regex::escape(package);
    // Every import shape that names a bare package specifier, each
    // requiring the same terminator (`/` for a subpath, or the closing
    // quote) right after the package name so `@org/lib-extra` never
    // matches a site rule scoped to `@org/lib` (#1959 finding 3 — the
    // `require` branch used to lack this terminator entirely, so
    // `require('@org/lib-extra')` matched `@org/lib` as a bare prefix):
    //   - `from "<pkg>"` / `from "<pkg>/subpath"` (static ESM, covers
    //     `import type { X } from "<pkg>"` too — `from` appears regardless
    //     of what precedes it on the line)
    //   - `import("<pkg>")` (dynamic import)
    //   - `import "<pkg>"` (side-effect import, no braces/`from`)
    //   - `require("<pkg>")` / `require("<pkg>/subpath")`
    let pattern = format!(
        r#"from\s*['"]{esc}(?:/|['"])|import\(\s*['"]{esc}(?:/|['"])|import\s*['"]{esc}(?:/|['"])|require\(\s*['"]{esc}(?:/|['"])"#
    );
    let re = Regex::new(&pattern)
        .with_context(|| format!("compiling import-site regex for package '{package}'"))?;
    let window = rule.window_or_default();
    let matching = files.matching(&rule.applies_to, &rule.exclude);

    let mut out = Vec::new();
    for rel in matching {
        let Some(text) = files.get(&rel, skipped, source_id) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let mut hits = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                hits.push(i + 1);
            }
        }
        if hits.is_empty() {
            continue;
        }
        for (s, e, first, hit_list) in merge_windows(&hits, window, total) {
            let tokens = estimate_tokens(&window_text(&lines, s, e));
            out.push((Site { file: rel.clone(), line: first, start: s, end: e, hits: hit_list }, tokens));
        }
    }
    Ok(out)
}

fn read_package_json(tree: &Path) -> Result<serde_json::Value> {
    let path = tree.join("package.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The four dependency-map fields npm resolves a specifier's pinned range
/// from, checked in this order (#1959 finding 10 — `peerDependencies` and
/// `optionalDependencies` used to be silently unchecked, so a peer-only
/// pin looked identical to "not pinned at all"). The edge ledger's
/// not-found note names all four so an operator reading it knows exactly
/// where the planner looked, not just where it happened to look first.
const DEPENDENCY_FIELDS: &[&str] =
    &["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"];

fn pinned_range(pkg: &serde_json::Value, package: &str) -> Option<String> {
    DEPENDENCY_FIELDS.iter().find_map(|field| {
        pkg.get(field)
            .and_then(|d| d.get(package))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
}

fn package_version(pkg: &serde_json::Value) -> Option<String> {
    pkg.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// The package.json fields naming a whole-file entry point — checked for
/// `main`, `module`, `types`, `typings` (#1959 finding 4). `exports` is
/// handled separately below since it can nest arbitrarily deep.
const ENTRY_POINT_FIELDS: &[&str] = &["main", "module", "types", "typings"];

/// Resolve a library's package.json-declared entry points (`main`,
/// `module`, `types`/`typings`, and every string value reachable under
/// `exports`) plus any top-level `CHANGELOG*` file, to paths RELATIVE to
/// `tree` that actually exist there — the "library surface" a
/// `stale-consumer` edge unit hands the model instead of the library's
/// entire tree (#1959 finding 4). Deduplicated, in the order resolved.
/// Returns a `note` explaining why when nothing resolved, so an empty
/// surface reads as "looked, found nothing" rather than "didn't look".
fn resolve_library_surface(
    tree: &Path,
    skipped: &mut Vec<SkippedEntry>,
    library_id: &str,
) -> (Vec<String>, Option<String>) {
    let mut rels: Vec<String> = Vec::new();

    if let Ok(pkg) = read_package_json(tree) {
        for field in ENTRY_POINT_FIELDS {
            if let Some(v) = pkg.get(field).and_then(|v| v.as_str()) {
                push_surface_path_if_exists(tree, v, &mut rels, skipped, library_id);
            }
        }
        if let Some(exports) = pkg.get("exports") {
            let mut export_strings = Vec::new();
            collect_export_strings(exports, &mut export_strings);
            for v in export_strings {
                push_surface_path_if_exists(tree, &v, &mut rels, skipped, library_id);
            }
        }
    }

    if let Ok(entries) = fs::read_dir(tree) {
        let mut changelogs: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let is_changelog = e
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.to_ascii_uppercase().starts_with("CHANGELOG"));
                (is_changelog && e.path().is_file()).then(|| e.file_name().to_string_lossy().into_owned())
            })
            .collect();
        changelogs.sort();
        for c in changelogs {
            if !rels.contains(&c) {
                rels.push(c);
            }
        }
    }

    if rels.is_empty() {
        let note = "no package.json entry point (main/module/types/typings/exports) or \
                     top-level CHANGELOG file resolved to an existing file in the library tree"
            .to_string();
        (rels, Some(note))
    } else {
        (rels, None)
    }
}

/// Push `rel` (a package.json-declared path, possibly `./`-prefixed) onto
/// `out` — deduplicated — if it resolves to an existing FILE under `tree`.
/// A declared entry point that doesn't exist (a stale field, a build
/// output that hasn't run) is silently skipped, same leniency as every
/// other "does this resolve" check in this module.
///
/// `rel` is THIRD-PARTY data — a library's own `package.json` — so it gets
/// the same treatment as any other untrusted path input: absolute paths
/// and `..` components are rejected outright (string-level, before ever
/// touching the filesystem), the resolved candidate's canonical form must
/// be a DESCENDANT of `tree`'s canonical form (subpaths are fine; this
/// guards an intermediate directory component that's itself a symlink
/// escaping the tree), and the final component is never followed if it's
/// a symlink (`symlink_metadata`, not `metadata`/`is_file` — a symlink
/// whose target happens to sit inside the tree is still rejected, same
/// "never follow a symlink" posture as the main file walk). A rejected
/// candidate is recorded in `skipped` with the raw candidate (not the
/// resolved path) so the refusal is visible rather than a silent
/// omission indistinguishable from "field absent" (#1959 second-round
/// finding: path traversal via `library_surface`).
fn push_surface_path_if_exists(
    tree: &Path,
    rel: &str,
    out: &mut Vec<String>,
    skipped: &mut Vec<SkippedEntry>,
    library_id: &str,
) {
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    if rel.is_empty() || out.iter().any(|r| r == rel) {
        return;
    }

    let reject = |skipped: &mut Vec<SkippedEntry>| {
        skipped.push(SkippedEntry {
            reason: "library_surface_outside_tree".to_string(),
            file: rel.to_string(),
            source: Some(library_id.to_string()),
        });
    };

    let path = Path::new(rel);
    if path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        reject(skipped);
        return;
    }

    let candidate = tree.join(rel);
    // Never follow a symlink for the final component, inside the tree or
    // not — `symlink_metadata` reports the link's own type without
    // resolving it, so a symlinked entry point is caught here regardless
    // of where it points.
    let meta = match fs::symlink_metadata(&candidate) {
        Ok(m) => m,
        Err(_) => return, // doesn't exist — same leniency as before, no skip entry
    };
    if !meta.file_type().is_file() {
        reject(skipped);
        return;
    }

    // Defense in depth: an intermediate directory component could itself
    // be a symlink escaping `tree` even though the final component is a
    // regular file. Canonicalize both sides and require descendant
    // containment (a subpath is fine; this is not the direct-child check
    // `sources::assert_direct_child` uses).
    let (Ok(canon_tree), Ok(canon_candidate)) = (tree.canonicalize(), candidate.canonicalize()) else {
        reject(skipped);
        return;
    };
    if !canon_candidate.starts_with(&canon_tree) {
        reject(skipped);
        return;
    }

    out.push(rel.to_string());
}

/// Recursively collect every string value reachable under a package.json
/// `exports` field — which can be a bare string, or arbitrarily nested
/// objects/arrays keyed by condition (`"import"`/`"require"`/`"types"`/a
/// subpath) or target platform (#1959 finding 4).
fn collect_export_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(map) => {
            for vv in map.values() {
                collect_export_strings(vv, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for vv in arr {
                collect_export_strings(vv, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    // (#2206) Every embedded rule's prefilter must compile as a `regex`
    // pattern — `plan_site_units` compiles them lazily per crawl, so a bad
    // pattern in a builtin rule would otherwise surface only at launch. And
    // the unnamed-predicate prefilter must actually HIT the shapes #2206
    // names, and be honest about the shapes it lets through: null guards and
    // default chains pass the prefilter by design — the rule's `no_match`
    // prose excludes them at judgment, where the model can read intent.
    #[test]
    fn every_embedded_prefilter_compiles_and_unnamed_predicate_hits_its_shapes() {
        let (rules, warnings) = darkmux_crew::rules::load_all(None);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut compiled = 0;
        for rule in rules.values() {
            for p in &rule.prefilter {
                Regex::new(p).unwrap_or_else(|e| panic!("rule '{}' prefilter {p:?}: {e}", rule.id));
                compiled += 1;
            }
        }
        assert!(compiled >= 4, "expected the builtin prefilters to be present, compiled {compiled}");

        let up = &rules["unnamed-predicate"];
        let res: Vec<Regex> = up.prefilter.iter().map(|p| Regex::new(p).unwrap()).collect();
        assert_eq!(res.len(), 3);
        let hits = |line: &str| res.iter().any(|r| r.is_match(line));
        // Each prefilter gets a positive the OTHER TWO cannot match, so
        // replacing any one of them with a never-matching pattern fails here
        // (review finding on #2266: the first version's positives all carried
        // two operators and were satisfied by prefilter 0 alone).
        // prefilter 0: two boolean operators on one line
        let two_ops = r#"if (status === "active" && daysOverdue < 30 && balance > 0) {"#;
        assert!(res[0].is_match(two_ops) && !res[1].is_match(two_ops) && !res[2].is_match(two_ops), "p0 only");
        // prefilter 1: a long `if (` — 80+ chars before the closing paren, ONE operator
        let long_if = "if (subscription.billingModel.effectiveRate.amountInMinorUnits === expectedRate.amountInMinorUnits) {";
        assert!(res[1].is_match(long_if) && !res[0].is_match(long_if) && !res[2].is_match(long_if), "p1 only");
        // prefilter 2: a long ternary test — 50+ chars between `?` and `:`, no `&&`/`||`
        let long_tern = "const label = isEligible ? formatEligibilityLabel(subscription, locale, options) : fallback;";
        assert!(res[2].is_match(long_tern) && !res[0].is_match(long_tern) && !res[1].is_match(long_tern), "p2 only");
        // the rule's other positive shape
        assert!(hits("const cls = a && (b || c) ? x : y;"), "mixed && and ||");
        // let through on purpose — excluded by `no_match` at judgment, not here
        assert!(hits("if (x && x.y && x.y.z) {"), "a null guard passes the prefilter");
        assert!(hits(r#"const d = a || b || "default";"#), "a default chain passes the prefilter");
        // genuinely not a candidate
        assert!(!hits("if (a && b) return;"), "two operands, short: not a candidate");
        assert!(!hits("const n = items.length;"), "no operators at all");
        assert!(!hits("const t = ok ? a : b;"), "a short ternary is not a candidate");
    }

    use super::*;
    use darkmux_crew::rules::EdgeRuleConfig;
    use darkmux_crew::workspace_spec::EdgeSpec;
    use tempfile::TempDir;

    fn site_rule() -> Rule {
        Rule {
            id: "swallowed-error".to_string(),
            kind: RuleKind::Site,
            title: None,
            applies_to: vec!["**/*.ts".to_string()],
            exclude: vec!["**/node_modules/**".to_string()],
            prefilter: vec![r"\bcatch\s*(\(|\{)".to_string(), r"\.catch\s*\(".to_string()],
            window: Some(5),
            chunk_tokens: None,
            edge: None,
            matches: None,
            no_match: None,
            evidence: None,
            why_hint: None,
            extras: Default::default(),
        }
    }

    fn read_rule() -> Rule {
        Rule {
            id: "doc-contradicts-code".to_string(),
            kind: RuleKind::Read,
            title: None,
            applies_to: vec!["**/*.ts".to_string(), "**/README.md".to_string()],
            exclude: vec![],
            prefilter: vec![],
            window: None,
            chunk_tokens: Some(50),
            edge: None,
            matches: None,
            no_match: None,
            evidence: None,
            why_hint: None,
            extras: Default::default(),
        }
    }

    fn edge_rule() -> Rule {
        Rule {
            id: "stale-consumer".to_string(),
            kind: RuleKind::Edge,
            title: None,
            applies_to: vec!["**/*.ts".to_string()],
            exclude: vec![],
            prefilter: vec![],
            window: Some(5),
            chunk_tokens: None,
            edge: Some(EdgeRuleConfig { ecosystem: "npm".to_string(), extras: Default::default() }),
            matches: None,
            no_match: None,
            evidence: None,
            why_hint: None,
            extras: Default::default(),
        }
    }

    fn resolved(id: &str, tree: &Path) -> MaterializedSource {
        MaterializedSource {
            id: id.to_string(),
            sha: "deadbeef".to_string(),
            git_ref: "main".to_string(),
            tree: tree.to_path_buf(),
        }
    }

    /// (#1959) Test-only recursive walk mirroring
    /// `workspace_spec::materialize`'s own symlink-skip behavior (same
    /// reason string, `"symlink — never followed"`) so a test checking
    /// `Plan.totals.skipped` sees the same shape production `materialize()`
    /// produces. Spec-level include/exclude filtering is out of scope for
    /// these rule-level tests — every file passes (matching the
    /// pre-#1959 shape: `CorpusManifest` never had include/exclude
    /// either).
    fn walk_all(tree: &Path, source_id: &str) -> (Vec<String>, Vec<darkmux_crew::workspace_spec::SkippedFile>) {
        fn go(
            root: &Path,
            dir: &Path,
            out: &mut Vec<String>,
            skipped: &mut Vec<darkmux_crew::workspace_spec::SkippedFile>,
            source_id: &str,
        ) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    continue;
                }
                let ft = entry.file_type().unwrap();
                let rel = || {
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                };
                if ft.is_symlink() {
                    skipped.push(darkmux_crew::workspace_spec::SkippedFile {
                        source_id: source_id.to_string(),
                        relative_path: rel(),
                        reason: "symlink — never followed".to_string(),
                    });
                    continue;
                }
                if ft.is_dir() {
                    go(root, &path, out, skipped, source_id);
                } else if ft.is_file() {
                    out.push(rel());
                }
            }
        }
        let mut out = Vec::new();
        let mut skipped = Vec::new();
        go(tree, tree, &mut out, &mut skipped, source_id);
        out.sort();
        (out, skipped)
    }

    /// (#1959) Build a `Materialized` fixture directly from already
    /// locally-written trees (bypassing real git resolution — the trees
    /// are plain tempdirs in these tests) — the direct twin of
    /// `crawl::sources`'s retired `manifest_with_edge` helper, now over
    /// `workspace_spec` types.
    fn materialized_for(sources: Vec<MaterializedSource>, edges: Vec<EdgeSpec>) -> Materialized {
        let mut files = BTreeMap::new();
        let mut skipped = Vec::new();
        for s in &sources {
            let (all, mut sk) = walk_all(&s.tree, &s.id);
            files.insert(s.id.clone(), all);
            skipped.append(&mut sk);
        }
        Materialized { name: "t".to_string(), root: PathBuf::new(), sources, edges, files, skipped, out_of_scope: Default::default() }
    }

    fn edge_spec(app: &str, lib: &str, package: &str) -> Vec<EdgeSpec> {
        vec![EdgeSpec {
            consumer: app.to_string(),
            library: lib.to_string(),
            package: package.to_string(),
            extras: Default::default(),
        }]
    }

    #[test]
    fn site_rule_produces_one_site_per_non_overlapping_catch() {
        let dir = TempDir::new().unwrap();
        // Two catch blocks far enough apart (window=5) that their windows
        // don't overlap.
        let mut lines = vec!["function a() {".to_string(), "  try {}".to_string(), "  catch (e) {}".to_string(), "}".to_string()];
        for _ in 0..20 {
            lines.push(String::new());
        }
        lines.push("function b() {".to_string());
        lines.push("  p().catch(e => {})".to_string());
        lines.push("}".to_string());
        fs::write(dir.path().join("x.ts"), lines.join("\n")).unwrap();

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &PlanParams::default()).unwrap();
        assert_eq!(units.len(), 1, "{units:?}");
        let Unit::Site { sites, .. } = &units[0] else { panic!("expected site unit") };
        assert_eq!(sites.len(), 2, "{sites:?}");
    }

    #[test]
    fn site_grouping_caps_at_40_sites_per_unit() {
        let dir = TempDir::new().unwrap();
        // 41 catch blocks, spaced far enough apart (window=5) to avoid merging.
        let mut lines = Vec::new();
        for _ in 0..41 {
            lines.push("catch (e) {}".to_string());
            for _ in 0..12 {
                lines.push(String::new());
            }
        }
        fs::write(dir.path().join("x.ts"), lines.join("\n")).unwrap();

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &PlanParams::default()).unwrap();
        assert_eq!(units.len(), 2, "expected a 40 + 1 split: {units:?}");
        let Unit::Site { sites: s0, .. } = &units[0] else { panic!() };
        let Unit::Site { sites: s1, .. } = &units[1] else { panic!() };
        assert_eq!(s0.len(), 40);
        assert_eq!(s1.len(), 1);
    }

    /// (#2190) Write `n` well-separated `catch` sites into one file — the
    /// shared fixture for every `PlanParams`-sizing test below.
    fn write_n_catch_sites(dir: &Path, n: usize) {
        let mut lines = Vec::new();
        for _ in 0..n {
            lines.push("catch (e) {}".to_string());
            for _ in 0..12 {
                lines.push(String::new());
            }
        }
        fs::write(dir.join("x.ts"), lines.join("\n")).unwrap();
    }

    #[test]
    fn site_grouping_respects_operator_max_sites_per_unit() {
        // (#2190) 25 sites at a cap of 6 must split ceil(25/6) = 5 units:
        // sizes 6,6,6,6,1 — proving the operator-supplied cap actually
        // binds, not just the built-in 40.
        let dir = TempDir::new().unwrap();
        write_n_catch_sites(dir.path(), 25);

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let params = PlanParams { max_sites_per_unit: 6, max_est_tokens_per_unit: MAX_SITE_TOKENS_PER_UNIT };
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &params).unwrap();
        assert_eq!(units.len(), 5, "expected ceil(25/6)=5 units: {units:?}");
        let sizes: Vec<usize> = units
            .iter()
            .map(|u| match u {
                Unit::Site { sites, .. } => sites.len(),
                _ => panic!("expected site unit"),
            })
            .collect();
        assert_eq!(sizes, vec![6, 6, 6, 6, 1]);
    }

    #[test]
    fn site_grouping_cap_larger_than_site_count_yields_one_unit() {
        // (#2190) A cap that exceeds the actual site count must not split
        // at all — one unit holding everything.
        let dir = TempDir::new().unwrap();
        write_n_catch_sites(dir.path(), 5);

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let params = PlanParams { max_sites_per_unit: 1000, max_est_tokens_per_unit: MAX_SITE_TOKENS_PER_UNIT };
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &params).unwrap();
        assert_eq!(units.len(), 1, "{units:?}");
        let Unit::Site { sites, .. } = &units[0] else { panic!("expected site unit") };
        assert_eq!(sites.len(), 5);
    }

    #[test]
    fn plan_with_default_params_matches_plan_byte_identical() {
        // (#2190) The key regression: `plan()` (no override) must produce
        // EXACTLY what `plan_with_params(.., PlanParams::default())`
        // produces — an operator who never passes `--param
        // max_sites_per_unit=`/`--param max_est_tokens_per_unit=` sees no
        // behavior change from before this knob existed.
        let dir = TempDir::new().unwrap();
        write_n_catch_sites(dir.path(), 45); // > 40, so the default cap actually bites
        fs::write(dir.path().join("README.md"), "docs\n").unwrap();

        let sources = vec![resolved("app", dir.path())];
        let materialized = materialized_for(sources, Vec::new());
        let rules = vec![site_rule(), read_rule()];

        let out1 = plan(&materialized, &rules).unwrap();
        let out2 = plan_with_params(&materialized, &rules, PlanParams::default()).unwrap();
        let units1 = serde_json::to_value(&out1.units).unwrap();
        let units2 = serde_json::to_value(&out2.units).unwrap();
        assert_eq!(units1, units2);
        assert_eq!(serde_json::to_value(&out1.totals).unwrap(), serde_json::to_value(&out2.totals).unwrap());
    }

    #[test]
    fn read_rule_groups_whole_files_and_splits_oversized_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.ts"), "x".repeat(20)).unwrap(); // 20 chars ~5 tokens
        fs::write(dir.path().join("README.md"), "y".repeat(400)).unwrap(); // 400 chars ~100 tokens > chunk_tokens=50

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let rule = read_rule();
        let read_rules: Vec<&Rule> = vec![&rule];
        let units = collect_read_units(&read_rules, &source, &mut files, &mut skipped, &mut seq).unwrap();

        // a.ts fits in one whole-file unit; README.md (100 tokens > 50 chunk_tokens)
        // becomes its own split unit(s).
        let whole: Vec<&Unit> = units
            .iter()
            .filter(|u| matches!(u, Unit::Read { files, .. } if files.iter().any(|f| matches!(f, ReadFileEntry::Whole(f) if f == "a.ts"))))
            .collect();
        assert_eq!(whole.len(), 1, "{units:?}");

        let split: Vec<&Unit> = units
            .iter()
            .filter(|u| matches!(u, Unit::Read { files, .. } if files.iter().any(|f| matches!(f, ReadFileEntry::Range { file, .. } if file == "README.md"))))
            .collect();
        assert!(!split.is_empty(), "{units:?}");
    }

    #[test]
    fn stale_consumer_produces_unit_when_range_excludes_version() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^5.5.0"}}).to_string(),
        )
        .unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1", "main": "index.js"}).to_string(),
        )
        .unwrap();
        fs::write(lib_dir.join("index.js"), "module.exports = {};\n").unwrap();
        fs::write(
            app_dir.join("uses-lib.ts"),
            "import { thing } from '@org/lib';\nthing();\n",
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];

        let out = plan(&materialized, &rules).unwrap();
        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert_eq!(edge_units.len(), 1, "{:?}", out.units);
        let Unit::Edge { range_admits, sites, pinned, library_version, .. } = edge_units[0] else { panic!() };
        assert!(!range_admits);
        assert_eq!(sites.len(), 1);
        assert_eq!(pinned, "^5.5.0");
        assert_eq!(library_version, "8.1.1");

        assert_eq!(out.totals.edges.len(), 1);
        assert_eq!(out.totals.edges[0].range_admits, Some(false));
    }

    #[test]
    fn stale_consumer_produces_no_unit_when_range_admits_version() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^8.0.0"}}).to_string(),
        )
        .unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1"}).to_string(),
        )
        .unwrap();
        fs::write(app_dir.join("uses-lib.ts"), "import { thing } from '@org/lib';\n").unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];

        let out = plan(&materialized, &rules).unwrap();
        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert!(edge_units.is_empty(), "{:?}", out.units);
        assert_eq!(out.totals.edges.len(), 1);
        assert_eq!(out.totals.edges[0].range_admits, Some(true));
    }

    #[test]
    fn skipped_records_oversized_and_non_utf8_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("big.ts"), vec![b'a'; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        fs::write(dir.path().join("bad.ts"), [0xFFu8, 0xFE, 0x00]).unwrap();

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let _ = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &PlanParams::default()).unwrap();
        let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
        assert!(reasons.iter().any(|r| r.contains("exceeds")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("UTF-8")), "{reasons:?}");
    }

    // ── #1959 finding 9: merge_windows carries every hit line, not just the first ──

    #[test]
    fn merge_windows_carries_every_hit_line_in_a_merged_span() {
        let spans = merge_windows(&[1, 62], 30, 1000);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (start, end, first, hits) = &spans[0];
        assert_eq!(*first, 1);
        assert_eq!(hits, &vec![1, 62]);
        assert_eq!(*start, 1);
        assert_eq!(*end, 92);
    }

    // ── #1959 finding 8: by_rule seeds every resolved rule at zero ──

    #[test]
    fn by_rule_seeds_a_rule_that_matched_nothing_at_zero() {
        let dir = TempDir::new().unwrap();
        // No catch/`.catch(`/`void fn(` anywhere — the site rule's
        // prefilter matches nothing in this tree.
        fs::write(dir.path().join("x.ts"), "const x = 1;\n").unwrap();

        let sources = vec![resolved("app", dir.path())];
        let materialized = materialized_for(sources, Vec::new());
        let rules = vec![site_rule()];

        let out = plan(&materialized, &rules).unwrap();
        assert_eq!(out.totals.units, 0);
        let t = out.totals.by_rule.get("swallowed-error").expect("rule seeded even with zero matches");
        assert_eq!(t.units, 0);
        assert_eq!(t.est_tokens, 0);
    }

    // ── #1959 finding 3: edge import regex covers require/dynamic import/side-effect import ──

    #[test]
    fn edge_import_regex_covers_every_import_shape_with_a_terminator() {
        let dir = TempDir::new().unwrap();
        let lines = [
            "const x = require('@org/lib-extra');", // must NOT match @org/lib
            "const y = require('@org/lib');",
            "const z = import('@org/lib');",
            "import '@org/lib';",
            "import type { X } from '@org/lib';",
            "import { Y } from '@org/lib/sub';",
            "import { Z } from '@org/lib-extra';", // must NOT match @org/lib
        ];
        fs::write(dir.path().join("x.ts"), lines.join("\n")).unwrap();

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let rule = edge_rule();
        let hits = collect_edge_import_sites(&rule, "@org/lib", &mut files, &mut skipped, "app").unwrap();
        let all_hit_lines: Vec<usize> = hits.iter().flat_map(|(s, _)| s.hits.clone()).collect();

        // 1-indexed line numbers: 2 (require), 3 (dynamic import), 4
        // (side-effect import), 5 (import type ... from), 6 (from .../sub).
        // Lines 1 and 7 (the `-extra` false positives) must be absent.
        assert!(all_hit_lines.contains(&2), "{all_hit_lines:?}");
        assert!(all_hit_lines.contains(&3), "{all_hit_lines:?}");
        assert!(all_hit_lines.contains(&4), "{all_hit_lines:?}");
        assert!(all_hit_lines.contains(&5), "{all_hit_lines:?}");
        assert!(all_hit_lines.contains(&6), "{all_hit_lines:?}");
        assert!(!all_hit_lines.contains(&1), "{all_hit_lines:?}");
        assert!(!all_hit_lines.contains(&7), "{all_hit_lines:?}");
    }

    // ── #1959 finding 10: pinned_range checks all four dependency fields ──

    #[test]
    fn pinned_range_resolves_a_peer_only_pin() {
        let pkg = serde_json::json!({"peerDependencies": {"@org/lib": "^5.5.0"}});
        assert_eq!(pinned_range(&pkg, "@org/lib"), Some("^5.5.0".to_string()));
    }

    #[test]
    fn pinned_range_resolves_an_optional_only_pin() {
        let pkg = serde_json::json!({"optionalDependencies": {"@org/lib": "^5.5.0"}});
        assert_eq!(pinned_range(&pkg, "@org/lib"), Some("^5.5.0".to_string()));
    }

    #[test]
    fn pinned_range_ledger_note_names_all_four_fields_when_absent() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(app_dir.join("package.json"), serde_json::json!({}).to_string()).unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1"}).to_string(),
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];
        let out = plan(&materialized, &rules).unwrap();

        let note = out.totals.edges[0].note.as_deref().unwrap_or("");
        for field in ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"] {
            assert!(note.contains(field), "{note}");
        }
    }

    // ── #1959 finding 11: prerelease library version is noted, not silent ──

    #[test]
    fn prerelease_library_version_notes_the_tag_in_the_ledger() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^5.5.0"}}).to_string(),
        )
        .unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1-rc.1"}).to_string(),
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];
        let out = plan(&materialized, &rules).unwrap();

        assert_eq!(out.totals.edges.len(), 1);
        let note = out.totals.edges[0].note.as_deref().unwrap_or("");
        assert!(note.contains("prerelease"), "{note}");
        assert!(note.contains("rc.1"), "{note}");
    }

    // ── #1959 finding 4: edge units carry the library's entry surface ──

    #[test]
    fn edge_unit_carries_library_entry_points_and_changelog() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(lib_dir.join("dist")).unwrap();
        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^5.5.0"}}).to_string(),
        )
        .unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1", "main": "dist/index.js"}).to_string(),
        )
        .unwrap();
        fs::write(lib_dir.join("dist/index.js"), "module.exports = {};\n").unwrap();
        fs::write(lib_dir.join("CHANGELOG.md"), "## 8.1.1\n- breaking change\n").unwrap();
        fs::write(
            app_dir.join("uses-lib.ts"),
            "import { thing } from '@org/lib';\nthing();\n",
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];
        let out = plan(&materialized, &rules).unwrap();

        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert_eq!(edge_units.len(), 1, "{:?}", out.units);
        let Unit::Edge { library_tree, library_surface, note, .. } = edge_units[0] else { panic!() };
        assert_eq!(library_tree, &lib_dir);
        assert!(library_surface.contains(&"dist/index.js".to_string()), "{library_surface:?}");
        assert!(library_surface.contains(&"CHANGELOG.md".to_string()), "{library_surface:?}");
        assert!(note.is_none(), "{note:?}");
    }

    // ── #1959 second-round finding: library_surface path traversal ──

    #[test]
    fn edge_unit_rejects_library_surface_path_traversal() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        // Nested a few levels deep so "../../../secret.txt" resolves to a
        // real file OUTSIDE the library's own tree, at the fixture's workdir
        // root — proving the traversal would otherwise leak a file that
        // was never part of the library.
        let lib_dir = workdir.path().join("nested").join("deep").join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(lib_dir.join("dist")).unwrap();
        fs::write(workdir.path().join("secret.txt"), "TOP SECRET, DO NOT LEAK\n").unwrap();

        // A symlink entry point pointing outside the library tree too —
        // must be rejected the same way, without ever being followed.
        let outside_dir = TempDir::new().unwrap();
        fs::write(outside_dir.path().join("evil.js"), "leak_everything();\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside_dir.path().join("evil.js"), lib_dir.join("evil-link.js")).unwrap();

        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^5.5.0"}}).to_string(),
        )
        .unwrap();
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({
                "version": "8.1.1",
                "main": "../../../secret.txt",
                "module": "dist/index.js",
                "types": "evil-link.js",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(lib_dir.join("dist/index.js"), "module.exports = {};\n").unwrap();
        fs::write(
            app_dir.join("uses-lib.ts"),
            "import { thing } from '@org/lib';\nthing();\n",
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];
        let out = plan(&materialized, &rules).unwrap();

        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert_eq!(edge_units.len(), 1, "{:?}", out.units);
        let Unit::Edge { library_surface, est_tokens, .. } = edge_units[0] else { panic!() };
        assert!(!library_surface.iter().any(|s| s.contains("secret")), "{library_surface:?}");
        assert!(!library_surface.iter().any(|s| s.contains("evil")), "{library_surface:?}");
        assert_eq!(library_surface, &vec!["dist/index.js".to_string()], "{library_surface:?}");

        let traversal_skips: Vec<&SkippedEntry> = out
            .totals
            .skipped
            .iter()
            .filter(|s| s.reason == "library_surface_outside_tree")
            .collect();
        assert_eq!(traversal_skips.len(), 2, "{:?}", out.totals.skipped);
        assert!(
            traversal_skips.iter().any(|s| s.file == "../../../secret.txt"),
            "{traversal_skips:?}"
        );
        assert!(
            traversal_skips.iter().any(|s| s.file == "evil-link.js"),
            "{traversal_skips:?}"
        );
        assert!(
            traversal_skips.iter().all(|s| s.source.as_deref() == Some("lib")),
            "{traversal_skips:?}"
        );

        // Baseline: same setup but WITHOUT the malicious fields — est_tokens
        // must be identical, proving the rejected candidates contributed
        // nothing to the estimate (not even a partial/short read).
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1", "module": "dist/index.js"}).to_string(),
        )
        .unwrap();
        let sources_clean = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized_clean = materialized_for(sources_clean, edge_spec("app", "lib", "@org/lib"));
        let out_clean = plan(&materialized_clean, &rules).unwrap();
        let clean_edge_units: Vec<&Unit> =
            out_clean.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        let Unit::Edge { est_tokens: clean_tokens, .. } = clean_edge_units[0] else { panic!() };
        assert_eq!(est_tokens, clean_tokens);
    }

    // ── #1959 second-round CONSIDER 5: empty library surface emits no unit ──

    #[test]
    fn edge_with_no_library_surface_emits_no_unit_but_ledgers_the_reason() {
        let workdir = TempDir::new().unwrap();
        let app_dir = workdir.path().join("app");
        let lib_dir = workdir.path().join("lib");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(
            app_dir.join("package.json"),
            serde_json::json!({"dependencies": {"@org/lib": "^5.5.0"}}).to_string(),
        )
        .unwrap();
        // No main/module/types/typings/exports, no CHANGELOG — nothing for
        // the library surface to resolve.
        fs::write(
            lib_dir.join("package.json"),
            serde_json::json!({"version": "8.1.1"}).to_string(),
        )
        .unwrap();
        fs::write(
            app_dir.join("uses-lib.ts"),
            "import { thing } from '@org/lib';\nthing();\n",
        )
        .unwrap();

        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let materialized = materialized_for(sources, edge_spec("app", "lib", "@org/lib"));
        let rules = vec![edge_rule()];
        let out = plan(&materialized, &rules).unwrap();

        // No unit emitted — nothing for the model to compare against.
        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert!(edge_units.is_empty(), "{:?}", out.units);

        // But the edge still reads as "checked": the ledger entry carries
        // both the original surface-resolution note AND the reason no
        // unit followed.
        assert_eq!(out.totals.edges.len(), 1, "{:?}", out.totals.edges);
        let note = out.totals.edges[0].note.as_deref().unwrap_or("");
        assert!(note.contains("no package.json entry point"), "{note}");
        assert!(note.contains("no unit emitted: nothing to compare against"), "{note}");
    }

    // ── #1959 finding 6: symlinks are recorded in totals.skipped, never followed ──

    #[test]
    fn symlinks_are_recorded_in_skipped_and_never_followed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("real.ts"), "const x = 1;\n").unwrap();

        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("secret")).unwrap();
        fs::write(outside.path().join("secret/leak.ts"), "catch (e) {}").unwrap();
        fs::write(outside.path().join("file-leak.ts"), "catch (e) {}").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link-dir")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("file-leak.ts"),
                dir.path().join("link-file.ts"),
            )
            .unwrap();
        }

        let sources = vec![resolved("app", dir.path())];
        let materialized = materialized_for(sources, Vec::new());
        let rules = vec![site_rule()];
        let out = plan(&materialized, &rules).unwrap();

        // Never followed: nothing from outside the tree shows up as a hit.
        for unit in &out.units {
            if let Unit::Site { sites, .. } = unit {
                for s in sites {
                    assert!(!s.file.contains("leak"), "{s:?}");
                }
            }
        }
        // Recorded: both the dir symlink and the file symlink are in skipped.
        // (#1959) The symlink walk moved into `workspace_spec::materialize`
        // -- its own reason string ("symlink -- never followed") is what
        // `plan()` now surfaces verbatim from `Materialized.skipped`.
        let symlink_skips: Vec<&SkippedEntry> =
            out.totals.skipped.iter().filter(|s| s.reason.contains("symlink")).collect();
        assert_eq!(symlink_skips.len(), 2, "{:?}", out.totals.skipped);
        assert!(symlink_skips.iter().any(|s| s.file == "link-dir"), "{symlink_skips:?}");
        assert!(symlink_skips.iter().any(|s| s.file == "link-file.ts"), "{symlink_skips:?}");
    }

    // ── #1959 finding 18: plan() is deterministic for a fixed set of trees ──

    #[test]
    fn plan_is_deterministic_across_repeated_runs() {
        let dir = TempDir::new().unwrap();
        let mut lines = vec!["function a() {".to_string(), "  try {}".to_string(), "  catch (e) {}".to_string(), "}".to_string()];
        for _ in 0..80 {
            lines.push(String::new());
        }
        lines.push("function b() {".to_string());
        lines.push("  p().catch(e => {})".to_string());
        fs::write(dir.path().join("x.ts"), lines.join("\n")).unwrap();
        fs::write(dir.path().join("README.md"), "docs\n").unwrap();

        let sources = vec![resolved("app", dir.path())];
        let materialized = materialized_for(sources, Vec::new());
        let rules = vec![site_rule(), read_rule()];

        let out1 = plan(&materialized, &rules).unwrap();
        let out2 = plan(&materialized, &rules).unwrap();
        let units1 = serde_json::to_value(&out1.units).unwrap();
        let units2 = serde_json::to_value(&out2.units).unwrap();
        assert_eq!(units1, units2);
    }

    #[test]
    fn glob_matcher_mutation_excludes_pattern_must_actually_filter() {
        // Mutation self-check target: if `applies` stopped honoring `exclude`,
        // this must fail.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        fs::write(dir.path().join("node_modules/pkg.ts"), "catch (e) {}").unwrap();
        fs::write(dir.path().join("real.ts"), "catch (e) {}").unwrap();

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = resolved("app", dir.path());
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq, &PlanParams::default()).unwrap();
        let files_hit: Vec<&str> = units
            .iter()
            .flat_map(|u| match u {
                Unit::Site { sites, .. } => sites.iter().map(|s| s.file.as_str()).collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        assert_eq!(files_hit, vec!["real.ts"], "{files_hit:?}");
    }
}
