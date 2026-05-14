//! `darkmux mission propose` — AI-built-in verb that takes unstructured
//! operator intent and emits a structured Mission + Sprint proposal
//! JSON via internal dispatch to the `mission-compiler` admin role
//! (#113 Sprint 1). Operator approval gate is mandatory; the proposal
//! is only persisted to disk after the operator accepts (operator-
//! sovereignty per #44).
//!
//! Flow:
//!   1. Read unstructured input (stdin or file)
//!   2. Resolve engagement context (CLI flag or interactive prompt)
//!   3. Dispatch internally to `darkmux/mission-compiler`
//!   4. Parse the proposal JSON from the response (fenced ```json block)
//!   5. Render a human-readable summary
//!   6. Prompt operator: approve / edit / reject / regenerate
//!   7. On approve: write Mission + Sprint JSONs to
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
    engagement: Option<&str>,
    yes: bool,
) -> Result<i32> {
    // 1. Read input
    let input = read_input(from_stdin, from_file)?;
    if input.trim().is_empty() {
        return Err(anyhow!("mission propose: empty input — nothing to compile"));
    }

    // 2. Resolve engagement
    let engagement = match engagement {
        Some(s) => s.to_string(),
        None => prompt_engagement()?,
    };

    let mut hint: Option<String> = None;
    loop {
        // 3. Dispatch
        eprintln!("mission propose: dispatching mission-compiler …");
        let response = dispatch_compiler(&input, &engagement, hint.as_deref())?;

        // 4. Parse the proposal
        let proposal = match parse_proposal(&response) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mission propose: failed to parse proposal: {e}");
                eprintln!("--- raw response ---\n{response}\n--- end response ---");
                return Err(anyhow!("mission propose: unparseable response"));
            }
        };

        // 5. Render summary
        render_proposal(&proposal);

        // 6. Operator decision
        if yes {
            eprintln!("mission propose: --yes flag set, applying without prompt");
            return persist(&proposal);
        }
        match prompt_decision()? {
            Decision::Approve => return persist(&proposal),
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

fn prompt_engagement() -> Result<String> {
    eprint!("engagement context (e.g. \"darkmux\", \"wife time\", \"job hunt\"): ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).context("reading engagement")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Err(anyhow!("mission propose: engagement required (empty input)"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn dispatch_compiler(input: &str, engagement: &str, hint: Option<&str>) -> Result<String> {
    let message = build_compiler_message(input, engagement, hint);
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

/// Build the message the mission-compiler sees. Format chosen for
/// clarity to the admin agent: the unstructured intent first, then
/// the engagement, then any regeneration hint.
fn build_compiler_message(input: &str, engagement: &str, hint: Option<&str>) -> String {
    let mut msg = String::new();
    msg.push_str("Unstructured intent:\n---\n");
    msg.push_str(input.trim());
    msg.push_str("\n---\n\n");
    msg.push_str(&format!("Engagement context: {engagement}\n"));
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
fn parse_proposal(response: &str) -> Result<Proposal> {
    let json_str = extract_json_block(response)?;
    serde_json::from_str(&json_str).with_context(|| format!("parsing proposal JSON:\n{json_str}"))
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
    std::io::stdin().lock().read_line(&mut line).context("reading decision")?;
    match line.trim().to_lowercase().chars().next() {
        Some('y') => Ok(Decision::Approve),
        Some('n') => Ok(Decision::Reject),
        Some('e') => {
            eprint!("Hint (one line): ");
            std::io::stderr().flush().ok();
            let mut hint = String::new();
            std::io::stdin().lock().read_line(&mut hint).context("reading hint")?;
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

    // Write everything (mission first, then sprints).
    let mission_json = serde_json::to_string_pretty(&mission).context("serializing mission")?;
    std::fs::write(&mission_path, format!("{mission_json}\n"))
        .with_context(|| format!("writing {}", mission_path.display()))?;
    println!("wrote mission: {}", mission_path.display());

    for s in &p.sprints {
        let mut sprint = s.clone();
        if sprint.created_ts == 0 { sprint.created_ts = now; }
        let sp = sprints_dir.join(format!("{}.json", sprint.id));
        let sprint_json = serde_json::to_string_pretty(&sprint).context("serializing sprint")?;
        std::fs::write(&sp, format!("{sprint_json}\n"))
            .with_context(|| format!("writing {}", sp.display()))?;
        println!("wrote sprint:  {}", sp.display());
    }

    eprintln!("mission propose: persisted {} mission + {} sprints", 1, p.sprints.len());
    eprintln!("next: `darkmux mission start {}` to begin, or `--start` flag (#113 Sprint 4)", p.mission.id);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let msg = build_compiler_message("intent", "darkmux", Some("merge sprints 3 and 4"));
        assert!(msg.contains("intent"));
        assert!(msg.contains("darkmux"));
        assert!(msg.contains("merge sprints 3 and 4"));
        assert!(msg.contains("regeneration hint"));
    }

    #[test]
    fn build_compiler_message_omits_hint_when_absent() {
        let msg = build_compiler_message("intent", "darkmux", None);
        assert!(msg.contains("intent"));
        assert!(!msg.contains("regeneration hint"));
    }
}
