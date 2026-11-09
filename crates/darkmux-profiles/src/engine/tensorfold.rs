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
//! is reported rather than hanging `doctor`. `capabilities --json` names
//! the `serve` flags but not which take a value; `serve --help`'s usage line
//! says that, and is read beside it ([`Tensorfold::serve_flags`]).

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

/// One `serve` flag: the values it takes when the engine lists them, and
/// whether it takes a value at all when its usage says (`None`: it does not
/// say, so a following token may be its value or not).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeFlag {
    pub values: Option<Vec<String>>,
    pub takes_value: Option<bool>,
}

/// The flags `tensorfold-native serve` accepts, from `capabilities --json`,
/// each with what it takes ([`ServeFlag`]); arity from `serve --help`
/// ([`ServeFlags::with_usage`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeFlags(BTreeMap<String, ServeFlag>);

/// Where a left-to-right read of `engine_args` stands: the flag the next
/// token is the value of, and whether it must be.
enum Pending<'a> {
    Required(&'a str, &'a ServeFlag),
    Optional,
    /// A switch was just read; a token after it would be a positional.
    Switch(&'a str),
}

impl ServeFlags {
    pub fn knows(&self, flag: &str) -> bool {
        self.0.contains_key(flag)
    }

    /// The arity each flag has in `serve --help`'s usage line (`[--port
    /// PORT]` takes a value, `[--slide]` does not). A flag the usage leaves
    /// out keeps what `capabilities --json` said: a value when it lists
    /// values, else unknown.
    pub fn with_usage(mut self, usage: &str) -> Self {
        for (flag, takes_value) in usage_arity(usage) {
            if let Some(spec) = self.0.get_mut(&flag) {
                spec.takes_value = Some(takes_value || spec.values.is_some());
            }
        }
        self
    }

    /// Why `args` (a profile model's `engine_args`) would not start a server,
    /// or `Ok`: a flag this engine does not accept, a value outside the list
    /// it declares for that flag, a flag missing the value it takes, a value
    /// given to a switch, or a token with no flag before it (the engine would
    /// read it as its model). Never echoes a value (it may be a secret).
    pub fn check(&self, args: &[String]) -> std::result::Result<(), String> {
        let mut pending: Option<Pending<'_>> = None;
        for arg in args {
            pending = match arg.strip_prefix("--") {
                None => {
                    value_after(pending.take(), arg)?;
                    None
                }
                Some(stripped) => {
                    if let Some(Pending::Required(flag, _)) = pending {
                        return Err(missing_value(flag));
                    }
                    self.flag(stripped)?
                }
            };
        }
        match pending {
            Some(Pending::Required(flag, _)) => Err(missing_value(flag)),
            _ => Ok(()),
        }
    }

    /// A `--flag` or `--flag=value` token (`stripped` of its `--`): what the
    /// next token must be, or why this one is refused.
    fn flag(&self, stripped: &str) -> std::result::Result<Option<Pending<'_>>, String> {
        let (flag, inline) = match stripped.split_once('=') {
            Some((f, v)) => (format!("--{f}"), Some(v)),
            None => (format!("--{stripped}"), None),
        };
        let Some((name, spec)) = self.0.get_key_value(&flag) else {
            return Err(format!("`{flag}` is not a flag `tensorfold-native serve` accepts"));
        };
        Ok(match (spec.takes_value.or(spec.values.as_ref().map(|_| true)), inline) {
            (Some(false), Some(_)) => return Err(format!("`{name}` takes no value")),
            (Some(false), None) => Some(Pending::Switch(name)),
            (_, Some(v)) => {
                spec.values.as_deref().map_or(Ok(()), |allowed| one_of(name, allowed, v))?;
                None
            }
            (Some(true), None) => Some(Pending::Required(name, spec)),
            (None, None) => Some(Pending::Optional),
        })
    }
}

/// A token that is not a flag, read after `pending`: the value it takes,
/// or why it is refused.
fn value_after(pending: Option<Pending<'_>>, arg: &str) -> std::result::Result<(), String> {
    match pending {
        Some(Pending::Required(flag, spec)) => spec.values.as_deref().map_or(Ok(()), |v| one_of(flag, v, arg)),
        Some(Pending::Optional) => Ok(()),
        Some(Pending::Switch(flag)) => Err(format!("`{flag}` takes no value, and a token follows it")),
        None => Err("a value with no flag before it; remove it".to_string()),
    }
}

fn missing_value(flag: &str) -> String {
    format!("`{flag}` takes a value, and none follows it")
}

/// Each `--flag` in a usage line and whether a value follows it inside its
/// own `[...]` group (a bracket or `|` ends one): `[--port PORT]` and
/// `[--kv {a,b}]` take one, `[--slide]` and `[--a | --b]` do not.
fn usage_arity(usage: &str) -> Vec<(String, bool)> {
    let spaced = usage.replace('[', " [ ").replace(']', " ] ").replace('|', " | ");
    let tokens: Vec<&str> = spaced.split_whitespace().collect();
    tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.starts_with("--"))
        .map(|(i, t)| {
            let next = tokens.get(i + 1);
            (t.to_string(), next.is_some_and(|n| !n.starts_with('-') && !["[", "]", "|"].contains(n)))
        })
        .collect()
}

/// `value` is one of the values `flag` lists, or the refusal naming them
/// (never the value, which may be a secret).
fn one_of(flag: &str, allowed: &[String], value: &str) -> std::result::Result<(), String> {
    if allowed.iter().any(|a| a == value) {
        Ok(())
    } else {
        Err(format!("`{flag}` takes one of {}", allowed.join(", ")))
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
            (flag.clone(), ServeFlag { values, takes_value: None })
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
            // Its first stderr line only, bounded: doctor's row reaches a
            // remote console viewer, and a path the binary prints is not a
            // configured location the redaction knows to hide.
            let first = run.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
            let first: String = first.replace(&self.bin, self.label()).chars().take(160).collect();
            bail!("{shown} exited with {}: {first}", run.status);
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

    /// The `serve` flags `capabilities --json` lists, with the arity
    /// `serve --help` gives them. A usage that cannot be read leaves the
    /// arity unknown: the flags are still checked, and the server refuses a
    /// missing value itself when it starts.
    pub fn serve_flags(&self) -> Result<ServeFlags> {
        let flags = parse_capabilities(&self.run(&["capabilities", "--json"])?)?;
        Ok(match self.run(&["serve", "--help"]) {
            Ok(usage) => flags.with_usage(&usage),
            Err(_) => flags,
        })
    }

    pub fn models(&self) -> Result<Vec<EngineModel>> {
        parse_models(&self.run(&["models", "--json"])?)
    }

    pub fn info(&self, model: &str) -> Result<ModelInfo> {
        parse_info(&self.run(&["info", model])?)
    }
}

/// What the installed binary says about itself, each answer kept apart so
/// one failed probe does not hide the others. `darkmux doctor`'s Tensorfold
/// row and a profile listing's availability read the same facts, asked the
/// same way.
#[derive(Debug, Clone)]
pub struct TensorfoldProbe {
    pub version: Result<Version, String>,
    pub flags: Result<ServeFlags, String>,
    /// The repos `models --json` lists.
    pub models: Result<Vec<String>, String>,
}

impl TensorfoldProbe {
    /// Ask `tf` its three read-only questions, all at once: each is bounded
    /// by [`PROBE_BOUND`], so the whole probe is too.
    pub fn probe(tf: &Tensorfold) -> Self {
        let msg = |e: anyhow::Error| format!("{e:#}");
        // A caller's cap on its probes (a listing's bound) holds on each
        // probe's own thread too.
        let cap = crate::gestalt_host::lms_host::deadline_cap();
        std::thread::scope(|s| {
            let version = s.spawn(|| crate::gestalt_host::lms_host::with_deadline_cap(cap, || tf.version().map_err(msg)));
            let flags = s.spawn(|| crate::gestalt_host::lms_host::with_deadline_cap(cap, || tf.serve_flags().map_err(msg)));
            let models = s.spawn(|| {
                crate::gestalt_host::lms_host::with_deadline_cap(cap, || {
                    tf.models().map(|ms| ms.into_iter().map(|m| m.repo).collect()).map_err(msg)
                })
            });
            let gone = |what: &str| format!("the `{what}` probe panicked");
            TensorfoldProbe {
                version: version.join().unwrap_or_else(|_| Err(gone("--version"))),
                flags: flags.join().unwrap_or_else(|_| Err(gone("capabilities --json"))),
                models: models.join().unwrap_or_else(|_| Err(gone("models --json"))),
            }
        })
    }

    /// Whether the engine has `model` (`models --json` lists it); `Err` when
    /// it could not list its models.
    pub fn has_model(&self, model: &darkmux_types::ProfileModel) -> std::result::Result<bool, String> {
        match &self.models {
            Ok(have) => Ok(have.iter().any(|r| r == &model.id)),
            Err(e) => Err(format!("could not list its models: {e}")),
        }
    }

    /// Why the engine would not take `model`'s `engine_args` (`None`: it
    /// takes them, or there are none); `Err` when it could not say which
    /// flags it accepts.
    pub fn args_problem(&self, model: &darkmux_types::ProfileModel) -> std::result::Result<Option<String>, String> {
        if model.engine_args.is_empty() {
            return Ok(None);
        }
        match &self.flags {
            Ok(flags) => Ok(flags.check(&model.engine_args).err()),
            Err(e) => Err(format!("could not read its serve flags: {e}")),
        }
    }
}

impl Tensorfold {
    /// The command darkmux runs for `model` on `port`, and the name it serves
    /// the model under: `tensorfold-native serve <id>` with the model's
    /// `engine_args` first and darkmux's own flags after them (loopback,
    /// `port`, the model's `n_ctx` as `--context`, its `darkmux:` name,
    /// `host.docker.internal` allowed as a Host so a dispatch container can
    /// reach it, no update check), so darkmux's win wherever the engine takes
    /// the last of a repeated flag. Only the `engine_args` entries
    /// `ProfileModel::engine_args_problem` names are refused before this is
    /// reached; any other flag darkmux sets here may also appear among them,
    /// ahead of darkmux's own.
    pub fn serve_argv(&self, port: u16, model: &darkmux_types::ProfileModel) -> Result<(Vec<String>, String)> {
        let n_ctx = model.require_n_ctx()?;
        let served_name = crate::ownership::namespaced_identifier(model);
        let mut argv = vec![self.bin.clone(), "serve".to_string(), model.id.clone()];
        argv.extend(model.engine_args.iter().cloned());
        for (flag, value) in [
            ("--host", "127.0.0.1".to_string()),
            ("--port", port.to_string()),
            ("--context", n_ctx.to_string()),
            ("--name", served_name.clone()),
            // A dispatch container dials the server as `host.docker.internal`
            // (`loopback_to_docker_host`), and Tensorfold answers 403 to a
            // Host it does not allow.
            ("--allowed-host", "host.docker.internal".to_string()),
        ] {
            argv.push(flag.to_string());
            argv.push(value);
        }
        argv.push("--no-update-check".to_string());
        Ok((argv, served_name))
    }

    /// The server darkmux runs for `model` on `endpoint`: [`Self::serve_argv`]
    /// and what the record says about it.
    pub fn serve_spec(
        &self,
        endpoint: &str,
        port: u16,
        model: &darkmux_types::ProfileModel,
        version: Version,
    ) -> Result<super::server::ServerSpec> {
        let n_ctx = model.require_n_ctx()?;
        let (argv, served_name) = self.serve_argv(port, model)?;
        Ok(super::server::ServerSpec {
            endpoint: endpoint.to_string(),
            engine: "tensorfold".to_string(),
            model: model.id.clone(),
            n_ctx,
            port,
            served_name,
            argv,
            engine_version: format!("tensorfold-native {version}"),
        })
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
        assert_eq!(flags.check(&args(&["--drafter", "z/d", "--keep-warm", "30", "--kv-cache", "fp8"])), Ok(()));
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

    /// The command darkmux runs: the model's `engine_args` first, darkmux's
    /// own flags after them, served under the model's `darkmux:` name, and
    /// answering a container's `Host: host.docker.internal` (Tensorfold
    /// answers 403 to a Host it does not allow, measured on 1.2.4).
    #[test]
    fn the_serve_command_puts_darkmuxs_flags_after_the_models_engine_args() {
        let model: darkmux_types::ProfileModel = serde_json::from_value(serde_json::json!({
            "id": "TensorFold/Qwen3.8-27B-MLX-4bit", "n_ctx": 262144, "engine_args": ["--drafter", "z/d"]
        }))
        .unwrap();
        let v = Version { major: 1, minor: 2, patch: 4 };
        let spec = Tensorfold::at("/opt/tf/tensorfold-native").serve_spec("tf", 8091, &model, v).unwrap();
        assert_eq!(
            spec.argv,
            [
                "/opt/tf/tensorfold-native", "serve", "TensorFold/Qwen3.8-27B-MLX-4bit", "--drafter", "z/d", "--host",
                "127.0.0.1", "--port", "8091", "--context", "262144", "--name", "darkmux:TensorFold/Qwen3.8-27B-MLX-4bit",
                "--allowed-host", "host.docker.internal", "--no-update-check",
            ]
        );
        assert_eq!(spec.served_name, "darkmux:TensorFold/Qwen3.8-27B-MLX-4bit");
        assert_eq!((spec.port, spec.n_ctx, spec.engine_version.as_str()), (8091, 262_144, "tensorfold-native 1.2.4"));
        let no_ctx: darkmux_types::ProfileModel = serde_json::from_value(serde_json::json!({"id": "m"})).unwrap();
        assert!(Tensorfold::at("t").serve_spec("tf", 8091, &no_ctx, v).is_err(), "n_ctx is the server's --context");
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

    /// `serve --help` from 1.2.5 on this machine, cut to the flags of
    /// [`CAPABILITIES`].
    const USAGE: &str = "usage: tensorfold serve [-h] [--host HOST] [--port PORT] [--name NAME] [--context CONTEXT] \
        [--drafter DRAFTER] [--drafter-bits {0,4}] [--keep-warm SECONDS] [--idle-unload SECONDS] [--slide] model";

    /// A stand-in binary answering `capabilities --json` and `serve --help`.
    fn flags_of_fake(dir: &std::path::Path) -> ServeFlags {
        let fake = dir.join("tensorfold-native");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n  'capabilities --json') echo '{}' ;;\n  'serve --help') echo '{USAGE}' ;;\n  *) exit 9 ;;\nesac\n",
                CAPABILITIES.replace('\n', " ").replace(r#""--keep-warm":{},"#, r#""--keep-warm":{},"--slide":{},"#)
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A freshly written script's first exec can take seconds on a loaded
        // Mac; run it once so the bounded probes measure the probe.
        let _ = std::process::Command::new(&fake).arg("x").output();
        Tensorfold::at(fake.to_string_lossy().into_owned()).serve_flags().unwrap()
    }

    /// The flags the installed binary takes carry their arity from its own
    /// `serve --help` (`capabilities --json` says none): a flag that takes a
    /// value must have one, a switch must not, and nothing is left over as
    /// a stray positional argument the engine would read as its model.
    #[test]
    fn engine_args_are_checked_for_arity_from_the_engines_own_usage() {
        let dir = tempfile::tempdir().unwrap();
        let flags = flags_of_fake(dir.path());
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(flags.check(&args(&["--keep-warm", "30", "--slide", "--drafter=z/d"])), Ok(()));
        for (bad, says) in [
            (&["--keep-warm"][..], "takes a value"),
            (&["--keep-warm", "--slide"][..], "takes a value"),
            (&["--slide", "other/model"][..], "takes no value"),
            (&["--slide=1"][..], "takes no value"),
            (&["other/model"][..], "no flag before it"),
        ] {
            let why = flags.check(&args(bad)).expect_err(&format!("{bad:?} refused"));
            assert!(why.contains(says), "{bad:?}: {why}");
            assert!(!why.contains("other/model"), "a refusal never repeats a value: {why}");
        }
    }
}
