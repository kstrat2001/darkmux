//! `darkmux run inspect|stats|compare`: the recorded-run verbs that read a
//! run's on-disk record. Only lab runs leave one (manifest, trajectory,
//! verify outcome under the lab dir), so each verb refuses, naming where to
//! look instead, when handed the id of a mission or dispatch run.
//! `run list` is the cross-kind union and lives in `run_list.rs`.

use anyhow::{bail, Result};
use darkmux_serve::RunKind;

use crate::cli::RunFamilyCmd;
use crate::lab;
use crate::workloads;

/// `darkmux run <verb>`: `list` is the cross-kind union, the rest read a lab
/// run's record.
pub(crate) fn cmd_run(sub: RunFamilyCmd) -> Result<i32> {
    match sub {
        RunFamilyCmd::List { kind, limit, all, usage, since, json } => {
            crate::run_list::run(kind, limit, all, json.json, usage, since.as_deref())
        }
        RunFamilyCmd::Inspect { run } => cmd_inspect(&run),
        RunFamilyCmd::Stats { runs, baseline, json } => cmd_stats(&runs, &baseline, json.json),
        RunFamilyCmd::Compare { run_a, run_b } => cmd_compare(&run_a, &run_b),
    }
}

/// `run inspect <run>`.
fn cmd_inspect(run: &str) -> Result<i32> {
    lab_records_only("inspect", &[run])?;
    inspect_run(run)
}

/// `run stats <run>... [--baseline <run>...]`.
fn cmd_stats(runs: &[String], baseline: &[String], json: bool) -> Result<i32> {
    let ids: Vec<&str> = runs.iter().chain(baseline).map(String::as_str).collect();
    lab_records_only("stats", &ids)?;
    stats_runs(runs, baseline, json)
}

/// `run compare <a> <b>`.
fn cmd_compare(run_a: &str, run_b: &str) -> Result<i32> {
    lab_records_only("compare", &[run_a, run_b])?;
    let result = lab::compare::lab_compare(run_a, run_b)?;
    for n in &result.notes {
        println!("{n}");
    }
    Ok(0)
}

/// The gate every verb here passes first: no id names a mission or dispatch
/// run.
fn lab_records_only(verb: &str, ids: &[&str]) -> Result<()> {
    let missing: Vec<&str> = ids.iter().copied().filter(|id| !is_lab_record(id)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let flows_dir = darkmux_types::config_access::flows_dir();
    let lab_dir = darkmux_types::config_access::lab_dir();
    let runs = darkmux_serve::build_runs(&flows_dir, Some(&lab_dir), &[]);
    for id in missing {
        if let Some(run) = runs.iter().find(|r| r.id == id) {
            if let Some(refusal) = refusal_for(verb, id, run.kind) {
                bail!("{refusal}");
            }
        }
    }
    Ok(())
}

/// A lab run's record is a directory: a path the caller gave, or an id under
/// the lab dir (the same resolution `inspect` reads through).
fn is_lab_record(id: &str) -> bool {
    lab::inspect::resolve_run_path(id).exists()
}

/// The refusal for a run of `kind`, or `None` for a lab run.
fn refusal_for(verb: &str, id: &str, kind: RunKind) -> Option<String> {
    let (noun, instead) = match kind {
        RunKind::Lab => return None,
        RunKind::Mission => ("mission", "`darkmux mission status`"),
        RunKind::Dispatch => ("dispatch", "`darkmux run list --kind dispatch`"),
    };
    Some(format!(
        "`{id}` is a {noun} run; `darkmux run {verb}` reads lab runs only (a mission or \
         dispatch run leaves no manifest to read). See it with {instead}."
    ))
}

fn inspect_run(run: &str) -> Result<i32> {
    let report = lab::inspect::lab_inspect(run)?;
    print_inspection(&report);
    Ok(0)
}

fn print_inspection(report: &workloads::types::InspectionReport) {
    for line in inspection_lines(report) {
        println!("{line}");
    }
}

/// What `run show` prints for a lab run. A note or a verify detail can quote
/// what the run's own commands printed, which the model's code wrote, so
/// every text field is escaped (`escape_text`): an escape sequence in it is
/// shown, never acted on.
fn inspection_lines(report: &workloads::types::InspectionReport) -> Vec<String> {
    use darkmux_types::user_files::escape_text as shown;
    let mut lines = vec![
        format!("run:         {}", shown(&report.run_id)),
        format!("workload:    {}", shown(&report.workload_id)),
        format!("wall:        {}s", report.walltime_ms / 1000),
    ];
    // (#2094 finding 7) Shown next to wall so a rested run's wall clock is
    // never misread as a slow model. Milliseconds, because a seconds display
    // can round a real rest down to "0s". `0` means the run predates the
    // field or took no rests.
    if report.rest_ms > 0 {
        lines.push(format!("rest:        {}ms", report.rest_ms));
    }
    lines.push(format!("turns:       {}", report.turns));
    lines.push(format!("compactions: {}", report.compactions));
    lines.push(format!("verify:      {}", shown(&verify_line(report.verify.as_ref()))));
    lines.extend(report.mode.map(|m| {
        let word = match m {
            workloads::types::RunMode::Fast => "fast",
            workloads::types::RunMode::Slow => "slow",
        };
        format!("mode:        {word}")
    }));
    lines.push("notes:".to_string());
    lines.extend(report.notes.iter().map(|n| format!("  - {}", shown(n))));
    lines
}

/// (#2494) The workload's OWN result, distinct from the dispatch path's `ok`.
/// "not checked" is said in words: an omitted line would read as a pass.
fn verify_line(verify: Option<&workloads::types::VerifyReport>) -> String {
    match verify {
        Some(v) if v.passed => "ok".to_string(),
        Some(v) if v.details.is_empty() => "FAILED".to_string(),
        Some(v) => format!("FAILED: {}", v.details),
        None => "not checked".to_string(),
    }
}

/// (#2855) `run stats`: one run alone prints the single-run view; a set,
/// or one run with a baseline, prints the set view.
fn stats_runs(runs: &[String], baseline: &[String], json: bool) -> Result<i32> {
    use lab::stats_render as render;
    if runs.len() == 1 && baseline.is_empty() {
        let s = lab::stats::run_stats(&runs[0])?;
        if json {
            crate::cli_json::emit(&s)?;
        } else {
            print!("{}", render::run_text(&s));
        }
        return Ok(0);
    }
    let cand = render::load_set(runs);
    let base = (!baseline.is_empty()).then(|| render::load_set(baseline));
    if json {
        crate::cli_json::emit(&render::sets_json(&cand, base.as_ref()))?;
    } else {
        print!("{}", render::sets_text(&cand, base.as_ref()));
    }
    Ok(render::exit_code(&cand, base.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verify detail and a note quoting command output that holds an OSC 0
    /// title, a screen clear and an OSC 52 clipboard write print as text.
    #[test]
    fn run_show_prints_a_hostile_verify_detail_and_note_inert() {
        let hostile = "\x1b]0;owned\x07\x1b[2J\x1b]52;c;aGk=\x07\u{9b}2J\u{202e}";
        let report = workloads::types::InspectionReport {
            run_id: "r1".into(),
            workload_id: "w".into(),
            walltime_ms: 2000,
            turns: 3,
            compactions: 0,
            rest_ms: 0,
            mode: None,
            verify: Some(workloads::types::VerifyReport { passed: false, details: format!("test failed{hostile}") }),
            notes: vec![format!("verify: fail: {hostile}"), "café 日本語".into()],
        };
        let lines = inspection_lines(&report);
        assert!(lines.iter().all(|l| !l.chars().any(|c| c.is_control() || c == '\u{202e}')), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  - café 日本語");
    }

    #[test]
    fn verify_line_says_not_checked_rather_than_omitting_it() {
        use workloads::types::VerifyReport;
        let v = |passed, details: &str| VerifyReport { passed, details: details.to_string() };
        assert_eq!(verify_line(None), "not checked");
        assert_eq!(verify_line(Some(&v(true, "whatever"))), "ok");
        assert_eq!(verify_line(Some(&v(false, ""))), "FAILED");
        assert_eq!(verify_line(Some(&v(false, "missing ack"))), "FAILED: missing ack");
    }

    /// A bare run id names a run under the lab dir; a directory of that name
    /// in the cwd is not a lab record. An explicit path (absolute) still is.
    #[serial_test::serial]
    #[test]
    fn a_bare_id_is_resolved_against_the_lab_dir_not_the_cwd() {
        let home = darkmux_types::test_isolation::IsolatedState::new();
        let cwd = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(cwd.path().join("m-1")).unwrap();
        std::fs::create_dir_all(home.join("lab").join("quick-q-1")).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        let (in_cwd_only, in_lab, absolute) = (
            is_lab_record("m-1"),
            is_lab_record("quick-q-1"),
            is_lab_record(&cwd.path().join("m-1").display().to_string()),
        );
        std::env::set_current_dir(prev).unwrap();
        assert!(!in_cwd_only, "a cwd dir named like a mission id is not a lab record");
        assert!(in_lab, "an id under the lab dir is");
        assert!(absolute, "an explicit path is taken as a path");
    }

    #[test]
    fn a_mission_or_dispatch_run_is_refused_naming_where_to_look_and_a_lab_run_is_not() {
        let m = refusal_for("inspect", "m1", RunKind::Mission).unwrap();
        assert!(m.contains("is a mission") && m.contains("darkmux run inspect") && m.contains("darkmux mission status"), "{m}");
        let d = refusal_for("stats", "d1", RunKind::Dispatch).unwrap();
        assert!(d.contains("is a dispatch") && d.contains("darkmux run list --kind dispatch"), "{d}");
        assert_eq!(refusal_for("compare", "l1", RunKind::Lab), None);
    }
}
