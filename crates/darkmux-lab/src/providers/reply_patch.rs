//! Reply-patch workload provider: read the code, reply with the fix, and the
//! harness applies it and runs the tests.
//!
//! For models too small to drive a tool loop (an 8 GB machine running a 4B
//! model has no room for a tool-calling system prompt), the code goes in the
//! prompt and the model gets no tools. Its reply names corrected lines as
//! `FIX <line>: <corrected line>`; the provider writes those lines into the
//! workload's `patchTarget` in the per-run sandbox and runs the workload's
//! verify command there. So a pass means the same thing it means for a
//! `coding-task` run, the tests went green, and the work gate
//! (`lab::verify_gate`) applies unchanged.
//!
//! The dispatch is forced onto the container path (`force_container`), so a
//! tool-less run still records a trajectory and per-turn telemetry, which is
//! what `darkmux run stats` and every lab surface read.
//!
//! Setup: seed the sandbox exactly as `coding-task` does.
//! Run: number the target file into the prompt (`${PATCH_FILE}`), dispatch,
//! apply the reply's fixes, run verify, record `patch.json`.

use crate::lab::dispatch_end::{DispatchEnd, Dispatched};
use crate::lab::manifest::RunManifest;
use crate::providers::coding_task::{
    finish_run, named_out_dir, preserve_runtime_artifacts, reject_escaping_relpath,
    run_verify_command, seed_sandbox, FinishInputs, TrajectoryPreserver,
};
use crate::providers::prompt::extract_reply_text;
use crate::workloads::types::{InspectionReport, LoadedWorkload, RunResult, VerifyReport, WorkloadProvider};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_types::session_id::{RunId, SessionId};
use darkmux_types::Profile;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// The prompt placeholder the numbered target file replaces.
pub(crate) const PATCH_FILE_PLACEHOLDER: &str = "${PATCH_FILE}";

pub(crate) struct ReplyPatchProvider;

impl WorkloadProvider for ReplyPatchProvider {
    fn id(&self) -> &'static str {
        "reply-patch"
    }

    fn dispatch_role(&self, loaded: &LoadedWorkload) -> Option<String> {
        Some(pick_role(loaded))
    }

    fn setup(&self, loaded: &LoadedWorkload, run_dir: &Path, sandbox_dir: &Path) -> Result<()> {
        fs::create_dir_all(run_dir).with_context(|| format!("creating {}", run_dir.display()))?;
        fs::create_dir_all(sandbox_dir).with_context(|| format!("creating {}", sandbox_dir.display()))?;
        seed_sandbox(loaded, sandbox_dir)?;
        let target = patch_target(loaded)?;
        if !sandbox_dir.join(&target).is_file() {
            bail!(
                "reply-patch workload `{}` names patchTarget `{target}`, which is not a file in \
                 the sandbox {}. Register the fixture that holds it, or ship it in setupContent.",
                loaded.manifest.workload.id,
                sandbox_dir.display()
            );
        }
        if loaded.manifest.workload.verify.as_ref().and_then(|v| v.command.as_ref()).is_none() {
            bail!(
                "reply-patch workload `{}` declares no verify.command: the patched file is only \
                 judged by running it, so a run with nothing to run would check nothing.",
                loaded.manifest.workload.id
            );
        }
        // The point of this provider is a model with no tools. A role that
        // grants tools would be dispatched into a tool loop whose edits this
        // provider then overwrites, measuring neither thing.
        let role_id = pick_role(loaded);
        if let Ok(roles) = darkmux_crew::loader::load_roles() {
            if let Some(role) = roles.iter().find(|r| r.id == role_id) {
                // An empty allow with an empty deny is the runtime's full
                // catalog (`compute_runtime_allowed_tools`), not "no tools".
                let palette = &role.tool_palette;
                if !palette.allow.is_empty() || palette.deny.is_empty() {
                    bail!(
                        "reply-patch workload `{}` is paired with role `{role_id}`, which grants \
                         tools (allow: [{}], deny: [{}]). A reply-patch run measures reading \
                         without tools: pair it with a role whose tool_palette allows nothing \
                         and denies the belt, such as the built-in `code-reader`.",
                        loaded.manifest.workload.id,
                        palette.allow.join(", "),
                        palette.deny.join(", ")
                    );
                }
            }
        }
        Ok(())
    }

    fn run(
        &self,
        loaded: &LoadedWorkload,
        run_dir: &Path,
        sandbox_dir: &Path,
        profile: &Profile,
        profile_name: &str,
        config_path: Option<&str>,
        // A single-turn dispatch has no compaction to override.
        _loop_override: Option<&crate::lab::loop_report::LoopCompactionOverride>,
        run: &RunId,
        on_session_id: &mut dyn FnMut(&SessionId),
    ) -> Result<RunResult> {
        let target = patch_target(loaded)?;
        let target_path = sandbox_dir.join(&target);
        let original = fs::read_to_string(&target_path)
            .with_context(|| format!("reading patchTarget {}", target_path.display()))?;
        let prompt = build_prompt(&resolve_prompt(loaded)?, &original)?;
        let role = pick_role(loaded);
        let session_id = SessionId::adhoc(run.clone(), &role, &loaded.manifest.workload.id);
        on_session_id(&session_id);

        let started = std::time::Instant::now();
        let out_dir = named_out_dir(&role);
        let mut preserve = TrajectoryPreserver::new(out_dir.clone(), run_dir.to_path_buf());
        let dispatched = dispatch(&role, &prompt, &session_id, profile_name, config_path, loaded, out_dir.clone())?;
        let duration_ms = started.elapsed().as_millis();
        fs::write(run_dir.join("qa-reply.json"), &dispatched.stdout)?;
        if !dispatched.stderr.is_empty() {
            fs::write(run_dir.join("qa-reply.err"), &dispatched.stderr)?;
        }
        let preserved = preserve_runtime_artifacts(&out_dir, run_dir, &["trajectory.jsonl"]);
        preserve.disarm();

        // Apply the reply, then judge the file by running it. A dispatch
        // that errored still gets verify: its reply is empty, nothing is
        // applied, and the untouched suite reports the baseline.
        let reply = extract_reply_text(&dispatched.stdout);
        let fixes = parse_fixes(&reply);
        let patched = apply_fixes(&original, &fixes);
        fs::write(&target_path, &patched.text)
            .with_context(|| format!("writing the patched {}", target_path.display()))?;
        let record = PatchRecord { target: target.clone(), applied: patched.applied, refused: patched.refused };
        fs::write(run_dir.join("patch.json"), serde_json::to_string_pretty(&record)? + "\n")?;
        let verify = run_verify_command(loaded, run_dir, sandbox_dir)?.map(|mut v| {
            v.details = format!("{}; {}", record.summary(), v.details);
            v
        });

        let final_hash = match crate::lab::sandbox_hash::hash_sandbox_dir(sandbox_dir) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("darkmux: warn — final_hash for {} skipped: {e}", sandbox_dir.display());
                None
            }
        };
        finish_run(
            FinishInputs {
                loaded,
                run_dir,
                profile,
                profile_name,
                session_id: &session_id,
                duration_ms,
                sandbox_dir,
                final_hash,
                refused_artifacts: &preserved.refused,
                verify,
                provider: self.id(),
            },
            &dispatched,
        )
    }

    fn inspect(&self, loaded: &LoadedWorkload, run_dir: &Path) -> Result<InspectionReport> {
        let meta = RunManifest::read_or_default(run_dir)?;
        let run_id = meta
            .run_id
            .clone()
            .or_else(|| run_dir.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .unwrap_or_else(|| "(unknown)".to_string());
        let verify = meta.verify_report().map(|v| VerifyReport { passed: v.passed, details: v.details.clone() });
        let patch = fs::read_to_string(run_dir.join("patch.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<PatchRecord>(&t).ok())
            .map(|r| format!("patch: {}", r.summary()))
            .unwrap_or_else(|| "patch: none recorded".to_string());
        Ok(InspectionReport {
            run_id,
            workload_id: loaded.manifest.workload.id.clone(),
            walltime_ms: meta.duration_ms.unwrap_or(0) as u128,
            turns: 1,
            compactions: 0,
            rest_ms: 0,
            mode: None,
            notes: [format!("provider={}", self.id()), patch]
                .into_iter()
                .chain(DispatchEnd::inspect_note(&meta))
                .collect(),
            verify,
        })
    }
}

fn dispatch(
    role_id: &str,
    prompt: &str,
    session_id: &SessionId,
    profile_name: &str,
    config_path: Option<&str>,
    loaded: &LoadedWorkload,
    host_out: PathBuf,
) -> Result<Dispatched> {
    use darkmux_crew::dispatch::{dispatch, DispatchOpts};
    let opts = DispatchOpts {
        finding_sites: None,
        allow_utility_model: true,
        remote_origin: None,
        live_channel: false,
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: Some(host_out),
        // One reply is the whole task: a second turn has nothing to act on.
        max_turns_override: Some(1),
        timeout_override_seconds: None,
        role_id: role_id.to_string(),
        message: prompt.to_string(),
        session: session_id.clone(),
        timeout_seconds: 3600,
        skip_preflight: false,
        json: true,
        // The model sees the code in its prompt, never the sandbox.
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: darkmux_crew::dispatch::CompactionDispatchArgs::default(),
        profile_name: Some(profile_name.to_string()),
        config_path: config_path.map(str::to_string),
        // (#1199) The container path even for a tool-less role: one
        // substrate, so the run has a trajectory and per-turn telemetry.
        force_container: true,
        max_completion_tokens: None,
        image: loaded.manifest.workload.image.clone(),
        model_base_url_override: None,
        step_id: None,
        system_prompt_override: None,
    };
    let result = dispatch(opts).context("internal-runtime dispatch via lab harness")?;
    Ok(Dispatched { exit_code: result.exit_code, stdout: result.stdout, stderr: result.stderr })
}

fn pick_role(loaded: &LoadedWorkload) -> String {
    loaded.manifest.workload.role.clone().unwrap_or_else(|| "code-reader".to_string())
}

fn resolve_prompt(loaded: &LoadedWorkload) -> Result<String> {
    if let Some(p) = loaded.manifest.workload.prompt.as_ref() {
        return Ok(p.clone());
    }
    if let Some(rel) = loaded.manifest.workload.prompt_file.as_ref() {
        let path = loaded.base_dir.join(rel);
        return fs::read_to_string(&path).with_context(|| format!("reading promptFile at {}", path.display()));
    }
    Err(anyhow!("workload \"{}\" must define prompt or promptFile", loaded.manifest.workload.id))
}

/// The workload's `patchTarget`, refused when absent or when it would leave
/// the sandbox.
fn patch_target(loaded: &LoadedWorkload) -> Result<String> {
    let Some(raw) = loaded.manifest.workload.patch_target.as_deref() else {
        bail!(
            "reply-patch workload `{}` declares no patchTarget: name the file the reply's fixes \
             apply to, relative to the sandbox (e.g. \"src/duration.js\")",
            loaded.manifest.workload.id
        );
    };
    reject_escaping_relpath(raw, "patchTarget")?;
    Ok(raw.to_string())
}

/// The prompt with `${PATCH_FILE}` replaced by the target file, numbered. A
/// prompt without the placeholder is refused: the model would be asked for
/// line numbers of a file it was never shown.
pub(crate) fn build_prompt(template: &str, file: &str) -> Result<String> {
    if !template.contains(PATCH_FILE_PLACEHOLDER) {
        bail!(
            "reply-patch prompt has no {PATCH_FILE_PLACEHOLDER}: the numbered file goes where it \
             stands, so the line numbers the model replies with are the ones it was shown"
        );
    }
    Ok(template.replace(PATCH_FILE_PLACEHOLDER, &number_lines(file)))
}

/// `file` with a right-aligned line number before each line.
pub(crate) fn number_lines(file: &str) -> String {
    let lines: Vec<&str> = file.lines().collect();
    let width = lines.len().max(1).to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| if l.is_empty() { format!("{:>width$}", i + 1) } else { format!("{:>width$}  {l}", i + 1) })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `FIX <line>: <text>` line from a reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fix {
    pub line: usize,
    pub text: String,
}

/// Every `FIX <line>: <text>` line in the reply, in order. Case and leading
/// markdown (`-`, `*`, backticks around the whole line) are tolerated, since
/// a small model adds them; anything else is not a fix.
pub(crate) fn parse_fixes(reply: &str) -> Vec<Fix> {
    reply
        .lines()
        .filter_map(|raw| {
            let l = raw.trim().trim_start_matches(['-', '*']).trim();
            let l = l.strip_prefix('`').and_then(|x| x.strip_suffix('`')).unwrap_or(l);
            let rest = l.get(..4).filter(|p| p.eq_ignore_ascii_case("fix "))?;
            let rest = &l[rest.len()..];
            let (num, text) = rest.split_once(':')?;
            let line = num.trim().parse::<usize>().ok()?;
            let text = text.strip_prefix(' ').unwrap_or(text);
            let text = text.strip_prefix('`').and_then(|x| x.strip_suffix('`')).unwrap_or(text);
            Some(Fix { line, text: text.to_string() })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct AppliedFix {
    pub line: usize,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct RefusedFix {
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Patched {
    pub text: String,
    pub applied: Vec<AppliedFix>,
    pub refused: Vec<RefusedFix>,
}

/// Apply `fixes` to `original`, one line each. A line out of range or named
/// twice is refused (the first fix to a line stands), never guessed at. A
/// fix with no indentation keeps the original line's, since a model copying
/// a line often drops it. The trailing newline is kept as it was.
pub(crate) fn apply_fixes(original: &str, fixes: &[Fix]) -> Patched {
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut applied = Vec::new();
    let mut refused = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for f in fixes {
        if f.line == 0 || f.line > lines.len() {
            refused.push(RefusedFix { line: f.line, reason: format!("no line {} (the file has {})", f.line, lines.len()) });
            continue;
        }
        if !seen.insert(f.line) {
            refused.push(RefusedFix { line: f.line, reason: "line already fixed earlier in the reply".into() });
            continue;
        }
        let before = lines[f.line - 1].clone();
        let after = if f.text.starts_with(char::is_whitespace) {
            f.text.clone()
        } else {
            let indent: String = before.chars().take_while(|c| c.is_whitespace()).collect();
            format!("{indent}{}", f.text)
        };
        lines[f.line - 1] = after.clone();
        applied.push(AppliedFix { line: f.line, before, after });
    }
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    Patched { text, applied, refused }
}

/// What `patch.json` records: which lines the reply changed and which of its
/// fixes were refused.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub(crate) struct PatchRecord {
    pub target: String,
    pub applied: Vec<AppliedFix>,
    pub refused: Vec<RefusedFix>,
}

impl PatchRecord {
    pub(crate) fn summary(&self) -> String {
        let lines = |v: &mut dyn Iterator<Item = usize>| v.map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
        let mut s = match self.applied.len() {
            0 => format!("no fix applied to {}", self.target),
            n => format!("{n} fix(es) applied to {} (line {})", self.target, lines(&mut self.applied.iter().map(|a| a.line))),
        };
        if !self.refused.is_empty() {
            s.push_str(&format!(", {} refused (line {})", self.refused.len(), lines(&mut self.refused.iter().map(|r| r.line))));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "const UNIT_MS = {\n  m: 60 * 100,\n};\n\nif (total >= MAX_MS) {\n  throw 1;\n}\n";

    #[test]
    fn number_lines_aligns_and_leaves_blank_lines_bare() {
        let n = number_lines(FILE);
        assert!(n.starts_with("1  const UNIT_MS = {\n2    m: 60 * 100,\n"), "{n}");
        assert!(n.contains("\n4\n5  if (total"), "a blank line is its number alone: {n}");
        let ten = "x\n".repeat(10);
        assert!(number_lines(&ten).starts_with(" 1  x"), "numbers right-align to the widest");
    }

    #[test]
    fn build_prompt_refuses_a_template_without_the_placeholder() {
        assert!(build_prompt("fix it", FILE).is_err());
        let p = build_prompt("code:\n${PATCH_FILE}\nend", FILE).unwrap();
        assert!(p.contains("2    m: 60 * 100,"), "{p}");
    }

    #[test]
    fn parse_fixes_reads_the_reply_shape_small_models_actually_write() {
        let reply = "Here you go:\nFIX 2: m: 60 * 1000,\n- fix 5: `if (total > MAX_MS) {`\n`FIX 7: }`\nVALUE: 120000\nFIX x: nope\nI think FIX 3: is wrong";
        let got = parse_fixes(reply);
        assert_eq!(
            got,
            vec![
                Fix { line: 2, text: "m: 60 * 1000,".into() },
                Fix { line: 5, text: "if (total > MAX_MS) {".into() },
                Fix { line: 7, text: "}".into() },
            ]
        );
    }

    #[test]
    fn apply_fixes_keeps_indentation_and_the_trailing_newline() {
        let p = apply_fixes(FILE, &[Fix { line: 2, text: "m: 60000,".into() }, Fix { line: 5, text: "if (total > MAX_MS) {".into() }]);
        assert_eq!(p.text, "const UNIT_MS = {\n  m: 60000,\n};\n\nif (total > MAX_MS) {\n  throw 1;\n}\n");
        assert_eq!(p.applied.len(), 2);
        assert_eq!(p.applied[0].before, "  m: 60 * 100,");
        assert!(p.refused.is_empty());
    }

    #[test]
    fn apply_fixes_refuses_out_of_range_and_repeated_lines() {
        let p = apply_fixes(
            FILE,
            &[
                Fix { line: 0, text: "x".into() },
                Fix { line: 99, text: "x".into() },
                Fix { line: 2, text: "m: 60000,".into() },
                Fix { line: 2, text: "m: 1,".into() },
            ],
        );
        assert!(p.text.contains("  m: 60000,"), "the first fix to a line stands");
        assert_eq!(p.refused.iter().map(|r| r.line).collect::<Vec<_>>(), vec![0, 99, 2]);
        let rec = PatchRecord { target: "src/d.js".into(), applied: p.applied, refused: p.refused };
        assert_eq!(rec.summary(), "1 fix(es) applied to src/d.js (line 2), 3 refused (line 0, 99, 2)");
    }

    #[test]
    fn an_empty_reply_changes_nothing() {
        let p = apply_fixes(FILE, &parse_fixes(""));
        assert_eq!(p.text, FILE);
        let rec = PatchRecord { target: "f".into(), applied: p.applied, refused: p.refused };
        assert_eq!(rec.summary(), "no fix applied to f");
    }

    /// The 4B's real reply from the darkbook probe (2026-10-22), which the
    /// substring check failed for writing `m: 60000`: applied to the fixture's
    /// own file, it produces exactly the two-line fix.
    #[test]
    fn the_4b_probe_reply_fixes_salt_shaker() {
        let file = include_str!("reply_patch_testdata/duration.js");
        let reply = "FIX 15: m: 60000,\nFIX 40: if (total > MAX_MS) {";
        let p = apply_fixes(file, &parse_fixes(reply));
        let want = file.replace("  m: 60 * 100,", "  m: 60000,").replace("total >= MAX_MS", "total > MAX_MS");
        assert_eq!(p.text, want);
        assert_eq!(p.applied.iter().map(|a| a.line).collect::<Vec<_>>(), vec![15, 40]);
    }

    /// A workload with no patchTarget, or one that escapes the sandbox, is
    /// refused by name.
    #[test]
    fn patch_target_is_required_and_stays_in_the_sandbox() {
        let mut loaded = crate::workloads::types::LoadedWorkload {
            manifest: serde_json::from_str(r#"{"workload":{"id":"w","provider":"reply-patch","prompt":"${PATCH_FILE}"}}"#).unwrap(),
            base_dir: PathBuf::from("."),
            source: crate::workloads::types::WorkloadSource::OnDisk,
        };
        assert!(patch_target(&loaded).unwrap_err().to_string().contains("patchTarget"));
        loaded.manifest.workload.patch_target = Some("../etc/passwd".into());
        assert!(patch_target(&loaded).is_err());
        loaded.manifest.workload.patch_target = Some("src/duration.js".into());
        assert_eq!(patch_target(&loaded).unwrap(), "src/duration.js");
    }
}
