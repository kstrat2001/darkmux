//! (5.0, #3035) Data-shape version markers: one constant per authored or
//! persisted shape, and the one comparison every reader shares.
//!
//! **The contract.** A shape's marker is a data-shape version, independent of
//! the darkmux release number. It is written on every save and read
//! leniently: an absent marker means "written before the marker existed" and
//! is accepted. After 5.0 a change to a shape is additive, so a file written
//! by a NEWER darkmux can carry keys or variants this binary cannot place.
//! A reader that sees a marker newer than its own constant refuses the file
//! with [`newer_refusal`] ("upgrade darkmux") instead of misreading it or
//! reporting its new keys as typos. A marker the same or older changes
//! nothing: an unknown key is still a typo.
//!
//! **Where each is enforced.** The authored user files go through the
//! unknown-key gate (`crate::user_files`, via `UserFileKind::schema_version`);
//! mission state goes through `darkmux_crew::retired_state::parse_state`;
//! `lessons.db` through its `user_version` pragma; the lab run manifest, the
//! trajectory and `resume_origin` through their own loaders.
//!
//! Bump discipline, the same for every constant here: additive field or
//! variant is a minor bump, a rename / retype / removal is major.

use serde_json::Value;

/// The JSON key every marker lives under.
pub const KEY: &str = "schema_version";

/// Role manifest (`roles/<id>.json`).
pub const ROLE_SCHEMA_VERSION: &str = "1.0";
/// Skill manifest (`skills/<id>.json`).
pub const SKILL_SCHEMA_VERSION: &str = "1.0";
/// Crew manifest (`crews/<id>.json`).
pub const CREW_SCHEMA_VERSION: &str = "1.0";
/// Rule file (`rules/<id>.json`).
pub const RULE_SCHEMA_VERSION: &str = "1.0";
/// Workload manifest (`workloads/<id>.json`).
pub const WORKLOAD_SCHEMA_VERSION: &str = "1.0";
/// Lab fixture manifest (`.fixture.json`).
pub const LAB_FIXTURE_SCHEMA_VERSION: &str = "1.0";
/// `mission.json`.
pub const MISSION_SCHEMA_VERSION: &str = "1.0";
/// A phase JSON.
pub const PHASE_SCHEMA_VERSION: &str = "1.0";
/// A task JSON.
pub const TASK_SCHEMA_VERSION: &str = "1.0";
/// A step JSON.
pub const STEP_SCHEMA_VERSION: &str = "1.0";
/// The `resume_origin` sidecar a resumed run writes.
pub const RESUME_ORIGIN_SCHEMA_VERSION: &str = "1.0";
/// A lab run's `manifest.json`, shared by every provider. It is a separate key
/// ([`RUN_MANIFEST_KEY`]) beside each provider's own integer `schema_version`,
/// which versions that provider's fields and which the enricher raises.
pub const RUN_MANIFEST_SCHEMA_VERSION: &str = "1.0";
/// The key the lab run manifest's shared marker lives under.
pub const RUN_MANIFEST_KEY: &str = "manifest_schema_version";
/// The runtime trajectory JSONL: the `schema_version` of its first-line
/// `trajectory.header` event.
pub const TRAJECTORY_SCHEMA_VERSION: &str = "1.0";

/// The workspace spec (`WorkspaceSpec::schema_version`).
pub const WORKSPACE_SPEC_SCHEMA_VERSION: &str = "1.0";

/// Current schema version for mission config documents. Plain semver
/// applied to the DATA SHAPE — mirrors `RULES_SCHEMA_VERSION`
/// (`darkmux-eureka`), `FLOW_SCHEMA_VERSION` (`darkmux-flow`),
/// `CONFIG_SCHEMA_VERSION` (`darkmux-types::config`). Started at "1.0"
/// (#1284 Packet 1); bumped to "1.1" in Packet 3 — additive: `TaskConfig`
/// gained the optional `expand` field (`ExpansionSpec`, since removed — see
/// below), replacing review.json's original `expands_per_staffed_seat`
/// prose-`notes` bool placeholder with a typed, interpretable primitive.
/// Bumped to "1.2" (#1398) — additive: `PhaseConfig` and `TaskConfig`
/// gained the optional `display_name` field (an operator-facing short
/// label, split from `description` which is deliberately long — see each
/// field's own doc), and `ExpansionSpec` gained the optional
/// `display_name_pattern` twin of its existing `description_pattern`.
/// Bumped to "1.3" (#1475 packet 2) — additive: `ExpansionSpec` gained the
/// optional `role_pattern` (a per-expanded-copy `role_id`, so the review
/// probe stage binds one distinct role per expanded task). Bumped to "1.4"
/// (#1619) — additive: `TaskConfig` gained the optional `reads` field (the
/// run-scoped output ledger made nameable).
///
/// Bumped to **"2.1"** (#1684) — additive: `MissionConfig` gained an
/// optional `panel` block that made `darkmux acp` advertise one config as
/// its own slash command. The block is RETIRED in the 4.0 release
/// (`PANEL_RETIRED`): the panel runs every launchable config through the
/// generic `/mission launch <id>`, and a document still carrying `panel` is
/// refused by the user-file gate and by `MissionConfig::validate`.
///
/// Bumped to **"2.0"** (#1550 cluster item 2) — a MAJOR bump, not minor:
/// `TaskConfig::expand`/`ExpansionSpec`/`interpret::LaunchParams::expansions`
/// were REMOVED (all three retired from this crate — no longer valid
/// doc-links). Per this constant's own bump discipline (below) — and
/// `CLAUDE.md`'s "Versioning" section, the same rule applied to a different
/// data shape — a field REMOVAL is breaking, full stop, regardless of
/// whether the removed field was ever load-bearing in production. (It
/// wasn't, as it happens: both production launchers always fed an empty
/// `expansions` map, so a document declaring `expand` interpreted to ZERO
/// real copies on every real run — retired per #1512's dissolution, once the
/// one real consumer, the review probe stage, moved to static per-role
/// tasks and stopped needing runtime expansion. That history explains WHY
/// the field was safe to remove; it does not downgrade the bump — a
/// removed field is major by the rule itself, not by how much a specific
/// removal happened to matter in practice.)
///
/// Lenient-on-read (contract 7) still holds at the wire level: a document
/// that still declares `expand` parses cleanly — the key overflows into
/// `extras`, inert — but `MissionConfig::validate` now flags it as a loud
/// `Error` (a REMOVED field silently losing its meaning is never safe to
/// stay quiet about, unlike an ADDITIVE field a future consumer can safely
/// ignore per the minor-bump contract).
///
/// Bumped to **"2.2"** (#1684 Packet 2) — additive: `StepConfig` gained
/// the optional `gate` field (recognized value today: `"operator"`) — the
/// operator sign-off gate. Presence blocks the step at run time until the
/// caller-supplied gate handler approves it (`darkmux_crew::gate`); absence
/// (every pre-2.2 document) is a pure no-op. A future consumer that doesn't
/// understand the field can safely ignore it per the minor-bump contract —
/// but a document that DOES declare an unrecognized gate VALUE is never
/// silently treated as ungated (see `StepConfig::gate`'s own doc on the
/// fail-closed contract).
///
/// Bumped to **"3.0"** (#2004) — a MAJOR bump: `gh_verb` was RENAMED to
/// `MissionConfig::cmd`, and the config block it is checked against
/// (`darkmux_types::config::GhConfig`) to `CmdConfig` (`cmd.enabled` /
/// `cmd.allowed`). A rename is a removal plus an addition, and per this
/// constant's own discipline a removed field is major regardless of whether
/// anything depended on it — as it happens nothing did: no built-in and no
/// user document declared `gh_verb`, so this rename migrates zero data.
///
/// The old name was a lie about the mechanism. `GhConfig`'s own doc already
/// stated the design — "GitHub never enters darkmux core ... just a list of
/// operator-chosen VERB NAMES" — and the mechanism honors it: the gate does
/// nothing but compare a string a config declares against a list the
/// operator allowlisted. But naming it `gh_verb` meant a GitLab user
/// declared `"gh_verb": "mr-merge"`, and a config gating `terraform apply`
/// or `kubectl delete` — which want this gate just as much — had to declare
/// a GitHub-shaped field to get it. `cmd` is neutral across forges AND
/// across domains, which is what the mechanism always was.
///
/// A document still declaring `gh_verb` is a loud `Error` at validate time,
/// NOT a silent overflow into `extras`. That is not merely tidiness: the
/// gate fails OPEN by design (a config declaring no verb is never blocked,
/// so an ungated config stays ungated), so a stale `gh_verb` key would make
/// a config that used to be gated run UNGATED with no signal at all. Same
/// reasoning as the `expand` removal in 2.0, with a sharper edge.
///
/// Bumped to **"2.3"** (#1685) — additive: `MissionConfig` gained the
/// optional `cmd` field. Presence names the `gh`-verb allowlist entry
/// (`darkmux_types::config::CmdConfig`) this config requires before it may
/// run at ALL, on either entry point (`darkmux acp`'s ephemeral panel route
/// via `check_cmd`, or a direct `darkmux mission launch <id>`) — see
/// `MissionConfig::cmd`'s own doc. Absence (every pre-2.3 document,
/// and every config that isn't an operator-authored GitHub-CLI verb) is a
/// pure no-op.
///
/// Bumped to **"3.1"** (#2299) — additive: `PhaseConfig`, `TaskConfig`
/// and `StepConfig` gained the optional `enabled` field (default `true`).
/// `false` prunes the item when a run is minted (`mission_config::prune`):
/// it never exists in the run, so the graph shows exactly what will execute
/// and nothing gray. The resolved-config snapshot every run keeps carries
/// the flags, and the run's `graph-report.json` names what was pruned and
/// why. There is deliberately NO CLI override — edit the JSON and run; the
/// snapshot is the record. A pre-3.1 reader ignores the field and mints
/// everything, which is the additive contract.
///
/// Bumped to **"3.2"** (#2300) — additive: `TaskConfig` gained the optional
/// `grow` field (`GrowSpec`). A task declaring it is a TEMPLATE, never
/// minted itself: at the boundary of the phase that owns it, the launcher
/// reads the `from` task's last step `output` as a PATH to a JSON file,
/// takes the array at `items`, and mints one copy of the template (all its
/// steps) per item. `{{item.<field>}}` in `id`/`config` renders from the
/// item's own top-level scalar fields.
///
/// This is NOT the schema-1.1 `expand`/`ExpansionSpec` primitive coming
/// back (removed in 2.0 above, and deliberately not resurrected). That one
/// was fed by a LAUNCH PARAM — a collection the launcher already held
/// before the run started — which is exactly why both production launchers
/// always handed it an empty map and nothing ever grew. `grow` is fed by a
/// STEP'S OUTPUT, produced by work the run itself did: the fan-out cannot
/// be known at launch time, because the plan it fans out over does not
/// exist yet. Different input, different lifetime, different mechanism.
///
/// A pre-3.2 reader ignores the field and mints nothing for that task,
/// which is the additive contract (the template is not executable on its
/// own in any reader).
///
/// Bumped to **"3.3"** (#2310 P3) — additive: `MissionConfig` gained the
/// optional `outcome_from` field — the document task id whose last step's
/// body the launcher promotes as the `mission close` record's payload,
/// overriding the positional "the last phase's last task" default (see
/// `src/mission_launch.rs::run_summary_payload`'s own doc for the full
/// promotion rule). Absence (every pre-3.3 document) keeps the positional
/// rule unchanged — a pre-3.3 reader ignores the field and gets exactly the
/// pre-existing behavior, the additive contract.
///
/// Bumped to **"3.4"** (#2310 P4/P4a) — additive: `TaskConfig` gained
/// the optional `run_on` field — which TERMINAL statuses of this task's
/// dependencies satisfy readiness (see `crate::types::Task::run_on`'s doc
/// and `scheduler::dependency_satisfies_run_on`). Absence (every pre-3.4
/// document) resolves to `crate::types::default_run_on()`
/// (`["complete"]`), the exact readiness rule every pre-3.4 document
/// already had — a pre-3.4 reader ignores the field and gets identical
/// behavior, the additive contract. (P4a) A task declaring `"error"` also
/// accepts a dependency that reached `Abandoned` — the scheduler's
/// `scheduler::cascade_abandon` rolls a task's TRANSITIVE dependents to
/// `Abandoned`, eagerly, the moment an ancestor errors, so a task several
/// hops downstream of the failure still sees a resolved terminal status
/// this same pass rather than staying wedged `Planned`. See DESIGN.md's
/// "A task's `run_on` decides which of its dependencies' failures it
/// survives" (under "Mission configs") for the full cascade design.
///
/// Bumped to **"3.5"** (#2310 P4f) — additive: `TaskConfig` gained the
/// optional `excludes` field — document-wide task ids that must not be
/// `enabled` alongside this one, so a phase can ship TWO templates for one
/// slot with exactly one live. `review.json`'s `create-mods` phase is
/// the first user: the attended `create-mod` (wait for a frontier-written
/// mod) and the unattended `create-mod-dispatch` (a coder on a
/// hosted-endpoint profile, for the self-hosted runner where no
/// orchestrator session exists). Enforced only in
/// `MissionConfig::validate`, never at run time. A pre-3.5 reader
/// overflows the field into `extras` and mints whatever `enabled` says —
/// and since every SHIPPED document keeps one of each excluded pair
/// disabled, an older binary's behavior on one is unchanged, the additive
/// contract.
///
/// Bumped to **"4.0"** (the 4.0 release), a MAJOR bump, because two changes
/// break documents that parsed before: the `panel` key (and per-config slash
/// commands) is refused rather than ignored, and a step's `config` is checked
/// against its kind's schema at validate time instead of passing through. A
/// 3.x document naming either now fails its gate, and `darkmux doctor` notes
/// a user-tier config whose schema major is older than this build's.
///
/// Bump discipline (see `CLAUDE.md`'s "Versioning" — same rule, different
/// data shape): additive field/section → minor; rename/retype/removed
/// field/new-required-field → major.
pub const MISSION_CONFIG_SCHEMA: &str = "4.0";

/// The numeric components of a version written as `"2.1"`, `"1"` or the
/// JSON number `1`; `None` for anything else.
pub fn parse(v: &Value) -> Option<Vec<u64>> {
    let text = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    text.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

/// Whether `file` is a newer version than `known`. Components compare in
/// order, a missing one counting as 0 (`"1"` equals `"1.0"`). A version
/// neither side can parse is never newer: the marker is read leniently.
pub fn is_newer(file: &Value, known: &str) -> bool {
    let (Some(mut f), Some(mut k)) = (parse(file), parse(&Value::String(known.to_string()))) else {
        return false;
    };
    let len = f.len().max(k.len());
    f.resize(len, 0);
    k.resize(len, 0);
    f > k
}

/// The document's marker under `key`, when it is newer than `known`.
pub fn newer_under(doc: &Value, key: &str, known: &str) -> Option<String> {
    let v = doc.get(key)?;
    is_newer(v, known).then(|| display(v))
}

/// The document's `schema_version`, when it is newer than `known`.
pub fn newer(doc: &Value, known: &str) -> Option<String> {
    newer_under(doc, KEY, known)
}

fn display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The refusal for a file written by a newer darkmux. `kind` names the shape
/// the way a message does (`role manifest`); one text for every reader.
pub fn newer_refusal(kind: &str, file_version: &str, known: &str) -> String {
    format!(
        "this file was written by a newer darkmux ({kind} `{file_version}`; this binary reads `{known}`). Upgrade darkmux."
    )
}

/// Set `doc`'s marker under `key` to `version` (the document must be a JSON
/// object; anything else is left alone). Every save of a versioned shape
/// calls this, so the marker always matches the binary that wrote the file.
pub fn stamp_under(doc: &mut Value, key: &str, version: &str) {
    if let Value::Object(map) = doc {
        map.insert(key.to_string(), Value::String(version.to_string()));
    }
}

/// [`stamp_under`] at [`KEY`].
pub fn stamp(doc: &mut Value, version: &str) {
    stamp_under(doc, KEY, version);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn newer_compares_numerically_and_pads() {
        assert!(is_newer(&json!("1.1"), "1.0"));
        assert!(is_newer(&json!("2"), "1.9"));
        assert!(is_newer(&json!("1.10"), "1.9"), "numeric, not lexicographic");
        assert!(is_newer(&json!(3), "2"));
        assert!(!is_newer(&json!("1"), "1.0"));
        assert!(!is_newer(&json!("1.0"), "1.0"));
        assert!(!is_newer(&json!("0.9"), "1.0"));
    }

    #[test]
    fn an_unparseable_marker_is_never_newer() {
        assert!(!is_newer(&json!("banana"), "1.0"));
        assert!(!is_newer(&json!(null), "1.0"));
        assert!(!is_newer(&json!("1.x"), "1.0"));
    }

    #[test]
    fn absent_means_pre_marker() {
        assert_eq!(newer(&json!({"id": "x"}), "1.0"), None);
        assert_eq!(newer(&json!({"schema_version": "9.0"}), "1.0").as_deref(), Some("9.0"));
    }

    #[test]
    fn stamp_writes_the_key_and_ignores_non_objects() {
        let mut doc = json!({"id": "x"});
        stamp(&mut doc, "1.0");
        assert_eq!(doc["schema_version"], "1.0");
        let mut arr = json!([1]);
        stamp(&mut arr, "1.0");
        assert_eq!(arr, json!([1]));
    }

    #[test]
    fn refusal_text_names_both_versions() {
        let m = newer_refusal("role manifest", "2.0", "1.0");
        assert_eq!(
            m,
            "this file was written by a newer darkmux (role manifest `2.0`; this binary reads `1.0`). Upgrade darkmux."
        );
    }
}
