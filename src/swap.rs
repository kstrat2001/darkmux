use crate::lms;
use crate::runtime::apply_runtime;
use crate::types::{Profile, ProfileHookCommand, ProfileModel, ProfileRegistry};
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
}

pub fn swap(profile: &Profile, registry: &ProfileRegistry, opts: SwapOpts) -> Result<SwapResult> {
    let t0 = Instant::now();
    let mut result = SwapResult::default();

    let pre = registry.hooks.as_ref().map(|h| h.pre_swap.as_slice()).unwrap_or(&[]);
    result.hooks_ran += run_hooks(pre, profile, &opts);

    // Map of namespaced identifier → desired context length for everything
    // the new profile wants loaded.
    let mut want: HashMap<String, u32> = HashMap::new();
    for m in &profile.models {
        want.insert(namespaced_identifier(m), m.n_ctx);
    }

    let loaded = lms::list_loaded()?;

    // Pass 1 — unload anything in the darkmux namespace that doesn't match
    // the new profile (wrong ctx, or absent from profile entirely). Never
    // touch entries outside the namespace; those are user state.
    for cur in &loaded {
        if !is_darkmux_owned(&cur.identifier) {
            result.user_state_respected.push(cur.identifier.clone());
            continue;
        }
        let desired_ctx = want.get(&cur.identifier).copied();
        let same_ctx = desired_ctx == Some(cur.context as u32);
        if same_ctx {
            continue; // already correctly loaded
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
    let loaded_after_unload: HashMap<&str, &crate::types::LoadedModel> = loaded
        .iter()
        .filter(|lm| !result.unloaded.iter().any(|u| u == &lm.identifier))
        .map(|lm| (lm.identifier.as_str(), lm))
        .collect();
    for m in &profile.models {
        let ident = namespaced_identifier(m);
        if let Some(c) = loaded_after_unload.get(ident.as_str()) {
            if c.context as u32 == m.n_ctx {
                continue; // already correctly loaded under our namespace
            }
        }
        if !opts.quiet {
            println!("load {ident} @ ctx={}", m.n_ctx);
        }
        if !opts.dry_run {
            lms::load_with_identifier(&m.id, m.n_ctx, &ident, opts.quiet)?;
        }
        result.loaded.push(ident);
    }

    if !opts.dry_run {
        result.runtime_modified = apply_runtime(profile)?;
        if result.runtime_modified && !opts.quiet {
            println!("runtime config patched");
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
    use crate::types::{ModelRole, Profile, ProfileModel, ProfileRuntime};

    fn opts() -> SwapOpts {
        SwapOpts { quiet: true, dry_run: true }
    }

    fn profile_with(desc: &str, runtime: Option<ProfileRuntime>) -> Profile {
        Profile {
            description: Some(desc.to_string()),
            models: vec![ProfileModel {
                id: "m".into(),
                n_ctx: 1000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            runtime,
            use_when: None,
        }
    }

    #[test]
    fn namespaced_identifier_uses_prefix_when_no_override() {
        let m = ProfileModel {
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: 100_000,
            role: ModelRole::Primary,
            identifier: None,
        };
        assert_eq!(namespaced_identifier(&m), "darkmux:qwen3.6-35b-a3b");
    }

    #[test]
    fn namespaced_identifier_passes_through_explicit_id() {
        let m = ProfileModel {
            id: "qwen3.6-35b-a3b".into(),
            n_ctx: 100_000,
            role: ModelRole::Primary,
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
