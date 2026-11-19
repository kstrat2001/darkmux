//! The keys `init`'s completion adds to an existing config.json must read the
//! same absent as present: a default whose absence darkmux reads differently
//! (`machine_id`, named by the hostname when absent) belongs in
//! [`NEVER_COMPLETED`], or completing an established machine's config changes
//! what it does. Every zero-arg accessor in `darkmux_types::config_access` is
//! read with each completable leaf removed and with the full default, and
//! every registered enum setting is resolved the same way; a leaf whose
//! removal changes any of them, and is not in `NEVER_COMPLETED`, fails naming
//! the leaf and the accessor.

use super::*;
use darkmux_types::config::DarkmuxConfig;
use darkmux_types::config_access::{self as access, set_config_for_test};

/// Each accessor's resolved value under `cfg`, by name.
fn resolved(cfg: &DarkmuxConfig) -> Vec<(&'static str, String)> {
    let _guard = set_config_for_test(cfg.clone());
    macro_rules! read { ($($f:ident),* $(,)?) => { vec![$((stringify!($f), format!("{:?}", access::$f()))),*] }; }
    let mut out = read!(
        machine_id,
        fleet_mode,
        declared_fleet_mode,
        fleet_defaults_radio_answerer_profile,
        fleet_busy_policy,
        fleet_identity_provider,
        fleet_identity_bin,
        fleet_listener_enabled,
        fleet_listener_port,
        lms_bin,
        tensorfold_bin,
        flint_bin,
        default_engine,
        lmstudio_url,
        redis_stream,
        redis_maxlen,
        redis_telemetry_stream,
        redis_telemetry_maxlen,
        redis_enabled,
        redis_host,
        redis_port,
        redis_db,
        audit_dir_override,
        audit_enabled,
        cmd_enabled,
        cmd_allowed_verbs,
        clients_remote,
        clients_pairing_verified_teams,
        clients_profiles,
        hooks_enabled,
        hooks_outbox_dir,
        hooks_rules,
        hooks_max_outbox_mb,
        hooks_adapters_dir,
        hooks_jq_timeout_ms,
        hooks_jq_max_output_bytes,
        serve_port,
        serve_bind,
        serve_client_addr,
        serve_client_port,
        serve_listen_addr,
        inactivity_timeout_seconds,
        model_load_timeout_seconds,
        step_command_timeout_seconds,
        mission_wall_clock_timeout_seconds,
        dispatch_free_concurrency,
        max_turns,
        max_tokens,
        max_tokens_per_call,
        reasoning_checkpoint_interval_tokens,
        generation_checkpoint_interval_tokens,
        max_stall_recoveries,
        retired_env_leftovers,
        check_retired_env,
        radio_answerer_profile,
        radio_humor,
        acp_idle_exit_minutes,
        liveness_retention_hours,
        role_profiles,
        default_role,
        daemon_cors_origins,
        serve_token_keychain,
        serve_read_auth,
        injected_context_fraction,
        strict_selection,
        check_updates,
        feedback_injection,
        turn_delay_ms,
        tests_on_host,
        sandbox_memory,
        sandbox_memory_problem,
        container_user_override,
        host_sampler_interval_ms,
        live_sample_ms,
        liveness_dir,
        host_sampler_lock_path,
        thermal_enabled,
        detection_degeneracy_policy,
        memory_pressure_gate,
        sandbox,
        sandbox_default,
        thermal_pause_at,
        thermal_resume_at,
        thermal_resume_hold_ms,
        thermal_max_pause_ms,
        thermal_pace_staleness_ceiling_ms,
        thermal_min_cpu_speed_limit_pct,
        thermal_speed_limit_hold_samples,
        thermal_speed_limit_hold_samples_raw,
        thermal_duty_delay_ms,
        thermal_ratchet_factor,
        thermal_ratchet_factor_raw,
        thermal_episode_threshold,
        thermal_tier4_enabled,
        host_source_script,
        power_min_battery_pct,
        power_refuse_start_below_min,
        power_pause_running_below_min,
        mission_stale_active_days,
        flows_dir,
        findings_dir,
        mods_dir,
        lab_dir,
        runtime_cache_dir,
        cache_dir,
        toolchains_dir,
        fleet_file,
        identity_path_override,
        templates_override_dirs,
        skills_override_dirs,
        resolved_locations
    );
    for setting in darkmux_types::config_enum::ENUM_SETTINGS
        .iter()
        .filter(|s| !s.is_per_item())
    {
        out.push((
            setting.key,
            format!("{:?}", access::resolve_enum_token(setting).map(|(token, _)| token)),
        ));
    }
    out
}

/// Accessors left out of [`resolved`], each because its value carries where it
/// came from (config, env or built-in), which differs absent and present by
/// construction; the value itself is read through the plain accessor beside
/// it. Every `*_with_source` accessor is left out for the same reason.
const UNREAD_ACCESSORS: &[&str] = &["hooks_enabled_provenance", "live_cadence", "serve_client_endpoint"];

/// Every leaf path of a JSON object (an empty object or an array is a leaf).
fn leaves(value: &Value, prefix: &str, out: &mut Vec<String>) {
    match value.as_object() {
        Some(obj) if !obj.is_empty() => {
            for (k, v) in obj {
                leaves(
                    v,
                    &if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    },
                    out,
                );
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

fn never_completed(leaf: &str) -> bool {
    NEVER_COMPLETED
        .iter()
        .any(|k| leaf == *k || leaf.starts_with(&format!("{k}.")))
}

/// The accessors whose value differs between `with` and `with` minus `leaf`.
fn read_differently(defaults: &Value, leaf: &str, present: &[(&'static str, String)]) -> Vec<String> {
    let mut without = defaults.clone();
    remove_dotted(&mut without, leaf);
    let cfg: DarkmuxConfig = serde_json::from_value(without).expect("the defaults minus one key still load");
    resolved(&cfg)
        .into_iter()
        .zip(present)
        .filter(|((_, absent), (_, with))| absent != with)
        .map(|((name, absent), (_, with))| format!("{name}: absent {absent}, present {with}"))
        .collect()
}

/// Every `DARKMUX_*` setting variable cleared (the state-location ones stay
/// as `IsolatedState` pinned them), so an override in the shell running the
/// tests cannot mask a key's absence: the env tier outranks config.json.
struct NoSettingEnv(Vec<(String, std::ffi::OsString)>);

impl NoSettingEnv {
    fn new() -> Self {
        let pinned = |k: &str| {
            darkmux_types::test_isolation::PINNED_STATE_VARS
                .iter()
                .any(|(v, _)| *v == k)
        };
        let cleared: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
            .filter(|(k, _)| k.starts_with("DARKMUX_") && !pinned(k))
            .collect();
        for (k, _) in &cleared {
            // SAFETY: #[serial]; restored on drop.
            unsafe { std::env::remove_var(k) };
        }
        Self(cleared)
    }
}

impl Drop for NoSettingEnv {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            // SAFETY: as in `new`.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

/// The defaults `init` would complete with, generated under one darkmux root
/// and read under another: a root-derived directory written as a literal
/// then reads differently from the absent key, as it would once a machine's
/// root moves.
fn defaults_read_under_a_moved_root() -> (
    Value,
    NoSettingEnv,
    darkmux_types::test_isolation::IsolatedState,
    tempfile::TempDir,
) {
    let state = darkmux_types::test_isolation::IsolatedState::new();
    let env = NoSettingEnv::new();
    let defaults = default_config().unwrap();
    let moved = tempfile::TempDir::new().unwrap();
    // SAFETY: #[serial]; `IsolatedState` restores DARKMUX_HOME on drop.
    unsafe { std::env::set_var("DARKMUX_HOME", moved.path()) };
    (defaults, env, state, moved)
}

#[test]
#[serial_test::serial]
fn every_key_completion_adds_reads_the_same_absent_as_present() {
    let (defaults, _env, _state, _moved) = defaults_read_under_a_moved_root();
    let present = resolved(&serde_json::from_value(defaults.clone()).unwrap());
    let mut paths = Vec::new();
    leaves(&defaults, "", &mut paths);
    let mut failures = Vec::new();
    for leaf in paths.iter().filter(|l| !never_completed(l)) {
        let differs = read_differently(&defaults, leaf, &present);
        if !differs.is_empty() {
            failures.push(format!(
                "`{leaf}` (add it to NEVER_COMPLETED):\n    {}",
                differs.join("\n    ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "completion would add keys darkmux reads differently when absent:\n{}",
        failures.join("\n")
    );
}

/// The other direction: every NEVER_COMPLETED key does read differently
/// absent, so the list never keeps a key that completion could add.
#[test]
#[serial_test::serial]
fn every_never_completed_key_reads_differently_absent() {
    let (defaults, _env, _state, _moved) = defaults_read_under_a_moved_root();
    let present = resolved(&serde_json::from_value(defaults.clone()).unwrap());
    for key in NEVER_COMPLETED {
        assert!(
            !read_differently(&defaults, key, &present).is_empty(),
            "`{key}` reads the same absent; complete it"
        );
    }
}

/// [`resolved`] reads every zero-arg accessor `config_access` declares, so a
/// new setting's accessor is compared the moment it exists.
#[test]
#[serial_test::serial]
fn the_comparison_reads_every_config_accessor() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/darkmux-types/src/config_access.rs"),
    )
    .unwrap();
    // A signature may break after `(`, so the argument list is read across lines.
    let declared: std::collections::BTreeSet<&str> = source
        .match_indices("\npub fn ")
        .filter_map(|(at, m)| source[at + m.len()..].split_once('('))
        .filter(|(_, args)| args.trim_start().starts_with(')'))
        .map(|(name, _)| name)
        .filter(|n| !n.ends_with("_with_source") && !UNREAD_ACCESSORS.contains(n))
        .collect();
    assert!(
        declared.contains("fleet_identity_provider"),
        "a signature broken after `(` is read too"
    );
    let _state = darkmux_types::test_isolation::IsolatedState::new();
    let read: std::collections::BTreeSet<&str> = resolved(&DarkmuxConfig::default())
        .into_iter()
        .map(|(name, _)| name)
        .filter(|n| !n.contains('.'))
        .collect();
    let missing: Vec<&&str> = declared.difference(&read).collect();
    assert!(
        missing.is_empty(),
        "add these accessors to `resolved` (or UNREAD_ACCESSORS, with why): {missing:?}"
    );
}
