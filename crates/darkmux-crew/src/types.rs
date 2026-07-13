//! Schema types for the darkmux crew architecture.
//!
//! These are scaffolding types — consumed by downstream phases (crew orchestration,
//! mission management) but not yet wired into any CLI command.
//!
//! User files at `~/.darkmux/<entity-type>/` take precedence over bundled templates.
//! Bundled templates are starting points, never the source-of-truth.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// `Capability` + `CapabilityProfile` moved to the foundation crate
// (darkmux-types) so the profile/model layer can carry them too — a role
// requests capabilities, a model offers them, both speak one vocabulary
// (E14 / #450). Re-exported here so existing `crate::types::Capability`
// references resolve unchanged.
pub use darkmux_types::{Capability, CapabilityProfile};

/// A named skill with keyword-based relevance scoring + an intrinsic
/// capability profile. A role declares which skills it has; the
/// allocator matches mission keywords against role skills to route
/// work (the keyword routing); the dispatch's model selection composes
/// per-skill capability profiles via union-via-max into a per-role
/// capability vector (the capability routing). Renamed from
/// `Capability` to free the word for the industry-conventional
/// model-capability meaning (E14).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<KeywordWeight>,
    /// Intrinsic capability profile — which AI capabilities this skill
    /// inherently demands at what relative weights. Phase-1 schema
    /// addition (E14). Absent in older manifests; defaults to empty
    /// map (sparse-as-zero — the skill demands nothing). Phase-1
    /// dispatch routing falls back to `default` regardless, so absence
    /// is harmless until phase 2 activates scoring.
    #[serde(default)]
    pub capabilities: CapabilityProfile,
}

/// A keyword paired with a relevance weight (0.0–1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeywordWeight {
    pub keyword: String,
    pub weight: f32,
}

/// Role positioning within a crew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Lead,
    Support,
}

/// Escalation contract — what a role does when it can't solve an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationContract {
    BailWithExplanation,
    RetryWithHint,
    HandOffTo(String), // role id to hand off to
}

/// A single role definition: skills, tool palette, escalation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub skills: Vec<String>, // skill ids
    pub tool_palette: ToolPalette,
    pub escalation_contract: EscalationContract,
    /// (#1038) Optional JSON Schema for the role's final output. When set, the
    /// dispatcher passes it to the runtime (`--response-schema`), which sends it
    /// to LMStudio as `response_format: json_schema` so the model is
    /// grammar-constrained to emit exactly this shape — the structural cure for
    /// local-model JSON malformation (vs post-hoc repair). Absent ⇒ free-form
    /// output (today's default). Schema-compatible: older manifests omit it.
    ///
    /// Intended for **tool-less terminal-output roles** (empty `tool_palette`):
    /// the model emits one final object and stops. Pairing it with a non-empty
    /// tool palette is unsupported — a grammar lock to a fixed shape structurally
    /// precludes emitting `tool_calls`, so mid-loop tool turns can't happen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Path to the sibling `<role-id>.md` prompt file if present. The loader
    /// resolves this from the same directory as the role's JSON manifest; it
    /// does NOT read the prompt content — just stores the resolvable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
    /// (#377) Per-role escalation bound — overrides
    /// `profile.runtime.compaction.reserve.bail_after_compactions`
    /// when set. Phase-shaped roles (coder, reviewer) typically pin
    /// a low value (2-3) for tight bound; long-arc roles (researcher,
    /// mission-compiler) may pin higher (5-7) for more local-tier
    /// runway. Absent ⇒ profile default ⇒ runtime default (unbounded).
    /// Schema-compatible: older role manifests without the field
    /// continue to work unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bail_after_compactions: Option<u32>,
    /// (#377) What to do when an escalation bound fires. `"auto"`
    /// (default) emits the `EscalationTriggered` terminal and exits
    /// the dispatch — frontier-tier picks up via the
    /// `darkmux-escalation-handler` skill. `"pause"` is the operator
    /// opt-in for roles where work should NOT auto-escalate (e.g. a
    /// human-supervised long-arc role). The runtime treats both the
    /// same today; the field is plumbed for the host/skill layer to
    /// branch on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_posture: Option<String>,
    /// (#425) Role family — a **scope** distinction (#590): `"specialist"`
    /// roles work the mission/phases (the deliverable); `"utility"` roles
    /// support the runtime outside mission scope (mission-compiler, scribe,
    /// and — once #590 lands it — the compactor). The split also drives
    /// dispatch shape: specialists run the multi-turn agent loop and get the
    /// autonomous-dispatch preamble prepended; utility roles are bounded-I/O
    /// transformers with no agent loop, no asking-mode failure shape, and no
    /// preamble. Absent ⇒ treat as `"specialist"` (preventive safety: better
    /// an unneeded preamble than a missing one). Built-in roles all declare
    /// it explicitly. `validate_role_family` in `loader.rs` enforces the
    /// two-value axis — the legacy `"admin"` value and any unknown value are
    /// rejected with an operator-actionable message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_family: Option<String>,
    /// (#457 Step 2) Per-role overrides for the runtime's feedback-
    /// injection templates. Keys are signal-kind names matching
    /// `FeedbackInjector::queue_*` methods today:
    ///
    /// - `cycle_suspected` — fired when the cycle detector observes
    ///   the same tool+args called 3+ times in 10 turns
    /// - `tool_failure_cascade` — fired when a tool fails 3+ times
    ///   in a row
    ///
    /// Values are template strings with placeholder substitution.
    /// Supported placeholders depend on the signal kind:
    ///
    /// - `cycle_suspected`: `{tool_name}`, `{count}`, `{window_size}`
    /// - `tool_failure_cascade`: `{tool_name}`, `{count}`
    ///
    /// Missing key ⇒ runtime falls back to the hardcoded default
    /// (the [darkmux-runtime]-prefixed directive shipped in PR #455).
    /// Absent field entirely ⇒ all defaults apply.
    ///
    /// **Per-role nuance the operator named** (Beat 53 roadmap):
    /// a coder hitting a cycle gets the imperative *"regroup and
    /// change the strategy"*; a code-reviewer (read-only palette)
    /// might benefit from *"narrow the scope of your review;
    /// commit to a verdict"* instead — same signal, different
    /// actionable next move per the role's tool surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_templates: Option<std::collections::BTreeMap<String, String>>,
}

impl Role {
    /// (#425) True when this role drives a multi-turn agent loop and
    /// therefore needs the autonomous-dispatch preamble prepended to
    /// its system prompt. Default (field absent) = specialist =
    /// needs preamble. Explicit `"utility"` opts out. The legacy
    /// `"admin"` value was renamed to `"utility"` to match the
    /// codebase's broader utility/specialist nomenclature; pre-1.0,
    /// no compat alias — `validate_role_family` in `loader.rs` rejects
    /// the legacy value with a clear migration message.
    pub fn is_specialist(&self) -> bool {
        !matches!(self.role_family.as_deref(), Some("utility"))
    }

    /// (#450, E14) Derive this role's effective capability profile from
    /// the union-via-max composition of its declared skills' profiles.
    ///
    /// Composition rule: for each capability dimension, take the
    /// maximum weight from any of the role's skills. A role with
    /// `skills: ["coding", "code-reviewing"]` ends up needing whichever
    /// skill demands MORE on each dimension — the model has to satisfy
    /// the strongest demand from ANY of the role's skills.
    ///
    /// Sparse-as-zero: a skill that doesn't declare a capability is
    /// treated as weight=0 for that dimension. A capability that no
    /// skill declares stays absent in the output (also weight=0 for
    /// the role).
    ///
    /// `skill_lookup` returns the `Skill` for a given id, or `None` if
    /// the id is unknown. Unknown skills are silently skipped (with
    /// the assumption that the index-rebuild phase already validated
    /// skill ids exist; future hardening could log a warning).
    pub fn capabilities<'a, F>(&self, skill_lookup: F) -> CapabilityProfile
    where
        F: Fn(&str) -> Option<&'a Skill>,
    {
        let mut profile: CapabilityProfile = CapabilityProfile::new();
        for skill_id in &self.skills {
            let Some(skill) = skill_lookup(skill_id) else {
                continue;
            };
            for (cap, weight) in &skill.capabilities {
                // Defensive guard: skip non-finite (NaN/Infinity) and
                // negative weights. The docstring promises "non-
                // negative weights"; this enforces it at composition
                // time so phase-2 scoring never divides into garbage
                // values seeded from an operator-edited manifest with
                // a typo (e.g., `"reasoning": NaN` or `"code": -0.5`).
                // `NaN > x` is always false in IEEE-754, which would
                // otherwise silently no-op and seed 0.0; explicit
                // guard makes the intent visible.
                if !weight.is_finite() || *weight < 0.0 {
                    continue;
                }
                let entry = profile.entry(*cap).or_insert(0.0);
                if *weight > *entry {
                    *entry = *weight;
                }
            }
        }
        profile
    }
}

/// Which tool operations a role is allowed or denied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPalette {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// A single crew member: which role they play and their position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewMember {
    pub role_id: String,
    pub position: Position,
}

/// A crew — a named collection of role assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crew {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub members: Vec<CrewMember>,
}

/// Status of a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MissionStatus {
    #[default]
    Active,
    Closed,
    Paused,
}

/// A mission — a named objective tying phases together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: MissionStatus,
    /// Sprint→Phase rename read-compat: pre-rename mission JSON on disk
    /// carries this list under the old key `sprint_ids`. `alias` lets
    /// serde accept either wire name on read; every subsequent
    /// `save_json` write emits the canonical `phase_ids` key, so a
    /// mission self-migrates its field name the next time it's touched.
    #[serde(default, alias = "sprint_ids")]
    pub phase_ids: Vec<String>,
    pub created_ts: u64,
    /// When the mission first transitioned to `Active`. None until
    /// `darkmux mission start` runs. Used by the wall-clock UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    /// When the mission transitioned to `Closed`. Closed is terminal —
    /// once set, lifecycle verbs can't move the mission elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_ts: Option<u64>,
    /// When the mission most recently transitioned to `Paused`. Resume
    /// flips status back to `Active` but does NOT clear this field —
    /// the operator may want to see when the most recent pause occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_ts: Option<u64>,
    /// (#815) The operator's VERBATIM `mission propose` input — the
    /// unabridged prose the mission-compiler summarized into the
    /// description + phase descriptions. `mission run` dispatches this
    /// alongside each phase's compiled description so exact strings and
    /// constraints survive the compiler's compression (the lost-in-
    /// translation seam from the 2026-06-12 dogfood). None on
    /// hand-authored or pre-#815 missions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_input: Option<String>,
    /// (#816) Work-item / ticket id this mission realizes (e.g.
    /// `SYS-2598`), set via `mission propose --ticket`. Referenced as
    /// `{ticket}` by the target repo's `.darkmux/conventions.json`
    /// templates for branch names, commit subjects, and PR titles. None
    /// on ticketless missions — templates referencing `{ticket}` then
    /// fall back to darkmux defaults with a soft warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

/// Status of a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PhaseStatus {
    #[default]
    Planned,
    Running,
    Complete,
    Abandoned,
}

/// A phase — a time-boxed work unit within a mission. **Strictly linear
/// (#1341, verified against real NASA mission-phase documentation: Launch
/// → Cruise → Encounter → Extended Operations, each explicitly bounded by
/// its neighbors, no branching)** — ordered PURELY by position in
/// `Mission::phase_ids`, with zero scheduling semantics of its own (no
/// `depends_on` field). All real dependency/concurrency/data-flow lives
/// one level down, at `Task::depends_on` — a Task in one Phase may depend
/// on a Task in an EARLIER Phase (this is how cross-phase data actually
/// flows, e.g. `adjudicate`'s `judge` Task depending on `investigate`'s
/// `dedup` Task); Phase itself carries no dependency semantics to check.
/// "The phase before this one" (needed by `dispatch::
/// augment_message_with_phase_context`'s one-hop context injection and
/// `mission_run::select_phase`'s "next runnable phase" scan) is simply
/// `Mission::phase_ids[i - 1]` for a phase at index `i`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub mission_id: String,
    pub description: String,
    #[serde(default)]
    pub status: PhaseStatus,
    pub created_ts: u64,
    /// When the phase first transitioned to `Running` (or last transitioned
    /// to `Running` after being `Abandoned` and restarted). None until
    /// `darkmux phase start` runs. Wall-clock UI shows live elapsed when
    /// `status == Running` (now - started_ts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    /// When the phase transitioned to `Complete`. Complete is terminal —
    /// once set, lifecycle verbs can't move the phase elsewhere. Wall-clock
    /// duration = completed_ts - started_ts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<u64>,
    /// When the phase transitioned to `Abandoned`. Cleared when the
    /// operator changes their mind and runs `phase start` again — the
    /// state machine treats `Abandoned → Running` as a legal restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_ts: Option<u64>,
    /// (#1230 Packet 2) IDs of the DAG-native `Task`s that make up this
    /// phase's actual execution graph. Additive-only field — everything
    /// else about `Phase`'s own lifecycle (`status`, `*_ts`) is unchanged;
    /// a Phase stays the real operator-gated unit it always was.
    /// `#[serde(default)]` so every pre-#1230 phase JSON on disk continues
    /// to parse with an empty `task_ids` (no Task/Step graph underneath it
    /// yet).
    #[serde(default)]
    pub task_ids: Vec<String>,
}

/// (#1230 Packet 2) Status of a DAG node (`Step`, and — via the
/// `DependencyNode` adapter in `scheduler.rs` — `Phase` too). Distinct
/// from `PhaseStatus`: a Phase is an operator-gated planning unit
/// (`start`/`complete`/`abandon` are explicit operator verbs); a `Step` is
/// scheduler-driven (`Planned` → `Running` → `Complete`/`Error` happens
/// automatically as `run_step_graph` walks the graph). `Error` has no
/// `PhaseStatus` analog — a Phase either completes or is abandoned by
/// the operator; a Step can fail its own execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeStatus {
    #[default]
    Planned,
    Running,
    Complete,
    Abandoned,
    Error,
}

/// (#1230 Packet 2, revised #1341) The smallest executable unit in the flow
/// graph — one dispatch, one native function, or one shell command. `kind`
/// names a registered `step_kinds::StepKind` id (e.g. `"dispatch.internal"`,
/// `"procedural.shell"`); `config` is kind-specific (flat-extras-bag
/// pattern, matching `ProfileModel`'s `#[serde(flatten)] extras`).
///
/// **No `depends_on` field (#1341).** Step ordering is implied PURELY by
/// position in the owning `Task.step_ids: Vec<String>` — the step at index
/// `i` (if any) depends on the step at index `i-1`; index `0` has no
/// intra-task predecessor. This makes a branching/multi-predecessor Step
/// structurally UNREPRESENTABLE rather than something a runtime check has
/// to catch — "can't be written wrong" beats "gets rejected if written
/// wrong." All cross-Step dependency/concurrency/data-flow lives ONE level
/// up, on `Task.depends_on` — see that field's doc and
/// `scheduler::run_step_graph`'s Task-aware readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub task_id: String,
    /// step-kind registry id, e.g. `"dispatch.internal"` |
    /// `"dispatch.single_shot"` | `"procedural.shell"` | `"procedural.noop"`.
    pub kind: String,
    #[serde(default)]
    pub status: NodeStatus,
    /// Kind-specific config. `serde_json::Value::Null` (the `Default`) for
    /// step kinds that need none (e.g. `procedural.noop` with no `output`
    /// override).
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<u64>,
    /// The step kind's own output text on success (or an error summary on
    /// `NodeStatus::Error`). Generalizes the one-hop
    /// `<phase-id>-output.txt` context file `dispatch.rs`'s `phase_id`
    /// plumbing already writes — a downstream Step (same-task successor, or
    /// the first step of a Task naming this one's Task in its own
    /// `Task.depends_on`) reads a completed dependency's `output` as its
    /// own input (see `scheduler::gather_inputs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// (#1230 Packet 2, revised #1341) A real (not synthesized) grouping of
/// Steps within a Phase — operators reference a Task directly (e.g. "the
/// coder task"), so it's a first-class schema level, not just a derived
/// view over Step edges. A Task is the ASSIGNABLE unit — like a Jira
/// ticket assigned to one crew member: `role_id`/`profile_name`/
/// `workdir`/`image` are properties of the whole job, fixed for its
/// duration; `depends_on` is the ONLY place dependency/concurrency/
/// data-flow is declared above the Step level (Steps within one Task are
/// ordered purely by `step_ids` position — see `Step`'s doc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Sprint→Phase rename read-compat: accepts the pre-rename wire key
    /// `sprint_id` as well as the canonical `phase_id` (see `Mission::phase_ids`'s
    /// alias for the same rationale). Task/Step storage postdates the rename
    /// by only a few days (#1230 Packet 2), so this is defensive more than
    /// load-bearing, but costs nothing to keep consistent.
    #[serde(alias = "sprint_id")]
    pub phase_id: String,
    pub description: String,
    /// Ordered — step at index `i` depends on the step at index `i-1` (see
    /// `Step`'s doc). A Task with 0 or 1 steps has nothing to order.
    #[serde(default)]
    pub step_ids: Vec<String>,
    /// (#1341) Task ids this Task depends on — governs BOTH which Tasks
    /// can run concurrently (unrelated Tasks, no edge between them
    /// directly or transitively, run concurrently; dependent ones run in
    /// order) AND the output→input wiring for merge points: when Task B
    /// depends on Task A, Task B's FIRST step receives Task A's LAST
    /// step's `output` in its `input` map, keyed by Task A's id (see
    /// `scheduler::gather_inputs`). **Settled design**: an upstream Task's
    /// output reaches ONLY the downstream Task's FIRST step — a later step
    /// in a multi-step Task sees only the immediately-previous SAME-TASK
    /// step's output; it's that first step's job to carry forward anything
    /// from an upstream Task a later same-task step still needs (into its
    /// own `output`, which the next step then reads as ITS input — the
    /// same step-to-step handoff every same-task pair already uses, no
    /// separate mechanism). Every Task built by this codebase today is
    /// single-step, so the distinction has no observable effect yet — it
    /// applies only once a real multi-step Task exists.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// (#1230/#1341) A Task is the ASSIGNABLE unit — like a Jira ticket
    /// assigned to one crew member, the assignee/environment/profile are
    /// properties of the whole job, fixed for its duration, not
    /// re-declared at every Step. `role_id` names the crew role dispatched
    /// for this Task (e.g. `"coder"`); `None` on purely-procedural Tasks
    /// (dedup, bundle, synthesis) that carry no crew assignment at all.
    /// Step-level dispatch kinds (`DispatchInternalStepKind`) source
    /// `role_id`/`profile_name`/`workdir`/`image` from the OWNING Task
    /// first, falling back to `Step.config` only when the Task doesn't set
    /// a field — see `scheduler::run_step_graph`'s Task lookup and
    /// `StepKind::run`'s `task` parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// The swap-profile this Task's dispatch(es) resolve models against.
    /// `None` ⇒ the default registry (no `--profiles-file` override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    /// The working directory a dispatch runs in (e.g. a mission phase's
    /// git worktree). `None` ⇒ no fixed workdir (a role-default dispatch,
    /// or a Task with no filesystem dependency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    /// The container image a dispatch runs in. `None` ⇒ the runtime's
    /// default image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_with(id: &str, caps: &[(Capability, f32)]) -> Skill {
        Skill {
            id: id.into(),
            description: format!("test skill {id}"),
            keywords: vec![],
            capabilities: caps.iter().copied().collect(),
        }
    }

    fn make_role(id: &str, skill_ids: &[&str]) -> Role {
        Role {
            output_schema: None,
            id: id.into(),
            description: format!("test role {id}"),
            skills: skill_ids.iter().map(|s| s.to_string()).collect(),
            tool_palette: ToolPalette::default(),
            escalation_contract: EscalationContract::BailWithExplanation,
            prompt_path: None,
            bail_after_compactions: None,
            escalation_posture: None,
            role_family: None,
            feedback_templates: None,
        }
    }

    /// (#450) Capability serde — snake_case round-trip. Verifies the
    /// AI-industry-conventional naming convention is the wire format.
    #[test]
    fn capability_snake_case_round_trip() {
        let cases = [
            (Capability::Code, "\"code\""),
            (Capability::Reasoning, "\"reasoning\""),
            (Capability::InstructionFollowing, "\"instruction_following\""),
            (Capability::AgenticToolUse, "\"agentic_tool_use\""),
        ];
        for (variant, expected_json) in cases {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected_json, "serialize {:?}", variant);
            let back: Capability = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant, "round-trip {:?}", variant);
        }
    }

    /// (#450) Parse-time validation: unknown capability names fail to
    /// deserialize. This is what guards against silent zero-weight
    /// bugs from typos in operator-edited skill manifests.
    #[test]
    fn capability_unknown_variant_fails_to_deserialize() {
        let bad_json = "\"coding\""; // not a Capability (it's a Skill id)
        let result: Result<Capability, _> = serde_json::from_str(bad_json);
        assert!(
            result.is_err(),
            "unknown capability name `coding` must fail to deserialize, got {:?}",
            result
        );

        let typo = "\"reason\""; // typo of `reasoning`
        let result: Result<Capability, _> = serde_json::from_str(typo);
        assert!(
            result.is_err(),
            "typo capability name `reason` must fail to deserialize"
        );
    }

    /// (#450) Skill capabilities field — JSON round-trip with the new
    /// schema. Verifies sparse-as-zero by absence (the JSON doesn't
    /// have to list every capability).
    #[test]
    fn skill_capabilities_field_round_trips() {
        let json = r#"{
            "id": "coding",
            "description": "test",
            "keywords": [],
            "capabilities": { "code": 0.9, "reasoning": 0.5 }
        }"#;
        let s: Skill = serde_json::from_str(json).unwrap();
        assert_eq!(s.id, "coding");
        assert_eq!(s.capabilities.len(), 2);
        assert_eq!(s.capabilities.get(&Capability::Code).copied(), Some(0.9));
        assert_eq!(s.capabilities.get(&Capability::Reasoning).copied(), Some(0.5));
        // Sparse-as-zero: absent dimensions are absent in the map.
        assert!(!s.capabilities.contains_key(&Capability::InstructionFollowing));
        assert!(!s.capabilities.contains_key(&Capability::AgenticToolUse));
    }

    /// (#450) Older skill manifests (refactor-0 era) without the
    /// `capabilities` field continue to parse — the field is
    /// `#[serde(default)]`, so absence yields empty map (no demands).
    /// Backward-compat is required for pre-refactor-1 user-edited
    /// skill manifests under `~/.darkmux/skills/`.
    #[test]
    fn skill_without_capabilities_field_parses_with_empty_profile() {
        let legacy_json = r#"{
            "id": "old-skill",
            "description": "from refactor-0 era",
            "keywords": []
        }"#;
        let s: Skill = serde_json::from_str(legacy_json).unwrap();
        assert!(s.capabilities.is_empty());
    }

    /// (#450, E14) Role capability derivation — union-via-max across
    /// the role's declared skills. The composed profile reflects the
    /// strongest demand from ANY of the role's skills on each
    /// dimension. This is the model-selection input.
    #[test]
    fn role_capabilities_union_via_max_composition() {
        let coding = skill_with(
            "coding",
            &[
                (Capability::Code, 0.9),
                (Capability::Reasoning, 0.5),
                (Capability::AgenticToolUse, 0.7),
            ],
        );
        let reviewing = skill_with(
            "code-reviewing",
            &[
                (Capability::Code, 0.7),
                (Capability::Reasoning, 0.85),
                (Capability::InstructionFollowing, 0.7),
            ],
        );
        let skills = [coding.clone(), reviewing.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("multi-skill", &["coding", "code-reviewing"]);
        let profile = role.capabilities(lookup);

        // max(0.9, 0.7) = 0.9 — coding demands more code than reviewing
        assert_eq!(profile.get(&Capability::Code).copied(), Some(0.9));
        // max(0.5, 0.85) = 0.85 — reviewing demands more reasoning
        assert_eq!(profile.get(&Capability::Reasoning).copied(), Some(0.85));
        // Only reviewing declares it; sparse-as-zero treats coding as 0
        assert_eq!(profile.get(&Capability::InstructionFollowing).copied(), Some(0.7));
        // Only coding declares it
        assert_eq!(profile.get(&Capability::AgenticToolUse).copied(), Some(0.7));
    }

    /// (#450, E14) Sparse-as-zero semantics — a capability dimension
    /// that NO skill declares is absent from the role's profile (not
    /// present-with-zero). Distinguishes "unspecified" from "demanded
    /// at zero" — same effective behavior under union-via-max but
    /// cleaner in iteration (no zero-padding entries).
    #[test]
    fn role_capabilities_absent_dimensions_stay_absent() {
        let analyzing = skill_with(
            "analyzing",
            &[(Capability::Reasoning, 0.9), (Capability::InstructionFollowing, 0.6)],
        );
        let skills = [analyzing.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("analyst", &["analyzing"]);
        let profile = role.capabilities(lookup);

        assert_eq!(profile.len(), 2);
        assert!(profile.contains_key(&Capability::Reasoning));
        assert!(profile.contains_key(&Capability::InstructionFollowing));
        // Code + AgenticToolUse aren't declared by analyzing → absent.
        assert!(!profile.contains_key(&Capability::Code));
        assert!(!profile.contains_key(&Capability::AgenticToolUse));
    }

    /// (#450, E14) Empty-skills role → empty profile. Edge case worth
    /// pinning since some roles (or in-flight role manifests) may
    /// declare zero skills.
    #[test]
    fn role_capabilities_empty_skills_yields_empty_profile() {
        let role = make_role("bare", &[]);
        let profile = role.capabilities(|_: &str| None);
        assert!(profile.is_empty());
    }

    /// (#450, E14) Unknown skill ids are silently skipped — the
    /// derivation doesn't fail when a role references a skill that
    /// doesn't exist in the lookup. Index-rebuild phase is expected
    /// to validate skill-id references; this layer is defensive only.
    #[test]
    fn role_capabilities_skips_unknown_skill_ids() {
        let coding = skill_with("coding", &[(Capability::Code, 0.9)]);
        let skills = [coding.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        // Role declares two skills; one doesn't exist in the lookup.
        let role = make_role("partial", &["coding", "ghost-skill"]);
        let profile = role.capabilities(lookup);

        // Only the resolvable skill contributes.
        assert_eq!(profile.len(), 1);
        assert_eq!(profile.get(&Capability::Code).copied(), Some(0.9));
    }

    /// (#450, E14) Same-skill-twice idempotency. A role with
    /// `skills: ["coding", "coding"]` should produce the same
    /// effective profile as a role with `skills: ["coding"]`.
    /// Union-via-max is naturally idempotent (max(x, x) = x), but
    /// pinning this prevents a future refactor from accidentally
    /// breaking the invariant via e.g. sum-composition.
    #[test]
    fn role_capabilities_is_idempotent_under_duplicate_skill_ids() {
        let coding = skill_with(
            "coding",
            &[(Capability::Code, 0.9), (Capability::Reasoning, 0.5)],
        );
        let skills = [coding.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let single = make_role("single", &["coding"]).capabilities(lookup);
        let duplicate = make_role("duplicate", &["coding", "coding"]).capabilities(lookup);

        assert_eq!(single, duplicate, "same-skill-twice must equal single declaration");
        assert_eq!(duplicate.get(&Capability::Code).copied(), Some(0.9));
    }

    /// (#450, E14) Defensive guard against NaN / Infinity / negative
    /// weights in skill manifests. The docstring promises non-
    /// negative finite weights; the impl enforces it at composition.
    /// `NaN > x` is always false in IEEE-754, which would otherwise
    /// silently no-op and seed 0.0 — phase-2 scoring would then
    /// divide into garbage. Explicit skip makes the intent visible.
    #[test]
    fn role_capabilities_skips_nan_infinity_and_negative_weights() {
        let bad_skill = Skill {
            id: "bad".into(),
            description: "test".into(),
            keywords: vec![],
            capabilities: [
                (Capability::Code, f32::NAN),
                (Capability::Reasoning, f32::INFINITY),
                (Capability::InstructionFollowing, -0.5),
                (Capability::AgenticToolUse, 0.7), // the only valid one
            ]
            .into_iter()
            .collect(),
        };
        let skills = [bad_skill.clone()];
        let lookup = |id: &str| skills.iter().find(|s| s.id == id);

        let role = make_role("defensive", &["bad"]);
        let profile = role.capabilities(lookup);

        // Only the finite non-negative weight survives.
        assert_eq!(profile.len(), 1);
        assert_eq!(profile.get(&Capability::AgenticToolUse).copied(), Some(0.7));
        assert!(!profile.contains_key(&Capability::Code), "NaN must be skipped");
        assert!(!profile.contains_key(&Capability::Reasoning), "Infinity must be skipped");
        assert!(!profile.contains_key(&Capability::InstructionFollowing), "negative must be skipped");
    }

    /// (#450 follow-up to review note) `BTreeMap` iteration is
    /// deterministic — verifies the `CapabilityProfile` ordering
    /// guarantee phase-1b dispatch + `darkmux doctor` display will
    /// rely on for stable, reproducible-by-flow-record output.
    #[test]
    fn capability_profile_iteration_is_deterministic() {
        let mut profile = CapabilityProfile::new();
        // Insert in non-enum-declaration order
        profile.insert(Capability::AgenticToolUse, 0.7);
        profile.insert(Capability::Code, 0.9);
        profile.insert(Capability::Reasoning, 0.8);
        profile.insert(Capability::InstructionFollowing, 0.5);

        // BTreeMap orders by key (Capability's derived Ord — enum
        // declaration order: Code, Reasoning, InstructionFollowing,
        // AgenticToolUse).
        let keys: Vec<Capability> = profile.keys().copied().collect();
        assert_eq!(
            keys,
            vec![
                Capability::Code,
                Capability::Reasoning,
                Capability::InstructionFollowing,
                Capability::AgenticToolUse,
            ],
        );
    }
}
