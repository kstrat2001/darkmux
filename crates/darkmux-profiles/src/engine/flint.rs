//! (#3453) rec's Flint, the second engine darkmux starts servers for (DESIGN.md,
//! "Engine servers darkmux starts"), on the shape Tensorfold set (#3448).
//!
//! rec serves one model per server process, so darkmux loads Flint by
//! starting rec's own server, `<runtime>/rec-mlx serve flint`, and unloads it
//! by stopping the one it started. It never runs `rec _supervise`, and never
//! stops or adopts a server rec started itself (`rec serve`, `rec launch`).
//!
//! Every path comes from rec's install, never the profile: rec 0.1.0 keeps its
//! data in `$XDG_DATA_HOME/rec` (else `~/.local/share/rec`), with each runtime
//! in `runtimes/<sha256>/` (its `rec-mlx` a wrapper that `exec`s Python, so the
//! spawned pid is the server) and each model in `models/<hash>/`, beside a
//! manifest `<revision>.json`. A profile model's `id` names the revision
//! (`flint-int4`). That layout is rec's own and undocumented, so darkmux reads
//! it only from the release it was verified against ([`VERIFIED_RELEASE`]) and
//! refuses any other, naming the version.

use super::server::ServerSpec;
use crate::gestalt_host::lms_host::{run_bounded, StdoutMode};
use anyhow::{bail, Context, Result};
use darkmux_gestalt::Deadline;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Bound on one `rec` call (`--version`, `status`), spawn through exit.
pub const PROBE_BOUND: Duration = Duration::from_secs(5);

/// The rec release darkmux was verified against: the install layout above is
/// read only from it.
pub const VERIFIED_RELEASE: &str = "0.1.0";

/// The `managed` token, and the engine an [`crate::engine::server`] record names.
pub const ENGINE: &str = "flint";

/// The model a Flint server answers to. rec has no `--name`, so it is always
/// `flint` and never `darkmux:<id>`: a server is darkmux's by its record (pid
/// and start stamp), not by its name.
pub const SERVED_NAME: &str = "flint";

/// What `rec --version` prints (`rec 0.1.0`) to the version (`0.1.0`).
pub fn parse_version(text: &str) -> Result<String> {
    let line = text.lines().next().unwrap_or_default().trim();
    match line.split_once(' ') {
        Some(("rec", v)) if !v.trim().is_empty() && !v.contains(' ') => Ok(v.trim().to_string()),
        _ => bail!("`rec --version` printed {line:?}, not `rec <version>`"),
    }
}

/// Whether rec's own server is present, from what `rec status` printed (on a
/// zero exit), naming its endpoint: when it says `"ready": true`, or when it
/// names an `endpoint` whose port something listens on (`listening`), a
/// server still loading. rec prints `ready` on every status (its field has no
/// `omitempty`), so a stopped rec says `false`; `ready: false` with nothing
/// on its port is free, as is output that is not a JSON object.
///
/// With its server stopped, rec 0.1.0's `rec status` prints nothing on
/// stdout, says no instance is reachable on stderr and exits 1, which
/// [`Rec::own_server`] reads as no server before this is reached. Accepted
/// window: a server so early in its start that it has not
/// bound its port reads as free, and darkmux may start its own beside it;
/// the memory check still applies to that start.
pub fn parse_status(stdout: &str, listening: &dyn Fn(u16) -> bool) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let obj = doc.as_object()?;
    let endpoint = obj.get("endpoint").and_then(|e| e.as_str());
    let ready = obj.get("ready").and_then(|r| r.as_bool()) == Some(true);
    let bound = endpoint.and_then(endpoint_port).is_some_and(listening);
    (ready || bound).then(|| endpoint.unwrap_or("an endpoint it did not name").to_string())
}

/// (#3453) Whether the server rec reports at `address` is darkmux's own, not
/// rec's: darkmux's recorded server on the endpoint has that port and is what
/// listens there (`server::listens`). A name or a port alone proves nothing.
/// Dispatch refuses `engine_in_use` when it is not; a listing reads
/// `unavailable`.
pub fn own_server_is_darkmuxs(address: &str, darkmuxs: Option<&darkmux_types::engine_record::EngineRecord>) -> bool {
    darkmuxs.is_some_and(|r| endpoint_port(address) == Some(r.port) && super::server::listens(r))
}

/// Why darkmux will not serve Flint while rec's own server at `address` is
/// up: what a dispatch refuses with (`engine_in_use`) and a listing reads.
pub fn own_server_refusal(address: &str) -> String {
    format!(
        "rec's own server is up or starting (at {address}), and rec runs one server per machine; darkmux never stops or \
         takes over a server it did not start. Run `rec stop`, then dispatch again"
    )
}

/// The port of `http://host:port/...`.
pub fn endpoint_port(address: &str) -> Option<u16> {
    let rest = address.split_once("://").map_or(address, |(_, r)| r);
    let authority = rest.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

/// The bytes of a model's weights on disk (its `*.safetensors`): the floor a
/// Flint server needs in memory, or `None` when the directory lists none.
pub fn weights_bytes(model_dir: &Path) -> Option<u64> {
    let total: u64 = std::fs::read_dir(model_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "safetensors"))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum();
    (total > 0).then_some(total)
}

/// What a Flint server takes beyond its weights before its context fills:
/// 2 GB.
///
/// rec says nothing of a model's memory, so this and
/// [`KV_BYTES_PER_CONTEXT_TOKEN`] are measurements standing in for an
/// engine-reported estimate. Measured on rec 0.1.0, 2026-11-10, on a clean
/// 128 GB M5 Max, with the command darkmux starts (`--context-limit
/// 262144`, 22.6 GB of weights), the server's footprint by prompt size:
///
/// | prompt tokens | footprint |
/// |---|---|
/// | none (ready) | 22 GB |
/// | 12,007 | 24 GB |
/// | 48,007 | 24 GB |
/// | 144,007 | 28 GB |
///
/// So Flint grows its KV cache as the context fills (Tensorfold reserves
/// its at start), and the cache is small: 6 GB at 144,007 tokens. The first
/// requests took the footprint from 22 to 24 GB, which is this margin.
pub const START_MARGIN_BYTES: u64 = 2_000_000_000;

/// What a context token grows a Flint server by: 42 KB, the 6 GB between
/// ready and a 144,007-token prompt (see [`START_MARGIN_BYTES`] for the
/// measurement).
pub const KV_BYTES_PER_CONTEXT_TOKEN: u64 = 42_000;

/// What a Flint start is priced at: its weights, [`START_MARGIN_BYTES`],
/// and the cache its whole context can grow to
/// ([`KV_BYTES_PER_CONTEXT_TOKEN`] at `n_ctx`). Flint commits only the
/// first two at start, but the price is what the reservation and the fit
/// are judged on, and a server priced at its weights alone is admitted
/// beside memory its context then grows into. Every measured point is
/// priced at or above what it took.
pub fn conservative_commitment(weights_bytes: u64, n_ctx: u32) -> u64 {
    weights_bytes.saturating_add(START_MARGIN_BYTES).saturating_add(KV_BYTES_PER_CONTEXT_TOKEN.saturating_mul(u64::from(n_ctx)))
}

/// rec's install: its command and its data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rec {
    bin: String,
    /// `<XDG data home>/rec`; `None` when this machine has no such place.
    data: Option<PathBuf>,
}

/// How rec's data directory is named in a message: never the full path,
/// which may leave the machine in a doctor row.
const DATA_DIR: &str = "rec's data directory ($XDG_DATA_HOME/rec, else ~/.local/share/rec)";

impl Rec {
    /// The `rec` command `engines.flint.bin` names (`rec` on `PATH` by
    /// default) and the data directory the platform's XDG layout gives.
    pub fn installed() -> Self {
        let data = darkmux_platform::current().xdg_data_home().map(|d| d.join("rec"));
        Self::at(darkmux_types::config_access::flint_bin(), data)
    }

    pub fn at(bin: impl Into<String>, data: Option<PathBuf>) -> Self {
        Self { bin: bin.into(), data }
    }

    /// The command's file name, for messages that leave this machine.
    pub fn label(&self) -> &str {
        self.bin.rsplit('/').next().unwrap_or(&self.bin)
    }

    fn call(&self, args: &[&str]) -> Result<crate::gestalt_host::lms_host::BoundedRun> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args);
        let shown = format!("`{} {}`", self.label(), args.join(" "));
        run_bounded(cmd, "rec probe", Deadline(PROBE_BOUND), StdoutMode::Capture)
            .map_err(|e| anyhow::anyhow!("{shown} failed: {}", e.to_string().replace(&self.bin, self.label())))
    }

    pub fn version(&self) -> Result<String> {
        let run = self.call(&["--version"])?;
        if !run.status.success() {
            bail!("`{} --version` exited with {}", self.label(), run.status);
        }
        parse_version(&run.stdout)
    }

    /// The version, refused unless it is the one darkmux was verified
    /// against: the install layout darkmux reads is rec's own, undocumented.
    pub fn verified_version(&self) -> Result<String> {
        let v = self.version()?;
        if v != VERIFIED_RELEASE {
            bail!(
                "rec {v} is installed; this darkmux supports rec {VERIFIED_RELEASE}, the release whose install layout it \
                 was verified against. Install rec {VERIFIED_RELEASE}, or a darkmux that supports rec {v}"
            );
        }
        Ok(v)
    }

    /// The endpoint rec's own server serves, when it is up or starting
    /// (`rec status`, [`parse_status`]). A non-zero exit reads as no server.
    /// An error only when `rec status` could not be run at all.
    pub fn own_server(&self) -> Result<Option<String>> {
        let run = self.call(&["status"])?;
        let host = darkmux_platform::current();
        // A host that cannot say who listens counts as someone listening.
        let listening = |port: u16| host.tcp_listeners(port).is_none_or(|pids| !pids.is_empty());
        Ok(if run.status.success() { parse_status(&run.stdout, &listening) } else { None })
    }

    fn data(&self) -> Result<&Path> {
        self.data.as_deref().with_context(|| format!("this machine has no home directory for {DATA_DIR}"))
    }

    /// The runtime's `rec-mlx`: the one `runtimes/<sha256>/rec-mlx` the
    /// install holds. None, or several, is refused naming what was found.
    pub fn runtime(&self) -> Result<PathBuf> {
        let runtimes = self.data()?.join("runtimes");
        let found: Vec<PathBuf> = subdirs(&runtimes).into_iter().map(|d| d.join("rec-mlx")).filter(|p| p.is_file()).collect();
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!("{DATA_DIR} holds no runtime (`runtimes/<sha256>/rec-mlx`); run `rec pull` to install one"),
            many => bail!(
                "{DATA_DIR} holds {} runtimes ({}), and darkmux runs one only when there is exactly one; remove the \
                 ones rec no longer uses, or reinstall with `rec pull`",
                many.len(),
                many.iter().map(|p| short(p, &runtimes)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The model directory holding `<revision>.json`: exactly one under
    /// `models/`. None, or several, is refused naming what was found.
    pub fn model_dir(&self, revision: &str) -> Result<PathBuf> {
        if revision.is_empty() || revision.contains('/') || revision.contains('\\') || revision.starts_with('.') {
            bail!("the profile model's `id` `{revision}` is not a rec revision (a manifest name such as `flint-int4`)");
        }
        let models = self.data()?.join("models");
        let dirs = subdirs(&models);
        let manifest = format!("{revision}.json");
        let found: Vec<&PathBuf> = dirs.iter().filter(|d| d.join(&manifest).is_file()).collect();
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] => {
                let have: Vec<String> = dirs.iter().flat_map(|d| manifests(d)).collect();
                let have = if have.is_empty() { "none".to_string() } else { have.join(", ") };
                bail!(
                    "{DATA_DIR} has no model whose manifest is `{manifest}` (manifests found: {have}); run `rec pull` \
                     to download it, or set the profile model's `id` to a revision rec has"
                )
            }
            many => bail!(
                "{DATA_DIR} has {} model directories with `{manifest}` ({}), and darkmux serves one only when there is \
                 exactly one; remove the stale copies, or reinstall with `rec pull`",
                many.len(),
                many.iter().map(|p| short(p, &models)).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// The server darkmux runs for `model` on `endpoint`: rec's
    /// `<runtime>/rec-mlx serve flint` with the model's `engine_args` first and
    /// darkmux's own flags after them (the model directory and its manifest,
    /// the model's `n_ctx` as `--context-limit`, the endpoint's `port`). No
    /// `--api-key-file`: the server is loopback only, as Tensorfold's is. The
    /// flags darkmux sets are refused in `engine_args` before this is reached
    /// (`ProfileModel::engine_args_problem`).
    pub fn serve_spec(&self, endpoint: &str, port: u16, model: &darkmux_types::ProfileModel, version: &str) -> Result<ServerSpec> {
        let n_ctx = model.require_n_ctx()?;
        let runtime = self.runtime()?;
        let dir = self.model_dir(&model.id)?;
        let manifest = dir.join(format!("{}.json", model.id));
        let mut argv = vec![runtime.to_string_lossy().into_owned(), "serve".to_string(), "flint".to_string()];
        argv.extend(model.engine_args.iter().cloned());
        for (flag, value) in [
            ("--model-dir", dir.to_string_lossy().into_owned()),
            ("--manifest", manifest.to_string_lossy().into_owned()),
            ("--context-limit", n_ctx.to_string()),
            ("--port", port.to_string()),
        ] {
            argv.push(flag.to_string());
            argv.push(value);
        }
        Ok(ServerSpec {
            endpoint: endpoint.to_string(),
            engine: ENGINE.to_string(),
            model: model.id.clone(),
            n_ctx,
            port,
            served_name: SERVED_NAME.to_string(),
            argv,
            engine_version: format!("rec {version}"),
        })
    }
}

/// The directories directly under `dir`, sorted; none when it is missing.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> =
        std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
    out.sort();
    out
}

/// The `*.json` manifests directly in a model directory, by file name.
fn manifests(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.ends_with(".json") && n != "config.json" && n != "generation_config.json")
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// `path` relative to `root`, for a message.
fn short(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// rec 0.1.0's layout under a temp data directory: one runtime and one
    /// model `flint-int4`, with a `rec` that prints `version` and `status`.
    pub(crate) fn fake_install(root: &Path, version: &str, status: &str) -> Rec {
        let data = root.join("share").join("rec");
        let runtime = data.join("runtimes").join("0906aa");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("rec-mlx"), "#!/bin/sh\nexit 0\n").unwrap();
        let model = data.join("models").join("fdc852");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("flint-int4.json"), "{}").unwrap();
        std::fs::write(model.join("config.json"), "{}").unwrap();
        std::fs::write(model.join("model-00000.safetensors"), vec![0u8; 1000]).unwrap();
        std::fs::write(model.join("model-00001.safetensors"), vec![0u8; 24]).unwrap();
        let bin = root.join("rec");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncase \"$1\" in\n  --version) echo 'rec {version}' ;;\n  status) echo '{status}' ;;\nesac\n"),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        Rec::at(bin.to_string_lossy().into_owned(), Some(data))
    }

    const RUNNING: &str = r#"{"engine":"mlx","ready":true,"version":"0.1.0","model":"flint","revision":"flint-int4","endpoint":"http://127.0.0.1:11500/v1","context":262144}"#;

    fn model(id: &str, args: &[&str]) -> darkmux_types::ProfileModel {
        serde_json::from_value(serde_json::json!({"id": id, "n_ctx": 262144, "engine_args": args})).unwrap()
    }

    /// Only rec 0.1.0 is verified; another release is refused naming both.
    #[test]
    fn only_rec_0_1_0_is_verified_and_another_is_refused_by_name() {
        assert_eq!(parse_version("rec 0.1.0\n").unwrap(), "0.1.0");
        for bad in ["", "rec", "tensorfold-native 1.2.4", "rec 0.1.0 extra"] {
            assert!(parse_version(bad).unwrap_err().to_string().contains("rec --version"), "{bad:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fake_install(dir.path(), "0.1.0", "").verified_version().unwrap(), "0.1.0");
        let dir = tempfile::tempdir().unwrap();
        let err = fake_install(dir.path(), "0.2.0", "").verified_version().unwrap_err().to_string();
        assert!(err.contains("rec 0.2.0 is installed") && err.contains("rec 0.1.0"), "{err}");
    }

    /// (#3453 re-review) rec's server is present when `rec status` says
    /// `ready: true`, or names an endpoint whose port something listens on
    /// (a server still loading). `ready: false` with nothing on its port is
    /// free (rec prints `ready` on every status, so a stopped rec says
    /// `false`); output that is not a JSON object is free.
    #[test]
    fn rec_status_is_ready_true_or_a_listener_on_its_endpoint() {
        let listening = |port: u16| port == 11500;
        let nobody = |_: u16| false;
        let parse = |out: &str, l: &dyn Fn(u16) -> bool| parse_status(out, l);
        assert_eq!(parse(RUNNING, &nobody).as_deref(), Some("http://127.0.0.1:11500/v1"), "ready:true, whatever listens");
        assert_eq!(parse(r#"{"ready":true}"#, &nobody).as_deref(), Some("an endpoint it did not name"));
        let not_ready = r#"{"ready":false,"endpoint":"http://127.0.0.1:11500/v1"}"#;
        assert_eq!(parse(not_ready, &listening).as_deref(), Some("http://127.0.0.1:11500/v1"), "starting: its port is bound");
        assert_eq!(parse(not_ready, &nobody), None, "stopped: nothing on its port");
        assert_eq!(parse(r#"{"ready":false}"#, &listening), None, "no endpoint, not ready");
        for free in ["", "rec is not running", "[]", "\"stopped\""] {
            assert_eq!(parse(free, &listening), None, "{free:?}");
        }
        assert_eq!(endpoint_port("http://127.0.0.1:11500/v1"), Some(11500));
        assert_eq!(endpoint_port("http://127.0.0.1/v1"), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fake_install(dir.path(), "0.1.0", RUNNING).own_server().unwrap().as_deref(), Some("http://127.0.0.1:11500/v1"));
        // A non-zero exit is no server, whatever it printed.
        let dir = tempfile::tempdir().unwrap();
        let failing = dir.path().join("rec-failing");
        std::fs::write(&failing, format!("#!/bin/sh\necho '{RUNNING}'\nexit 1\n")).unwrap();
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(Rec::at(failing.to_string_lossy().into_owned(), None).own_server().unwrap(), None);
    }

    /// The runtime and model directory come from rec's install: exactly one
    /// of each, and none or several is refused naming what was found and
    /// suggesting `rec pull`.
    #[test]
    fn the_runtime_and_model_dir_are_the_one_rec_installed() {
        let dir = tempfile::tempdir().unwrap();
        let rec = fake_install(dir.path(), "0.1.0", "");
        let data = dir.path().join("share").join("rec");
        assert_eq!(rec.runtime().unwrap(), data.join("runtimes/0906aa/rec-mlx"));
        assert_eq!(rec.model_dir("flint-int4").unwrap(), data.join("models/fdc852"));

        let missing = rec.model_dir("flint-int8").unwrap_err().to_string();
        assert!(missing.contains("`flint-int8.json`") && missing.contains("flint-int4.json") && missing.contains("rec pull"), "{missing}");
        assert!(!missing.contains(&*dir.path().to_string_lossy()), "no full path in a message: {missing}");
        for bad in ["../x", "a/b", ""] {
            assert!(rec.model_dir(bad).unwrap_err().to_string().contains("not a rec revision"), "{bad:?}");
        }

        let copy = data.join("models").join("aa11");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("flint-int4.json"), "{}").unwrap();
        let two = rec.model_dir("flint-int4").unwrap_err().to_string();
        assert!(two.contains("2 model directories") && two.contains("aa11") && two.contains("fdc852"), "{two}");

        let other = data.join("runtimes").join("77bb");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("rec-mlx"), "").unwrap();
        let runtimes = rec.runtime().unwrap_err().to_string();
        assert!(runtimes.contains("2 runtimes") && runtimes.contains("0906aa") && runtimes.contains("77bb"), "{runtimes}");

        let empty = Rec::at("rec", Some(dir.path().join("nothing")));
        let none = empty.runtime().unwrap_err().to_string();
        assert!(none.contains("no runtime") && none.contains("rec pull"), "{none}");
        assert!(empty.model_dir("flint-int4").unwrap_err().to_string().contains("manifests found: none"));
        assert!(Rec::at("rec", None).runtime().unwrap_err().to_string().contains("no home directory"));
    }

    /// The command darkmux runs: rec's own `rec-mlx serve flint`, the
    /// model's `engine_args` first and darkmux's flags after, no API key,
    /// and served under `flint`, never a `darkmux:` name.
    #[test]
    fn the_serve_command_is_recs_runtime_with_darkmuxs_flags_after_engine_args() {
        let dir = tempfile::tempdir().unwrap();
        let rec = fake_install(dir.path(), "0.1.0", "");
        let data = dir.path().join("share").join("rec");
        let spec = rec.serve_spec("fl", 8092, &model("flint-int4", &["--grammar", "flint"]), "0.1.0").unwrap();
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        assert_eq!(
            spec.argv,
            [
                s(data.join("runtimes/0906aa/rec-mlx")),
                "serve".into(),
                "flint".into(),
                "--grammar".into(),
                "flint".into(),
                "--model-dir".into(),
                s(data.join("models/fdc852")),
                "--manifest".into(),
                s(data.join("models/fdc852/flint-int4.json")),
                "--context-limit".into(),
                "262144".into(),
                "--port".into(),
                "8092".into(),
            ]
        );
        assert!(!spec.argv.iter().any(|a| a.contains("api-key")), "loopback, no key");
        assert_eq!(spec.served_name, "flint");
        assert_eq!((spec.engine.as_str(), spec.engine_version.as_str(), spec.port), ("flint", "rec 0.1.0", 8092));
        let no_ctx: darkmux_types::ProfileModel = serde_json::from_value(serde_json::json!({"id": "flint-int4"})).unwrap();
        assert!(rec.serve_spec("fl", 8092, &no_ctx, "0.1.0").is_err(), "n_ctx is the server's --context-limit");
        assert_eq!(weights_bytes(&data.join("models/fdc852")), Some(1024), "the safetensors only");
        assert_eq!(weights_bytes(&data.join("runtimes")), None);
        // Priced at or over every measured footprint of a 22.6 GB model,
        // whether the context is the prompt's size or the whole 262,144.
        for (tokens, measured_gb) in [(12_007u32, 24u64), (48_007, 24), (144_007, 28)] {
            for n_ctx in [tokens, 262_144] {
                let priced = conservative_commitment(22_600_000_000, n_ctx);
                assert!(priced >= measured_gb * 1_000_000_000, "{tokens} tokens at context {n_ctx}: {priced}");
            }
        }
        assert!(conservative_commitment(22_600_000_000, 262_144) < 36_000_000_000, "and not far over");
    }
}
