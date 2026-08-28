//! `darkmux crawl` command handlers (#1959 packet 1) — the CLI twin of
//! `darkmux_lab::crawl`'s manifest/rules/sources/plan machinery. Split out
//! of `main.rs` alongside `fleet_cli`/`lab_cli`, matching the established
//! per-family module convention.

use anyhow::{Context, Result};
use darkmux_lab::crawl::{manifest::CorpusManifest, plan, rules, sources};
use darkmux_types::style;
use std::path::PathBuf;

use crate::cli::CrawlCmd;

pub(crate) fn cmd_crawl(sub: CrawlCmd) -> Result<i32> {
    match sub {
        CrawlCmd::Plan { manifest, out, no_fetch, json } => cmd_crawl_plan(&manifest, out, no_fetch, json),
    }
}

fn cmd_crawl_plan(manifest_path: &std::path::Path, out: Option<PathBuf>, no_fetch: bool, json: bool) -> Result<i32> {
    let manifest = CorpusManifest::load(manifest_path)
        .with_context(|| format!("loading corpus manifest {}", manifest_path.display()))?;

    let (rules, rule_warnings) = rules::resolve_default(&manifest.rules)?;
    for w in &rule_warnings {
        eprintln!("{}", style::warn(w));
    }

    let resolved = sources::resolve(&manifest, !no_fetch)
        .with_context(|| format!("resolving sources for corpus '{}'", manifest.name))?;

    let the_plan = plan::plan(&manifest, &rules, &resolved)
        .with_context(|| format!("planning corpus '{}'", manifest.name))?;

    let out_path = out.unwrap_or_else(|| manifest.resolved_root().join("plan.json"));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let plan_json = serde_json::to_string_pretty(&the_plan)?;
    std::fs::write(&out_path, &plan_json)
        .with_context(|| format!("writing plan to {}", out_path.display()))?;

    if json {
        println!("{plan_json}");
        return Ok(0);
    }

    print_plan_table(&the_plan, &out_path);
    Ok(0)
}

fn print_plan_table(the_plan: &plan::Plan, out_path: &std::path::Path) {
    println!("{}", style::header(&format!("darkmux crawl plan — {}", the_plan.corpus)));
    println!("{}", style::dim(&format!("planned_at: {}", the_plan.planned_at)));
    println!("{}", style::dim(&format!("written to: {}", out_path.display())));
    println!();

    println!("{}", style::header("sources"));
    if the_plan.sources.is_empty() {
        println!("  (no sources)");
    } else {
        for s in &the_plan.sources {
            let short_sha = &s.sha[..s.sha.len().min(8)];
            println!(
                "  {:<16} {:<10} files_walked={}",
                s.id, short_sha, s.files_walked
            );
        }
    }
    println!();

    println!("{}", style::header("by rule"));
    if the_plan.totals.by_rule.is_empty() {
        println!("  (no rules matched anything)");
    } else {
        for (rule_id, t) in &the_plan.totals.by_rule {
            let extent = match (t.sites, t.files) {
                (Some(n), _) => format!("sites={n}"),
                (_, Some(n)) => format!("files={n}"),
                _ => "extent=0".to_string(),
            };
            println!(
                "  {:<24} units={:<4} {:<14} est_tokens={}",
                rule_id, t.units, extent, t.est_tokens
            );
        }
    }
    println!();

    // The load-bearing line: a plan that matched nothing must say so
    // loudly, not print an empty section that reads as success-by-silence.
    println!(
        "{}",
        style::header(&format!(
            "totals: {} units, {} est_tokens",
            the_plan.totals.units, the_plan.totals.est_tokens
        ))
    );

    if the_plan.totals.skipped.is_empty() {
        println!("  skipped: (none)");
    } else {
        println!("  skipped: {} files", the_plan.totals.skipped.len());
        for s in &the_plan.totals.skipped {
            println!("    {} — {}", s.file, s.reason);
        }
    }

    if the_plan.totals.edges.is_empty() {
        println!("  edges: (none)");
    } else {
        println!("  edges: {} checked", the_plan.totals.edges.len());
        for e in &the_plan.totals.edges {
            let admits = match e.range_admits {
                Some(true) => "admits",
                Some(false) => "STALE",
                None => "unknown",
            };
            println!(
                "    {} -> {} ({}) [{admits}]{}",
                e.consumer,
                e.library,
                e.package,
                e.note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default()
            );
        }
    }
}
