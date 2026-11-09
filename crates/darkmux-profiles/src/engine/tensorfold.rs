//! (#3448) Tensorfold, the first engine darkmux starts servers for
//! (DESIGN.md, "Engine servers darkmux starts").
//!
//! `tensorfold-native serve MODEL` holds one model for its whole life and has
//! no HTTP call that loads or unloads one, so darkmux loads a model by
//! starting a server and unloads it by stopping the one it started. This
//! stage reads only what the installed binary says about itself, through its
//! own read-only commands: `--version`, `capabilities --json` (the `serve`
//! flags it accepts), `models --json` (the models it has) and `info MODEL`
//! (a model's weights and memory floor). Starting and stopping servers comes
//! later.
//!
//! darkmux supports the releases it was verified against
//! ([`VERIFIED_RELEASES`]) and refuses any other, naming the version: a flag
//! renamed between releases would otherwise start a server darkmux cannot
//! account for. Every call is bounded ([`PROBE_BOUND`]), so a wedged binary
//! is reported rather than hanging `doctor`.

use crate::gestalt_host::lms_host::{run_bounded, StdoutMode};
use anyhow::{bail, Context, Result};
use darkmux_gestalt::Deadline;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

/// Bound on one read-only call, spawn through exit: the bound the ledger
/// gives each `lms` metadata call. `models --json` on this machine answers
/// in well under a second.
pub const PROBE_BOUND: Duration = Duration::from_secs(5);

/// The releases darkmux was verified against, as an operator reads them.
pub const VERIFIED_RELEASES: &str = "1.2.x";

/// A Tensorfold release, `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Whether darkmux was verified against this release ([`VERIFIED_RELEASES`]).
    pub fn is_verified(self) -> bool {
        self.major == 1 && self.minor == 2
    }
}

/// `tensorfold-native 1.2.4` (what `--version` prints) to its version.
pub fn parse_version(text: &str) -> Result<Version> {
    let line = text.lines().next().unwrap_or_default().trim();
    let raw = line.rsplit(' ').next().unwrap_or_default();
    let mut parts = raw.split('.').map(str::parse::<u32>);
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) => Ok(Version { major, minor, patch }),
        _ => bail!("`--version` printed {line:?}, not `tensorfold-native <major>.<minor>.<patch>`"),
    }
}

/// The flags `tensorfold-native serve` accepts, from `capabilities --json`:
/// each flag, and the values it takes when the engine lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeFlags(BTreeMap<String, Option<Vec<String>>>);

impl ServeFlags {
    pub fn knows(&self, flag: &str) -> bool {
        self.0.contains_key(flag)
    }

    /// Why `args` (a profile model's `engine_args`) would not start a server,
    /// or `Ok`: a flag this engine does not accept, or a value outside the
    /// list it declares for that flag. A token that is not a flag is the
    /// value of the flag before it. Never echoes a value (it may be a secret).
    pub fn check(&self, args: &[String]) -> std::result::Result<(), String> {
        let mut pending: Option<(&str, &[String])> = None;
        for arg in args {
            if let Some(stripped) = arg.strip_prefix("--") {
                let (flag, inline) = match stripped.split_once('=') {
                    Some((f, v)) => (f, Some(v)),
                    None => (stripped, None),
                };
                let flag = format!("--{flag}");
                let Some(values) = self.0.get(&flag) else {
                    return Err(format!("`{flag}` is not a flag `tensorfold-native serve` accepts"));
                };
                pending = None;
                match (values, inline) {
                    (Some(allowed), Some(v)) if !allowed.iter().any(|a| a == v) => {
                        return Err(format!("`{flag}` takes one of {}", allowed.join(", ")));
                    }
                    (Some(allowed), None) => {
                        pending = self.0.get_key_value(&flag).map(|(k, _)| (k.as_str(), allowed.as_slice()));
                    }
                    _ => {}
                }
            } else if let Some((flag, allowed)) = pending.take() {
                if !allowed.iter().any(|a| a == arg) {
                    return Err(format!("`{flag}` takes one of {}", allowed.join(", ")));
                }
            }
        }
        Ok(())
    }
}

/// `capabilities --json` to the `serve` flags. Refuses a document whose
/// `schema` is not the one darkmux reads (1), rather than misreading it.
pub fn parse_capabilities(json: &str) -> Result<ServeFlags> {
    let doc: serde_json::Value = serde_json::from_str(json).context("`capabilities --json` is not JSON")?;
    expect_schema(&doc, "capabilities --json")?;
    let flags = doc
        .pointer("/serve/flags")
        .and_then(|f| f.as_object())
        .context("`capabilities --json` has no `serve.flags` object")?;
    let parsed = flags
        .iter()
        .map(|(flag, spec)| {
            let values = spec.get("values").and_then(|v| v.as_array()).map(|vs| {
                vs.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()
            });
            (flag.clone(), values)
        })
        .collect();
    Ok(ServeFlags(parsed))
}

/// One model `models --json` lists: a Hugging Face repo id, which is what a
/// profile model's `id` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineModel {
    pub repo: String,
    pub family: Option<String>,
}

/// `models --json` to the models it lists, in its order.
pub fn parse_models(json: &str) -> Result<Vec<EngineModel>> {
    let doc: serde_json::Value = serde_json::from_str(json).context("`models --json` is not JSON")?;
    expect_schema(&doc, "models --json")?;
    let rows = doc.get("models").and_then(|m| m.as_array()).context("`models --json` has no `models` list")?;
    rows.iter()
        .map(|row| {
            let repo = row.get("repo").and_then(|r| r.as_str()).context("a `models --json` row has no `repo`")?;
            let family = row.get("family").and_then(|f| f.as_str()).map(str::to_string);
            Ok(EngineModel { repo: repo.to_string(), family })
        })
        .collect()
}

/// What `info MODEL` says a model costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    /// The weights on disk.
    pub weights_bytes: u64,
    /// The engine's own floor for serving it: the weights plus its runtime
    /// margin, before any KV cache.
    pub memory_floor_bytes: u64,
}

/// `info MODEL`'s text to its sizes. The byte counts are read from the
/// `weights:` line's parenthesized bytes and the floor's GiB, the two forms
/// 1.2.4 prints.
pub fn parse_info(text: &str) -> Result<ModelInfo> {
    let line = |key: &str| text.lines().map(str::trim).find_map(|l| l.strip_prefix(key)).map(str::trim);
    let weights = line("weights:").context("`info` printed no `weights:` line")?;
    let weights_bytes = weights
        .split_once('(')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .with_context(|| format!("`info`'s weights line {weights:?} names no byte count"))?;
    let floor = line("memory floor:").context("`info` printed no `memory floor:` line")?;
    let gib = floor
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<f64>().ok())
        .filter(|g| g.is_finite() && *g >= 0.0)
        .with_context(|| format!("`info`'s memory floor line {floor:?} names no GiB figure"))?;
    let memory_floor_bytes = (gib * 1024.0 * 1024.0 * 1024.0).round() as u64;
    Ok(ModelInfo { weights_bytes, memory_floor_bytes })
}

fn expect_schema(doc: &serde_json::Value, what: &str) -> Result<()> {
    match doc.get("schema").and_then(|s| s.as_u64()) {
        Some(1) => Ok(()),
        other => bail!("`{what}` reports schema {other:?}; this darkmux reads schema 1"),
    }
}

/// The installed Tensorfold binary, asked only read-only questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tensorfold {
    bin: String,
}

impl Tensorfold {
    /// The binary `engines.tensorfold.bin` names (`tensorfold-native` on
    /// `PATH` by default), resolved at each call.
    pub fn installed() -> Self {
        Self::at(darkmux_types::config_access::tensorfold_bin())
    }

    pub fn at(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// The binary's file name, for messages that leave this machine (a
    /// configured path under a home directory must not).
    pub fn label(&self) -> &str {
        self.bin.rsplit('/').next().unwrap_or(&self.bin)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        let shown = format!("`{} {}`", self.label(), args.join(" "));
        let run = run_bounded(cmd, "tensorfold probe", Deadline(PROBE_BOUND), StdoutMode::Capture)
            .map_err(|e| anyhow::anyhow!("{shown} failed: {}", e.to_string().replace(&self.bin, self.label())))?;
        if !run.status.success() {
            bail!("{shown} {}", run.exit_detail().replace(&self.bin, self.label()));
        }
        Ok(run.stdout)
    }

    pub fn version(&self) -> Result<Version> {
        parse_version(&self.run(&["--version"])?)
    }

    /// The version, refused unless darkmux was verified against it.
    pub fn verified_version(&self) -> Result<Version> {
        let v = self.version()?;
        if !v.is_verified() {
            bail!(
                "Tensorfold {v} is installed; this darkmux supports Tensorfold {VERIFIED_RELEASES}, the releases it was \
                 verified against. Install a {VERIFIED_RELEASES} release, or a darkmux that supports {v}"
            );
        }
        Ok(v)
    }

    pub fn serve_flags(&self) -> Result<ServeFlags> {
        parse_capabilities(&self.run(&["capabilities", "--json"])?)
    }

    pub fn models(&self) -> Result<Vec<EngineModel>> {
        parse_models(&self.run(&["models", "--json"])?)
    }

    pub fn info(&self, model: &str) -> Result<ModelInfo> {
        parse_info(&self.run(&["info", model])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `capabilities --json` from 1.2.4 on this machine, cut to a few flags.
    const CAPABILITIES: &str = r#"{"schema":1,"engine":"zig","version":"1.2.4","families":{"qwen3_5":["mlx-q4g64"]},
        "serve":{"flags":{"--host":{},"--port":{},"--name":{},"--context":{},"--drafter":{},"--keep-warm":{},
        "--idle-unload":{},"--kv-cache":{"values":["bf16","fp8"]},"--drafter-bits":{"values":["0","4"]}}}}"#;

    /// `models --json` from 1.2.4 on this machine, two rows.
    const MODELS: &str = r#"{"schema":1,"models":[
        {"repo":"TensorFold/Qwen3.8-27B-MLX-4bit","family":"Qwen3.5-2B or Qwen3.8-27B","model_type":"qwen3_5"},
        {"repo":"mlx-community/Qwen3.6-35B-A3B-4bit","family":"Qwen3.5-MoE (Ornith-1.5-122B)","model_type":"qwen3_5_moe"}]}"#;

    /// `info TensorFold/Qwen3.8-27B-MLX-4bit` from 1.2.4 on this machine.
    const INFO: &str = "family: Qwen3.5-2B or Qwen3.8-27B (qwen3_5)\nformat: 4-bit, group 64\n\
        weights: 14.98 GiB (16081519429 bytes)\nmemory floor: 16.98 GiB (weights plus a 2.00 GiB runtime margin)\n";

    #[test]
    fn the_version_line_parses_and_only_1_2_is_verified() {
        let v = parse_version("tensorfold-native 1.2.4\n").unwrap();
        assert_eq!(v, Version { major: 1, minor: 2, patch: 4 });
        assert!(v.is_verified());
        for other in ["tensorfold-native 1.3.0", "tensorfold-native 1.1.9", "tensorfold-native 2.2.0"] {
            assert!(!parse_version(other).unwrap().is_verified(), "{other}");
        }
        for bad in ["", "tensorfold-native", "tensorfold-native 1.2", "tensorfold-native 1.2.x", "tensorfold-native 1.2.4.1"] {
            let err = parse_version(bad).unwrap_err().to_string();
            assert!(err.contains("--version"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn serve_flags_are_read_with_their_value_lists() {
        let flags = parse_capabilities(CAPABILITIES).unwrap();
        assert!(flags.knows("--drafter") && flags.knows("--kv-cache") && !flags.knows("--nope"));
        let other_schema = CAPABILITIES.replacen(r#""schema":1"#, r#""schema":2"#, 1);
        assert!(parse_capabilities(&other_schema).unwrap_err().to_string().contains("schema"));
        assert!(parse_capabilities(r#"{"schema":1}"#).unwrap_err().to_string().contains("serve.flags"));
    }

    /// `engine_args` are checked against the installed engine: an unknown
    /// flag is refused, a listed flag's value must be one it lists (inline
    /// or as the next token), and a refusal never repeats a value.
    #[test]
    fn engine_args_are_checked_against_the_engines_own_flags() {
        let flags = parse_capabilities(CAPABILITIES).unwrap();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(flags.check(&args(&["--drafter", "z/d", "--keep-warm", "--kv-cache", "fp8"])), Ok(()));
        assert_eq!(flags.check(&args(&["--kv-cache=bf16", "--drafter-bits", "4"])), Ok(()));
        let unknown = flags.check(&args(&["--drafter", "z/d", "--turbo"])).unwrap_err();
        assert!(unknown.contains("--turbo"), "{unknown}");
        let bad_value = flags.check(&args(&["--kv-cache", "s3cret"])).unwrap_err();
        assert!(bad_value.contains("--kv-cache") && bad_value.contains("bf16, fp8") && !bad_value.contains("s3cret"), "{bad_value}");
        let bad_inline = flags.check(&args(&["--drafter-bits=8"])).unwrap_err();
        assert!(bad_inline.contains("--drafter-bits") && bad_inline.contains("0, 4"), "{bad_inline}");
        assert_eq!(flags.check(&args(&["--drafter", "fp8"])), Ok(()), "a free-form flag takes any value");
    }

    #[test]
    fn models_are_the_repos_the_engine_lists() {
        let models = parse_models(MODELS).unwrap();
        let repos: Vec<&str> = models.iter().map(|m| m.repo.as_str()).collect();
        assert_eq!(repos, ["TensorFold/Qwen3.8-27B-MLX-4bit", "mlx-community/Qwen3.6-35B-A3B-4bit"]);
        assert!(parse_models(r#"{"schema":1,"models":[{"family":"x"}]}"#).unwrap_err().to_string().contains("repo"));
        assert!(parse_models(r#"{"models":[]}"#).unwrap_err().to_string().contains("schema"));
    }

    #[test]
    fn info_reads_the_weights_and_the_memory_floor() {
        let info = parse_info(INFO).unwrap();
        assert_eq!(info.weights_bytes, 16_081_519_429);
        assert_eq!(info.memory_floor_bytes, (16.98_f64 * 1024.0 * 1024.0 * 1024.0).round() as u64);
        assert!(parse_info("weights: 14.98 GiB\nmemory floor: 16.98 GiB\n").unwrap_err().to_string().contains("byte count"));
        assert!(parse_info("weights: 1 GiB (5 bytes)\n").unwrap_err().to_string().contains("memory floor"));
    }

    /// A probe of a binary that is not there, or not Tensorfold, is an error
    /// naming the binary by file name, never a guess and never a full path.
    #[test]
    fn a_missing_binary_is_an_error_naming_only_its_file_name() {
        let tf = Tensorfold::at("/nonexistent/home/someone/tensorfold-native");
        let err = format!("{:#}", tf.version().unwrap_err());
        assert!(err.contains("tensorfold-native --version") && !err.contains("/home/someone"), "{err}");
        assert_eq!(tf.label(), "tensorfold-native");
    }

    /// A binary that prints an unverified version is refused by name.
    #[test]
    fn an_unverified_release_is_refused_naming_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("tensorfold-native");
        std::fs::write(&fake, "#!/bin/sh\necho 'tensorfold-native 1.3.0'\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tf = Tensorfold::at(fake.to_string_lossy().into_owned());
        assert_eq!(tf.version().unwrap(), Version { major: 1, minor: 3, patch: 0 });
        let err = tf.verified_version().unwrap_err().to_string();
        assert!(err.contains("1.3.0") && err.contains(VERIFIED_RELEASES), "{err}");
    }
}
