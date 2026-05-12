//! Dispatch a crew member (role) for a single turn.
//!
//! This is the operator-facing entry point that ties the crew schema
//! (`templates/builtin/crew/roles/<id>.{json,md}`) to the actual runtime
//! (openclaw). Three responsibilities:
//!
//!   1. **Load the role** — manifest + `.md` system prompt
//!   2. **Pre-flight check** — verify the corresponding openclaw agent
//!      exists under the `darkmux/<role-id>` namespace and matches the
//!      manifest's expectations (system prompt + tool palette)
//!   3. **Dispatch** — invoke `openclaw agent darkmux/<role-id>` and return
//!      the result
//!
//! `darkmux crew sync` is the operator-explicit way to make openclaw's
//! `agents.list[]` reflect the manifests on disk — writes/updates the
//! `darkmux/<role>` entries to match what the manifests + `.md` prompts say.

use crate::crew::loader::{load_role_prompt, load_roles};
use crate::crew::types::Role;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default openclaw config path. `DARKMUX_OPENCLAW_CONFIG` env var overrides
/// (e.g., for tests).
fn default_openclaw_config() -> PathBuf {
    if let Ok(p) = std::env::var("DARKMUX_OPENCLAW_CONFIG") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".openclaw/openclaw.json")
}

/// The openclaw agent id darkmux uses for a given role. Per the
/// CLAUDE.md namespace convention: `darkmux/<role-id>`.
fn agent_id_for(role_id: &str) -> String {
    format!("darkmux/{role_id}")
}

/// Slug for the agent's on-disk dirs. Translates the `darkmux/<role>` id
/// to a filesystem-safe nested path (`agents/darkmux/<role>/agent`) and a
/// flat workspace slug (`workspace-darkmux-<role>`).
fn agent_dirs_for(role_id: &str, openclaw_root: &Path) -> (PathBuf, PathBuf) {
    let agent_dir = openclaw_root.join("agents").join("darkmux").join(role_id).join("agent");
    let workspace = openclaw_root.join(format!("workspace-darkmux-{role_id}"));
    (agent_dir, workspace)
}

#[derive(Debug)]
pub struct DispatchOpts {
    pub role_id: String,
    pub message: String,
    /// Optional delivery target in `<channel>:<target>` form
    /// (e.g. `discord:1500166601909993503`).
    pub deliver: Option<String>,
    pub session_id: Option<String>,
    pub timeout_seconds: u32,
    /// Skip the pre-flight checks. Use only when explicitly debugging.
    pub skip_preflight: bool,
}

#[derive(Debug)]
pub struct DispatchResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run a single dispatch end-to-end.
pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    // 1. Load the role + its .md prompt
    let role = load_role_or_bail(&opts.role_id)?;
    let prompt = role_prompt_or_bail(&role)?;
    let agent_id = agent_id_for(&opts.role_id);

    // 2. Pre-flight against openclaw config
    if !opts.skip_preflight {
        let openclaw_path = default_openclaw_config();
        let openclaw_config = read_openclaw_config(&openclaw_path)?;
        preflight_check(&openclaw_config, &agent_id, &role, &prompt)
            .with_context(|| {
                format!(
                    "pre-flight failed for `{agent_id}`. Run `darkmux crew sync` to update openclaw config from the manifests."
                )
            })?;
    }

    // 3. Invoke openclaw agent
    let mut cmd = Command::new("openclaw");
    cmd.args(["agent", "--local", "--agent", &agent_id, "--json"]);
    if let Some(sess) = &opts.session_id {
        cmd.args(["--session-id", sess]);
    }
    cmd.args(["--timeout", &opts.timeout_seconds.to_string()]);
    if let Some(deliver) = &opts.deliver {
        let (chan, target) = deliver
            .split_once(':')
            .ok_or_else(|| anyhow!("--deliver must be `<channel>:<target>`, got `{deliver}`"))?;
        cmd.args(["--channel", chan, "--reply-to", target, "--deliver"]);
    }
    cmd.args(["--message", &opts.message]);

    let output = cmd
        .output()
        .with_context(|| format!("running `openclaw agent {agent_id}`"))?;

    Ok(DispatchResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

fn load_role_or_bail(role_id: &str) -> Result<Role> {
    let roles = load_roles().context("loading crew role manifests")?;
    roles
        .into_iter()
        .find(|r| r.id == role_id)
        .ok_or_else(|| anyhow!("no role with id `{role_id}` found in crew manifests"))
}

fn role_prompt_or_bail(role: &Role) -> Result<String> {
    load_role_prompt(&role.id).ok_or_else(|| {
        anyhow!(
            "role `{}` has no `.md` system prompt. Author one at \
             `templates/builtin/crew/roles/{}.md` (or override at \
             `<crew_root>/roles/{}.md`).",
            role.id, role.id, role.id
        )
    })
}

fn read_openclaw_config(path: &Path) -> Result<Value> {
    if !path.exists() {
        bail!(
            "openclaw config not found at {}. \
             Set DARKMUX_OPENCLAW_CONFIG to override.",
            path.display()
        );
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading openclaw config at {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing openclaw config at {}", path.display()))
}

/// The three pre-flight checks from issue #55, scoped to what's verifiable
/// pre-dispatch:
///
///   - inventory: the `darkmux/<role>` agent entry exists
///   - field consistency: systemPromptOverride matches the manifest's `.md`
///   - tool palette matches the manifest's `tool_palette`
fn preflight_check(config: &Value, agent_id: &str, role: &Role, expected_prompt: &str) -> Result<()> {
    let agents_list = config
        .get("agents")
        .and_then(|a| a.get("list"))
        .and_then(|l| l.as_array())
        .ok_or_else(|| anyhow!("openclaw config has no `agents.list` array"))?;

    let entry = agents_list
        .iter()
        .find(|a| a.get("id").and_then(|s| s.as_str()) == Some(agent_id))
        .ok_or_else(|| {
            anyhow!(
                "agent `{agent_id}` not found in openclaw `agents.list[]`. \
                 The crew dispatch expects darkmux-namespaced agents to exist; \
                 run `darkmux crew sync` to create them from the manifests."
            )
        })?;

    // System prompt match
    let actual_prompt = entry
        .get("systemPromptOverride")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("agent `{agent_id}` has no systemPromptOverride"))?;
    if actual_prompt.trim() != expected_prompt.trim() {
        bail!(
            "agent `{agent_id}` systemPromptOverride drifted from the role manifest's `.md`. \
             Manifest expects {expected_chars} chars; openclaw has {actual_chars} chars. \
             Run `darkmux crew sync` to reconcile.",
            expected_chars = expected_prompt.len(),
            actual_chars = actual_prompt.len(),
        );
    }

    // Tool palette match (allow set)
    let expected_allow: Vec<&str> = role.tool_palette.allow.iter().map(|s| s.as_str()).collect();
    let actual_allow: Vec<&str> = entry
        .get("tools")
        .and_then(|t| t.get("allow"))
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let expected_sorted = {
        let mut v = expected_allow.clone();
        v.sort();
        v
    };
    let actual_sorted = {
        let mut v = actual_allow.clone();
        v.sort();
        v
    };
    if expected_sorted != actual_sorted {
        bail!(
            "agent `{agent_id}` tool palette (allow) drifted from manifest. \
             Manifest expects {expected:?}; openclaw has {actual:?}. \
             Run `darkmux crew sync` to reconcile.",
            expected = expected_sorted,
            actual = actual_sorted,
        );
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
//   sync
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct SyncResult {
    /// Agents added.
    pub added: Vec<String>,
    /// Agents updated (entry existed but drifted; reconciled to match manifest).
    pub updated: Vec<String>,
    /// Agents already in sync — no change.
    pub unchanged: Vec<String>,
    /// Roles skipped because they have no `.md` prompt (can't dispatch them).
    pub skipped_no_prompt: Vec<String>,
}

#[derive(Debug)]
pub struct SyncOpts {
    pub dry_run: bool,
}

/// Reconcile openclaw's `agents.list[]` with the crew role manifests:
/// for every role that has a `.md` system prompt, ensure a
/// `darkmux/<role-id>` agent exists with the manifest-derived shape.
pub fn sync(opts: SyncOpts) -> Result<SyncResult> {
    let roles = load_roles().context("loading crew role manifests")?;
    let openclaw_path = default_openclaw_config();
    let mut config = read_openclaw_config(&openclaw_path)?;
    let openclaw_root = openclaw_path
        .parent()
        .ok_or_else(|| anyhow!("openclaw config path has no parent: {}", openclaw_path.display()))?
        .to_path_buf();

    let mut result = SyncResult::default();
    let mut config_modified = false;

    // Ensure agents.list exists.
    let agents = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("openclaw config root is not an object"))?
        .entry("agents".to_string())
        .or_insert_with(|| json!({}));
    let agents_obj = agents
        .as_object_mut()
        .ok_or_else(|| anyhow!("`agents` is not an object"))?;
    let agents_list = agents_obj
        .entry("list".to_string())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| anyhow!("`agents.list` is not an array"))?;

    for role in &roles {
        // Only sync roles that have an authored `.md` prompt — otherwise
        // there's nothing to dispatch with. Checks user dir first, then
        // bundled BUILTIN_ROLE_PROMPTS.
        let prompt = match load_role_prompt(&role.id) {
            Some(p) => p,
            None => {
                result.skipped_no_prompt.push(role.id.clone());
                continue;
            }
        };

        let agent_id = agent_id_for(&role.id);
        let (agent_dir, workspace_dir) = agent_dirs_for(&role.id, &openclaw_root);

        let expected_entry = build_agent_entry(role, &prompt, &agent_dir, &workspace_dir);
        let existing_pos = agents_list.iter().position(|a| {
            a.get("id").and_then(|s| s.as_str()) == Some(&agent_id)
        });

        match existing_pos {
            None => {
                if !opts.dry_run {
                    agents_list.push(expected_entry);
                    // Create the on-disk dirs the openclaw runtime will expect.
                    fs::create_dir_all(&agent_dir)
                        .with_context(|| format!("creating {}", agent_dir.display()))?;
                    fs::create_dir_all(&workspace_dir)
                        .with_context(|| format!("creating {}", workspace_dir.display()))?;
                    config_modified = true;
                }
                result.added.push(agent_id);
            }
            Some(i) => {
                if agents_list[i] != expected_entry {
                    if !opts.dry_run {
                        agents_list[i] = expected_entry;
                        fs::create_dir_all(&agent_dir)
                            .with_context(|| format!("creating {}", agent_dir.display()))?;
                        fs::create_dir_all(&workspace_dir)
                            .with_context(|| format!("creating {}", workspace_dir.display()))?;
                        config_modified = true;
                    }
                    result.updated.push(agent_id);
                } else {
                    result.unchanged.push(agent_id);
                }
            }
        }
    }

    if config_modified && !opts.dry_run {
        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(&openclaw_path, pretty + "\n")
            .with_context(|| format!("writing {}", openclaw_path.display()))?;
    }

    Ok(result)
}

fn build_agent_entry(role: &Role, prompt: &str, agent_dir: &Path, workspace: &Path) -> Value {
    let mut tools = Map::new();
    tools.insert(
        "allow".to_string(),
        Value::Array(role.tool_palette.allow.iter().cloned().map(Value::String).collect()),
    );
    if !role.tool_palette.deny.is_empty() {
        tools.insert(
            "deny".to_string(),
            Value::Array(role.tool_palette.deny.iter().cloned().map(Value::String).collect()),
        );
    }
    json!({
        "id": agent_id_for(&role.id),
        "name": role.id,
        "agentDir": agent_dir.display().to_string(),
        "workspace": workspace.display().to_string(),
        "systemPromptOverride": prompt,
        "tools": Value::Object(tools),
        "skills": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::types::{EscalationContract, Role, ToolPalette};
    use tempfile::TempDir;

    fn sample_role() -> Role {
        Role {
            id: "code-reviewer".to_string(),
            description: "test".to_string(),
            capabilities: vec!["code-reviewing".to_string()],
            tool_palette: ToolPalette {
                allow: vec!["read".to_string(), "exec".to_string()],
                deny: vec!["edit".to_string(), "write".to_string()],
            },
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
        }
    }

    #[test]
    fn agent_id_uses_darkmux_namespace() {
        assert_eq!(agent_id_for("code-reviewer"), "darkmux/code-reviewer");
        assert_eq!(agent_id_for("analyst"), "darkmux/analyst");
    }

    #[test]
    fn agent_dirs_use_nested_namespace_for_agentdir() {
        let root = Path::new("/tmp/.openclaw");
        let (agent_dir, workspace) = agent_dirs_for("code-reviewer", root);
        assert_eq!(agent_dir, Path::new("/tmp/.openclaw/agents/darkmux/code-reviewer/agent"));
        assert_eq!(workspace, Path::new("/tmp/.openclaw/workspace-darkmux-code-reviewer"));
    }

    #[test]
    fn build_agent_entry_includes_tools_and_prompt() {
        let role = sample_role();
        let agent_dir = PathBuf::from("/tmp/agent");
        let workspace = PathBuf::from("/tmp/ws");
        let entry = build_agent_entry(&role, "PROMPT", &agent_dir, &workspace);
        assert_eq!(entry["id"], "darkmux/code-reviewer");
        assert_eq!(entry["systemPromptOverride"], "PROMPT");
        assert_eq!(entry["tools"]["allow"], json!(["read", "exec"]));
        assert_eq!(entry["tools"]["deny"], json!(["edit", "write"]));
        assert_eq!(entry["skills"], json!([]));
    }

    #[test]
    fn build_agent_entry_omits_empty_deny() {
        let mut role = sample_role();
        role.tool_palette.deny.clear();
        let entry = build_agent_entry(&role, "PROMPT", Path::new("/x"), Path::new("/y"));
        // No "deny" key when the list is empty.
        assert!(entry["tools"].get("deny").is_none());
    }

    #[test]
    fn preflight_passes_when_config_matches() {
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "tools": {"allow": ["read", "exec"], "deny": ["edit", "write"]}
                    }
                ]
            }
        });
        assert!(preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").is_ok());
    }

    #[test]
    fn preflight_fails_when_agent_missing() {
        let role = sample_role();
        let config = json!({"agents": {"list": []}});
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("not found in openclaw"));
    }

    #[test]
    fn preflight_fails_when_prompt_drifts() {
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "STALE",
                        "tools": {"allow": ["read", "exec"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("systemPromptOverride drifted"));
    }

    #[test]
    fn preflight_fails_when_tool_palette_drifts() {
        let role = sample_role();
        let config = json!({
            "agents": {
                "list": [
                    {
                        "id": "darkmux/code-reviewer",
                        "systemPromptOverride": "EXPECTED",
                        "tools": {"allow": ["read", "edit"]}
                    }
                ]
            }
        });
        let err = preflight_check(&config, "darkmux/code-reviewer", &role, "EXPECTED").unwrap_err();
        assert!(err.to_string().contains("tool palette"));
    }

    #[test]
    fn sync_adds_missing_agent_to_empty_config() {
        let tmp = TempDir::new().unwrap();
        let openclaw_path = tmp.path().join("openclaw.json");
        fs::write(&openclaw_path, "{}").unwrap();

        // Test the build_agent_entry logic directly — sync() itself depends
        // on load_roles() which reads on-disk manifests; we test the
        // single-role write path here, then exercise the integration path
        // via a CLI test (in tests/cli.rs).
        let role = sample_role();
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&openclaw_path).unwrap()).unwrap();
        let agents = config.as_object_mut().unwrap().entry("agents".to_string()).or_insert_with(|| json!({}));
        let list = agents.as_object_mut().unwrap().entry("list".to_string()).or_insert_with(|| json!([]));
        let entry = build_agent_entry(&role, "PROMPT", Path::new("/x"), Path::new("/y"));
        list.as_array_mut().unwrap().push(entry);

        assert_eq!(
            config["agents"]["list"][0]["id"],
            "darkmux/code-reviewer"
        );
    }
}
