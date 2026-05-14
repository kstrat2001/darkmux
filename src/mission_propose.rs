//! `darkmux mission propose` — AI-built-in verb that takes unstructured
//! operator intent and emits a structured Mission + Sprint proposal
//! JSON via internal dispatch to the `mission-compiler` admin role
//! (#113 Sprint 1). Operator approval gate is mandatory; the proposal
//! is only persisted to disk after the operator accepts (operator-
//! sovereignty per #44).
//!
//! Engagement context is intentionally NOT a parameter of this verb —
//! see CLAUDE.md's "Engagements (operator-defined dreamscapes)"
//! section for doctrine. The frontier orchestrator carries engagement
//! nuance natively into the input text it crafts; quantizing it into a
//! CLI arg would (a) compress a dreamscape into a token, (b) push the
//! interpretation tier onto a 4B admin agent that doesn't have the
//! shape for it, (c) violate the #49 rule that engagement lives in the
//! operator-judgment layer above the system.
//!
//! Flow:
//!   1. Read unstructured input (stdin or file)
//!   2. Dispatch internally to `darkmux/mission-compiler`
//!   3. Parse the proposal JSON from the response (fenced ```json block)
//!   4. Render a human-readable summary
//!   5. Prompt operator: approve / edit / reject / regenerate
//!   6. On approve: write Mission + Sprint JSONs to
//!      `~/.darkmux/crew/missions/` and `~/.darkmux/crew/sprints/`

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

/// The shape the mission-compiler emits inside its fenced ```json block.
/// Matches the schema documented in
/// `templates/builtin/crew/roles/mission-compiler.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Proposal {
    mission: ProposedMission,
    sprints: Vec<ProposedSprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedMission {
    id: String,
    description: String,
    #[serde(default = "default_active")]
    status: String,
    sprint_ids: Vec<String>,
    #[serde(default)]
    created_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedSprint {
    id: String,
    mission_id: String,
    description: String,
    #[serde(default = "default_planned")]
    status: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    created_ts: u64,
}

fn default_active() -> String { "active".to_string() }
fn default_planned() -> String { "planned".to_string() }

/// Top-level entry called from main.rs's dispatch table.
pub fn propose(
    from_stdin: bool,
    from_file: Option<&std::path::Path>,
    yes: bool,
    start: bool,
) -> Result<i32> {
    // 1. Read input
    let input = read_input(from_stdin, from_file)?;
    if input.trim().is_empty() {
        return Err(anyhow!("mission propose: empty input — nothing to compile"));
    }

    let mut hint: Option<String> = None;
    loop {
        // 2. Dispatch
        eprintln!("mission propose: dispatching mission-compiler …");
        let response = dispatch_compiler(&input, hint.as_deref())?;

        // 3. Parse the proposal
        let proposal = match parse_proposal(&response) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mission propose: failed to parse proposal: {e}");
                eprintln!("--- raw response ---\n{response}\n--- end response ---");
                return Err(anyhow!("mission propose: unparseable response"));
            }
        };

        // 3b. Validate cross-references inside the proposal. A
        // hallucinated mission_id, missing sprint_id, or dangling
        // depends_on slips through serde happily but lands a broken
        // mission on disk. Surface it now so the operator can hit `e`
        // and regenerate with a hint instead of debugging the viewer.
        if let Err(e) = validate_proposal_invariants(&proposal) {
            eprintln!("mission propose: proposal failed validation: {e}");
            eprintln!("--- raw response ---\n{response}\n--- end response ---");
            return Err(anyhow!("mission propose: invalid proposal cross-references"));
        }

        // 4. Render summary
        render_proposal(&proposal);

        // 5. Operator decision
        if yes {
            eprintln!("mission propose: --yes flag set, applying without prompt");
            return persist_and_maybe_start(&proposal, start);
        }
        match prompt_decision()? {
            Decision::Approve => return persist_and_maybe_start(&proposal, start),
            Decision::Reject  => {
                eprintln!("mission propose: rejected. No files written.");
                return Ok(1);
            }
            Decision::Regenerate(text) => {
                hint = Some(text);
                continue; // loop dispatches again with the hint
            }
        }
    }
}

enum Decision {
    Approve,
    Reject,
    Regenerate(String),
}

fn read_input(
    from_stdin: bool,
    from_file: Option<&std::path::Path>,
) -> Result<String> {
    if from_stdin {
        let mut buf = String::new();
        std::io::stdin().lock().read_to_string(&mut buf).context("reading stdin")?;
        Ok(buf)
    } else if let Some(p) = from_file {
        std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
    } else {
        Err(anyhow!("internal: exactly one of --from-stdin / --from-file required"))
    }
}

fn dispatch_compiler(input: &str, hint: Option<&str>) -> Result<String> {
    let message = build_compiler_message(input, hint);
    let opts = crate::crew::dispatch::DispatchOpts {
        role_id: "mission-compiler".to_string(),
        message,
        deliver: None,
        session_id: None,
        timeout_seconds: 600,
        skip_preflight: false,
        watch_paths: Vec::new(),
    };
    let result = crate::crew::dispatch::dispatch(opts)
        .context("dispatch to mission-compiler failed")?;
    Ok(result.stdout)
}

/// Build the message the mission-compiler sees. The unstructured intent
/// is the only required content; engagement context lives in the input
/// itself (operator + frontier orchestrator threaded it there) and is
/// never carried as a separate field — see CLAUDE.md's Engagements
/// doctrine.
fn build_compiler_message(input: &str, hint: Option<&str>) -> String {
    let mut msg = String::new();
    msg.push_str("Unstructured intent:\n---\n");
    msg.push_str(input.trim());
    msg.push_str("\n---\n");
    if let Some(h) = hint {
        msg.push_str("\nOperator-provided regeneration hint (apply this when restructuring):\n---\n");
        msg.push_str(h);
        msg.push_str("\n---\n");
    }
    msg.push_str(
        "\nEmit a single fenced ```json block containing the Proposal object, per your role's schema.\n",
    );
    msg
}

/// Extract the JSON object from a ```json fenced block in the
/// response. The admin agent's role prompt requires exactly one such
/// block; we strip everything outside it and parse the inner.
///
/// First unwraps the openclaw envelope (if response is wrapped); the
/// fall-through path uses the response as-is so non-envelope inputs
/// (other runtimes, future format changes, tests with clean fenced
/// blocks) still work.
fn parse_proposal(response: &str) -> Result<Proposal> {
    let unwrapped = unwrap_openclaw_envelope(response).unwrap_or_else(|| response.to_string());
    let json_str = extract_json_block(&unwrapped)?;
    serde_json::from_str(&json_str).with_context(|| format!("parsing proposal JSON:\n{json_str}"))
}

/// Validate cross-references inside a Proposal so a hallucinated /
/// drifted admin-agent output can't land on disk as a broken
/// mission+sprints set. `add_sprint_to_mission` already enforces these
/// invariants for the mid-flight add-sprint path; `propose` should hold
/// the same bar at first-write time. Three checks:
///
///   - Every `sprints[].mission_id` matches `mission.id`
///   - Every entry in `mission.sprint_ids` exists as a `sprints[].id`
///   - Every `sprints[].depends_on` value references an in-proposal id
///
/// Returns a readable error so the operator can hit `e` to regenerate
/// with a hint instead of having to discover the problem post-persist
/// in the viewer's sprint-progress widget.
fn validate_proposal_invariants(p: &Proposal) -> Result<()> {
    let sprint_ids: std::collections::HashSet<&str> =
        p.sprints.iter().map(|s| s.id.as_str()).collect();

    for s in &p.sprints {
        if s.mission_id != p.mission.id {
            return Err(anyhow!(
                "proposal invariant: sprint `{}` has mission_id `{}` but mission is `{}`",
                s.id,
                s.mission_id,
                p.mission.id
            ));
        }
        for dep in &s.depends_on {
            if !sprint_ids.contains(dep.as_str()) {
                return Err(anyhow!(
                    "proposal invariant: sprint `{}` depends_on `{}` which is not in the proposal",
                    s.id,
                    dep
                ));
            }
        }
    }

    for sid in &p.mission.sprint_ids {
        if !sprint_ids.contains(sid.as_str()) {
            return Err(anyhow!(
                "proposal invariant: mission.sprint_ids references `{}` which is not in sprints[]",
                sid
            ));
        }
    }

    Ok(())
}

/// Best-effort unwrap of an openclaw CLI envelope. Openclaw's
/// `agent` invocation prints a structured JSON envelope to stdout:
///
/// ```json
/// {
///   "payloads": [{"text": "<agent reply, may include ```json fence>",
///                 "mediaUrl": null}, ...],
///   "meta": {...}
/// }
/// ```
///
/// The agent's actual `\`\`\`json` fenced block lives INSIDE
/// `payloads[].text` with `\n` as escape sequences, not real newlines.
/// If `extract_json_block` ran on the raw envelope, `.lines()` would
/// iterate the envelope's outer JSON lines and never see a fence —
/// returns "no fenced json block found" even though the agent emitted
/// one perfectly. Returns `None` if `response` doesn't parse as an
/// envelope (defensive fall-through for non-openclaw runtimes or
/// format changes); callers should then use `response` as-is.
///
/// Surfaced 2026-05-14 during the Japan-trip-planning dogfood — the
/// first real-engagement `mission propose` run. The unit tests for
/// `parse_proposal` used clean fenced blocks (the agent-reply shape)
/// and passed; the integration with openclaw's envelope wasn't
/// exercised until live dispatch.
fn unwrap_openclaw_envelope(response: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
    let payloads = envelope.get("payloads")?.as_array()?;
    let texts: Vec<&str> = payloads
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect();
    if texts.is_empty() {
        return None;
    }
    Some(texts.join("\n\n"))
}

/// Find the first ```json … ``` fenced block (or just the first ``` …
/// ``` if no language tag) and return its inner content. Forgiving
/// about whitespace/case for the language tag.
fn extract_json_block(response: &str) -> Result<String> {
    // Scan line-by-line; track when we're inside a fenced block.
    let mut lines = response.lines();
    let mut buf = String::new();
    let mut inside = false;
    for line in &mut lines {
        let trimmed = line.trim_start();
        if !inside && (trimmed.starts_with("```json") || trimmed.starts_with("```JSON") || trimmed == "```") {
            inside = true;
            continue;
        }
        if inside && trimmed.starts_with("```") {
            return Ok(buf);
        }
        if inside {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    Err(anyhow!("no fenced ```json block found in response"))
}

fn render_proposal(p: &Proposal) {
    eprintln!();
    eprintln!("┌─ Proposal ──────────────────────────────");
    eprintln!("  Mission: {}", p.mission.id);
    eprintln!("  Description:");
    for line in p.mission.description.lines() {
        eprintln!("    {line}");
    }
    eprintln!();
    eprintln!("  Sprints ({}):", p.sprints.len());
    for (i, s) in p.sprints.iter().enumerate() {
        let deps = if s.depends_on.is_empty() {
            "—".to_string()
        } else {
            s.depends_on.join(", ")
        };
        eprintln!("    {}. {} (depends_on: {deps})", i + 1, s.id);
        // Show only the first line of each sprint description in the
        // summary — keeps the table-style render readable when sprint
        // descriptions are multi-line.
        if let Some(first) = s.description.lines().next() {
            eprintln!("       {first}");
        }
    }
    eprintln!("└─────────────────────────────────────────");
    eprintln!();
}

fn prompt_decision() -> Result<Decision> {
    eprint!("Approve [y] · Edit/regenerate with hint [e] · Reject [n]: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let bytes = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading decision")?;
    // EOF (Ctrl-D, closed-stdin pipe, etc.) reads 0 bytes. Treat as a
    // clean reject so operators don't see a confusing
    // "unrecognized decision" on what was effectively a cancel.
    if bytes == 0 {
        eprintln!("\nmission propose: no decision (EOF) — treating as reject");
        return Ok(Decision::Reject);
    }
    match line.trim().to_lowercase().chars().next() {
        Some('y') => Ok(Decision::Approve),
        Some('n') => Ok(Decision::Reject),
        Some('e') => {
            eprint!("Hint (one line): ");
            std::io::stderr().flush().ok();
            let mut hint = String::new();
            let hbytes = std::io::stdin()
                .lock()
                .read_line(&mut hint)
                .context("reading hint")?;
            if hbytes == 0 {
                // EOF mid-hint — same posture as the main prompt EOF.
                eprintln!("\nmission propose: no hint (EOF) — treating as reject");
                return Ok(Decision::Reject);
            }
            let trimmed = hint.trim().to_string();
            if trimmed.is_empty() {
                Err(anyhow!("regenerate requires a non-empty hint"))
            } else {
                Ok(Decision::Regenerate(trimmed))
            }
        }
        _ => Err(anyhow!("unrecognized decision (expected y/e/n)")),
    }
}

fn persist(p: &Proposal) -> Result<i32> {
    let crew_root = crate::crew::loader::crew_root();
    let missions_dir = crew_root.join("missions");
    let sprints_dir = crew_root.join("sprints");
    std::fs::create_dir_all(&missions_dir).with_context(|| format!("creating {}", missions_dir.display()))?;
    std::fs::create_dir_all(&sprints_dir).with_context(|| format!("creating {}", sprints_dir.display()))?;

    // Stamp created_ts on everything that has 0 (the schema convention
    // the mission-compiler emits — see role .md).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut mission = p.mission.clone();
    if mission.created_ts == 0 { mission.created_ts = now; }

    // Refuse to overwrite existing mission file.
    let mission_path = missions_dir.join(format!("{}.json", mission.id));
    if mission_path.exists() {
        return Err(anyhow!(
            "mission propose: mission `{}` already exists at {} — aborting (no overwrite)",
            mission.id,
            mission_path.display()
        ));
    }

    // Same for each sprint.
    for s in &p.sprints {
        let sp = sprints_dir.join(format!("{}.json", s.id));
        if sp.exists() {
            return Err(anyhow!(
                "mission propose: sprint `{}` already exists at {} — aborting (no overwrite)",
                s.id,
                sp.display()
            ));
        }
    }

    // Atomic-ish persist: track every file we land on disk, and if any
    // write later in the sequence fails, roll back the earlier ones so
    // a partial-failure mid-loop (disk full, EPERM, signal) doesn't
    // leave the operator with a half-written mission whose retry
    // confusingly fails on the no-overwrite gate.
    let mut written: Vec<std::path::PathBuf> = Vec::new();
    let result = write_all(p, &mission, &mission_path, &sprints_dir, now, &mut written);
    if let Err(e) = result {
        for path in &written {
            let _ = std::fs::remove_file(path);
        }
        eprintln!(
            "mission propose: write failed mid-flight — rolled back {} file(s); state on disk matches pre-call",
            written.len()
        );
        return Err(e);
    }

    eprintln!("mission propose: persisted {} mission + {} sprints", 1, p.sprints.len());
    Ok(0)
}

/// Inner write loop, factored so persist() can roll back on any failure.
/// Pushes each successfully-written path into `written` BEFORE moving
/// on to the next write — so the rollback in persist() sees only files
/// that actually landed.
fn write_all(
    p: &Proposal,
    mission: &ProposedMission,
    mission_path: &std::path::Path,
    sprints_dir: &std::path::Path,
    now: u64,
    written: &mut Vec<std::path::PathBuf>,
) -> Result<()> {
    let mission_json = serde_json::to_string_pretty(mission).context("serializing mission")?;
    std::fs::write(mission_path, format!("{mission_json}\n"))
        .with_context(|| format!("writing {}", mission_path.display()))?;
    written.push(mission_path.to_path_buf());
    println!("wrote mission: {}", mission_path.display());

    for s in &p.sprints {
        let mut sprint = s.clone();
        if sprint.created_ts == 0 {
            sprint.created_ts = now;
        }
        let sp = sprints_dir.join(format!("{}.json", sprint.id));
        let sprint_json = serde_json::to_string_pretty(&sprint).context("serializing sprint")?;
        std::fs::write(&sp, format!("{sprint_json}\n"))
            .with_context(|| format!("writing {}", sp.display()))?;
        written.push(sp.clone());
        println!("wrote sprint:  {}", sp.display());
    }

    Ok(())
}

/// Helper that persists a proposal and optionally starts the mission.
fn persist_and_maybe_start(p: &Proposal, start: bool) -> Result<i32> {
    let exit = persist(p)?;
    if exit != 0 { return Ok(exit); }
    if start {
        eprintln!("mission propose: --start flag set, transitioning mission to Running …");
        let m = crate::crew::lifecycle::mission_start(&p.mission.id)
            .context("mission_start failed after successful persist")?;
        println!("mission `{}` → Active  started_ts={}", m.id, m.started_ts.unwrap_or(0));
    } else {
        eprintln!("next: `darkmux mission start {}` to begin (or pass `--start` next time)", p.mission.id);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// RAII guard that points `DARKMUX_CREW_DIR` at a TempDir for the
    /// test's duration, then restores the previous value (or unsets it)
    /// on drop. Mirrors the `CrewDirGuard` in `src/crew/loader.rs` —
    /// inlined here because that one is `#[cfg(test)]` inside its own
    /// module and not reachable from a sibling crate module. Every
    /// test using this guard MUST also be `#[serial_test::serial]`
    /// because env-var mutation is a global concern.
    struct CrewDirGuard {
        prev: Option<String>,
        _tmp: TempDir,
    }

    impl CrewDirGuard {
        fn new(tmp: TempDir) -> Self {
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()); }
            Self { prev, _tmp: tmp }
        }

        fn path(&self) -> &std::path::Path {
            self._tmp.path()
        }
    }

    impl Drop for CrewDirGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    /// Minimal valid Proposal for tests. Adjust fields per-test as needed.
    fn sample_proposal(mission_id: &str, sprint_ids: &[&str]) -> Proposal {
        Proposal {
            mission: ProposedMission {
                id: mission_id.to_string(),
                description: "test mission".to_string(),
                status: "active".to_string(),
                sprint_ids: sprint_ids.iter().map(|s| s.to_string()).collect(),
                created_ts: 0,
            },
            sprints: sprint_ids
                .iter()
                .map(|sid| ProposedSprint {
                    id: sid.to_string(),
                    mission_id: mission_id.to_string(),
                    description: format!("sprint {sid}"),
                    status: "planned".to_string(),
                    depends_on: Vec::new(),
                    created_ts: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn extract_json_block_finds_fenced_block() {
        let resp = "preamble text\n```json\n{\"mission\": {}, \"sprints\": []}\n```\nepilogue";
        let block = extract_json_block(resp).expect("should extract");
        assert!(block.contains("\"mission\""));
        assert!(block.contains("\"sprints\""));
    }

    #[test]
    fn extract_json_block_errors_on_missing_fence() {
        let resp = "no fenced block here, just prose";
        let err = extract_json_block(resp).expect_err("should error");
        assert!(err.to_string().contains("no fenced"));
    }

    #[test]
    fn parse_proposal_round_trips() {
        let resp = r#"some preamble
```json
{
  "mission": {
    "id": "test-mission",
    "description": "a test mission",
    "status": "active",
    "sprint_ids": ["test-s1"],
    "created_ts": 0
  },
  "sprints": [
    {
      "id": "test-s1",
      "mission_id": "test-mission",
      "description": "first sprint",
      "status": "planned",
      "depends_on": [],
      "created_ts": 0
    }
  ]
}
```
some epilogue"#;
        let p = parse_proposal(resp).expect("should parse");
        assert_eq!(p.mission.id, "test-mission");
        assert_eq!(p.sprints.len(), 1);
        assert_eq!(p.sprints[0].id, "test-s1");
    }

    #[test]
    fn build_compiler_message_includes_hint_when_present() {
        let msg = build_compiler_message("intent", Some("merge sprints 3 and 4"));
        assert!(msg.contains("intent"));
        assert!(msg.contains("merge sprints 3 and 4"));
        assert!(msg.contains("regeneration hint"));
    }

    #[test]
    fn build_compiler_message_omits_hint_when_absent() {
        let msg = build_compiler_message("intent", None);
        assert!(msg.contains("intent"));
        assert!(!msg.contains("regeneration hint"));
    }

    #[test]
    fn build_compiler_message_never_mentions_engagement() {
        // Doctrine regression guard: engagement context lives in the
        // frontier orchestrator layer per CLAUDE.md, never in the
        // mission-compiler's message scaffold. If a future change
        // re-introduces a separate `engagement` arg or string in the
        // builder, this test catches it.
        let msg = build_compiler_message("plan a trip — relaxation focus, no work", None);
        let lower = msg.to_lowercase();
        assert!(
            !lower.contains("engagement context:"),
            "engagement must not be a labeled field in the compiler message"
        );
    }

    #[serial_test::serial]
    #[test]
    fn persist_fails_on_existing_mission() {
        // Regression: persist must refuse to overwrite an existing
        // mission file so a duplicate proposal can't clobber operator
        // state. Wrapped in CrewDirGuard so the test runs in an
        // isolated TempDir — earlier version of this test wrote into
        // the operator's REAL ~/.darkmux/crew/missions/ which was a
        // footgun on every CI run.
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let missions_dir = guard.path().join("missions");
        std::fs::create_dir_all(&missions_dir).unwrap();

        let existing_path = missions_dir.join("test-existing-mission.json");
        std::fs::write(&existing_path, r#"{"id":"test-existing-mission","description":"existing","status":"active","sprint_ids":[],"created_ts":0}"#)
            .expect("writing existing mission");

        let proposal = sample_proposal("test-existing-mission", &[]);
        let err = persist(&proposal).expect_err("persist should fail for existing mission");
        assert!(err.to_string().contains("already exists"));
    }

    #[serial_test::serial]
    #[test]
    fn persist_rolls_back_mission_when_sprint_write_fails() {
        // Atomicity regression: if a sprint write fails mid-loop, the
        // mission file + any earlier sprint files must be cleaned up
        // so the operator's retry sees a clean slate.
        //
        // To trigger the failure DURING the write loop (not in the
        // pre-flight `.exists()` check), the second sprint's id
        // contains a `/`, so its target path is
        // `<sprints_dir>/deep/test-s2.json` whose parent directory
        // (`deep/`) doesn't exist. `.exists()` returns false on the
        // full path (pre-flight passes); `std::fs::write` returns
        // ENOENT during the loop (rollback fires).
        //
        // The slashed-id smell is acknowledged — sprint ids with `/`
        // should arguably be rejected by `validate_proposal_invariants`
        // as a path-traversal vector. Future hardening; tests-only
        // use of the form is fine.
        let guard = CrewDirGuard::new(TempDir::new().unwrap());
        let missions_dir = guard.path().join("missions");
        let sprints_dir = guard.path().join("sprints");
        std::fs::create_dir_all(&sprints_dir).unwrap();

        let proposal = sample_proposal("test-rollback", &["test-s1", "deep/test-s2"]);

        let err = persist(&proposal).expect_err("persist should fail mid-loop");
        assert!(
            err.to_string().contains("writing"),
            "expected a write error, got: {err}"
        );

        // Mission file and the first sprint file must both be cleaned up.
        let mission_path = missions_dir.join("test-rollback.json");
        let sprint1_path = sprints_dir.join("test-s1.json");
        assert!(
            !mission_path.exists(),
            "mission file should have been rolled back; found {}",
            mission_path.display()
        );
        assert!(
            !sprint1_path.exists(),
            "sprint 1 file should have been rolled back; found {}",
            sprint1_path.display()
        );
    }

    #[test]
    fn validate_proposal_accepts_well_formed() {
        let p = sample_proposal("m1", &["s1", "s2"]);
        validate_proposal_invariants(&p).expect("well-formed proposal should pass");
    }

    #[test]
    fn validate_proposal_rejects_dangling_sprint_id_in_mission() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Add a sprint_id to the mission that doesn't exist in sprints[].
        p.mission.sprint_ids.push("ghost".to_string());
        let err = validate_proposal_invariants(&p).expect_err("should reject dangling sprint_id");
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn validate_proposal_rejects_dangling_depends_on() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Reference a non-existent sprint id in depends_on.
        p.sprints[0].depends_on = vec!["missing".to_string()];
        let err = validate_proposal_invariants(&p).expect_err("should reject dangling depends_on");
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn unwrap_openclaw_envelope_returns_text_payload() {
        // Real openclaw envelope shape captured during the 2026-05-14
        // Japan-trip dogfood that surfaced this bug. The fenced ```json
        // block in `payloads[0].text` uses `\n` escape sequences inside
        // the JSON string, not real newlines. Without unwrap, the parser
        // would scan the envelope's outer JSON and never see a fence line.
        let envelope = r#"{
          "payloads": [
            {"text": "```json\n{\"k\": 1}\n```", "mediaUrl": null}
          ],
          "meta": {}
        }"#;
        let unwrapped = unwrap_openclaw_envelope(envelope).expect("envelope should unwrap");
        assert!(unwrapped.contains("```json"));
        assert!(unwrapped.contains("\"k\": 1"));
        assert!(unwrapped.contains("```"));
    }

    #[test]
    fn unwrap_openclaw_envelope_concatenates_multiple_payloads() {
        // Coder dispatches emit several payloads (intermediate thinking +
        // final SIGNOFF). Concat with blank-line separator so a
        // downstream fence search isn't fooled by adjacency.
        let envelope = r#"{
          "payloads": [
            {"text": "step 1", "mediaUrl": null},
            {"text": "step 2", "mediaUrl": null}
          ]
        }"#;
        let unwrapped = unwrap_openclaw_envelope(envelope).expect("multi-payload should unwrap");
        assert!(unwrapped.contains("step 1"));
        assert!(unwrapped.contains("step 2"));
        assert!(unwrapped.contains("\n\n"));
    }

    #[test]
    fn unwrap_openclaw_envelope_returns_none_on_non_envelope() {
        // Non-envelope input should return None so callers fall through to
        // using the response as-is. Covers: raw agent reply (legacy test
        // shape), unrelated JSON, and prose.
        assert!(unwrap_openclaw_envelope("just plain prose").is_none());
        assert!(unwrap_openclaw_envelope(r#"{"unrelated": "json"}"#).is_none());
        assert!(unwrap_openclaw_envelope("").is_none());
        assert!(unwrap_openclaw_envelope(r#"{"payloads": []}"#).is_none());
    }

    #[test]
    fn parse_proposal_handles_openclaw_envelope() {
        // Integration regression: end-to-end parse from envelope -> proposal.
        // Without the unwrap step, this would fail with "no fenced ```json
        // block found in response."
        let envelope = r#"{
          "payloads": [
            {
              "text": "```json\n{\n  \"mission\": {\n    \"id\": \"env-test\",\n    \"description\": \"envelope-wrapped\",\n    \"status\": \"active\",\n    \"sprint_ids\": [\"env-s1\"],\n    \"created_ts\": 0\n  },\n  \"sprints\": [\n    {\n      \"id\": \"env-s1\",\n      \"mission_id\": \"env-test\",\n      \"description\": \"first sprint\",\n      \"status\": \"planned\",\n      \"depends_on\": [],\n      \"created_ts\": 0\n    }\n  ]\n}\n```",
              "mediaUrl": null
            }
          ],
          "meta": {"durationMs": 1000}
        }"#;
        let p = parse_proposal(envelope).expect("envelope-wrapped proposal should parse");
        assert_eq!(p.mission.id, "env-test");
        assert_eq!(p.sprints.len(), 1);
        assert_eq!(p.sprints[0].id, "env-s1");
    }

    #[test]
    fn validate_proposal_rejects_mismatched_mission_id() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Make a sprint claim a different mission_id than the mission's id.
        p.sprints[0].mission_id = "other-mission".to_string();
        let err = validate_proposal_invariants(&p).expect_err("should reject mismatched mission_id");
        assert!(err.to_string().contains("other-mission"));
    }
}
