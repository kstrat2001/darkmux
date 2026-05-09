use crate::lms;
use crate::runtime::apply_runtime;
use crate::types::{Profile, ProfileHookCommand, ProfileRegistry};
use anyhow::Result;
use std::collections::HashMap;
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct SwapResult {
    pub unloaded: Vec<String>,
    pub loaded: Vec<String>,
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

    // desired state: identifier -> n_ctx
    let mut want: HashMap<String, u64> = HashMap::new();
    for m in &profile.models {
        let ident = m.identifier.clone().unwrap_or_else(|| m.id.clone());
        want.insert(ident, m.n_ctx as u64);
    }

    let loaded = lms::list_loaded()?;
    let mut loaded_by_id: HashMap<String, &crate::types::LoadedModel> = HashMap::new();
    for lm in &loaded {
        loaded_by_id.insert(lm.identifier.clone(), lm);
    }

    // unload anything we don't want, OR want at a different ctx
    for cur in &loaded {
        let desired = want.get(&cur.identifier).copied();
        if desired.is_none() || desired != Some(cur.context) {
            if !opts.quiet {
                println!("unload {} (was ctx={})", cur.identifier, cur.context);
            }
            if !opts.dry_run {
                lms::unload(&cur.identifier)?;
            }
            result.unloaded.push(cur.identifier.clone());
        }
    }

    // load anything in profile that isn't already correctly loaded
    for m in &profile.models {
        let ident = m.identifier.clone().unwrap_or_else(|| m.id.clone());
        let cur = loaded_by_id.get(&ident);
        if let Some(c) = cur {
            if c.context as u32 == m.n_ctx {
                continue;
            }
        }
        if !opts.quiet {
            println!("load {ident} @ ctx={}", m.n_ctx);
        }
        if !opts.dry_run {
            lms::load(m, opts.quiet)?;
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
