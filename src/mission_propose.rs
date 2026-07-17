//! `darkmux mission propose` — AI-built-in verb that takes unstructured
//! operator intent and emits a structured Mission + Phase proposal
//! JSON via internal dispatch to the `mission-compiler` utility role
//! (#113 Phase 1). Operator approval gate is mandatory; the proposal
//! is only persisted to disk after the operator accepts (operator-
//! sovereignty per #44).
//!
//! Engagement context is intentionally NOT a parameter of this verb —
//! see CLAUDE.md's "Engagements (operator-defined dreamscapes)"
//! section for doctrine. The frontier orchestrator carries engagement
//! nuance natively into the input text it crafts; quantizing it into a
//! CLI arg would (a) compress a dreamscape into a token, (b) push the
//! interpretation layer onto a 4B utility agent that doesn't have the
//! shape for it, (c) violate the #49 rule that engagement lives in the
//! operator-judgment layer above the system.
//!
//! Flow:
//!   1. Read unstructured input (stdin or file)
//!   2. Dispatch internally to `darkmux/mission-compiler`
//!   3. Parse the proposal JSON from the response (fenced ```json block)
//!   4. Render a human-readable summary
//!   5. Prompt operator: approve / edit / reject / regenerate
//!   6. On approve: write Mission + Phase JSONs to
//!      `~/.darkmux/crew/missions/` and `~/.darkmux/crew/phases/`

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

/// The shape the mission-compiler emits inside its fenced ```json block.
/// Matches the schema documented in
/// `templates/builtin/roles/mission-compiler.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Proposal {
    mission: ProposedMission,
    phases: Vec<ProposedPhase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedMission {
    id: String,
    description: String,
    #[serde(default = "default_active")]
    status: String,
    phase_ids: Vec<String>,
    #[serde(default)]
    created_ts: u64,
    /// (#815) The operator's verbatim propose input, stamped at persist
    /// time (never round-tripped through the compiler — the whole point
    /// is that this text survives unsummarized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_input: Option<String>,
    /// (#816) Work-item / ticket id, stamped at persist time from the
    /// `--ticket` flag (never compiler-derived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ticket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedPhase {
    id: String,
    mission_id: String,
    description: String,
    /// (#1398) A short operator-facing label, alongside the (deliberately
    /// long) `description` — see `PhaseConfig::display_name`'s doc for why
    /// the two are separate fields. `mission-compiler.md`'s schema asks
    /// the model to draft one for every phase; absent (an older-schema
    /// response, or a model that skipped it) is tolerated — the graph lens
    /// falls back to `id`, never a hard error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(default = "default_planned")]
    status: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    created_ts: u64,
}

fn default_active() -> String {
    "active".to_string()
}
fn default_planned() -> String {
    "planned".to_string()
}

/// Top-level entry called from main.rs's dispatch table.
pub fn propose(
    from_stdin: bool,
    from_file: Option<&std::path::Path>,
    yes: bool,
    start: bool,
    ticket: Option<&str>,
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
        // hallucinated mission_id, missing phase_id, or dangling
        // depends_on slips through serde happily but lands a broken
        // mission on disk. Surface it now so the operator can hit `e`
        // and regenerate with a hint instead of debugging the viewer.
        if let Err(e) = validate_proposal_invariants(&proposal) {
            eprintln!("mission propose: proposal failed validation: {e}");
            eprintln!("--- raw response ---\n{response}\n--- end response ---");
            return Err(anyhow!(
                "mission propose: invalid proposal cross-references"
            ));
        }

        // 4. Render summary
        render_proposal(&proposal);

        // 5. Operator decision
        if yes {
            eprintln!("mission propose: --yes flag set, applying without prompt");
            return persist_and_maybe_start(&proposal, start, &input, ticket);
        }
        match prompt_decision()? {
            Decision::Approve => return persist_and_maybe_start(&proposal, start, &input, ticket),
            Decision::Reject => {
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

fn read_input(from_stdin: bool, from_file: Option<&std::path::Path>) -> Result<String> {
    if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        Ok(buf)
    } else if let Some(p) = from_file {
        std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))
    } else {
        Err(anyhow!(
            "internal: exactly one of --from-stdin / --from-file required"
        ))
    }
}

/// Builds the `mission.compile.error` record fired by
/// `dispatch_compiler`'s bookend guard on `Drop` when the guarded region
/// exits without reaching its own terminal emit (a panic, or a future
/// early return added between `open` and `close`). Named separately so a
/// test can drive the abort shape directly, without a real dispatch
/// (#1413). `pub(crate)` so `phase_cli`'s test module can exercise the
/// same guard shape mission_propose.rs wires up in production.
pub(crate) fn mission_compile_abort_record(session_id: &str) -> crate::flow::FlowRecord {
    crate::crew::dispatch::build_dispatch_record_with_payload(
        crate::flow::Level::Error,
        "mission.compile.error",
        "mission-compiler",
        session_id,
        None,
        None,
        None,
        Some(serde_json::json!({
            "result_class": "error",
            "error": "mission compile terminated before completion (early return or panic)",
        })),
    )
}

fn dispatch_compiler(input: &str, hint: Option<&str>) -> Result<String> {
    let message = build_compiler_message(input, hint);

    // Emit mission.compile.start flow record (#204) — pairs with
    // .complete below via the synthesized session_id so the viewer can
    // measure compile wall-time + render the input/output sizes.
    let synth_session_id = format!(
        "mission-compile-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0)
    );
    let input_chars = input.chars().count();

    // (#1413) `mission.compile.start`/`.complete`/`.error` used to be plain
    // paired writes: a panic between them (or a future early return added
    // to this function) orphaned the start record. Bookend-guard the pair
    // so every start gets a matching terminal on every exit path, same
    // contract the INNER `dispatch_routed` call already honors for its own
    // `dispatch.start`/`dispatch.complete`.
    let mut sink = |r: crate::flow::FlowRecord| {
        let _ = crate::flow::record(r);
    };
    let abort_session_id = synth_session_id.clone();
    let on_abort = move |_id: &str, _kind: &str| mission_compile_abort_record(&abort_session_id);
    let mut bookend = crate::flow::BookendGuard::new(&mut sink, on_abort);

    let compile_start_payload = serde_json::json!({
        "input_chars": input_chars,
        "has_hint": hint.is_some(),
    });
    bookend.open(
        "mission.compile",
        "mission.compile",
        crate::crew::dispatch::build_dispatch_record_with_payload(
            crate::flow::Level::Info,
            "mission.compile.start",
            "mission-compiler",
            &synth_session_id,
            None,
            None,
            None,
            Some(compile_start_payload),
        ),
    );
    let compile_start_instant = std::time::Instant::now();

    let opts = crate::crew::dispatch::DispatchOpts {
        role_id: "mission-compiler".to_string(),
        message,
        session_id: None,
        timeout_seconds: 600,
        skip_preflight: false,
        // mission-compiler parses its compiled output from the dispatch's
        // human-readable stdout — no JSON envelope needed.
        json: false,
        // mission-compiler reads stdin-piped intent; no scope override.
        workdir: None,
        // mission-compiler runs BEFORE any mission/phases exist on disk —
        // there's nothing to bind to and no parent context to inject.
        phase_id: None,
        // Mission propose is a system-level utility dispatch; runs
        // through the internal Docker-bounded runtime — the only
        // dispatch path (#309, #1405).
        machine: None,
        wait: true,
        // Mission-compile dispatch uses runtime-default compaction;
        // mission-compiler role is utility-family and doesn't accumulate
        // large per-turn context.
        compaction: crate::crew::dispatch::CompactionDispatchArgs::default(),
        // (#549) No `--profile` override; fall back to `default_profile`.
        profile_name: None,
        // (#984) No --profiles-file here; dispatch resolves from env > default.
        config_path: None,
        // (#703) default image.
        // (#1199) Bench-only knobs; defaults preserve existing behavior.
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: None,
    };
    let dispatch_result = crate::fleet::dispatch_routed(opts);

    let wall_ms = compile_start_instant.elapsed().as_millis() as u64;
    let (success, output_chars) = match &dispatch_result {
        Ok(r) => (true, r.stdout.chars().count()),
        Err(_) => (false, 0),
    };
    let compile_complete_payload = serde_json::json!({
        "input_chars": input_chars,
        "output_chars": output_chars,
        "wall_ms": wall_ms,
        "result_class": if success { "ok" } else { "error" },
    });
    let (action, level) = if success {
        ("mission.compile.complete", crate::flow::Level::Info)
    } else {
        ("mission.compile.error", crate::flow::Level::Error)
    };
    // `close()` emits the terminal record and disarms the guard, so the
    // `?`-propagation below (the pre-existing error path) never re-fires
    // the Drop-time abort record on top of this one.
    bookend.close(
        "mission.compile",
        crate::crew::dispatch::build_dispatch_record_with_payload(
            level,
            action,
            "mission-compiler",
            &synth_session_id,
            None,
            None,
            None,
            Some(compile_complete_payload),
        ),
    );

    let result = dispatch_result.context("dispatch to mission-compiler failed")?;
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
        msg.push_str(
            "\nOperator-provided regeneration hint (apply this when restructuring):\n---\n",
        );
        msg.push_str(h);
        msg.push_str("\n---\n");
    }
    msg.push_str(
        "\nEmit a single fenced ```json block containing the Proposal object, per your role's schema.\n",
    );
    msg
}

/// Extract the JSON object from a ```json fenced block in the
/// response. The utility agent's role prompt requires exactly one such
/// block; we strip everything outside it and parse the inner.
///
fn parse_proposal(response: &str) -> Result<Proposal> {
    let json_str = extract_json_block(response)?;
    serde_json::from_str(&json_str).with_context(|| format!("parsing proposal JSON:\n{json_str}"))
}

/// Validate cross-references inside a Proposal so a hallucinated /
/// drifted utility-agent output can't land on disk as a broken
/// mission+phases set. `add_phase_to_mission` already enforces these
/// invariants for the mid-flight add-phase path; `propose` should hold
/// the same bar at first-write time. Three checks:
///
///   - Every `phases[].mission_id` matches `mission.id`
///   - Every entry in `mission.phase_ids` exists as a `phases[].id`
///   - Every `phases[].depends_on` value references an in-proposal id
///
/// Returns a readable error so the operator can hit `e` to regenerate
/// with a hint instead of having to discover the problem post-persist
/// in the viewer's phase-progress widget.
/// (#867 security) Reject any MODEL-supplied mission/phase id that isn't a
/// safe single path component, BEFORE it reaches `lifecycle::mission_path` /
/// `phase_path` (which build paths by raw `join`). A `../`- or `/`-bearing id
/// would otherwise escape the crew dir — a controlled `.json`/`mission.json`
/// write-primitive from untrusted local-model output. `validate_identifier`
/// rejects anything outside `[a-z0-9_-]`, so `.`/`/` (hence `..`) can't pass;
/// this mirrors the guard `mission_run.rs:316` / `main.rs:1645` already apply
/// to operator-supplied ids. Shared by `validate_proposal_invariants` (early /
/// UX rejection) and `persist` (the write boundary) so the two never drift.
fn validate_proposal_ids(p: &Proposal) -> Result<()> {
    crate::fleet::validate_identifier("mission_id", &p.mission.id)?;
    for s in &p.phases {
        crate::fleet::validate_identifier("phase_id", &s.id)?;
    }
    Ok(())
}

fn validate_proposal_invariants(p: &Proposal) -> Result<()> {
    // Charset-validate the path-bearing ids before any cross-ref logic (#867).
    validate_proposal_ids(p)?;

    let phase_ids: std::collections::HashSet<&str> =
        p.phases.iter().map(|s| s.id.as_str()).collect();

    for s in &p.phases {
        if s.mission_id != p.mission.id {
            return Err(anyhow!(
                "proposal invariant: phase `{}` has mission_id `{}` but mission is `{}`",
                s.id,
                s.mission_id,
                p.mission.id
            ));
        }
        for dep in &s.depends_on {
            if !phase_ids.contains(dep.as_str()) {
                return Err(anyhow!(
                    "proposal invariant: phase `{}` depends_on `{}` which is not in the proposal",
                    s.id,
                    dep
                ));
            }
        }
    }

    for sid in &p.mission.phase_ids {
        if !phase_ids.contains(sid.as_str()) {
            return Err(anyhow!(
                "proposal invariant: mission.phase_ids references `{}` which is not in phases[]",
                sid
            ));
        }
    }

    Ok(())
}

/// Find the JSON fenced block and return its inner content. Prefers a
/// ```json-tagged opener over a bare ``` fence (#896), falling back to the
/// first bare fence only when no tagged opener exists. Forgiving about
/// whitespace/case for the language tag. On a truncated/unterminated block
/// it returns a distinct "unterminated" error rather than "no block found".
fn extract_json_block(response: &str) -> Result<String> {
    let lines: Vec<&str> = response.lines().collect();

    // (#896) Prefer a ```json-tagged opener over a bare ``` fence. A bare
    // fence wrapping some OTHER block before the real JSON would otherwise
    // be picked as the opener and capture the wrong region. Fall back to
    // the first bare fence only when no tagged opener exists anywhere.
    let is_tagged = |t: &str| t.starts_with("```json") || t.starts_with("```JSON");
    let opener = lines
        .iter()
        .position(|l| is_tagged(l.trim_start()))
        .or_else(|| lines.iter().position(|l| l.trim_start() == "```"));
    let Some(open_idx) = opener else {
        return Err(anyhow!("no fenced ```json block found in response"));
    };

    // Capture from the line after the opener up to the next closing ```.
    let mut buf = String::new();
    for line in &lines[open_idx + 1..] {
        if line.trim_start().starts_with("```") {
            return Ok(buf);
        }
        buf.push_str(line);
        buf.push('\n');
    }

    // (#896) Opener found but no closing fence: the response was almost
    // certainly truncated mid-block (common with token-capped local
    // models). Emit a DISTINCT error rather than the misleading "no block
    // found" — there WAS a block, it just didn't finish. We deliberately do
    // NOT brace-balance the partial: truncated JSON balanced into valid-but-
    // incomplete JSON would parse into a wrong proposal, which is worse than
    // a clear "retry with more tokens" signal.
    Err(anyhow!(
        "unterminated fenced ```json block (response likely truncated at {} captured chars)",
        buf.len()
    ))
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
    eprintln!("  Phases ({}):", p.phases.len());
    for (i, s) in p.phases.iter().enumerate() {
        let deps = if s.depends_on.is_empty() {
            "—".to_string()
        } else {
            s.depends_on.join(", ")
        };
        eprintln!("    {}. {} (depends_on: {deps})", i + 1, s.id);
        // Show only the first line of each phase description in the
        // summary — keeps the table-style render readable when phase
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

/// Build a mission CONFIG document (#1284 Packet 4a) from a parsed
/// `Proposal` — the artifact `persist` now writes, replacing the pre-
/// Packet-4a direct Mission+Phase instance emission. Every phase becomes a
/// trivial, TASK-LESS `PhaseConfig` (the mission-compiler's schema has no
/// notion of a Task/Step graph — it only ever proposed the freeform/manual
/// shape), so a launched proposal always takes the "mint + start, leave
/// phase transitions operator-driven" path `mission_launch::launch`
/// documents for zero-task configs.
///
/// Phase ORDER follows `mission.phase_ids` (the compiler's own intended
/// execution order, matching `PhaseConfig`'s #1341 strictly-linear-phase
/// doctrine — list POSITION is the only ordering a config expresses).
/// `ProposedPhase.depends_on` is validated for cross-reference sanity by
/// `validate_proposal_invariants` upstream but has no field on
/// `PhaseConfig` to land in — dropped here, same as it always was silently
/// dropped by the pre-Packet-4a `crew::types::Phase` (no `depends_on`
/// field there either).
///
/// `source_input`/`ticket` are genuinely PER-INSTANCE data with no home in
/// `MissionConfig`'s schema (a config is meant to be launched more than
/// once) — preserved anyway, in `extras` (the schema's forward-compat
/// overflow bag), so the operator's original words + ticket id aren't
/// silently discarded. `mission_launch::launch` doesn't read them back
/// today; a future minor schema bump can promote them to real fields if a
/// consumer needs them.
fn build_mission_config(
    p: &Proposal,
    source_input: &str,
    ticket: Option<&str>,
) -> darkmux_crew::mission_config::MissionConfig {
    use darkmux_crew::mission_config::{MissionConfig, PhaseConfig, MISSION_CONFIG_SCHEMA};
    use std::collections::BTreeMap;

    let mut extras = BTreeMap::new();
    if !source_input.trim().is_empty() {
        extras.insert("source_input".to_string(), serde_json::Value::String(source_input.to_string()));
    }
    if let Some(t) = ticket.map(str::trim).filter(|t| !t.is_empty()) {
        extras.insert("ticket".to_string(), serde_json::Value::String(t.to_string()));
    }

    let by_id: std::collections::HashMap<&str, &ProposedPhase> =
        p.phases.iter().map(|s| (s.id.as_str(), s)).collect();
    let phases: Vec<PhaseConfig> = p
        .mission
        .phase_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()))
        .map(|s| PhaseConfig {
            id: s.id.clone(),
            description: Some(s.description.clone()),
            display_name: s.display_name.clone(),
            tasks: Vec::new(),
            extras: BTreeMap::new(),
        })
        .collect();

    MissionConfig {
        id: p.mission.id.clone(),
        name: humanize(&p.mission.id),
        description: Some(p.mission.description.clone()),
        schema_version: Some(MISSION_CONFIG_SCHEMA.to_string()),
        inputs: Vec::new(),
        phases,
        extras,
    }
}

/// `"my-trip-plan"` -> `"My Trip Plan"` — a readable default `name` derived
/// from the compiler's own `id` (the compiler schema has no separate
/// human-readable name field; `MissionConfig.name` is required non-empty).
fn humanize(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Persist the operator-approved proposal as a mission CONFIG JSON at the
/// user tier (`~/.darkmux/mission-configs/<id>.json`, #1284 Packet 4a —
/// deferred from Packet 1). The old direct Mission+Phase INSTANCE emission
/// is retired (clean break, pre-1.0 posture — no compat alias); `darkmux
/// mission launch <id>` is now the follow-up verb, printed below on success.
fn persist(p: &Proposal, source_input: &str, ticket: Option<&str>) -> Result<i32> {
    use crate::crew::loader;

    // (#867 security boundary) Re-assert id safety at the path-construction
    // site — a model-supplied `../`/`/`-id must never reach here even if a
    // future caller skips `validate_proposal_invariants`. This function,
    // not its callers, owns the path-write boundary.
    validate_proposal_ids(p)?;

    let config = build_mission_config(p, source_input, ticket);

    let dir = loader::mission_configs_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = dir.join(format!("{}.json", config.id));
    if path.exists() {
        return Err(anyhow!(
            "mission propose: mission config `{}` already exists at {} — aborting (no overwrite)",
            config.id,
            path.display()
        ));
    }

    let json = serde_json::to_string_pretty(&config).context("serializing mission config")?;
    std::fs::write(&path, format!("{json}\n")).with_context(|| format!("writing {}", path.display()))?;

    eprintln!(
        "mission propose: persisted mission config `{}` ({} phase(s))",
        config.id,
        config.phases.len()
    );
    println!("wrote mission config: {}", path.display());
    Ok(0)
}

/// Helper that persists a proposal and optionally launches it.
fn persist_and_maybe_start(
    p: &Proposal,
    start: bool,
    source_input: &str,
    ticket: Option<&str>,
) -> Result<i32> {
    let exit = persist(p, source_input, ticket)?;
    if exit != 0 {
        return Ok(exit);
    }
    if start {
        eprintln!("mission propose: --start flag set, launching `{}` …", p.mission.id);
        // `None` -> per-config default (600 for the generic path; a proposed
        // config is never `review`, but the resolution lives in one place —
        // see `mission_launch::launch`'s doc).
        crate::mission_launch::launch(&p.mission.id, None, &[], None)
    } else {
        eprintln!(
            "next: `darkmux mission launch {}` to begin (or pass `--start` next time)",
            p.mission.id
        );
        Ok(0)
    }
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
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
            }
            Self { prev, _tmp: tmp }
        }

        #[allow(dead_code)]
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
    fn sample_proposal(mission_id: &str, phase_ids: &[&str]) -> Proposal {
        Proposal {
            mission: ProposedMission {
                id: mission_id.to_string(),
                description: "test mission".to_string(),
                status: "active".to_string(),
                phase_ids: phase_ids.iter().map(|s| s.to_string()).collect(),
                created_ts: 0,
                source_input: None,
                ticket: None,
            },
            phases: phase_ids
                .iter()
                .map(|sid| ProposedPhase {
                    id: sid.to_string(),
                    mission_id: mission_id.to_string(),
                    description: format!("phase {sid}"),
                    display_name: None,
                    status: "planned".to_string(),
                    depends_on: Vec::new(),
                    created_ts: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn extract_json_block_finds_fenced_block() {
        let resp = "preamble text\n```json\n{\"mission\": {}, \"phases\": []}\n```\nepilogue";
        let block = extract_json_block(resp).expect("should extract");
        assert!(block.contains("\"mission\""));
        assert!(block.contains("\"phases\""));
    }

    #[test]
    fn extract_json_block_errors_on_missing_fence() {
        let resp = "no fenced block here, just prose";
        let err = extract_json_block(resp).expect_err("should error");
        assert!(err.to_string().contains("no fenced"));
    }

    #[test]
    fn extract_json_block_prefers_tagged_over_bare_fence() {
        // (#896) A bare fence wrapping some OTHER block precedes the real
        // ```json block. The old scanner opened on the bare fence and
        // captured the wrong region; we must prefer the tagged opener.
        let resp = "```\nnot json — some shell snippet\n```\n```json\n{\"mission\": {}, \"phases\": []}\n```";
        let block = extract_json_block(resp).expect("should extract the tagged block");
        assert!(block.contains("\"mission\""));
        assert!(!block.contains("shell snippet"));
    }

    #[test]
    fn extract_json_block_distinct_error_on_unterminated() {
        // (#896) A ```json opener with no closing fence (token-truncated
        // local model). Must be a DISTINCT "unterminated" error, not the
        // misleading "no fenced block" — there was a block, it didn't finish.
        let resp = "preamble\n```json\n{\"mission\": {}, \"phases\": [";
        let err = extract_json_block(resp).expect_err("should error on truncation");
        let msg = err.to_string();
        assert!(msg.contains("unterminated"), "got: {msg}");
        assert!(!msg.contains("no fenced"), "should not be the missing-fence error: {msg}");
    }

    #[test]
    fn extract_json_block_falls_back_to_bare_fence() {
        // No language tag anywhere → fall back to the first bare fence.
        let resp = "```\n{\"mission\": {}, \"phases\": []}\n```";
        let block = extract_json_block(resp).expect("should extract the bare-fenced block");
        assert!(block.contains("\"mission\""));
    }

    #[test]
    fn extract_json_block_opener_on_last_line_is_unterminated_not_panic() {
        // (#896) Opener is the final line with nothing after it: the
        // `[open_idx + 1..]` slice is empty (must not panic), and it's
        // treated as truncated — a distinct unterminated error.
        let resp = "preamble\n```json";
        let err = extract_json_block(resp).expect_err("opener-only is unterminated");
        assert!(err.to_string().contains("unterminated"));
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
    "phase_ids": ["test-s1"],
    "created_ts": 0
  },
  "phases": [
    {
      "id": "test-s1",
      "mission_id": "test-mission",
      "description": "first phase",
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
        assert_eq!(p.phases.len(), 1);
        assert_eq!(p.phases[0].id, "test-s1");
    }

    #[test]
    fn build_compiler_message_includes_hint_when_present() {
        let msg = build_compiler_message("intent", Some("merge phases 3 and 4"));
        assert!(msg.contains("intent"));
        assert!(msg.contains("merge phases 3 and 4"));
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
    fn persist_fails_on_existing_mission_config() {
        // Regression: persist must refuse to overwrite an existing mission
        // CONFIG so a duplicate proposal can't clobber an operator's
        // hand-edited config. Wrapped in CrewDirGuard so the test runs in
        // an isolated TempDir — never touches the operator's real
        // ~/.darkmux/mission-configs/.
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());

        let existing_path = crate::crew::loader::mission_configs_dir().join("test-existing-mission.json");
        std::fs::create_dir_all(existing_path.parent().unwrap()).unwrap();
        std::fs::write(&existing_path, r#"{"id":"test-existing-mission","name":"Existing"}"#)
            .expect("writing existing mission config");

        let proposal = sample_proposal("test-existing-mission", &[]);
        let err = persist(&proposal, "test input", None).expect_err("persist should fail for existing config");
        assert!(err.to_string().contains("already exists"));
    }

    #[serial_test::serial]
    #[test]
    fn persist_writes_a_mission_config_with_the_expected_shape() {
        // (#1284 Packet 4a) persist's output is now a MissionConfig
        // document, not a Mission+Phase instance — every phase is
        // task-less (the mission-compiler never proposed a Task/Step
        // graph), phase order follows mission.phase_ids, and the id/
        // schema_version land as documented.
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());

        let proposal = sample_proposal("test-config-shape", &["s1", "s2"]);
        persist(&proposal, "test input", Some("SYS-1")).expect("persist should succeed");

        let path = crate::crew::loader::mission_configs_dir().join("test-config-shape.json");
        assert!(path.is_file(), "expected a mission config at {}", path.display());
        let cfg: darkmux_crew::mission_config::MissionConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.id, "test-config-shape");
        assert_eq!(cfg.schema_version.as_deref(), Some(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA));
        assert!(cfg.inputs.is_empty(), "a freeform-only proposal declares no runtime inputs");
        assert_eq!(cfg.phases.len(), 2);
        assert_eq!(cfg.phases[0].id, "s1");
        assert_eq!(cfg.phases[1].id, "s2");
        assert!(cfg.phases.iter().all(|p| p.tasks.is_empty()), "compiler proposals are always freeform");
        assert_eq!(cfg.extras.get("source_input"), Some(&serde_json::json!("test input")));
        assert_eq!(cfg.extras.get("ticket"), Some(&serde_json::json!("SYS-1")));
        // (contract 7) The document must itself validate cleanly.
        assert!(cfg.is_valid(&[]));
    }

    /// (#1398) `ProposedPhase::display_name` threads through
    /// `build_mission_config` onto the persisted `PhaseConfig::display_name`
    /// — absent when the compiler didn't set one (lenient-on-read: an
    /// older-schema response still persists cleanly).
    #[serial_test::serial]
    #[test]
    fn persist_threads_display_name_onto_the_mission_config() {
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        let mut proposal = sample_proposal("test-display-name", &["s1", "s2"]);
        proposal.phases[0].display_name = Some("Investigate".to_string());
        // s2 deliberately left without a display_name.
        persist(&proposal, "test input", None).expect("persist should succeed");

        let path = crate::crew::loader::mission_configs_dir().join("test-display-name.json");
        let cfg: darkmux_crew::mission_config::MissionConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.phases[0].display_name.as_deref(), Some("Investigate"));
        assert_eq!(cfg.phases[1].display_name, None);
    }

    #[test]
    fn validate_proposal_accepts_well_formed() {
        let p = sample_proposal("m1", &["s1", "s2"]);
        validate_proposal_invariants(&p).expect("well-formed proposal should pass");
    }

    #[test]
    fn validate_proposal_rejects_dangling_phase_id_in_mission() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Add a phase_id to the mission that doesn't exist in phases[].
        p.mission.phase_ids.push("ghost".to_string());
        let err = validate_proposal_invariants(&p).expect_err("should reject dangling phase_id");
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn validate_proposal_rejects_dangling_depends_on() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Reference a non-existent phase id in depends_on.
        p.phases[0].depends_on = vec!["missing".to_string()];
        let err = validate_proposal_invariants(&p).expect_err("should reject dangling depends_on");
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn validate_proposal_rejects_path_traversal_mission_id() {
        // (#867 security) A model-supplied mission id with `..`/`/` must be
        // rejected BEFORE it reaches lifecycle::mission_path's raw join.
        let p = sample_proposal("../../../tmp/evil", &["s1"]);
        let err = validate_proposal_invariants(&p)
            .expect_err("a traversal mission id must be rejected");
        assert!(
            err.to_string().contains("invalid char"),
            "expected an identifier-charset rejection, got: {err}"
        );
    }

    #[test]
    fn validate_proposal_rejects_path_traversal_phase_id() {
        // (#867 security) Same guard for phase ids → phase_path raw join.
        let p = sample_proposal("m1", &["../../etc/passwd"]);
        let err = validate_proposal_invariants(&p)
            .expect_err("a traversal phase id must be rejected");
        assert!(
            err.to_string().contains("invalid char"),
            "expected an identifier-charset rejection, got: {err}"
        );
    }

    #[serial_test::serial]
    #[test]
    fn persist_rejects_path_traversal_id_before_any_write() {
        // (#867 security boundary) persist self-defends even when reached
        // without validate_proposal_invariants: a traversal id is refused
        // before any path construction / write.
        let _guard = CrewDirGuard::new(TempDir::new().unwrap());
        let p = sample_proposal("../../../tmp/evil", &["s1"]);
        let err = persist(&p, "test input", None)
            .expect_err("persist must refuse a traversal mission id");
        assert!(
            err.to_string().contains("invalid char"),
            "expected an identifier-charset rejection at the persist boundary, got: {err}"
        );
    }

    #[test]
    fn validate_proposal_rejects_mismatched_mission_id() {
        let mut p = sample_proposal("m1", &["s1"]);
        // Make a phase claim a different mission_id than the mission's id.
        p.phases[0].mission_id = "other-mission".to_string();
        let err =
            validate_proposal_invariants(&p).expect_err("should reject mismatched mission_id");
        assert!(err.to_string().contains("other-mission"));
    }
}
