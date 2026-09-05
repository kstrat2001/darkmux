//! Crawl planning (#1959) — turns a MATERIALIZED workspace (a resolved
//! `darkmux_crew::workspace_spec::Materialized`, checked out sources +
//! rules) into a deterministic `Plan` of work units with token estimates.
//! NO model dispatch happens here; this is the mechanical, free-to-compute
//! half of the crawler (prefilters, globs, the npm range check) that the
//! `crawl.unit` step kind (`unit_step.rs`) consumes, one unit per step.

use darkmux_crew::workspace_spec::{glob, Materialized, MaterializedSource};
use darkmux_crew::rules::{Rule, RuleKind};
use darkmux_crew::step_kinds::patterns::plan_sites;
use crate::crawl::semver::{prerelease_tag, range_admits};
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// (#2310 P4c) Deliberately UNCHANGED at "1.1", not bumped, even though
/// `Plan` gains an additive field this packet (`source_kind` — see its own
/// doc): the crawl-plan golden (`tests/golden/crawl-plan-golden/`) pins
/// the literal `schema_version` string this constant writes into every
/// tree-source plan, and this packet's own constraint is that the golden
/// stays byte-identical. `source_kind` is `None`/omitted
/// (`skip_serializing_if`) on every plan `plan()`/`plan_with_params`
/// produce — the only two producers a tree-source golden could ever come
/// from — so the JSON those two emit is unaffected regardless. A real
/// version bump (were one to change tree-source output too) belongs to
/// whichever future packet touches `plan()`/`plan_with_params` itself,
/// together with a deliberate golden update reviewed alongside it — not
/// bundled into a step kind this one didn't touch.
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
/// launch crawl` (#2190) — see `crawl.json`'s own `inputs`.
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

/// (#2310 P4c-2 review MUST-do 1) Shared `sizing.*`/`no_fetch` step-config
/// parsing for BOTH `crawl.plan` (`plan_step.rs`) and `plan.sites`
/// (`plan_sites_step.rs`). Lenient-on-read (contract 7): a `--param`
/// value always arrives at the launcher as a JSON STRING (the CLI
/// layer's convention), and `mission_config::substitute_step_config`'s
/// generic `{{<input-id>}}` substitution (#2310 P4c-2 item 0) carries
/// that string through verbatim into a step's config — a parser that
/// only accepted `as_u64`/`as_bool` would silently DROP every
/// CLI-sourced override the moment a document declares the placeholder
/// (exactly what `plan_sites_step.rs` still did until this review caught
/// it: `crawl.plan` was fixed in item 0's own packet, `plan.sites` was
/// not, and the two silently drifted apart). ONE helper, called by both
/// kinds, so a THIRD `plan.*` kind inherits the lenient parse for free
/// instead of re-deriving it.
///
/// Returns `(params, fetch)` — `params` defaults to `PlanParams::default()`
/// when `sizing` is absent or partial (an omitted key, per item 0's
/// key-omission rule for an unset optional input, keeps that one
/// default); `fetch` is `true` (the historical default) unless `no_fetch`
/// resolves truthy.
pub fn parse_sizing_and_no_fetch(
    config: &serde_json::Value,
    step_id: &str,
    kind: &str,
) -> Result<(PlanParams, bool)> {
    let mut params = PlanParams::default();
    if let Some(sizing) = config.get("sizing") {
        for (key, slot) in [
            ("max_sites_per_unit", &mut params.max_sites_per_unit),
            ("max_est_tokens_per_unit", &mut params.max_est_tokens_per_unit),
        ] {
            if let Some(v) = sizing.get(key) {
                let n = v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "step `{step_id}`: `{kind}` config.sizing.{key} must be a positive integer, got {v}"
                        )
                    })?;
                *slot = usize::try_from(n).context("sizing value does not fit usize")?;
            }
        }
    }
    let no_fetch = match config.get("no_fetch") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on")
        }
        _ => false,
    };
    Ok((params, !no_fetch))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum ReadFileEntry {
    Whole(String),
    Range { file: String, start: usize, end: usize },
}

/// One work unit. Internally tagged on `kind` — serializes as a flat
/// object, e.g. `{"kind":"site","id":"u-0001",...}`, matching the
/// packet-1 plan.json contract exactly.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
    /// (#2310 P4c — additive, NOT a `PLAN_SCHEMA_VERSION` bump; see that
    /// constant's own doc for why) Which [`plan_sites_step`]
    /// (`crate::crawl::plan_sites_step`) strategy produced this plan —
    /// `"diff"` from `plan::plan_diff_rule`. `None`/omitted from every
    /// `plan()`/`plan_with_params` plan (the tree-walk path `crawl.plan`
    /// and `plan.sites`'s own `"source": "tree"` both still call), so a
    /// reader cannot distinguish "planned by the tree strategy" from "this
    /// plan predates the field" — which is fine, because nothing needs to
    /// today: `crawl.unit` reads a `Plan` the same way regardless of what
    /// planned it. A future consumer that DOES need to tell tree from
    /// pre-field-tree apart is the moment this earns its own real minor
    /// bump, together with a deliberate golden update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
}

/// (#2298) The serialized form of [`PlanParams`] a plan records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
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
        // Tree-walk path — see `Plan::source_kind`'s own doc for why this
        // stays `None` (omitted) rather than `Some("tree")`.
        source_kind: None,
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

/// A [`plan_sites::SiteSource`] over a workspace tree walk: every line of
/// every file the rule's `applies_to`/`exclude` admits is a candidate
/// (#2310 P4b) — exactly what `collect_site_units` tested inline before
/// this existed. Borrows the SAME `SourceFiles` cache + `skipped` ledger
/// every other rule pass over this source shares, so a file two rules both
/// match is still read from disk only once.
struct TreeSource<'a> {
    rule: &'a Rule,
    files: &'a mut SourceFiles,
    skipped: &'a mut Vec<SkippedEntry>,
    source_id: &'a str,
}

impl plan_sites::SiteSource for TreeSource<'_> {
    fn files(&mut self) -> Result<Vec<plan_sites::SiteSourceFile>> {
        let matching = self.files.matching(&self.rule.applies_to, &self.rule.exclude);
        let mut out = Vec::with_capacity(matching.len());
        for rel in matching {
            let Some(content) = self.files.get(&rel, self.skipped, self.source_id) else { continue };
            let total = content.lines().count();
            out.push(plan_sites::SiteSourceFile { file: rel, content, candidates: (1..=total).collect() });
        }
        Ok(out)
    }
}

/// `site`-kind rules: rules × source → windows → units, delegated to the
/// Tier 2 pattern (#2310 P4b, `crates/darkmux-crew/src/step_kinds/
/// patterns/plan_sites.rs`) via [`TreeSource`] — every candidate line is
/// the whole file, so this is exactly `collect_site_units`'s pre-#2310-P4b
/// behavior, byte-identical (see `crawl_plan_golden.rs`): `max_span_lines:
/// None` means a merged span is never split, and the early return on an
/// empty prefilter (never even reads a file) is preserved here rather than
/// pushed into the pattern, which has no opinion on what "empty" means for
/// a caller it doesn't know.
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
    let mut tree_source = TreeSource { rule, files, skipped, source_id: &source.id };
    let planned = plan_sites::plan_site_units(
        &mut tree_source,
        &|line| regexes.iter().any(|re| re.is_match(line)),
        &plan_sites::SiteUnitParams {
            window: rule.window_or_default(),
            max_sites_per_unit: params.max_sites_per_unit,
            max_est_tokens_per_unit: params.max_est_tokens_per_unit,
            max_span_lines: None,
        },
        &estimate_tokens,
    )?;
    Ok(planned
        .into_iter()
        .map(|u| Unit::Site {
            id: next_unit_id(unit_seq),
            rule: rule.id.clone(),
            source: source.id.clone(),
            sites: u
                .sites
                .into_iter()
                .map(|s| Site { file: s.file, line: s.line, start: s.start, end: s.end, hits: s.hits })
                .collect(),
            est_tokens: u.est_tokens,
        })
        .collect())
}

/// The default cap on one diff-derived merged span's width, in lines
/// (#2310 P4b) — see `plan_sites`'s `SiteUnitParams::max_span_lines` doc
/// for why a diff needs a cap a tree walk never did: a rule declaring
/// `prefilter: none` (DESIGN.md "A rule is a procedure") makes every line
/// of a hunk a hit, so without a cap the whole hunk always merges into one
/// unbounded site. `2 * DEFAULT_WINDOW + 1` (61 lines) mirrors the width a
/// single prefilter hit would get from `Rule::window_or_default()` at its
/// own default — the same "how much should one site show" intuition,
/// applied to a hunk instead of a hit. A rule's own `window` scaling this
/// cap per-rule (the way it scales everything else about a site's width)
/// is a review plan step's job — #2310 P4c, not built here; this constant
/// is what that step's `plan_sites::SiteUnitParams::max_span_lines` reads
/// until it does.
///
/// (#2310 P4c) Wired: [`plan_diff_rule`] is now the non-test caller this
/// doc used to say didn't exist yet.
const DEFAULT_DIFF_SPAN_CAP: usize = 2 * darkmux_crew::rules::DEFAULT_WINDOW + 1;

/// A [`plan_sites::SiteSource`] over a unified diff's hunks (#2310 P4b) —
/// every line a hunk touches (added AND context, matching
/// `bundle::diff::Hunk::new_lines`) is a candidate, mirroring
/// `TreeSource`'s "every line of the file" for a tree walk. See DESIGN.md
/// "Code review as a second config on the crawl's building blocks":
/// "Hunks are natural windows. No prefilter is needed to find sites; every
/// hunk is a bounded site of the right size for a small seat."
///
/// Reads file CONTENT through the same `SourceFiles` cache `TreeSource`
/// does — DESIGN.md: "The tree is the confirmation surface" — the diff
/// says WHERE to look; the checked-out tree (at the diff's own head) says
/// what a window actually contains, including context beyond the hunk. A
/// caller therefore still needs a materialized workspace at the diff's
/// `head_sha`, not just the diff text.
///
/// `sha`/`ref` are deliberately NOT this struct's concern: DESIGN.md says
/// those "come from the launch inputs `head_sha`/`github` or the diff
/// file's own header" — that resolution belongs to whatever builds the
/// `MaterializedSource`/`PlanSource` a review plan step passes in, same as
/// `TreeSource` never resolves its own `sha` either (`resolved()` in this
/// module's own tests, or `workspace_spec::materialize` for a real run).
///
/// (#2310 P4c) Wired via [`plan_diff_rule`] — NOT through `plan()`/
/// `plan_with_params` (those stay tree-only, unchanged): the diff plan
/// step (`plan_sites_step.rs`) calls `plan_diff_rule` directly for its
/// `"source": "diff"` config.
struct DiffSource<'a> {
    files: &'a mut SourceFiles,
    skipped: &'a mut Vec<SkippedEntry>,
    source_id: &'a str,
    /// path -> {new-side line number -> the diff's OWN expected text at
    /// that line} — the union of every hunk's `new_lines`/`new_block` for
    /// that path, zipped by position (`new_block[i]` is the content of
    /// line `new_start + i`; both are built from the SAME sequential walk
    /// over context+added lines in `parse_diff`, so the correlation holds
    /// without `Hunk` needing to store it explicitly). The expected TEXT
    /// is what makes the M-C consistency check below possible — not just
    /// candidate line numbers, but what the diff itself says those lines
    /// should read.
    by_file: BTreeMap<String, BTreeMap<u32, String>>,
    /// Files this source's tree has that the diff never mentions AT ALL —
    /// counted like `Materialized.out_of_scope` (#1959): a number, never
    /// a list.
    out_of_scope: usize,
    /// (#2310 P4b review, CONSIDER) Files the diff mentions (its own
    /// `diff --git a/<old> b/<new>` header) but produced NO hunks for — a
    /// pure rename or a binary file, neither of which `bundle::diff::
    /// parse_diff` sees (it reacts only to `+++`/`@@` lines, and neither
    /// shape has either). Counted separately from `out_of_scope`: the diff
    /// DID touch these, there is simply nothing to plan a site from.
    diff_entries_without_hunks: usize,
}

impl<'a> DiffSource<'a> {
    /// Parses `diff_text` once at construction (`bundle::diff::parse_diff`
    /// — the SAME unified-diff parser the review bundler already uses, not
    /// a second one) and computes `out_of_scope` against `files.all` (the
    /// tree's own already-spec-filtered file list) up front, so both are
    /// cheap reads afterward rather than re-derived per call.
    fn new(files: &'a mut SourceFiles, skipped: &'a mut Vec<SkippedEntry>, source_id: &'a str, diff_text: &str) -> Self {
        let parsed = crate::lab::bundle::diff::parse_diff(diff_text);
        let mut by_file: BTreeMap<String, BTreeMap<u32, String>> = BTreeMap::new();
        for (path, hunks) in &parsed {
            let entry = by_file.entry(path.clone()).or_default();
            for h in hunks {
                // `new_block[i]` is line `new_start + i` — both are built
                // from the same sequential context/added walk in
                // `parse_diff`, so this zip is exact, not an assumption
                // about hunk shape.
                for (offset, text) in h.new_block.iter().enumerate() {
                    entry.insert(h.new_start + offset as u32, text.clone());
                }
            }
        }
        let mentioned = diff_git_header_paths(diff_text);
        let out_of_scope =
            files.all.iter().filter(|f| !by_file.contains_key(f.as_str()) && !mentioned.contains(f.as_str())).count();
        let diff_entries_without_hunks =
            files.all.iter().filter(|f| !by_file.contains_key(f.as_str()) && mentioned.contains(f.as_str())).count();
        Self { files, skipped, source_id, by_file, out_of_scope, diff_entries_without_hunks }
    }

    /// Files this source's tree has that the diff never MENTIONS at all —
    /// counted, never enumerated (#1959's own `Materialized.out_of_scope`
    /// rule, applied here to "not in the diff" instead of "spec-excluded").
    /// A future review plan step reads this to populate `PlanSource.
    /// out_of_scope` the same way `plan()` already does for
    /// `materialized.out_of_scope`.
    fn out_of_scope(&self) -> usize {
        self.out_of_scope
    }

    /// (#2310 P4b review, CONSIDER) Files the diff mentions with no hunks
    /// (a pure rename, a binary file) — see the field's own doc for why
    /// these are not `out_of_scope`.
    fn diff_entries_without_hunks(&self) -> usize {
        self.diff_entries_without_hunks
    }
}

/// Every path a unified diff mentions via its own `diff --git a/<old>
/// b/<new>` header, whether or not it produced any hunks (#2310 P4b
/// review, CONSIDER) — see [`DiffSource`]'s `diff_entries_without_hunks`
/// field doc. Kept local to this module rather than folded into the
/// shared `crate::diff` parser: no existing `bundle::diff` consumer needs
/// "which paths a diff mentions with no hunks", only this accounting does.
fn diff_git_header_paths(diff_text: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for ln in diff_text.lines() {
        let Some(rest) = ln.strip_prefix("diff --git ") else { continue };
        // (#2310 fix-loop B1) The `a/`/`b/` prefixes are a DIALECT, not
        // part of the path — `git diff --no-prefix` emits `diff --git
        // <old> <new>`, and reading only the prefixed form dropped every
        // rename/binary entry in such a diff out of
        // `diff_entries_without_hunks` and into `out_of_scope`, which
        // says the opposite thing (the diff never mentioned it).
        // Same optional-prefix rule `darkmux_crew::diff::header_path`
        // applies to `+++`/`---`.
        let new_side = match rest.strip_prefix("a/").and_then(|r| r.find(" b/").map(|i| &r[i + 3..])) {
            Some(p) => p,
            // Prefixless: `<old> <new>`. Paths with spaces are ambiguous
            // in this dialect (git itself quotes them; not handled here,
            // same as before) — take the last space-separated field,
            // which is the new side for every unquoted path.
            None => match rest.rsplit_once(' ') {
                Some((_, new_side)) => new_side,
                None => continue,
            },
        };
        if !new_side.is_empty() {
            out.insert(new_side.to_string());
        }
    }
    out
}

impl plan_sites::SiteSource for DiffSource<'_> {
    /// (#2310 P4b review, M-C) `DiffSource` reads WHERE from the diff and
    /// WHAT from the tree — two independent sources that can disagree
    /// (the wrong sha checked out, a stale mirror, the diff cut against a
    /// branch the tree isn't on). Silently trusting the diff's line
    /// numbers against whatever the tree happens to hold would plan sites
    /// with a window that doesn't match the code it claims to show. Every
    /// file gets its tree content checked against the diff's OWN recorded
    /// text at each of that hunk's new-side lines before it's ever
    /// windowed; the first disagreement (or a diff path the tree doesn't
    /// have at all) becomes a `SkippedEntry` naming the file and the line,
    /// and that file contributes NO candidates — loud, not silently
    /// dropped.
    fn files(&mut self) -> Result<Vec<plan_sites::SiteSourceFile>> {
        let mut out = Vec::with_capacity(self.by_file.len());
        for (rel, expected) in &self.by_file {
            if !self.files.all.iter().any(|f| f == rel) {
                self.skipped.push(SkippedEntry {
                    reason: "the diff names this file but the checked-out tree does not have it — wrong sha, or a rename the diff doesn't show?"
                        .to_string(),
                    file: rel.clone(),
                    source: Some(self.source_id.to_string()),
                });
                continue;
            }
            let Some(content) = self.files.get(rel, self.skipped, self.source_id) else { continue };
            let lines: Vec<&str> = content.lines().collect();
            let mismatch = expected.iter().find(|(&line_no, text)| {
                lines.get(line_no as usize - 1).copied() != Some(text.as_str())
            });
            if let Some((line_no, _)) = mismatch {
                self.skipped.push(SkippedEntry {
                    reason: format!(
                        "the checked-out tree disagrees with the diff at line {line_no} — this looks like the wrong checkout (a different sha than the diff was cut against)"
                    ),
                    file: rel.clone(),
                    source: Some(self.source_id.to_string()),
                });
                continue;
            }
            let candidates: Vec<usize> = expected.keys().map(|&n| n as usize).collect();
            out.push(plan_sites::SiteSourceFile { file: rel.clone(), content, candidates });
        }
        Ok(out)
    }
}

/// (#2310 P4c) A thin [`plan_sites::SiteSource`] adapter that filters
/// [`DiffSource`]'s output through a rule's OWN `applies_to`/`exclude`
/// globs, the same filter `TreeSource`/`SourceFiles::matching` already
/// apply for a tree walk. Without this, a rule reused across
/// `scope: ["tree","diff"]` would fire on every file a diff touches
/// regardless of its declared file types.
///
/// (#2310 P4c review round 2, MUST FIX 1) Deliberately NOT
/// `glob::applies` when `applies_to` is empty: `applies()` is `any-of
/// applies_to AND none-of exclude`, so an EMPTY `applies_to` makes the
/// `any()` vacuously `false` and `applies()` reject every file outright —
/// exactly backwards from what an empty `applies_to` means here ("every
/// file the diff touches", the same convention `plan_diff_rule`'s empty-
/// prefilter handling already documents). The bug this replaces called
/// `glob::applies` unconditionally when `applies_to` was non-empty but
/// short-circuited to `Ok(all)` — UNFILTERED, `exclude` included — the
/// moment `applies_to` was empty. Every rule in the P4c catalog with no
/// file-type opinion (`intent-vs-diff`, `existing-solution`,
/// `shared-symbol-callers`, `union-vs-enum`, `test-gap`) declares empty
/// `applies_to` alongside a real `exclude` (test paths), so that bug
/// meant every one of them planned sites inside its own excluded test
/// files. `exclude` is therefore checked with `glob::matches` directly,
/// independent of whether `applies_to` is empty.
struct FilteredDiffSource<'a, 'b> {
    inner: &'a mut DiffSource<'b>,
    applies_to: &'a [String],
    exclude: &'a [String],
}

impl plan_sites::SiteSource for FilteredDiffSource<'_, '_> {
    fn files(&mut self) -> Result<Vec<plan_sites::SiteSourceFile>> {
        let all = self.inner.files()?;
        let excluded = |f: &plan_sites::SiteSourceFile| self.exclude.iter().any(|p| glob::matches(p, &f.file));
        if self.applies_to.is_empty() {
            return Ok(all.into_iter().filter(|f| !excluded(f)).collect());
        }
        Ok(all.into_iter().filter(|f| glob::applies(self.applies_to, self.exclude, &f.file)).collect())
    }
}

/// (#2310 P4c) The first real caller of [`DiffSource`]/[`DEFAULT_DIFF_SPAN_CAP`]
/// — the `plan.sites` step kind (`plan_sites_step.rs`) when its config
/// declares `"source": "diff"`. Plans ONE `site`-kind rule against a diff,
/// over a tree already checked out at the diff's own head, mirroring
/// `plan_step::plan_one_rule`'s "resolve -> materialize -> plan" shape but
/// swapping `TreeSource` for `DiffSource` (DESIGN.md "The tree is the
/// confirmation surface: the diff is where triggers are detected; the
/// whole worktree ... is where they are confirmed").
///
/// `materialized` must hold EXACTLY one source — a diff correlates to one
/// tree at one sha, unlike a crawl's workspace spec, which can name
/// several. `rule` must be `site`-kind: DESIGN.md "Hunks are natural
/// windows" has no analog for a whole-file `read` pass or an `edge`
/// dependency check, which is also why `doc-contradicts-code`/
/// `stale-consumer` declare `scope: ["tree"]` only (see those rule files'
/// #2310 P4c comments).
pub fn plan_diff_rule(materialized: &Materialized, rule: &Rule, diff_text: &str, params: PlanParams) -> Result<Plan> {
    anyhow::ensure!(
        materialized.sources.len() == 1,
        "a diff-scoped plan needs exactly one workspace source (got {}) — a diff correlates to \
         one tree at one sha, not several",
        materialized.sources.len()
    );
    anyhow::ensure!(
        rule.kind == RuleKind::Site,
        "rule '{}' is `{:?}`-kind; a diff-scoped plan only supports `site`-kind rules \
         (DESIGN.md \"Hunks are natural windows\") — every rule this review catalog reuses from \
         the crawl must declare `scope: [\"diff\"]` (or omit `scope`) only if it is site-kind",
        rule.id,
        rule.kind
    );
    // (#2310 P4c review round 2, MUST FIX 2) The tree-side twin of this
    // check is `plan_step::plan_one_rule`'s own — see its doc for why
    // `Rule::scope` needs enforcement on BOTH sides, not just documented.
    if !rule.applies_to_scope(darkmux_crew::rules::RuleScope::Diff) {
        let scope = serde_json::to_string(rule.scope_or_default()).unwrap_or_default();
        anyhow::bail!(
            "rule '{}' declares scope {scope} — a rule with no `diff` scope cannot be planned \
             against a diff",
            rule.id
        );
    }
    let source = &materialized.sources[0];
    let mut skipped: Vec<SkippedEntry> = materialized
        .skipped
        .iter()
        .map(|sf| SkippedEntry { reason: sf.reason.clone(), file: sf.relative_path.clone(), source: Some(sf.source_id.clone()) })
        .collect();
    let all = materialized.files.get(&source.id).cloned().unwrap_or_default();
    let files_walked = all.len();
    let mut files = SourceFiles::new(&source.tree, all);

    let regexes: Vec<Regex> = rule
        .prefilter
        .iter()
        .map(|p| Regex::new(p).with_context(|| format!("compiling prefilter for rule '{}': {p}", rule.id)))
        .collect::<Result<_>>()?;
    // (#2310 P4b module doc, plan_sites.rs) An empty prefilter means
    // opposite things for a tree walk and a diff: `collect_site_units`
    // early-returns zero units for a tree (see its own doc), but here it
    // means "every hunk line is a candidate" — DESIGN.md "no prefilter is
    // needed to find sites; every hunk is a bounded site of the right size
    // for a small seat." A non-empty prefilter still narrows within those
    // candidates, same as a tree walk.
    let is_hit = |line: &str| regexes.is_empty() || regexes.iter().any(|re| re.is_match(line));

    let mut diff_source = DiffSource::new(&mut files, &mut skipped, &source.id, diff_text);
    // (#2310 fix-loop B1, S3-1 — PROVEN live) A diff file with content in
    // it that yields NO files is never a legitimate "nothing to review":
    // it is a diff this parser could not read, or one whose paths belong
    // to a different tree than the workspace names. Planning zero units
    // from it produced a green run with a noop payload, which the
    // operator reads as "the PR is clean" — indistinguishable from the
    // rule genuinely finding nothing, the same failure the rule-kind and
    // rule-scope guards above already refuse. Refuse by name and say what
    // to check. An EMPTY diff file is left alone: "nothing changed" is a
    // real, unambiguous input.
    if !diff_text.trim().is_empty() && diff_source.by_file.is_empty() {
        anyhow::bail!(
            "the diff has content but parses to no files — check the diff's dialect and paths. \
             darkmux reads `+++ <path>` headers with the `a/`/`b/` prefix optional and an \
             optional trailing tab+timestamp (`git diff`, `git diff --no-prefix`, `diff -u`, \
             `diff -Naur` all work); a diff of only renames or binary files has no `+++` header \
             at all and nothing to plan a site from. Planning it would have produced zero units, \
             which is indistinguishable from rule '{}' finding nothing.",
            rule.id
        );
    }
    // Read both counters BEFORE `plan_site_units` runs its own mutable
    // borrow through `&mut diff_source` for the rest of this function —
    // `DiffSource` holds `&mut skipped` as a field for its whole
    // lifetime, so a direct `skipped.push` while `diff_source` is still in
    // scope would be a second, conflicting mutable borrow (E0499); the
    // informational push below runs only after `diff_source`'s last use.
    let entries_without_hunks = diff_source.diff_entries_without_hunks();
    // (#2310 P4c) `DiffSource::files()` itself has no opinion on a rule's
    // OWN `applies_to`/`exclude` — it returns every path the diff touches,
    // the same way `TreeSource` would if `SourceFiles::matching` weren't
    // filtering its own output first. A rule reused from the crawl across
    // `scope: ["tree","diff"]` (`swallowed-error`/`unnamed-predicate`, both
    // TypeScript-only) still needs that filter under a diff, or it fires on
    // every changed file in a mixed-language diff, not just the ones its
    // globs admit. Empty `applies_to` (a rule with no file-type opinion at
    // all, e.g. `intent-vs-diff`) means "every file", mirroring the
    // empty-prefilter convention above.
    let mut filtered = FilteredDiffSource { inner: &mut diff_source, applies_to: &rule.applies_to, exclude: &rule.exclude };
    let planned = plan_sites::plan_site_units(
        &mut filtered,
        &is_hit,
        &plan_sites::SiteUnitParams {
            window: rule.window_or_default(),
            max_sites_per_unit: params.max_sites_per_unit,
            max_est_tokens_per_unit: params.max_est_tokens_per_unit,
            // (#2310 P4b) A diff rule declaring no prefilter makes every
            // hunk line a hit, so without a cap the whole hunk always
            // merges into one unbounded site — see `DEFAULT_DIFF_SPAN_CAP`'s
            // own doc.
            max_span_lines: Some(DEFAULT_DIFF_SPAN_CAP),
        },
        &estimate_tokens,
    )?;
    let out_of_scope = diff_source.out_of_scope();
    if entries_without_hunks > 0 {
        skipped.push(SkippedEntry {
            reason: format!(
                "the diff mentions {entries_without_hunks} file(s) with no hunks (a pure rename \
                 or a binary file) — nothing to plan a site from"
            ),
            file: String::new(),
            source: Some(source.id.clone()),
        });
    }

    let units: Vec<Unit> = {
        let mut unit_seq: usize = 1;
        planned
            .into_iter()
            .map(|u| Unit::Site {
                id: next_unit_id(&mut unit_seq),
                rule: rule.id.clone(),
                source: source.id.clone(),
                sites: u
                    .sites
                    .into_iter()
                    .map(|s| Site { file: s.file, line: s.line, start: s.start, end: s.end, hits: s.hits })
                    .collect(),
                est_tokens: u.est_tokens,
            })
            .collect()
    };

    let rules = std::slice::from_ref(rule);
    let plan_sources = vec![PlanSource {
        id: source.id.clone(),
        sha: source.sha.clone(),
        git_ref: source.git_ref.clone(),
        tree: source.tree.clone(),
        files_walked,
        out_of_scope,
    }];
    let totals = compute_totals(rules, &units, skipped, Vec::new());

    Ok(Plan {
        schema_version: PLAN_SCHEMA_VERSION.to_string(),
        workspace: materialized.name.clone(),
        planned_at: darkmux_flow::ts_utc_now(),
        sources: plan_sources,
        units,
        totals,
        rules: rules.iter().map(|r| r.id.clone()).collect(),
        params: Some(params.into()),
        source_kind: Some("diff".to_string()),
    })
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
            // (#2310 P4c) These test fixtures predate `scope`/`confirm`/
            // `search`/`compare` — `Default::default()` for each keeps
            // every existing assertion in this module byte-identical
            // (empty `scope` reads as "both", `confirm` reads as `Mod`).
            scope: Default::default(),
            confirm: Default::default(),
            search: None,
            compare: None,
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
            // (#2310 P4c) These test fixtures predate `scope`/`confirm`/
            // `search`/`compare` — `Default::default()` for each keeps
            // every existing assertion in this module byte-identical
            // (empty `scope` reads as "both", `confirm` reads as `Mod`).
            scope: Default::default(),
            confirm: Default::default(),
            search: None,
            compare: None,
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
            // (#2310 P4c) These test fixtures predate `scope`/`confirm`/
            // `search`/`compare` — `Default::default()` for each keeps
            // every existing assertion in this module byte-identical
            // (empty `scope` reads as "both", `confirm` reads as `Mod`).
            scope: Default::default(),
            confirm: Default::default(),
            search: None,
            compare: None,
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

    /// (#2310 P4b) The review-conformance diff fixture's post-image
    /// content, reconstructed by hand from `tests/fixtures/
    /// review-conformance/diff.patch`'s own `+`/context lines — DiffSource
    /// reads through the checked-out TREE (DESIGN.md: "the tree is the
    /// confirmation surface"), so a test needs the tree at the diff's
    /// head, not just the diff text. `docs/setup.md` and `src/config.ts`
    /// are also touched by the fixture diff; `src/untouched.ts` is not —
    /// the file `out_of_scope` must count.
    fn diff_fixture_tree(dir: &Path) {
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("docs")).unwrap();
        fs::write(
            dir.join("src/billing.ts"),
            "function computeTotal(start) {\n  const end = start.plus(30)\n  return end\n}\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/auth.ts"),
            "function checkAccess(user) {\n  if (user.role = \"admin\") {\n    return true\n  }\n",
        )
        .unwrap();
        fs::write(dir.join("docs/setup.md"), "# Setup\nA new heading appears here\nmore docs\n").unwrap();
        fs::write(
            dir.join("src/config.ts"),
            "function loadPort(env) {\n  const port = env.PORT + 1\n  return port\n}\n",
        )
        .unwrap();
        fs::write(dir.join("src/untouched.ts"), "export const x = 1;\n").unwrap();
    }

    fn diff_fixture_text() -> String {
        fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/review-conformance/diff.patch"),
        )
        .unwrap()
    }

    #[test]
    fn diff_source_yields_exactly_the_diffs_hunks_as_candidates_with_correct_line_ranges() {
        let dir = TempDir::new().unwrap();
        diff_fixture_tree(dir.path());
        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let diff_text = diff_fixture_text();
        let mut source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);

        let mut got = plan_sites::SiteSource::files(&mut source).unwrap();
        got.sort_by(|a, b| a.file.cmp(&b.file));
        let names: Vec<&str> = got.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(names, vec!["docs/setup.md", "src/auth.ts", "src/billing.ts", "src/config.ts"], "{names:?}");
        // 4 files, exactly what the diff touched — `src/untouched.ts` never
        // appears, whether or not it exists in the tree.
        assert_eq!(got.len(), 4);

        let billing = got.iter().find(|f| f.file == "src/billing.ts").unwrap();
        assert_eq!(billing.candidates, vec![1, 2, 3, 4], "context + added lines, the whole hunk's span");
        let setup = got.iter().find(|f| f.file == "docs/setup.md").unwrap();
        assert_eq!(setup.candidates, vec![1, 2, 3]);
    }

    #[test]
    fn diff_source_counts_a_file_the_diff_never_touched_as_out_of_scope() {
        let dir = TempDir::new().unwrap();
        diff_fixture_tree(dir.path());
        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let diff_text = diff_fixture_text();
        let source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);
        // Exactly one file in the tree (`src/untouched.ts`) is absent from
        // the diff's 4 touched paths — never enumerated, only counted.
        assert_eq!(source.out_of_scope(), 1);
    }

    #[test]
    fn diff_source_skips_a_file_whose_tree_content_disagrees_with_the_diff() {
        // (#2310 P4b review, M-C MUST FIX) `DiffSource` reads WHERE from
        // the diff and WHAT from the tree — a tree checked out at the
        // WRONG sha (here: one line SHORTER than the hunk expects, so its
        // line 3 doesn't exist at all) must be refused loudly, not
        // silently planned against a window that doesn't match the code
        // it claims to show.
        let dir = TempDir::new().unwrap();
        // The tree is missing "line three" — one line short of what the
        // diff's hunk (new-side lines 1-3) expects.
        std::fs::write(dir.path().join("x.ts"), "line one
line two
").unwrap();
        let diff_text = [
            "diff --git a/x.ts b/x.ts",
            "--- a/x.ts",
            "+++ b/x.ts",
            "@@ -1,2 +1,3 @@",
            " line one",
            "+line two",
            " line three",
            "",
        ]
        .join("
");

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let mut source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);
        let out = plan_sites::SiteSource::files(&mut source).unwrap();
        assert!(out.is_empty(), "a disagreeing file contributes NO candidates, got {} files", out.len());
        assert_eq!(skipped.len(), 1, "the disagreement is COUNTED, not silently dropped: {skipped:?}");
        assert_eq!(skipped[0].file, "x.ts");
        assert!(skipped[0].reason.contains('3'), "names the first mismatching line: {}", skipped[0].reason);
    }

    #[test]
    fn diff_source_counts_a_pure_rename_separately_from_out_of_scope() {
        // (#2310 P4b review, CONSIDER) A pure rename (or a binary file)
        // has a `diff --git` header and NO hunks at all — `parse_diff`
        // never sees it (it reacts only to `+++`/`@@`). It must not be
        // miscounted as `out_of_scope` (the diff DID mention it), but it
        // also isn't a normal touched file (nothing to plan a site from).
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("new_name.ts"), "export const x = 1;
").unwrap();
        std::fs::write(dir.path().join("untouched.ts"), "export const y = 1;
").unwrap();
        let diff_text = [
            "diff --git a/old_name.ts b/new_name.ts",
            "similarity index 100%",
            "rename from old_name.ts",
            "rename to new_name.ts",
            "",
        ]
        .join("
");

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);
        assert_eq!(source.diff_entries_without_hunks(), 1, "the rename target, counted separately");
        assert_eq!(source.out_of_scope(), 1, "only `untouched.ts` — the diff never mentions it at all");
    }

    #[test]
    fn diff_source_with_a_prefilter_windows_a_hit_inside_a_hunk_like_a_tree_walk_hit() {
        // (#2310 P4b) A review rule that DOES declare a prefilter runs it
        // over the hunk's candidate lines exactly like `TreeSource` runs
        // one over a file's lines — same `plan_sites::plan_site_units`
        // call, same window/merge behavior, only the candidate SET differs.
        let dir = TempDir::new().unwrap();
        diff_fixture_tree(dir.path());
        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let diff_text = diff_fixture_text();
        let mut source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);

        let is_hit = |line: &str| line.contains("role");
        let out = plan_sites::plan_site_units(
            &mut source,
            &is_hit,
            &plan_sites::SiteUnitParams {
                window: 1,
                max_sites_per_unit: 40,
                max_est_tokens_per_unit: 16_000,
                max_span_lines: Some(DEFAULT_DIFF_SPAN_CAP),
            },
            &estimate_tokens,
        )
        .unwrap();
        let sites: Vec<&plan_sites::PlannedSite> = out.iter().flat_map(|u| u.sites.iter()).collect();
        assert_eq!(sites.len(), 1, "only auth.ts's line 2 matches 'role': {sites:?}");
        assert_eq!(sites[0].file, "src/auth.ts");
        assert_eq!((sites[0].start, sites[0].end, sites[0].line), (1, 3, 2), "windowed +/-1 around the hit");
    }

    #[test]
    fn diff_source_with_no_prefilter_makes_every_hunk_a_site_and_a_hunk_longer_than_the_cap_splits() {
        // (#2310 P4b, DESIGN.md "Rules may declare prefilter: none so every
        // hunk is a site... a hunk with more lines than the window becomes
        // several sites") — a synthetic diff with one 12-line hunk in one
        // file, capped at 4 lines, must split into 3 sites (4, 4, 4) and
        // must NOT lose or duplicate a single line across the split.
        let dir = TempDir::new().unwrap();
        let mut post_image = Vec::new();
        for n in 1..=12 {
            post_image.push(format!("line{n}"));
        }
        fs::write(dir.path().join("wide.ts"), post_image.join("\n") + "\n").unwrap();

        let mut diff = vec!["diff --git a/wide.ts b/wide.ts".to_string(), "--- a/wide.ts".to_string(), "+++ b/wide.ts".to_string(), "@@ -1,12 +1,12 @@".to_string()];
        for n in 1..=12 {
            diff.push(format!(" line{n}"));
        }
        let diff_text = diff.join("\n") + "\n";

        let mut skipped = Vec::new();
        let mut files = SourceFiles::new(dir.path(), walk_all(dir.path(), "app").0);
        let mut source = DiffSource::new(&mut files, &mut skipped, "app", &diff_text);

        let out = plan_sites::plan_site_units(
            &mut source,
            &|_| true,
            &plan_sites::SiteUnitParams { window: 0, max_sites_per_unit: 40, max_est_tokens_per_unit: 1_000_000, max_span_lines: Some(4) },
            &estimate_tokens,
        )
        .unwrap();
        let sites: Vec<&plan_sites::PlannedSite> = out.iter().flat_map(|u| u.sites.iter()).collect();
        assert_eq!(sites.len(), 3, "12 lines / cap 4 = 3 sites: {sites:?}");
        assert_eq!((sites[0].start, sites[0].end), (1, 4));
        assert_eq!((sites[1].start, sites[1].end), (5, 8));
        assert_eq!((sites[2].start, sites[2].end), (9, 12));
        let mut every_hit: Vec<usize> = sites.iter().flat_map(|s| s.hits.clone()).collect();
        every_hit.sort_unstable();
        assert_eq!(every_hit, (1..=12).collect::<Vec<_>>(), "no line lost or duplicated across the split");
    }

    fn diff_source_at(dir: &Path, source_id: &str, sha: &str) -> MaterializedSource {
        MaterializedSource { id: source_id.to_string(), sha: sha.to_string(), git_ref: "HEAD".to_string(), tree: dir.to_path_buf() }
    }

    /// (#2310 P4c) `plan_diff_rule`'s happy path end to end: one file with
    /// one hunk, `swallowed-error` (a real embedded rule with a real
    /// prefilter), over a `Materialized` built the same way
    /// `workspace_spec::materialize` would for a one-source spec.
    #[test]
    fn plan_diff_rule_plans_one_site_kind_rule_over_a_diff() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("f.ts"),
            "function f() {\n  try {\n    risky();\n  } catch (e) {\n  }\n}\n",
        )
        .unwrap();
        let diff_text = [
            "diff --git a/f.ts b/f.ts",
            "--- a/f.ts",
            "+++ b/f.ts",
            "@@ -1,6 +1,6 @@",
            " function f() {",
            "   try {",
            "     risky();",
            "   } catch (e) {",
            "   }",
            " }",
        ]
        .join("\n")
            + "\n";
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"c".repeat(40))], Vec::new());
        let (rules, _) = darkmux_crew::rules::load_all(None);
        let rule = rules["swallowed-error"].clone();

        let plan = plan_diff_rule(&materialized, &rule, &diff_text, PlanParams::default()).unwrap();
        assert_eq!(plan.rules, vec!["swallowed-error".to_string()]);
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].sha, "c".repeat(40));
        assert_eq!(plan.units.len(), 1, "{:?}", plan.units);
        let Unit::Site { sites, .. } = &plan.units[0] else { panic!("expected a Site unit") };
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert_eq!(sites[0].file, "f.ts");
    }

    /// (#2310 P4c mutation-kill for `FilteredDiffSource`) A TypeScript-only
    /// rule reused across `scope: ["tree","diff"]` must NOT fire on a
    /// non-TypeScript file the diff also touches — `swallowed-error`'s
    /// `applies_to` (`**/*.ts` etc.) has to be honored under a diff the
    /// same way `TreeSource` already honors it for a tree walk.
    #[test]
    fn plan_diff_rule_still_honors_the_rules_own_applies_to_glob() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.ts"), "  } catch (e) {\n  }\n").unwrap();
        fs::write(dir.path().join("f.py"), "  } catch (e) {\n  }\n").unwrap();
        let diff_text = [
            "diff --git a/f.ts b/f.ts",
            "--- a/f.ts",
            "+++ b/f.ts",
            "@@ -1,2 +1,2 @@",
            "   } catch (e) {",
            "   }",
            "diff --git a/f.py b/f.py",
            "--- a/f.py",
            "+++ b/f.py",
            "@@ -1,2 +1,2 @@",
            "   } catch (e) {",
            "   }",
        ]
        .join("\n")
            + "\n";
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"f".repeat(40))], Vec::new());
        let (rules, _) = darkmux_crew::rules::load_all(None);
        let rule = rules["swallowed-error"].clone();
        assert!(!rule.applies_to.is_empty(), "the rule this test relies on must still declare applies_to");

        let plan = plan_diff_rule(&materialized, &rule, &diff_text, PlanParams::default()).unwrap();
        let files: Vec<&str> =
            plan.units.iter().flat_map(|u| if let Unit::Site { sites, .. } = u { sites.iter().map(|s| s.file.as_str()).collect() } else { vec![] }).collect();
        assert_eq!(files, vec!["f.ts"], "the .py file must be filtered out by the rule's own applies_to: {files:?}");
    }

    /// (#2310 P4c-2b) `plan_diff_rule_still_honors_the_rules_own_applies_to_glob`
    /// above proves the populated-`applies_to` branch of `FilteredDiffSource`
    /// (line ~1090, `glob::applies(self.applies_to, self.exclude, ...)`)
    /// honors `applies_to` — but every file it plants is either admitted by
    /// BOTH `applies_to` and `exclude`, or excluded from `applies_to`
    /// entirely (`f.py`), so `exclude` itself is unproven on THIS branch: a
    /// mutation dropping `self.exclude` from that call (leaving only
    /// `glob::applies(self.applies_to, &[], ...)`) would still pass every
    /// existing test — `plan_diff_rule_honors_exclude_even_when_applies_to_is_empty`
    /// only exercises the OTHER (empty-`applies_to`) branch. This plants a
    /// file that matches `applies_to` (`**/*.ts`) AND `exclude`
    /// (`**/tests/**`) at once, on a rule with a real, non-empty
    /// `applies_to` — `swallowed-error`, same rule the sibling test above
    /// uses.
    #[test]
    fn plan_diff_rule_honors_exclude_on_the_populated_applies_to_branch_too() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("src/f.ts"), "  } catch (e) {\n  }\n").unwrap();
        fs::write(dir.path().join("tests/f.ts"), "  } catch (e) {\n  }\n").unwrap();
        let diff_text = [
            "diff --git a/src/f.ts b/src/f.ts",
            "--- a/src/f.ts",
            "+++ b/src/f.ts",
            "@@ -1,2 +1,2 @@",
            "   } catch (e) {",
            "   }",
            "diff --git a/tests/f.ts b/tests/f.ts",
            "--- a/tests/f.ts",
            "+++ b/tests/f.ts",
            "@@ -1,2 +1,2 @@",
            "   } catch (e) {",
            "   }",
        ]
        .join("\n")
            + "\n";
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"b".repeat(40))], Vec::new());
        let (rules, _) = darkmux_crew::rules::load_all(None);
        let rule = rules["swallowed-error"].clone();
        assert!(!rule.applies_to.is_empty(), "the rule this test relies on must still declare applies_to");
        assert!(
            rule.exclude.iter().any(|p| p.contains("tests")),
            "the rule this test relies on must still exclude a tests/ path"
        );

        let plan = plan_diff_rule(&materialized, &rule, &diff_text, PlanParams::default()).unwrap();
        let files: Vec<&str> =
            plan.units.iter().flat_map(|u| if let Unit::Site { sites, .. } = u { sites.iter().map(|s| s.file.as_str()).collect() } else { vec![] }).collect();
        assert_eq!(
            files,
            vec!["src/f.ts"],
            "tests/f.ts matches applies_to but must still be filtered out by exclude, even on the \
             populated-applies_to branch: {files:?}"
        );
    }

    /// (#2310 P4c review round 2, MUST FIX 1 — proven) `FilteredDiffSource`
    /// used to short-circuit `if applies_to.is_empty() { return all }`,
    /// dropping `exclude` entirely for any rule with no `applies_to` glob
    /// — exactly the shape every new review-catalog rule ships (empty
    /// `applies_to` + a real `exclude` naming test paths). `test-gap`
    /// itself is the reproduction: a diff touching `src/a.ts` AND
    /// `tests/a.test.ts` must plan a site ONLY in `src/a.ts` — the
    /// excluded test file must never reach the planner, whether or not
    /// `applies_to` is empty.
    #[test]
    fn plan_diff_rule_honors_exclude_even_when_applies_to_is_empty() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("src/a.ts"), "function f() {\n  doThing();\n}\n").unwrap();
        fs::write(dir.path().join("tests/a.test.ts"), "function f() {\n  doThing();\n}\n").unwrap();
        let diff_text = [
            "diff --git a/src/a.ts b/src/a.ts",
            "--- a/src/a.ts",
            "+++ b/src/a.ts",
            "@@ -1,3 +1,3 @@",
            " function f() {",
            "   doThing();",
            " }",
            "diff --git a/tests/a.test.ts b/tests/a.test.ts",
            "--- a/tests/a.test.ts",
            "+++ b/tests/a.test.ts",
            "@@ -1,3 +1,3 @@",
            " function f() {",
            "   doThing();",
            " }",
        ]
        .join("\n")
            + "\n";
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"a".repeat(40))], Vec::new());
        let (rules, _) = darkmux_crew::rules::load_all(None);
        let rule = rules["test-gap"].clone();
        assert!(rule.applies_to.is_empty(), "test-gap declares no applies_to — that is the case this bug needs");
        assert!(!rule.exclude.is_empty(), "test-gap's exclude must be real for this test to mean anything");

        let plan = plan_diff_rule(&materialized, &rule, &diff_text, PlanParams::default()).unwrap();
        let files: Vec<&str> =
            plan.units.iter().flat_map(|u| if let Unit::Site { sites, .. } = u { sites.iter().map(|s| s.file.as_str()).collect() } else { vec![] }).collect();
        assert_eq!(files, vec!["src/a.ts"], "the excluded test file must never reach the planner: {files:?}");
    }

    /// A `read`/`edge`-kind rule has no diff-scoped meaning (DESIGN.md
    /// "Hunks are natural windows") — `plan_diff_rule` refuses rather than
    /// silently producing zero units, which would be indistinguishable
    /// from "the rule ran and found nothing".
    #[test]
    fn plan_diff_rule_refuses_a_non_site_rule() {
        let dir = TempDir::new().unwrap();
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"d".repeat(40))], Vec::new());
        let err = plan_diff_rule(&materialized, &read_rule(), "", PlanParams::default()).unwrap_err();
        assert!(err.to_string().contains("site"), "{err}");
    }

    /// (#2310 fix-loop B1, S3-1 — PROVEN live) A non-empty diff that
    /// parses to ZERO files is the loudest symptom of a dialect the
    /// parser can't read (or of a diff whose paths simply don't match the
    /// workspace). It used to plan zero units and exit green, which the
    /// operator reads as "the PR is clean" — the exact indistinguishable-
    /// from-nothing-found failure `plan_diff_rule_refuses_a_non_site_rule`
    /// above already refuses on the rule side. Refused by name instead,
    /// with the likely cause stated.
    ///
    /// After B1's parser fix a `--no-prefix` diff PARSES, so the fixture
    /// here is the other door to the same zero-files end state: a real
    /// rename-only diff, non-empty and legitimately hunk-less.
    #[test]
    fn plan_diff_rule_refuses_a_non_empty_diff_that_parses_to_zero_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.ts"), "export const x = 1;\n").unwrap();
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"f".repeat(40))], Vec::new());
        let diff_text =
            ["diff --git a/old.ts b/new.ts", "similarity index 100%", "rename from old.ts", "rename to new.ts", ""].join("\n");
        let err = plan_diff_rule(&materialized, &site_rule(), &diff_text, PlanParams::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no files"), "names the zero-file outcome: {msg}");
        assert!(msg.contains("dialect"), "names the likely cause the operator can act on: {msg}");
    }

    /// (#2310 fix-loop B1) The guard must NOT fire on an empty diff file —
    /// "nothing changed" is a legitimate, unambiguous input, and the
    /// message about diff dialects would be actively misleading there.
    #[test]
    fn plan_diff_rule_allows_a_genuinely_empty_diff() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.ts"), "export const x = 1;\n").unwrap();
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"a".repeat(40))], Vec::new());
        let plan = plan_diff_rule(&materialized, &site_rule(), "  \n\n", PlanParams::default()).unwrap();
        assert!(plan.units.is_empty(), "{:?}", plan.units);
    }

    /// (#2310 fix-loop B1) `diff_git_header_paths` read ONLY the
    /// `diff --git a/<old> b/<new>` form, so under `git diff --no-prefix`
    /// (`diff --git old new`) a rename/binary entry fell out of
    /// `diff_entries_without_hunks` and was miscounted as `out_of_scope`
    /// — the diff DID mention it. Verified shape: `git diff --cached -M
    /// --no-prefix`, 2026-09-05.
    #[test]
    fn diff_git_header_paths_reads_the_no_prefix_form_too() {
        let prefixed = diff_git_header_paths("diff --git a/src/old.ts b/src/new.ts\n");
        let bare = diff_git_header_paths("diff --git src/old.ts src/new.ts\n");
        assert_eq!(prefixed.iter().cloned().collect::<Vec<_>>(), vec!["src/new.ts".to_string()]);
        assert_eq!(
            bare.iter().cloned().collect::<Vec<_>>(),
            vec!["src/new.ts".to_string()],
            "the NEW side, same as the prefixed form"
        );
    }

    /// (#2310 P4c review round 2, MUST FIX 2 — proven) `Rule::scope` was
    /// enforced by nothing on the diff side either: a `site`-kind rule
    /// declaring `scope: ["tree"]` ONLY (no `"diff"`) could still be
    /// planned by `plan_diff_rule` with no error, silently running a
    /// tree-only rule's prose against hunks it was never written for.
    #[test]
    fn plan_diff_rule_refuses_a_rule_with_no_diff_scope() {
        let dir = TempDir::new().unwrap();
        let materialized = materialized_for(vec![diff_source_at(dir.path(), "app", &"e".repeat(40))], Vec::new());
        let mut tree_only = site_rule();
        tree_only.scope = vec![darkmux_crew::rules::RuleScope::Tree];
        let err = plan_diff_rule(&materialized, &tree_only, "", PlanParams::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&tree_only.id), "{msg}");
        assert!(msg.contains("tree"), "the message must name the rule's actual declared scope: {msg}");
    }

    /// A diff correlates to one tree at one sha — a `Materialized` with
    /// zero or several sources has no single tree `plan_diff_rule` could
    /// check the diff's line content against.
    #[test]
    fn plan_diff_rule_refuses_anything_but_exactly_one_source() {
        let dir = TempDir::new().unwrap();
        let mut zero = materialized_for(vec![diff_source_at(dir.path(), "app", &"e".repeat(40))], Vec::new());
        zero.sources.clear();
        let err = plan_diff_rule(&zero, &site_rule(), "", PlanParams::default()).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");

        let mut two = materialized_for(vec![diff_source_at(dir.path(), "app", &"e".repeat(40))], Vec::new());
        two.sources.push(two.sources[0].clone());
        let err = plan_diff_rule(&two, &site_rule(), "", PlanParams::default()).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
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

    /// (#2310 P4b review, CI MUST FIX) Rebuild a `serde_json::Value` with
    /// every object's keys inserted in SORTED order, recursively. Two
    /// reasons this exists rather than just comparing `Value == Value`
    /// directly (which is ALREADY order-independent — `serde_json::Map`'s
    /// `PartialEq`, whether it's a `BTreeMap` or an `IndexMap` under the
    /// `preserve_order` feature, compares by content, not order):
    ///
    /// 1. This IS still used for the comparison below, defensively, so the
    ///    assertion never depends on which map type is compiled in.
    /// 2. It is what makes the WRITTEN golden file byte-stable regardless
    ///    of which feature set generated it. `cargo llvm-cov --workspace`
    ///    (CI's coverage job) unifies features across the workspace,
    ///    which turns serde_json's `preserve_order` on for darkmux-lab
    ///    (`agent-client-protocol` enables it elsewhere in the tree) —
    ///    `cargo test -p darkmux-lab` alone does not. Before this fix,
    ///    `serde_json::to_string_pretty` on the raw `Plan` value emitted
    ///    keys in STRUCT-DECLARATION order under `preserve_order` and
    ///    ALPHABETICAL order without it (`serde_json::Map` defaults to a
    ///    `BTreeMap`), so the committed golden (written locally, without
    ///    the feature) read as drifted under CI's workspace build even
    ///    though nothing about the plan's CONTENT had changed. Same root
    ///    cause `step_output::body_hash`'s own canonicalizer exists to
    ///    fix (see that function's doc) — this is the same problem
    ///    surfacing in a second place.
    fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_unstable();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), canonicalize(&map[k]));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(canonicalize).collect())
            }
            other => other.clone(),
        }
    }

    /// (#2310 P4b review) The golden the brief requires: `TreeSource`'s
    /// plan output for a committed fixture tree must not drift when
    /// `collect_site_units` starts delegating to the Tier 2
    /// `plan_sites::plan_site_units` pattern. `sources[].tree` (an absolute
    /// path — differs per checkout) and `planned_at` (a timestamp) are the
    /// only two fields redacted before comparison; `sha` is already fixed
    /// by the `resolved()` test helper, so nothing else about the plan's
    /// content is test-environment-only.
    ///
    /// **Non-trivial by design** (review finding: the original fixture had
    /// one unit, one site, two merged hits — not enough to pin either
    /// sizing cap). Two files, four non-overlapping hits, `max_sites_per_
    /// unit: 2` + `max_est_tokens_per_unit: 300`: `orders.ts`'s two SMALL
    /// hits (28 tokens each) pack into one unit under both caps; `util.ts`'s
    /// first BIG hit (160 tokens) starts a new unit because adding it to
    /// the first would exceed the SITES cap (3 > 2) — pinning that cap
    /// independently of tokens; its second BIG hit (161 tokens) starts a
    /// THIRD unit because 160+161 exceeds the TOKENS cap (321 > 200) while
    /// the sites count alone (2) would still be allowed — pinning the
    /// tokens cap independently of site count. Three units from one rule
    /// is the proof neither cap is a no-op.
    ///
    /// To regenerate after a deliberate behavior change:
    /// `DARKMUX_CRAWL_PLAN_GOLDEN_UPDATE=1 cargo test -p darkmux-lab --lib
    /// crawl::plan::tests::golden_tree_source_plan_matches_committed_fixture`
    /// then review the diff before committing. Verified under BOTH
    /// `cargo test -p darkmux-lab --lib` and `cargo test --workspace --lib`
    /// (the latter is what CI's coverage job effectively exercises,
    /// feature-unification-wise) — see this module's own review notes.
    #[test]
    fn golden_tree_source_plan_matches_committed_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crawl-plan-golden");
        let sources = vec![resolved("app", &fixture_dir)];
        let materialized = materialized_for(sources, Vec::new());
        let rules = vec![site_rule()];
        let params = PlanParams { max_sites_per_unit: 2, max_est_tokens_per_unit: 300 };

        let plan = plan_with_params(&materialized, &rules, params).unwrap();
        let mut actual_value = serde_json::to_value(&plan).unwrap();
        actual_value["planned_at"] = serde_json::json!("<redacted: timestamp>");
        actual_value["sources"][0]["tree"] = serde_json::json!("<redacted: absolute checkout path>");
        let actual_canonical = canonicalize(&actual_value);
        let actual = serde_json::to_string_pretty(&actual_canonical).unwrap();

        let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/crawl-plan-golden/tree-source-site-rule.json");
        if std::env::var("DARKMUX_CRAWL_PLAN_GOLDEN_UPDATE").is_ok() {
            fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
            fs::write(&golden_path, format!("{actual}\n")).unwrap();
            return;
        }
        let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
            panic!(
                "read {} — run with DARKMUX_CRAWL_PLAN_GOLDEN_UPDATE=1 to generate it",
                golden_path.display()
            )
        });
        // (#2310 P4b review, CI MUST FIX) Compare as `serde_json::Value`
        // (semantic, order-independent equality), never as strings —
        // `preserve_order` on vs off changes what `to_string_pretty` on a
        // NON-canonicalized value would print, but never changes what this
        // parses back into. Parsing `expected` fresh (rather than trusting
        // its own on-disk key order) makes the assertion prove the CONTENT
        // matches regardless of which feature set wrote either side.
        let expected_value: serde_json::Value = serde_json::from_str(&expected)
            .unwrap_or_else(|e| panic!("golden at {} is not valid JSON: {e}", golden_path.display()));
        assert_eq!(
            actual_canonical,
            canonicalize(&expected_value),
            "TreeSource's plan output drifted from the committed golden at {}.\n\
             If this drift is an intended behavior change, regenerate with:\n\
             DARKMUX_CRAWL_PLAN_GOLDEN_UPDATE=1 cargo test -p darkmux-lab --lib \
             crawl::plan::tests::golden_tree_source_plan_matches_committed_fixture\n\
             then review the diff before committing.",
            golden_path.display()
        );
        // Sanity: the fixture must actually exercise both sizing caps, or
        // a broken fixture could pass this golden vacuously (review
        // finding: the original fixture had exactly one unit).
        assert_eq!(plan.units.len(), 3, "expected the sites-cap split AND the tokens-cap split to both fire: {:?}", plan.units.iter().map(|u| u.id()).collect::<Vec<_>>());
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

    // ─── #2310 P4c-2b item 5: review-v2 fixture + harness ──────────────

    /// Scopes `DARKMUX_HOME` for one test and restores the prior value —
    /// same pattern `crawl::unit_step_tests::HomeGuard` uses.
    struct ReviewV2HomeGuard(Option<String>);
    impl ReviewV2HomeGuard {
        fn set(p: &Path) -> Self {
            let prior = std::env::var("DARKMUX_HOME").ok();
            std::env::set_var("DARKMUX_HOME", p);
            Self(prior)
        }
    }
    impl Drop for ReviewV2HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    fn review_v2_fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/review-v2")
    }

    fn review_v2_diff_text() -> String {
        fs::read_to_string(review_v2_fixture_dir().join("diff.patch")).unwrap()
    }

    /// (#2310 P4c-2b item 5) `review-v2`'s own fixture + harness: five
    /// rules PLAN for real over the checked-in fixture tree + diff (the
    /// two rules review-v2 reuses from crawl, `swallowed-error`/
    /// `unnamed-predicate` — DESIGN.md's confirm=`mod` pair — plus the
    /// three P4c-2b names explicitly: `existing-solution` (confirm=
    /// `question`), `union-vs-enum` and `shared-symbol-callers` (both
    /// confirm=`search`)); then, with STUB finding/mod producers (no
    /// model, no container — the same discipline `deliver_github_review`'s
    /// own golden test uses), one finding per rule feeds `records.gather`
    /// → `deliver.github_review` (the SAME same-task-predecessor `input`
    /// wiring `review-v2.json`'s `deliver` phase uses) and the rendered
    /// payload is compared byte-for-byte against a committed golden.
    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn review_v2_fixture_plans_every_rule_and_delivers_one_comment_per_form() {
        use darkmux_crew::step_kinds::StepKind as _;
        let home = TempDir::new().unwrap();
        let _g = ReviewV2HomeGuard::set(home.path());
        const MISSION: &str = "review-v2-2310-fixture";
        const PHASE: &str = "review-v2-2310-fixture-deliver";
        darkmux_crew::lifecycle::save_phase(&darkmux_crew::types::Phase {
            id: PHASE.into(),
            mission_id: MISSION.into(),
            description: String::new(),
            display_name: None,
            status: darkmux_crew::types::PhaseStatus::Running,
            created_ts: 1,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: vec!["deliver".into()],
        })
        .unwrap();

        // ── plan every rule for real, over the fixture ─────────────────
        let tree = review_v2_fixture_dir().join("tree");
        let diff_text = review_v2_diff_text();
        let materialized = materialized_for(vec![diff_source_at(&tree, "app", &"f".repeat(40))], Vec::new());
        let (rules, _) = darkmux_crew::rules::load_all(None);
        for rule_id in ["existing-solution", "union-vs-enum", "shared-symbol-callers", "swallowed-error", "unnamed-predicate"] {
            let rule = rules.get(rule_id).unwrap_or_else(|| panic!("rule {rule_id} must be registered"));
            let plan = plan_diff_rule(&materialized, rule, &diff_text, PlanParams::default())
                .unwrap_or_else(|e| panic!("planning {rule_id}: {e:#}"));
            assert!(
                !plan.units.is_empty(),
                "rule {rule_id} must plan at least one site over the fixture diff — the mechanical prefilter/window step found nothing"
            );
            // Written to disk in the SAME envelope + path `plan.sites`
            // writes for a real launch (`<missions_dir>/<mission>/plan/
            // <rule>.json`) — `records.gather`'s own `plan_totals` reads
            // this to compute `scope.rules_run`/`hunks_covered`.
            let wrapped = darkmux_crew::step_output::Output::wrap(
                crate::crawl::plan_step::CRAWL_PLAN_OUTPUT_KIND,
                plan,
                darkmux_crew::step_output::Producer::of(MISSION, "plan-task", "plan-step"),
            );
            let plan_path =
                darkmux_crew::loader::missions_dir().join(MISSION).join("plan").join(format!("{rule_id}.json"));
            crate::crawl::plan_step::write_plan(&plan_path, &wrapped).unwrap();
        }

        // ── stub finding + mod producers — one finding per rule, one per delivery form ──
        let findings_root = darkmux_crew::findings::findings_dir();
        let mods_root = darkmux_crew::mods::mods_dir();
        let mk_finding = |dispatch: &str, seq: u64, rule: &str, confirm: Option<&str>, file: &str, line: u32, evidence: &str, why: &str| {
            let mut context = serde_json::json!({ "rule": rule });
            if let Some(c) = confirm {
                context["confirm"] = serde_json::json!(c);
            }
            let rec = darkmux_crew::findings::build_record(
                dispatch,
                seq,
                "2026-09-05T00:00:00Z".to_string(),
                "create_finding",
                darkmux_crew::findings::Proposer { handle: "reviewer".into(), model: "test".into(), machine_id: None },
                darkmux_crew::findings::Scope { mission_id: Some(MISSION.into()), phase_id: Some(PHASE.into()), step_id: None },
                Some(context),
                serde_json::json!({ "file": file, "line": line, "pattern": rule, "evidence": evidence, "why": why }),
            );
            darkmux_crew::findings::materialize(&findings_root, &rec).unwrap();
            rec.key
        };

        // swallowed-error (confirm=mod) — a GATE-PASSED, in-diff unified-diff
        // mod, so it renders as an inline suggestion.
        let se_key = mk_finding(
            "sess-fix",
            1,
            "swallowed-error",
            None,
            "src/billing.ts",
            5,
            "} catch (e) {",
            "the catch block swallows the error",
        );
        // unnamed-predicate (confirm=mod), gate-FAILED — a double-check bullet.
        let up_key = mk_finding(
            "sess-fix",
            2,
            "unnamed-predicate",
            None,
            "src/auth.ts",
            3,
            "if ((user.role === \"admin\" ...",
            "the condition is hard to read at a glance",
        );
        // existing-solution (confirm=question) — a question bullet.
        let es_key = mk_finding(
            "sess-fix",
            3,
            "existing-solution",
            Some("question"),
            "src/retry.ts",
            3,
            "export function retryWithBackoff",
            "did you check for an existing retry helper",
        );
        // union-vs-enum (confirm=search) — a search bullet. The search
        // form's rendered bullet reads `evidence` (the candidates found),
        // never `why` — see `render_github_review`'s `DeliveryForm::Search`
        // arm.
        let uve_key = mk_finding(
            "sess-fix",
            4,
            "union-vs-enum",
            Some("search"),
            "src/status.ts",
            3,
            "LogLevel in types.ts overlaps this new union",
            "a new string-literal union may duplicate an existing enum",
        );
        // shared-symbol-callers (confirm=search) — a second search bullet.
        let ssc_key = mk_finding(
            "sess-fix",
            5,
            "shared-symbol-callers",
            Some("search"),
            "src/shared.ts",
            4,
            "3 callers found: caller_a.ts, caller_b.ts, caller_c.ts",
            "a shared function's signature changed",
        );

        const CLAMP_KIT: &str = "diff --git a/src/billing.ts b/src/billing.ts\n--- a/src/billing.ts\n+++ b/src/billing.ts\n@@ -5,1 +5,1 @@\n-  } catch (e) {\n+  } catch (e) {\n+    console.error(e);\n";
        let mk_mod = |key: &str, for_key: &str, kit: &str, kit_kind: Option<&str>, gate: Option<bool>| {
            let rec = darkmux_crew::mods::ModRecord {
                key: key.to_string(),
                ts: "2026-09-05T00:00:01Z".to_string(),
                by: "coder".to_string(),
                r#for: vec![for_key.to_string()],
                kit: Some(kit.to_string()),
                kit_looks_json: false,
                kit_kind: kit_kind.map(str::to_string),
                attachments: Vec::new(),
                context: darkmux_crew::mods::ModContext {
                    findings: vec![darkmux_crew::mods::ForFinding {
                        key: for_key.to_string(),
                        mission_id: Some(MISSION.into()),
                        context: None,
                        emitted: None,
                        missing: false,
                    }],
                },
                warnings: Vec::new(),
                mission_id: Some(MISSION.into()),
                phase_id: Some(PHASE.into()),
                step_id: None,
                gate: None,
                gate_skipped_reason: None,
                schema_version: darkmux_crew::mods::MOD_SCHEMA_VERSION.to_string(),
                extras: Default::default(),
            };
            darkmux_crew::mods::materialize(&mods_root, &rec).unwrap();
            if let Some(passed) = gate {
                darkmux_crew::mods::record_gate(
                    &mods_root,
                    key,
                    Some(darkmux_crew::mods::GateOutcome { passed, command: "true".into(), exit_code: Some(if passed { 0 } else { 1 }), applied: Some(true), reason: None }),
                    None,
                )
                .unwrap();
            }
        };
        mk_mod("mod-se", &se_key, CLAMP_KIT, Some("unified-diff"), Some(true));
        mk_mod("mod-up", &up_key, "a proposed fix nobody sees", None, Some(false));

        // ── records.gather then deliver.github_review, same two-step-task wiring `review-v2.json` uses ──
        let gather_step = darkmux_crew::types::Step {
            id: "records-gather-step".into(),
            task_id: "deliver".into(),
            kind: darkmux_crew::step_kinds::RECORDS_GATHER_KIND.into(),
            gate: None,
            status: darkmux_crew::types::NodeStatus::Planned,
            config: serde_json::json!({ "diff_file": review_v2_fixture_dir().join("diff.patch").to_string_lossy() }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let task = darkmux_crew::types::Task {
            id: "deliver".into(),
            phase_id: PHASE.into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["records-gather-step".into(), "deliver-step".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
            run_on: darkmux_crew::types::default_run_on(),
        };
        let gather_out =
            darkmux_crew::step_kinds::RecordsGatherStepKind.run(&gather_step, &task, &std::collections::BTreeMap::new()).unwrap();
        let mut input = std::collections::BTreeMap::new();
        input.insert("records-gather-step".to_string(), gather_out.output);

        let emit_path = home.path().join("review-payload.json");
        let deliver_step = darkmux_crew::types::Step {
            id: "deliver-step".into(),
            task_id: "deliver".into(),
            kind: darkmux_crew::step_kinds::DELIVER_GITHUB_REVIEW_KIND.into(),
            gate: None,
            status: darkmux_crew::types::NodeStatus::Planned,
            config: serde_json::json!({
                "emit": emit_path.to_string_lossy(),
                "attribution": "darkmux review-v2 — advisory, not a merge gate."
            }),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        darkmux_crew::step_kinds::DeliverGithubReviewStepKind.run(&deliver_step, &task, &input).unwrap();
        let payload: darkmux_crew::step_kinds::DeliverOutcome =
            serde_json::from_str(&fs::read_to_string(&emit_path).unwrap()).unwrap();
        assert_eq!(payload.mode, "review");
        let review = payload.review.clone().unwrap();

        // one comment per delivery form, plus the scope line naming every rule run
        assert_eq!(review.comments.len(), 1, "exactly one in-diff suggestion (swallowed-error's gated mod): {review:?}");
        assert!(review.body.contains("review ran: 5 rule(s)"), "{}", review.body);
        assert!(review.body.contains(&up_key), "gate-failed unnamed-predicate mod renders as a double-check: {}", review.body);
        assert!(review.body.contains(&es_key), "existing-solution renders as a question: {}", review.body);
        assert!(review.body.contains("did you check for an existing retry helper"));
        assert!(review.body.contains(&uve_key), "union-vs-enum renders as a search bullet: {}", review.body);
        assert!(review.body.contains(&ssc_key), "shared-symbol-callers renders as a search bullet: {}", review.body);
        assert!(review.body.contains("3 callers found"));

        let actual = serde_json::to_string_pretty(&payload).unwrap();
        let golden_path = review_v2_fixture_dir().join("golden-payload.json");
        if std::env::var("DARKMUX_REVIEW_V2_GOLDEN_UPDATE").is_ok() {
            fs::write(&golden_path, format!("{actual}\n")).unwrap();
        } else {
            let expected = fs::read_to_string(&golden_path).unwrap_or_else(|_| {
                panic!("read {} — run with DARKMUX_REVIEW_V2_GOLDEN_UPDATE=1 to generate it", golden_path.display())
            });
            assert_eq!(
                actual.trim_end(),
                expected.trim_end(),
                "the rendered payload drifted from the committed golden at {}",
                golden_path.display()
            );
        }
    }
}
