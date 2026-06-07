use crate::lms;
use crate::runtime::apply_runtime;
use darkmux_types::{Profile, ProfileHookCommand, ProfileModel, ProfileRegistry};
use anyhow::Result;
use std::collections::HashMap;
use std::process::Command;
use std::time::Instant;

/// Prefix attached to identifiers darkmux uses for its own LMStudio loads.
/// Anything visible via `lms ps` starting with this prefix is owned by darkmux
/// and safe to unload during swap; anything else is user state and off-limits.
///
/// See [issue #52](https://github.com/kstrat2001/darkmux/issues/52) for the
/// design rationale (operator-sovereignty applied at model-state level —
/// darkmux never touches state it didn't bring up).
pub const DARKMUX_LMS_NAMESPACE: &str = "darkmux:";

/// Compute the darkmux-namespaced LMStudio identifier for a profile model.
///
/// If the profile sets an explicit `identifier`, use it as-is (allows
/// operators to opt out of the namespace for special cases). Otherwise wrap
/// the model id under the `darkmux:` namespace so unload-filtering can
/// distinguish darkmux's loads from user-managed ones.
pub fn namespaced_identifier(m: &ProfileModel) -> String {
    if let Some(explicit) = m.identifier.as_deref() {
        return explicit.to_string();
    }
    format!("{}{}", DARKMUX_LMS_NAMESPACE, m.id)
}

/// `true` if this identifier was minted by darkmux (begins with our
/// namespace). Used to filter `lms ps` results during swap.
pub fn is_darkmux_owned(identifier: &str) -> bool {
    identifier.starts_with(DARKMUX_LMS_NAMESPACE)
}

#[derive(Debug, Default)]
pub struct SwapResult {
    pub unloaded: Vec<String>,
    pub loaded: Vec<String>,
    /// Identifiers of loaded models we left alone because they're not in
    /// the darkmux namespace — surfaced so callers can report respect for
    /// user state when relevant.
    pub user_state_respected: Vec<String>,
    pub runtime_modified: bool,
    pub hooks_ran: usize,
    pub walltime_ms: u128,
}

pub struct SwapOpts {
    pub quiet: bool,
    pub dry_run: bool,
    /// (#590) Patch the openclaw runtime config (`~/.openclaw/openclaw.json`)
    /// to match this profile. `false` (the default — internal runtime) means
    /// swap touches ONLY LMStudio and never writes openclaw config. `true` is
    /// set only when the operator explicitly opts into openclaw via
    /// `--runtime openclaw` — openclaw config is touched on explicit intent,
    /// never via passive file-presence (openclaw independence).
    pub patch_openclaw: bool,
}

/// (#590) Whether this swap should patch the openclaw runtime config. True
/// ONLY when the operator explicitly opted into openclaw (`--runtime openclaw`
/// → `patch_openclaw`) AND this isn't a dry run. The gate is explicit operator
/// intent — never passive `openclaw.json` file-presence — so an operator
/// running the internal runtime (the default) never has `darkmux swap` mutate
/// `~/.openclaw/openclaw.json`.
fn should_patch_openclaw(opts: &SwapOpts) -> bool {
    opts.patch_openclaw && !opts.dry_run
}

/// (#590) Does a model already loaded at `loaded_ctx` satisfy a profile that
/// wants `wanted_n_ctx`? A profile's `n_ctx` is a **minimum**, not an exact
/// size: a model loaded with *at least* the wanted context satisfies the
/// profile, so swap keeps it rather than reloading it smaller. A larger
/// context is strictly more capable, and reloading down would discard a good
/// load just to shrink it — the operator who loaded it bigger has the RAM for
/// it (operator sovereignty over loaded state). Only an *insufficient* load
/// (smaller than the minimum) triggers a reload.
fn ctx_sufficient(loaded_ctx: u32, wanted_n_ctx: u32) -> bool {
    loaded_ctx >= wanted_n_ctx
}

/// (#590) The context window at which swap loads the machine's standing
/// **utility model**, when `internal.utility` is registered. The utility binding
/// (`RegistryInternal`) carries only a model id — no `n_ctx` — so swap needs a
/// default. 68K is the empirically-grounded small-utility context darkmux has
/// shipped as the compactor default (see `darkmux-heuristics` `m_series_*`
/// compactor sizing and `DEFAULT_COMPACTOR_ID`); a utility model summoned for
/// compaction / estimation rarely needs more. This is a *minimum*, not a
/// prescription (the #600 `ctx_sufficient` semantics): an operator who has
/// already loaded the utility model larger keeps that load. Tuning the utility
/// context via a `utility_n_ctx` config knob is a deliberate follow-up, not
/// shipped here (KISS).
const DEFAULT_UTILITY_N_CTX: u32 = 68_000;

/// One model swap intends to have resident: the LMStudio model key to load,
/// the darkmux-namespaced identifier to load it under, and the minimum context.
struct DesiredLoad {
    /// Bare LMStudio model key passed to `lms load` (the catalog id).
    model_key: String,
    /// Namespaced identifier the load is tagged with (`darkmux:<…>`), so the
    /// unload-filter and `model eject` recognize it as darkmux-owned.
    identifier: String,
    /// Minimum context window (n_ctx-as-minimum, #600).
    n_ctx: u32,
}

/// (#590) Resolve the `(model_key, namespaced_identifier)` pair to load for the
/// machine's configured utility model id. The operator may store **either** the
/// bare LMStudio model key (`qwen3-4b-instruct-2507`) or the darkmux-namespaced
/// identifier (`darkmux:qwen3-4b-instruct-2507`) — both are accepted, matching
/// the same dual-form tolerance the doctor + dispatch preflight use when
/// checking loaded state. Either way the model loads UNDER the darkmux
/// namespace so swap's unload-filter and `model eject` recognize it as
/// darkmux-owned (never double-prefixed).
fn utility_load_target(util_id: &str) -> (String, String) {
    match util_id.strip_prefix(DARKMUX_LMS_NAMESPACE) {
        // Already namespaced — load the bare key under the stored identifier.
        Some(bare) => (bare.to_string(), util_id.to_string()),
        // Bare key — wrap it under the namespace.
        None => (util_id.to_string(), format!("{DARKMUX_LMS_NAMESPACE}{util_id}")),
    }
}

/// (#590) Everything a swap to `profile` wants resident: the profile's
/// models, plus the machine's standing utility model (`internal.utility`) when
/// registered. The utility model is loaded *alongside* the profile's models so
/// compaction finds it resident mid-dispatch — and because it lives in the
/// machine-level `internal` binding (not any profile's `models[]`), it survives
/// every profile swap. A utility model that happens to duplicate a
/// declared model is loaded once (the declared entry wins, keeping its declared
/// context). Pure: the caller owns the `lms ps` / load I/O.
fn desired_loads(profile: &Profile, registry: &ProfileRegistry) -> Vec<DesiredLoad> {
    let mut loads: Vec<DesiredLoad> = profile
        .models
        .iter()
        .map(|m| DesiredLoad {
            model_key: m.id.clone(),
            identifier: namespaced_identifier(m),
            n_ctx: m.n_ctx,
        })
        .collect();
    if let Some(util_id) = registry.utility_model_id() {
        let (model_key, identifier) = utility_load_target(util_id);
        // Don't load twice if the utility model is also a declared model —
        // the declared entry already covers it (and keeps its declared context).
        if !loads.iter().any(|l| l.identifier == identifier) {
            loads.push(DesiredLoad { model_key, identifier, n_ctx: DEFAULT_UTILITY_N_CTX });
        }
    }
    loads
}

pub fn swap(profile: &Profile, registry: &ProfileRegistry, opts: SwapOpts) -> Result<SwapResult> {
    let t0 = Instant::now();
    let mut result = SwapResult::default();

    // Pre/post-swap hooks run on EVERY swap, independent of `--runtime`: they
    // are operator-authored shell commands (RegistryHooks), not darkmux-owned
    // openclaw writes, so the openclaw-independence gate (#590) intentionally
    // does NOT suppress them (operator sovereignty over their own hooks).
    let pre = registry.hooks.as_ref().map(|h| h.pre_swap.as_slice()).unwrap_or(&[]);
    result.hooks_ran += run_hooks(pre, profile, &opts);

    // Everything this swap wants resident: the profile's models plus the
    // machine's standing utility model (#590). Map of namespaced identifier →
    // desired (minimum) context length, used to drive the Pass-1 unload
    // decision below.
    let desired = desired_loads(profile, registry);
    let mut want: HashMap<String, u32> = HashMap::new();
    for d in &desired {
        want.insert(d.identifier.clone(), d.n_ctx);
    }

    let loaded = lms::list_loaded()?;

    // Pass 1 — unload anything in the darkmux namespace the new profile
    // doesn't want, or that's loaded with LESS than its n_ctx minimum. A
    // model loaded with at least the wanted context is kept (n_ctx is a min,
    // not an exact size — see `ctx_sufficient`). Never touch entries outside
    // the namespace; those are user state.
    for cur in &loaded {
        if !is_darkmux_owned(&cur.identifier) {
            result.user_state_respected.push(cur.identifier.clone());
            continue;
        }
        let desired_ctx = want.get(&cur.identifier).copied();
        if desired_ctx.is_some_and(|d| ctx_sufficient(cur.context as u32, d)) {
            continue; // already loaded with enough context
        }
        if !opts.quiet {
            println!("unload {} (was ctx={})", cur.identifier, cur.context);
        }
        if !opts.dry_run {
            lms::unload(&cur.identifier)?;
        }
        result.unloaded.push(cur.identifier.clone());
    }

    // Pass 2 — load anything the profile wants that isn't already correctly
    // loaded under our namespace.
    let loaded_after_unload: HashMap<&str, &darkmux_types::LoadedModel> = loaded
        .iter()
        .filter(|lm| !result.unloaded.iter().any(|u| u == &lm.identifier))
        .map(|lm| (lm.identifier.as_str(), lm))
        .collect();
    for d in &desired {
        if let Some(c) = loaded_after_unload.get(d.identifier.as_str()) {
            if ctx_sufficient(c.context as u32, d.n_ctx) {
                continue; // already loaded with enough context (n_ctx is a min)
            }
        }
        if !opts.quiet {
            println!("load {} @ ctx={}", d.identifier, d.n_ctx);
        }
        if !opts.dry_run {
            lms::load_with_identifier(&d.model_key, d.n_ctx, &d.identifier, opts.quiet)?;
        }
        result.loaded.push(d.identifier.clone());
    }

    // (#590) Openclaw config is patched ONLY when the operator explicitly
    // opted into openclaw (`--runtime openclaw`); the default internal-runtime
    // swap never touches `~/.openclaw/openclaw.json` (openclaw independence).
    if should_patch_openclaw(&opts) {
        result.runtime_modified = apply_runtime(profile)?;
        if result.runtime_modified && !opts.quiet {
            println!("openclaw config patched");
        }
    }

    let post = registry.hooks.as_ref().map(|h| h.post_swap.as_slice()).unwrap_or(&[]);
    result.hooks_ran += run_hooks(post, profile, &opts);

    result.walltime_ms = t0.elapsed().as_millis();
    Ok(result)
}

fn run_hooks(hooks: &[ProfileHookCommand], profile: &Profile, opts: &SwapOpts) -> usize {
    let mut count = 0;
    for hook in hooks {
        if hook.condition.as_deref() == Some("runtime != null") && profile.runtime.is_none() {
            continue;
        }
        let cmd = hook
            .command
            .replace("{{ profile }}", profile.description.as_deref().unwrap_or("(profile)"))
            .replace("{{profile}}", profile.description.as_deref().unwrap_or("(profile)"));
        if cmd.trim().is_empty() {
            continue;
        }
        if !opts.quiet {
            println!("hook: {cmd}");
        }
        if !opts.dry_run {
            let status = Command::new("/bin/sh").arg("-c").arg(&cmd).status();
            if let Ok(s) = status {
                if !s.success() && !opts.quiet {
                    eprintln!("hook failed (exit {}): {cmd}", s.code().unwrap_or(-1));
                }
            }
        }
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::{Profile, ProfileModel, ProfileRegistry, ProfileRuntime, RegistryInternal};

    fn opts() -> SwapOpts {
        SwapOpts { quiet: true, dry_run: true, patch_openclaw: false }
    }

    #[test]
    fn does_not_patch_openclaw_without_explicit_opt_in() {
        // The default (internal runtime) — swap never touches openclaw.json.
        assert!(!should_patch_openclaw(&SwapOpts {
            quiet: true,
            dry_run: false,
            patch_openclaw: false,
        }));
    }

    #[test]
    fn patches_openclaw_only_on_explicit_opt_in_and_real_run() {
        // `--runtime openclaw` opts in; a real (non-dry) run patches.
        assert!(should_patch_openclaw(&SwapOpts {
            quiet: true,
            dry_run: false,
            patch_openclaw: true,
        }));
        // A dry run never patches, even when opted in.
        assert!(!should_patch_openclaw(&SwapOpts {
            quiet: true,
            dry_run: true,
            patch_openclaw: true,
        }));
    }

    /// A registry whose `internal.utility` is `util` (or no `internal` block
    /// when `None`). Profiles are irrelevant to the utility-load helpers.
    fn registry_with_utility(util: Option<&str>) -> ProfileRegistry {
        ProfileRegistry {
            internal: util.map(|u| RegistryInternal { utility: Some(u.to_string()) }),
            ..Default::default()
        }
    }

    fn profile_with(desc: &str, runtime: Option<ProfileRuntime>) -> Profile {
        Profile {
            extras: Default::default(),
            description: Some(desc.to_string()),
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "m".into(),
                n_ctx: 1000,
                capabilities: Default::default(),
                identifier: None,
            }],
            default_model: None,
            runtime,
            use_when: None,
        }
    }

    #[test]
    fn namespaced_identifier_uses_prefix_when_no_override() {
        let m = ProfileModel {
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: 100_000,
            capabilities: Default::default(),
            identifier: None,
        };
        assert_eq!(namespaced_identifier(&m), "darkmux:qwen3.6-35b-a3b");
    }

    #[test]
    fn namespaced_identifier_passes_through_explicit_id() {
        let m = ProfileModel {
            extras: Default::default(),
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: 100_000,
            capabilities: Default::default(),
            identifier: Some("my-custom-alias".into()),
        };
        // Explicit override wins — operator opted out of the auto-namespace.
        assert_eq!(namespaced_identifier(&m), "my-custom-alias");
    }

    #[test]
    fn is_darkmux_owned_detects_namespace() {
        assert!(is_darkmux_owned("darkmux:qwen3.6-35b-a3b"));
        assert!(is_darkmux_owned("darkmux:anything-after"));
        // Non-namespaced ids are user state — off-limits.
        assert!(!is_darkmux_owned("qwen3.6-35b-a3b"));
        assert!(!is_darkmux_owned("user-loaded-model"));
        assert!(!is_darkmux_owned("my-custom-alias"));
        // Partial match isn't enough.
        assert!(!is_darkmux_owned("dark:foo"));
        assert!(!is_darkmux_owned("predarkmux:foo"));
    }

    #[test]
    fn ctx_sufficient_treats_n_ctx_as_a_minimum() {
        // The motivating case: a model loaded LARGER than the profile wants is
        // kept — no reload-down. (qwen @ 200k satisfies a 64k profile.)
        assert!(ctx_sufficient(200_000, 64_000));
        // Exactly enough is fine.
        assert!(ctx_sufficient(64_000, 64_000));
        // Only an insufficient load triggers a reload.
        assert!(!ctx_sufficient(64_000, 200_000));
    }

    #[test]
    fn utility_load_target_wraps_a_bare_model_key() {
        // The profiles.example.json form: a bare LMStudio catalog id. Loaded
        // under the darkmux namespace; the bare key drives the actual load.
        let (key, ident) = utility_load_target("qwen3-4b-instruct-2507");
        assert_eq!(key, "qwen3-4b-instruct-2507");
        assert_eq!(ident, "darkmux:qwen3-4b-instruct-2507");
    }

    #[test]
    fn utility_load_target_accepts_a_namespaced_id_without_double_prefixing() {
        // The DEFAULT_COMPACTOR_MODEL / doctor-test form: already namespaced.
        // The bare key is recovered for the load; the identifier is kept as-is.
        let (key, ident) = utility_load_target("darkmux:qwen3-4b-instruct-2507");
        assert_eq!(key, "qwen3-4b-instruct-2507");
        assert_eq!(ident, "darkmux:qwen3-4b-instruct-2507");
    }

    #[test]
    fn desired_loads_appends_the_registered_utility_model() {
        // Model "m" (ctx 1000) + a registered utility model "util-4b".
        let profile = profile_with("p", None);
        let registry = registry_with_utility(Some("util-4b"));
        let loads = desired_loads(&profile, &registry);
        assert_eq!(loads.len(), 2);
        assert!(loads.iter().any(|l| l.identifier == "darkmux:m" && l.n_ctx == 1000));
        let util = loads.iter().find(|l| l.identifier == "darkmux:util-4b").unwrap();
        assert_eq!(util.model_key, "util-4b");
        // The utility model loads at the empirical default context.
        assert_eq!(util.n_ctx, DEFAULT_UTILITY_N_CTX);
    }

    #[test]
    fn desired_loads_without_a_utility_binding_is_workers_only() {
        let profile = profile_with("p", None);
        let loads = desired_loads(&profile, &registry_with_utility(None));
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].identifier, "darkmux:m");
    }

    #[test]
    fn desired_loads_does_not_double_load_a_utility_that_is_also_a_worker() {
        // The operator declared the same model as both a profile model and the
        // utility model — load it once, keeping the model's declared context
        // (not the utility default).
        let profile = profile_with("p", None); // model id "m" @ ctx 1000
        let loads = desired_loads(&profile, &registry_with_utility(Some("m")));
        assert_eq!(loads.len(), 1, "duplicate utility/model isn't loaded twice");
        assert_eq!(loads[0].n_ctx, 1000, "the model's declared context wins over the utility default");
    }

    #[test]
    fn desired_loads_dedups_when_worker_identifier_collides_with_util_identifier() {
        // (#590 NIT) A model whose explicit `identifier` happens to equal the
        // util model's namespaced identifier dedups to ONE load (by identifier),
        // and the model entry wins — so the `darkmux:util-4b` slot loads the
        // declared model's key, not the util's. This is operator misconfiguration
        // (the doctor/preflight will WARN "util not loaded" because the stored
        // `util-4b` matches neither the loaded `.model` nor `.identifier`), but
        // it must never produce two competing loads of the same identifier.
        let profile = Profile {
            extras: Default::default(),
            description: None,
            models: vec![ProfileModel {
                extras: Default::default(),
                id: "worker-35b".into(),
                n_ctx: 100_000,
                capabilities: Default::default(),
                identifier: Some("darkmux:util-4b".into()),
            }],
            default_model: None,
            runtime: None,
            use_when: None,
        };
        let loads = desired_loads(&profile, &registry_with_utility(Some("util-4b")));
        assert_eq!(loads.len(), 1, "collision must dedup to a single load");
        assert_eq!(loads[0].identifier, "darkmux:util-4b");
        assert_eq!(loads[0].model_key, "worker-35b", "the model entry wins the identifier slot");
    }

    #[test]
    fn run_hooks_substitutes_profile_placeholder() {
        let hooks = vec![ProfileHookCommand {
            command: "echo hello {{ profile }}".to_string(),
            condition: None,
        }];
        let profile = profile_with("my-profile", None);
        // dry-run + quiet: counts substitution but doesn't shell out
        let count = run_hooks(&hooks, &profile, &opts());
        assert_eq!(count, 1);
    }

    #[test]
    fn run_hooks_skips_runtime_condition_when_no_runtime() {
        let hooks = vec![ProfileHookCommand {
            command: "echo gateway".to_string(),
            condition: Some("runtime != null".to_string()),
        }];
        let profile = profile_with("p", None);
        let count = run_hooks(&hooks, &profile, &opts());
        assert_eq!(count, 0);
    }

    #[test]
    fn run_hooks_runs_runtime_condition_when_runtime_set() {
        let hooks = vec![ProfileHookCommand {
            command: "echo gateway".to_string(),
            condition: Some("runtime != null".to_string()),
        }];
        let profile = profile_with("p", Some(ProfileRuntime::default()));
        let count = run_hooks(&hooks, &profile, &opts());
        assert_eq!(count, 1);
    }

    #[test]
    fn run_hooks_skips_empty_command() {
        let hooks = vec![ProfileHookCommand {
            command: "   ".to_string(),
            condition: None,
        }];
        let profile = profile_with("p", None);
        let count = run_hooks(&hooks, &profile, &opts());
        assert_eq!(count, 0);
    }

    #[test]
    fn placeholder_with_no_spaces_substitutes() {
        let hooks = vec![ProfileHookCommand {
            command: "echo {{profile}}".to_string(),
            condition: None,
        }];
        let profile = profile_with("p", None);
        let count = run_hooks(&hooks, &profile, &opts());
        assert_eq!(count, 1);
    }
}
