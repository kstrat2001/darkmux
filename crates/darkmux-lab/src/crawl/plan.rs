//! Crawl planning (#1959 packet 1) — turns a resolved corpus (manifest +
//! rules + checked-out sources) into a deterministic `Plan` of work units
//! with token estimates. NO model dispatch happens here; this is the
//! mechanical, free-to-compute half of the crawler (prefilters, globs, the
//! npm range check) that the (future) dispatch loop consumes.

use crate::crawl::glob;
use crate::crawl::manifest::CorpusManifest;
use crate::crawl::rules::{Rule, RuleKind};
use crate::crawl::semver::range_admits;
use crate::crawl::sources::ResolvedSource;
use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const PLAN_SCHEMA_VERSION: &str = "1.0";

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
/// whichever first".
pub const MAX_SITES_PER_UNIT: usize = 40;
pub const MAX_SITE_TOKENS_PER_UNIT: usize = 16_000;
/// Edge units cap at 80 sites (spilling into more units beyond that).
pub const MAX_EDGE_SITES_PER_UNIT: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub start: usize,
    pub end: usize,
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

#[derive(Debug, Clone, Serialize)]
pub struct PlanSource {
    pub id: String,
    pub sha: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub tree: PathBuf,
    /// Total regular files found under this source's tree (before any
    /// rule's glob filtering) — the CLI table's `files_walked` column.
    pub files_walked: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedEntry {
    pub reason: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Every manifest edge this plan attempted, whether or not it produced a
/// unit — the ledger that shows an edge WAS checked even when its range
/// admits the library version (or the check was inconclusive).
#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize, Default)]
pub struct RuleTotal {
    pub units: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sites: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<usize>,
    pub est_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Totals {
    pub units: usize,
    pub est_tokens: usize,
    pub by_rule: BTreeMap<String, RuleTotal>,
    pub skipped: Vec<SkippedEntry>,
    pub edges: Vec<EdgeLedgerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub schema_version: String,
    pub corpus: String,
    pub planned_at: String,
    pub sources: Vec<PlanSource>,
    pub units: Vec<Unit>,
    pub totals: Totals,
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
    fn new(tree: &Path) -> Result<Self> {
        let mut all = Vec::new();
        walk_relative(tree, tree, &mut all)?;
        all.sort();
        Ok(Self {
            tree: tree.to_path_buf(),
            all,
            content: HashMap::new(),
        })
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

fn walk_relative(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_relative(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"));
        }
        // symlinks: skipped (neither a plain file to read nor a dir to walk).
    }
    Ok(())
}

/// Merge 1-indexed hit line numbers into `(start, end, first_hit_line)`
/// spans using `window` lines of context on each side, clamped to
/// `[1, total_lines]`. `hits` must be sorted ascending (callers build it
/// that way via a single forward line scan). Overlapping/adjacent spans
/// merge into one — "overlapping windows in the same file count once".
fn merge_windows(hits: &[usize], window: usize, total_lines: usize) -> Vec<(usize, usize, usize)> {
    let clamp_end = total_lines.max(1);
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for &h in hits {
        let s = h.saturating_sub(window).max(1);
        let e = (h + window).min(clamp_end);
        match spans.last_mut() {
            Some(last) if s <= last.1 + 1 => {
                last.1 = last.1.max(e);
            }
            _ => spans.push((s, e, h)),
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

/// Plan a resolved corpus. Deterministic for a fixed set of trees: files
/// are walked once and sorted; unit ids are assigned sequentially in a
/// fixed pass order (every `site` rule, then the combined `read` pass per
/// source, then every `edge` rule).
pub fn plan(manifest: &CorpusManifest, rules: &[Rule], sources: &[ResolvedSource]) -> Result<Plan> {
    let mut files_by_id: BTreeMap<String, SourceFiles> = BTreeMap::new();
    for s in sources {
        files_by_id.insert(s.id.clone(), SourceFiles::new(&s.tree)?);
    }

    let mut skipped: Vec<SkippedEntry> = Vec::new();
    let mut units: Vec<Unit> = Vec::new();
    let mut unit_seq: usize = 1;

    // --- site rules ---
    for rule in rules.iter().filter(|r| r.kind == RuleKind::Site) {
        for source in sources {
            let files = files_by_id.get_mut(&source.id).expect("source resolved");
            units.extend(collect_site_units(rule, source, files, &mut skipped, &mut unit_seq)?);
        }
    }

    // --- read pass (all read rules share one pass per source) ---
    let read_rules: Vec<&Rule> = rules.iter().filter(|r| r.kind == RuleKind::Read).collect();
    if !read_rules.is_empty() {
        for source in sources {
            let files = files_by_id.get_mut(&source.id).expect("source resolved");
            units.extend(collect_read_units(&read_rules, source, files, &mut skipped, &mut unit_seq)?);
        }
    }

    // --- edge rules ---
    let mut edge_ledger: Vec<EdgeLedgerEntry> = Vec::new();
    let sources_by_id: BTreeMap<String, &ResolvedSource> =
        sources.iter().map(|s| (s.id.clone(), s)).collect();
    for rule in rules.iter().filter(|r| r.kind == RuleKind::Edge) {
        for edge in &manifest.edges {
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
                        "ecosystem '{ecosystem}' not supported by rule '{}' — packet 1 supports npm only",
                        rule.id
                    )),
                });
                continue;
            }

            let consumer_pkg = read_package_json(&sources_by_id[&edge.consumer].tree);
            let library_pkg = read_package_json(&library.tree);
            let pinned = consumer_pkg.as_ref().ok().and_then(|v| pinned_range(v, &edge.package));
            let lib_version = library_pkg.as_ref().ok().and_then(package_version);

            let (admits, note) = match (&pinned, &lib_version) {
                (Some(p), Some(v)) => match range_admits(p, v) {
                    Some(b) => (Some(b), None),
                    None => (None, Some(format!("unsupported range syntax '{p}' — skipped"))),
                },
                (None, _) => (
                    None,
                    Some(format!(
                        "package '{}' not found in {}'s dependencies/devDependencies",
                        edge.package, edge.consumer
                    )),
                ),
                (_, None) => (
                    None,
                    Some(format!("no `version` found in {}'s package.json", edge.library)),
                ),
            };

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

            let files = files_by_id.get_mut(&edge.consumer).expect("consumer resolved");
            let hits = collect_edge_import_sites(rule, &edge.package, files, &mut skipped, &edge.consumer)?;
            for batch in hits.chunks(MAX_EDGE_SITES_PER_UNIT) {
                let est_tokens: usize = batch.iter().map(|(_, tok)| *tok).sum();
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
                    est_tokens,
                });
            }
        }
    }

    let plan_sources: Vec<PlanSource> = sources
        .iter()
        .map(|s| PlanSource {
            id: s.id.clone(),
            sha: s.sha.clone(),
            git_ref: s.git_ref.clone(),
            tree: s.tree.clone(),
            files_walked: files_by_id.get(&s.id).map(|f| f.all.len()).unwrap_or(0),
        })
        .collect();

    let totals = compute_totals(&units, skipped, edge_ledger);

    Ok(Plan {
        schema_version: PLAN_SCHEMA_VERSION.to_string(),
        corpus: manifest.name.clone(),
        planned_at: darkmux_flow::ts_utc_now(),
        sources: plan_sources,
        units,
        totals,
    })
}

fn compute_totals(units: &[Unit], skipped: Vec<SkippedEntry>, edges: Vec<EdgeLedgerEntry>) -> Totals {
    let mut by_rule: BTreeMap<String, RuleTotal> = BTreeMap::new();
    for unit in units {
        match unit {
            Unit::Site { rule, sites, est_tokens, .. } => {
                let t = by_rule.entry(rule.clone()).or_default();
                t.units += 1;
                *t.sites.get_or_insert(0) += sites.len();
                t.est_tokens += est_tokens;
            }
            Unit::Read { rules, files, est_tokens, .. } => {
                for rid in rules {
                    let t = by_rule.entry(rid.clone()).or_default();
                    t.units += 1;
                    *t.files.get_or_insert(0) += files.len();
                    t.est_tokens += est_tokens;
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
    source: &ResolvedSource,
    files: &mut SourceFiles,
    skipped: &mut Vec<SkippedEntry>,
    unit_seq: &mut usize,
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
        for (s, e, first) in merge_windows(&hits, window, total) {
            let tokens = estimate_tokens(&window_text(&lines, s, e));
            batch.push((Site { file: rel.clone(), line: first, start: s, end: e }, tokens));
        }
    }

    let mut out = Vec::new();
    let mut cur_sites: Vec<Site> = Vec::new();
    let mut cur_tokens = 0usize;
    for (site, tok) in batch {
        let would_exceed_count = cur_sites.len() + 1 > MAX_SITES_PER_UNIT;
        let would_exceed_tokens = !cur_sites.is_empty() && cur_tokens + tok > MAX_SITE_TOKENS_PER_UNIT;
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
    source: &ResolvedSource,
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
            .unwrap_or(crate::crawl::rules::DEFAULT_READ_CHUNK_TOKENS);

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
    // `from "<pkg>"` / `from "<pkg>/subpath"` / `require("<pkg>...")`.
    let pattern = format!(r#"from\s*['"]{esc}(?:/|['"])|require\(\s*['"]{esc}"#);
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
        for (s, e, first) in merge_windows(&hits, window, total) {
            let tokens = estimate_tokens(&window_text(&lines, s, e));
            out.push((Site { file: rel.clone(), line: first, start: s, end: e }, tokens));
        }
    }
    Ok(out)
}

fn read_package_json(tree: &Path) -> Result<serde_json::Value> {
    let path = tree.join("package.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn pinned_range(pkg: &serde_json::Value, package: &str) -> Option<String> {
    pkg.get("dependencies")
        .and_then(|d| d.get(package))
        .and_then(|v| v.as_str())
        .or_else(|| pkg.get("devDependencies").and_then(|d| d.get(package)).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

fn package_version(pkg: &serde_json::Value) -> Option<String> {
    pkg.get("version").and_then(|v| v.as_str()).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::rules::EdgeRuleConfig;
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

    fn resolved(id: &str, tree: &Path) -> ResolvedSource {
        ResolvedSource {
            id: id.to_string(),
            sha: "deadbeef".to_string(),
            git_ref: "main".to_string(),
            tree: tree.to_path_buf(),
        }
    }

    fn manifest_with_edge(app: &str, lib: &str, package: &str) -> CorpusManifest {
        let json = serde_json::json!({
            "name": "t",
            "sources": [
                {"id": app, "path": "/x", "ref": "main"},
                {"id": lib, "path": "/y", "ref": "main"}
            ],
            "edges": [{"consumer": app, "library": lib, "package": package}]
        });
        serde_json::from_value(json).unwrap()
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

        let mut files = SourceFiles::new(dir.path()).unwrap();
        let source = resolved("app", dir.path());
        let mut skipped = Vec::new();
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq).unwrap();
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

        let mut files = SourceFiles::new(dir.path()).unwrap();
        let source = resolved("app", dir.path());
        let mut skipped = Vec::new();
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq).unwrap();
        assert_eq!(units.len(), 2, "expected a 40 + 1 split: {units:?}");
        let Unit::Site { sites: s0, .. } = &units[0] else { panic!() };
        let Unit::Site { sites: s1, .. } = &units[1] else { panic!() };
        assert_eq!(s0.len(), 40);
        assert_eq!(s1.len(), 1);
    }

    #[test]
    fn read_rule_groups_whole_files_and_splits_oversized_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.ts"), "x".repeat(20)).unwrap(); // 20 chars ~5 tokens
        fs::write(dir.path().join("README.md"), "y".repeat(400)).unwrap(); // 400 chars ~100 tokens > chunk_tokens=50

        let mut files = SourceFiles::new(dir.path()).unwrap();
        let source = resolved("app", dir.path());
        let mut skipped = Vec::new();
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
            serde_json::json!({"version": "8.1.1"}).to_string(),
        )
        .unwrap();
        fs::write(
            app_dir.join("uses-lib.ts"),
            "import { thing } from '@org/lib';\nthing();\n",
        )
        .unwrap();

        let mut files_by_id: BTreeMap<String, SourceFiles> = BTreeMap::new();
        files_by_id.insert("app".to_string(), SourceFiles::new(&app_dir).unwrap());
        files_by_id.insert("lib".to_string(), SourceFiles::new(&lib_dir).unwrap());
        let sources = vec![resolved("app", &app_dir), resolved("lib", &lib_dir)];
        let manifest = manifest_with_edge("app", "lib", "@org/lib");
        let rules = vec![edge_rule()];

        let out = plan(&manifest, &rules, &sources).unwrap();
        let edge_units: Vec<&Unit> = out.units.iter().filter(|u| matches!(u, Unit::Edge { .. })).collect();
        assert_eq!(edge_units.len(), 1, "{:?}", out.units);
        let Unit::Edge { range_admits, sites, pinned, library_version, .. } = edge_units[0] else { panic!() };
        assert!(!range_admits);
        assert_eq!(sites.len(), 1);
        assert_eq!(pinned, "^5.5.0");
        assert_eq!(library_version, "8.1.1");

        assert_eq!(out.totals.edges.len(), 1);
        assert_eq!(out.totals.edges[0].range_admits, Some(false));
        let _ = files_by_id; // constructed above to mirror plan()'s own per-source cache shape
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
        let manifest = manifest_with_edge("app", "lib", "@org/lib");
        let rules = vec![edge_rule()];

        let out = plan(&manifest, &rules, &sources).unwrap();
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

        let mut files = SourceFiles::new(dir.path()).unwrap();
        let source = resolved("app", dir.path());
        let mut skipped = Vec::new();
        let mut seq = 1usize;
        let _ = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq).unwrap();
        let reasons: Vec<&str> = skipped.iter().map(|s| s.reason.as_str()).collect();
        assert!(reasons.iter().any(|r| r.contains("exceeds")), "{reasons:?}");
        assert!(reasons.iter().any(|r| r.contains("UTF-8")), "{reasons:?}");
    }

    #[test]
    fn glob_matcher_mutation_excludes_pattern_must_actually_filter() {
        // Mutation self-check target: if `applies` stopped honoring `exclude`,
        // this must fail.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        fs::write(dir.path().join("node_modules/pkg.ts"), "catch (e) {}").unwrap();
        fs::write(dir.path().join("real.ts"), "catch (e) {}").unwrap();

        let mut files = SourceFiles::new(dir.path()).unwrap();
        let source = resolved("app", dir.path());
        let mut skipped = Vec::new();
        let mut seq = 1usize;
        let units = collect_site_units(&site_rule(), &source, &mut files, &mut skipped, &mut seq).unwrap();
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
