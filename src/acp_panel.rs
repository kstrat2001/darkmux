//! The generic `/mission` verbs of the ACP editor panel, plus the ephemeral
//! runner for procedural-only configs.
//!
//! The panel advertises ONE slash command, `/mission`, with three verbs:
//!
//! - `/mission list` lists every config `darkmux mission launch` can start
//!   (the `mission config list` derivation, [`crate::mission_config_cli::build_list`]).
//! - `/mission launch <config> [name=value ...] [free text]` launches one.
//! - `/mission show <id>` renders one mission, from the SAME derivation
//!   `darkmux mission show <id>` prints ([`crate::mission_show`]).
//!
//! No config names itself into the panel. Every config in the merged
//! registry (`~/.darkmux/mission-configs/` over the built-ins) is launchable
//! from the panel exactly when it is launchable from the CLI.
//!
//! **One launch code path.** [`plan_launch`] resolves the config through
//! [`crate::mission_launch::resolve_config`] (the CLI's own load and its
//! refusal text) and maps the words after the config id onto the config's
//! declared inputs: a `name=value` token whose name the config declares is
//! a `--param`, exactly what `darkmux mission launch <id> --param name=value`
//! takes ([`crate::mission_launch::collect_inputs`] parses them); any other
//! text goes to the config's `__panel_args__` reader as `--param args=...`,
//! and is refused when the config has none.
//!
//! - A config whose graph contains ZERO model-dispatching steps (every step
//!   kind is `procedural.*`) runs EPHEMERAL: [`run_ephemeral`] interprets the
//!   graph and drives it through `darkmux_crew::scheduler::run_step_graph`
//!   in-process, with NO mission instance minted, so the ACP
//!   `session/request_permission` handler can approve an operator-gated step.
//!   It resolves inputs through [`crate::mission_launch::resolve_inputs`], the
//!   function `mission launch` calls.
//! - Anything else launches as a `darkmux mission launch <id>` subprocess
//!   (`acp.rs`'s `run_launch_command`): the CLI's own launcher, not a copy.
//!
//! A panel invocation types no diff, so [`prepare_launch`] fills a required
//! `diff_file` (plus `workspace` and `head_sha`) from the session's cwd when
//! the operator did not pass one. The trigger is the config's declared
//! inputs, never its name. `darkmux mission launch` itself does not
//! synthesize: a terminal user names the diff.
//!
//! This module owns the panel's invocation grammar, its launch planning, and
//! the ephemeral runner. `acp.rs` owns the ACP wire-protocol plumbing.

use crate::crew::mission_config::{self, LaunchParams, MissionConfig};
use crate::crew::scheduler::SchedulerReport;
use crate::crew::step_kinds::{FixedEstimator, StepKindRegistry};
use crate::crew::types::{NodeStatus, Step, Task};
use darkmux_flow::payload::{GhVerbExecutedPayload, RunPayload};
use darkmux_types::session_id::{RunId, SessionId};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The one slash command the panel advertises.
pub const MISSION_COMMAND: &str = "mission";

/// The input hint the editor shows after `/mission`.
pub const MISSION_HINT: &str = "list | launch <config> [name=value ...] | show <id>";

/// The command-palette description of `/mission`.
pub const MISSION_DESCRIPTION: &str =
    "List the launchable mission configs, launch one, or show a mission's phases, runs and tokens";

/// The usage line refusals end with.
const MISSION_USAGE: &str = "Use `/mission list`, `/mission launch <config> [name=value ...]` or `/mission show <id>`.";

/// One config `/mission list` offers and radio's router may choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchableConfig {
    /// The registry-resolvable id: what `mission_config::list_ids` returned
    /// and `mission_config::load` resolves, never the document body's own
    /// `id` field (those can differ on a hand-edited config).
    pub id: String,
    /// One line saying what the config does: the first sentence of its
    /// `description`, else its `name`.
    pub summary: String,
    /// Whether the config takes free text after its id
    /// ([`mission_config::takes_panel_args`]).
    pub accepts_args: bool,
}

/// The longest summary line, in characters.
const SUMMARY_CAP_CHARS: usize = 160;

/// One line describing `config`: the first sentence of its `description`
/// (capped), else its `name`. Long-form description prose stays in
/// `mission config show`.
pub fn config_summary(config: &MissionConfig) -> String {
    let description = config.description.as_deref().map(str::trim).unwrap_or("");
    if description.is_empty() {
        return config.name.clone();
    }
    crate::mission_status::cap_note(crate::mission_status::first_sentence(description), SUMMARY_CAP_CHARS)
}

/// Every config `darkmux mission launch` can start, sorted by id: the
/// `mission config list` rows that load AND whose id `mission launch` accepts
/// ([`crate::fleet::validate_identifier`], which refuses an id with an
/// uppercase letter). A config that cannot be launched is skipped with a
/// stderr note, so it is never offered and one broken override never hides
/// the rest.
///
/// `Err` is the refusal every launch runs first
/// ([`crate::mission_launch::preflight_launch`], the SAME call): one stale
/// user-tier file blocks every launch, so nothing is offered and the
/// refusal text is the answer.
pub fn list_launchable() -> Result<Vec<LaunchableConfig>> {
    crate::mission_launch::preflight_launch()?;
    let mut out = Vec::new();
    for row in crate::mission_config_cli::build_list() {
        if let Some(err) = &row.error {
            eprintln!("[darkmux-acp] skipping mission config \"{}\" while listing: {err}", row.id);
            continue;
        }
        if let Err(e) = crate::fleet::validate_identifier("config_id", &row.id) {
            eprintln!("[darkmux-acp] skipping mission config \"{}\" while listing: {e:#}", row.id);
            continue;
        }
        match mission_config::load(&row.id) {
            Ok(loaded) => out.push(LaunchableConfig {
                summary: config_summary(&loaded.config),
                accepts_args: mission_config::takes_panel_args(&loaded.config),
                id: row.id,
            }),
            Err(e) => eprintln!("[darkmux-acp] skipping mission config \"{}\" while listing: {e:#}", row.id),
        }
    }
    Ok(out)
}

/// What the panel does with `/mission`'s verb word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionVerb {
    /// `/mission list`.
    List,
    /// `/mission launch <config> [rest]`: `rest` is the raw text after the id.
    Launch { config_id: String, rest: String },
    /// `/mission show <id>`.
    Show { id: String },
}

/// A parsed slash invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelInvocation {
    /// A well-formed `/mission ...`.
    Mission(MissionVerb),
    /// Text starting with `/` that is not a well-formed `/mission ...`: the
    /// reply to send back, naming what is available.
    Refused(String),
}

/// Split `text` into its first whitespace-delimited word and the trimmed rest.
fn split_first_word(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (text, ""),
    }
}

/// Parse the text after `/mission` into a verb. The verb word is matched
/// case-insensitively; config and mission ids keep their case.
pub fn parse_mission_verb(args: &str) -> std::result::Result<MissionVerb, String> {
    let (verb, rest) = split_first_word(args);
    let (id, extra) = split_first_word(rest);
    match verb.to_ascii_lowercase().as_str() {
        "list" if rest.is_empty() => Ok(MissionVerb::List),
        "launch" if !id.is_empty() => Ok(MissionVerb::Launch { config_id: id.to_string(), rest: extra.to_string() }),
        "show" if !id.is_empty() && extra.is_empty() => Ok(MissionVerb::Show { id: id.to_string() }),
        "list" => Err(format!("`/mission list` takes no arguments. {MISSION_USAGE}")),
        "launch" => Err(format!("`/mission launch` needs a config id; `/mission list` shows them. {MISSION_USAGE}")),
        "show" => Err(format!("`/mission show` takes exactly one mission id. {MISSION_USAGE}")),
        "" => Err(format!("`/mission` needs a verb. {MISSION_USAGE}")),
        other => Err(format!("`/mission {other}` is not a verb. {MISSION_USAGE}")),
    }
}

/// Parse a prompt into an invocation. `None` unless the first non-whitespace
/// character is a literal `/` (see [`parse_command`]).
pub fn parse_invocation(text: &str) -> Option<PanelInvocation> {
    let (name, args) = parse_command(text)?;
    if name != MISSION_COMMAND {
        return Some(PanelInvocation::Refused(not_a_command_message()));
    }
    Some(match parse_mission_verb(&args) {
        Ok(verb) => PanelInvocation::Mission(verb),
        Err(reply) => PanelInvocation::Refused(reply),
    })
}

/// The reply to a slash command the panel does not have.
pub fn not_a_command_message() -> String {
    format!("darkmux acp doesn't recognize that as a command. {}", command_listing())
}

/// The bare "Available commands: ..." line, without the didn't-recognize
/// preamble: the answering seat appends it after an answer that names a
/// command, where the full refusal would contradict itself.
pub fn command_listing() -> String {
    format!("Available commands: `/{MISSION_COMMAND}`. {MISSION_USAGE}")
}

/// `/mission list`, rendered from [`list_launchable`]: the refusal text when
/// launches are blocked, else one line per launchable config.
pub fn mission_list_text() -> String {
    match list_launchable() {
        Ok(configs) => render_mission_list(&configs),
        Err(refusal) => format!("No mission config can be launched right now.\n{refusal:#}"),
    }
}

/// One line per launchable config.
pub fn render_mission_list(configs: &[LaunchableConfig]) -> String {
    if configs.is_empty() {
        return "No mission configs are launchable (built-ins and ~/.darkmux/mission-configs/ are both empty).".to_string();
    }
    let lines: Vec<String> = configs.iter().map(|c| format!("- `{}`: {}", c.id, c.summary)).collect();
    format!("Launchable mission configs (`/mission launch <config>`):\n{}", lines.join("\n"))
}

/// Split a prompt into `(command name, raw args)`: **the mode bit** (issue
/// #1698, "the slash becomes the mode bit"). Matches ONLY when the first
/// non-whitespace character is a literal `/`; anything else, including
/// empty text, is `None`. A slash-less prompt is never a command: it goes to
/// the radio channel, which classifies it.
///
/// The first whitespace-delimited word after the slash, LOWERCASED, is the
/// command name; everything after it (trimmed, case PRESERVED) is the raw
/// args string.
pub fn parse_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let without_slash = trimmed.strip_prefix('/')?;
    if without_slash.is_empty() {
        return None;
    }
    let mut parts = without_slash.splitn(2, char::is_whitespace);
    let name = parts.next()?.to_ascii_lowercase();
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((name, args))
}

/// How a launch runs: see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRoute {
    /// The graph has no model-dispatching step: run in-process via
    /// [`run_ephemeral`], no mission instance minted.
    Ephemeral,
    /// The graph has at least one model-seated step: launch it as a
    /// `darkmux mission launch <id>` subprocess.
    Launch,
}

/// A resolved `/mission launch`: the config, how it runs, and its `--param`
/// values.
pub struct LaunchPlan {
    /// The registry-resolvable id the operator typed.
    pub config_id: String,
    pub config: MissionConfig,
    pub route: LaunchRoute,
    /// `name=value` strings, the `--param` values `mission launch` takes.
    pub params: Vec<String>,
    /// The raw text the operator typed after the config id, for the shelf.
    pub raw_args: String,
}

/// `true` iff the config declares an input `name`.
fn is_declared_input(config: &MissionConfig, name: &str) -> bool {
    config.inputs.iter().any(|i| i.name == name)
}

/// The refusal for free text sent to a config with nowhere to put it.
fn no_free_text_refusal(config: &MissionConfig, text: &str) -> String {
    let declared: Vec<&str> =
        config.inputs.iter().filter(|i| !i.is_launcher_supplied()).map(|i| i.name.as_str()).collect();
    if declared.is_empty() {
        return format!("`{}` takes no arguments, so `{text}` was not passed on. Run `/mission launch {}` on its own.", config.id, config.id);
    }
    format!(
        "`{}` takes no free text, so `{text}` was not passed on. Its inputs are {}: pass them as `name=value`.",
        config.id,
        declared.join(", ")
    )
}

/// One word of the text after a config id.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Word {
    /// `name=value` for a declared input, quotes around the value removed.
    Param(String),
    /// Anything else, verbatim.
    Free(String),
}

/// Split `rest` on whitespace into [`Word`]s. A declared input's value may be
/// quoted, `name="two words"` or `name='two words'`, so it can hold spaces;
/// the quotes are removed and the closing quote ends the quoted part. Quotes
/// anywhere else (an apostrophe in free text) are ordinary characters. There
/// are no escapes, so a backslash right before the closing quote is refused
/// rather than guessed at. `Err` names a declared input whose quote never
/// closes or that ends in such a backslash.
fn split_words(config: &MissionConfig, rest: &str) -> Result<Vec<Word>> {
    let mut words = Vec::new();
    let mut tail = rest.trim_start();
    while !tail.is_empty() {
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let token = &tail[..end];
        let quoted = token
            .split_once('=')
            .filter(|(name, value)| is_declared_input(config, name) && value.starts_with(['"', '\'']));
        let Some((name, value)) = quoted else {
            let word = match token.split_once('=') {
                Some((name, _)) if is_declared_input(config, name) => Word::Param(token.to_string()),
                _ => Word::Free(token.to_string()),
            };
            words.push(word);
            tail = tail[end..].trim_start();
            continue;
        };
        let quote = value.chars().next().unwrap_or('"');
        let value_start = name.len() + 2;
        let Some(close) = tail[value_start..].find(quote) else {
            bail!("`{name}=` opens a {quote} quote that never closes, so `{}` was not launched. Close the quote after the value.", config.id);
        };
        let quoted_value = &tail[value_start..value_start + close];
        if quoted_value.ends_with('\\') {
            bail!("`{name}=` has a backslash before a {quote} quote, but there are no escapes: the quote closes the value there, so `{}` was not launched. Use the other kind of quote around a value that holds this one.", config.id);
        }
        let after = value_start + close + 1;
        let suffix_end = tail[after..].find(char::is_whitespace).map_or(tail.len(), |i| after + i);
        words.push(Word::Param(format!("{name}={quoted_value}{}", &tail[after..suffix_end])));
        tail = tail[suffix_end..].trim_start();
    }
    Ok(words)
}

/// Map the words after a config id onto its declared inputs, the way
/// `mission launch --param` receives them (see the module doc).
pub fn map_launch_args(config: &MissionConfig, rest: &str) -> Result<Vec<String>> {
    let mut params = Vec::new();
    let mut free = Vec::new();
    for word in split_words(config, rest)? {
        match word {
            Word::Param(p) => params.push(p),
            Word::Free(w) => free.push(w),
        }
    }
    if free.is_empty() {
        return Ok(params);
    }
    let text = free.join(" ");
    if !mission_config::takes_panel_args(config) {
        bail!("{}", no_free_text_refusal(config, &text));
    }
    params.push(format!("args={text}"));
    Ok(params)
}

/// Resolve `/mission launch <config_id> <rest>` into a [`LaunchPlan`]. `Err`
/// carries the refusal text: the CLI's own for an unknown config
/// ([`crate::mission_launch::resolve_config`]), or this module's for
/// arguments the config cannot take.
pub fn plan_launch(config_id: &str, rest: &str) -> Result<LaunchPlan> {
    let loaded = crate::mission_launch::resolve_config(config_id)?;
    let params = map_launch_args(&loaded.config, rest)?;
    refuse_control_chars(config_id, &params)?;
    let route = if is_procedural_only(&loaded.config) { LaunchRoute::Ephemeral } else { LaunchRoute::Launch };
    Ok(LaunchPlan { config_id: config_id.to_string(), config: loaded.config, route, params, raw_args: rest.to_string() })
}

/// Whether `c` is an invisible Unicode format character (category Cf), which
/// `char::is_control` does not cover: bidi overrides and isolates, zero-width
/// characters, the byte-order mark, and the like. Any of them can make a
/// printed command read differently from what it holds.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
    )
}

/// Refuse a param whose value holds a control character or an invisible
/// format character ([`is_format_char`]): an escape sequence from a router's
/// output would otherwise reach the terminal or the dialog as one, and a
/// bidi override would reorder the command the user is asked to confirm.
/// `Err` names the input.
fn refuse_control_chars(config_id: &str, params: &[String]) -> Result<()> {
    for param in params {
        let (name, value) = param.split_once('=').unwrap_or((param.as_str(), ""));
        if value.chars().any(|c| c.is_control() || is_format_char(c)) {
            bail!("the value for `{name}` holds a control character or an invisible formatting character, so `{config_id}` was not launched. Retype the input without it.");
        }
    }
    Ok(())
}

/// A launch's params after the panel filled in what it could, holding the
/// tempdir of any synthesized input until it is dropped.
pub struct PreparedLaunch {
    pub params: Vec<String>,
    /// Non-empty when the working tree carries changes a synthesized diff
    /// does not cover.
    pub note: Option<String>,
    synth: Option<SynthesizedInputs>,
}

impl PreparedLaunch {
    /// The names of the inputs [`prepare_launch`] synthesized from the cwd,
    /// empty when the operator supplied their own or none were needed.
    pub fn synthesized_keys(&self) -> Vec<&str> {
        let Some(synth) = &self.synth else { return Vec::new() };
        synth.params().iter().filter_map(|p| p.split_once('=').map(|(k, _)| k)).collect()
    }
}

/// What [`prepare_launch`] decided.
pub enum Prepared {
    Ready(PreparedLaunch),
    /// A git repo with nothing committed to review: the message to show
    /// instead of launching.
    Nothing(String),
}

/// The inputs [`prepare_launch`] synthesizes from the cwd. The operator
/// supplying any one of them means it synthesizes none.
const SYNTHESIZED_INPUTS: [&str; 3] = ["diff_file", "workspace", "head_sha"];

/// `true` iff the operator's params already name `key`.
fn supplies(params: &[String], key: &str) -> bool {
    params.iter().any(|p| p.split_once('=').is_some_and(|(k, _)| k == key))
}

/// Add the inputs a panel invocation cannot type (see the module doc).
/// `Err` only for genuine IO failures and a cwd that is not a git repo.
pub fn prepare_launch(config: &MissionConfig, mut params: Vec<String>, cwd: &Path) -> Result<Prepared> {
    if SYNTHESIZED_INPUTS.iter().any(|key| supplies(&params, key)) {
        return Ok(Prepared::Ready(PreparedLaunch { params, note: None, synth: None }));
    }
    match synthesize_diff_launch_inputs(config, cwd)? {
        DiffLaunchInputs::NotNeeded => Ok(Prepared::Ready(PreparedLaunch { params, note: None, synth: None })),
        DiffLaunchInputs::Nothing(msg) => Ok(Prepared::Nothing(msg)),
        DiffLaunchInputs::Ready(synth) => {
            params.extend(synth.params().iter().cloned());
            let note = synth.excluded_note.clone();
            Ok(Prepared::Ready(PreparedLaunch { params, note, synth: Some(synth) }))
        }
    }
}

/// (#2310 P4d) The inputs a diff-scoped config needs that an invoked
/// COMMAND surface (`/mission launch review` in the editor panel, `radio
/// "review this"`) cannot type: the diff itself, and a workspace the
/// planner can read the post-diff tree through.
///
/// **Why this exists.** `review.json` declares `diff_file` REQUIRED, so a
/// panel invocation has to supply it or the launch bails on a missing
/// input. Deciding that STRUCTURALLY, from the config's declared inputs
/// (does it declare a required `diff_file`?), rather than by matching the
/// id `"review"`, keeps a renamed or copied variant working.
///
/// **What it reviews, stated plainly.** The diff is the branch's COMMITTED
/// work (`git diff <base>..HEAD`, base = the merge-base with the first of
/// `origin/HEAD`/`origin/main`/`origin/master`/local `main`/local
/// `master`/`init.defaultBranch` that resolves, else `HEAD~1` as a last
/// resort — see [`synthesize_diff_launch_inputs`]'s own comment for the
/// full order), NOT the uncommitted working tree the
/// retired launcher used to pass. That is forced by the planner's own
/// contract: `plan.sites`'s diff source reads the post-diff content through
/// a MATERIALIZED tree ("the tree is the confirmation surface"), and a
/// materialized checkout is a clone at a ref — it cannot contain
/// uncommitted work. Reviewing the working tree would hand the rules a diff
/// whose lines do not exist in the tree they are read against. Uncommitted
/// changes are reported as excluded rather than silently reviewed.
pub enum DiffLaunchInputs {
    /// The config declares no required `diff_file` — nothing to synthesize.
    NotNeeded,
    /// A git repo with nothing committed to review against its base.
    Nothing(String),
    /// The `--param key=value` values to append, plus the tempdir holding
    /// the diff + workspace spec.
    Ready(SynthesizedInputs),
}

/// The synthesized `--param` values and the tempdir they live in. The
/// tempdir is removed by [`Drop`] — every exit path, including a `?` out of
/// the caller and a cancelled subprocess, since the guard is dropped with
/// the caller's frame.
pub struct SynthesizedInputs {
    params: Vec<String>,
    dir: std::path::PathBuf,
    /// Non-empty when the working tree carries changes this review does NOT
    /// cover — surfaced by the caller so the operator is never told a
    /// review covered work it could not read.
    pub excluded_note: Option<String>,
}

impl SynthesizedInputs {
    /// The `key=value` strings to pass as `--param` arguments.
    pub fn params(&self) -> &[String] {
        &self.params
    }
}

impl Drop for SynthesizedInputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The synthesized param that names the diff file.
const DIFF_FILE_KEY: &str = "diff_file";
/// The synthesized param that names the workspace spec file.
const WORKSPACE_KEY: &str = "workspace";
/// The synthesized params whose values name temporary files (the others,
/// like `head_sha`, are plain values that outlive the process).
pub const SYNTHESIZED_FILE_KEYS: [&str; 2] = [DIFF_FILE_KEY, WORKSPACE_KEY];

/// Run `git` in `cwd`, returning trimmed stdout on success.
fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// See [`DiffLaunchInputs`]. `Err` only for genuine IO failures (the repo
/// itself missing, an unwritable temp dir) — an empty diff is a
/// [`DiffLaunchInputs::Nothing`] outcome the caller renders, not an error.
pub fn synthesize_diff_launch_inputs(config: &MissionConfig, cwd: &Path) -> Result<DiffLaunchInputs> {
    let needs_diff = config
        .inputs
        .iter()
        .any(|i| i.name == "diff_file" && i.is_required_of_operator());
    if !needs_diff {
        return Ok(DiffLaunchInputs::NotNeeded);
    }
    let head = git_out(cwd, &["rev-parse", "HEAD"]).ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` needs a diff, and {} is not a git repository with a commit to review",
            config.id,
            cwd.display()
        )
    })?;
    // The base to diff against, tried in order, first hit wins: the
    // merge-base with the remote's default branch (`origin/HEAD` covers a
    // clone whose remote HEAD symref is set; `origin/main`/`origin/master`
    // cover the common case where it isn't); then a LOCAL `main`/`master`
    // branch (a repo with no remote at all — the common shape for a
    // freshly-initialized or fully-local project — still finds its own
    // base branch instead of falling straight to "review only the last
    // commit"); then whatever `git config init.defaultBranch` names, if
    // that branch actually exists; and only then the previous commit, so
    // a repo with genuinely no base branch anywhere still reviews
    // something rather than refusing. `fallback_to_last_commit` records
    // whether that last resort fired, so the caller can say so.
    let mut fallback_to_last_commit = false;
    let default_branch_candidate = git_out(cwd, &["config", "init.defaultBranch"]);
    let mut candidates: Vec<String> =
        vec!["origin/HEAD".into(), "origin/main".into(), "origin/master".into(), "main".into(), "master".into()];
    if let Some(b) = &default_branch_candidate {
        if !b.trim().is_empty() {
            candidates.push(b.trim().to_string());
        }
    }
    let base = candidates
        .iter()
        .find_map(|r| git_out(cwd, &["merge-base", "HEAD", r]))
        // Deliberate: when the checkout itself IS one of the base-branch
        // candidates (e.g. running this panel command directly on `main`),
        // `merge-base HEAD <candidate>` returns HEAD's own sha — a
        // "diff against yourself" answer that would otherwise short-circuit
        // to an empty diff below. Rejecting that case here (turning it back
        // to `None`) falls through to the `HEAD~1` last resort instead, so
        // the panel still reviews the most recent commit rather than
        // silently reviewing nothing.
        .filter(|b| b != &head)
        .or_else(|| {
            let h1 = git_out(cwd, &["rev-parse", "HEAD~1"]);
            if h1.is_some() {
                fallback_to_last_commit = true;
            }
            h1
        })
        .unwrap_or_else(|| head.clone());
    let diff = if base == head {
        String::new()
    } else {
        git_out(cwd, &["diff", &format!("{base}..{head}")]).unwrap_or_default()
    };
    if diff.trim().is_empty() {
        return Ok(DiffLaunchInputs::Nothing(format!(
            "Nothing committed to review in {} (no changes between {} and HEAD).",
            cwd.display(),
            &base[..base.len().min(12)]
        )));
    }

    let dir = std::env::temp_dir().join(format!(
        "darkmux-{}-{}-review",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the synthesized-input dir {}", dir.display()))?;
    // Constructed BEFORE the writes below so an error on either one still
    // drops the guard and removes the directory.
    let mut synthesized = SynthesizedInputs { params: Vec::new(), dir: dir.clone(), excluded_note: None };

    let diff_path = dir.join("review.diff");
    std::fs::write(&diff_path, &diff)
        .with_context(|| format!("writing the synthesized diff {}", diff_path.display()))?;

    // One `path` source at THIS checkout, pinned to the diff's own head —
    // the same `workspace_spec` shape an operator writes by hand, so the
    // planner materializes it through exactly one mechanism.
    let name = cwd.file_name().and_then(|s| s.to_str()).unwrap_or("workspace").to_string();
    let spec_path = dir.join("workspace.json");
    let spec = serde_json::json!({
        "name": name,
        "sources": [{ "id": name, "path": cwd.to_string_lossy(), "ref": head }],
    });
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)
        .with_context(|| format!("writing the synthesized workspace spec {}", spec_path.display()))?;

    synthesized.params = vec![
        format!("{DIFF_FILE_KEY}={}", diff_path.display()),
        format!("{WORKSPACE_KEY}={}", spec_path.display()),
        format!("head_sha={head}"),
    ];
    let mut notes: Vec<String> = Vec::new();
    if fallback_to_last_commit {
        notes.push("no base branch found; reviewing only the last commit.".to_string());
    }
    if let Some(dirty) = git_out(cwd, &["status", "--porcelain"]).filter(|s| !s.trim().is_empty()) {
        let n = dirty.lines().count();
        notes.push(format!(
            "{n} uncommitted change{} in this tree {} NOT part of this review — it reads the \
             committed tree at {}.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" },
            &head[..head.len().min(12)]
        ));
    }
    if !notes.is_empty() {
        synthesized.excluded_note = Some(notes.join(" "));
    }
    Ok(DiffLaunchInputs::Ready(synthesized))
}

/// `true` iff the config's graph declares at least one step AND every
/// declared step kind's REGISTRY ID is prefixed `procedural.` — the
/// ephemeral-vs-mission-launch routing test (rule D). This is a
/// declared-KIND test, not a runtime guarantee that zero model work can
/// possibly happen: a `procedural.shell` step could itself invoke
/// `darkmux dispatch` (or any other model-touching command) from inside
/// its shell command. The routing rule governs what this LAUNCHER
/// declares/dispatches directly, matching every Tier 1 builtin's naming
/// convention (`procedural.*` vs `dispatch.*`) and failing safe for any
/// unrecognized Tier 2/3 kind (routes to `Launch`, a full instance, never
/// silently ephemeral). A config with zero steps anywhere (a freeform
/// document — every phase manual, nothing to dispatch) is NOT ephemeral:
/// `mission launch` already handles the freeform mint-and-work-by-hand
/// path correctly, and there is nothing for an in-process runner to
/// execute.
pub fn is_procedural_only(config: &MissionConfig) -> bool {
    let mut saw_step = false;
    for phase in &config.phases {
        for task in &phase.tasks {
            for step in &task.steps {
                saw_step = true;
                if !step.kind.starts_with("procedural.") {
                    return false;
                }
            }
        }
    }
    saw_step
}

/// Run a procedural-only config's graph in-process — no mission instance
/// minted, no lifecycle records (rule D: "instances for work you'd
/// revisit, flow records for acts you'd audit"). Steps still emit their
/// own flow records through the ordinary sink (`crate::flow::record`,
/// the SAME sink `mission launch`/`dispatch` use); only the
/// mission/phase/task/step INSTANCE persistence is skipped — the `persist`
/// callback below is a deliberate no-op, a documented-valid `run_step_graph`
/// caller shape (see that function's own doc on `persist`).
///
/// Returns the TERMINAL step's `output` (rule E) — the sink task (the one
/// no other task's `depends_on`/`reads` names) in DOCUMENT order, its last
/// step. `cwd` (rule C: "the subprocess/execution cwd is the session's cwd
/// — always") is filled into every `procedural.shell` step's config that
/// doesn't already declare its own `cwd`, never overriding a step-authored
/// one.
///
/// Blocking — every call in here (`interpret`, `run_step_graph`,
/// `procedural.shell`'s own `std::process::Command::output()`) is
/// synchronous. `acp.rs`'s caller runs this on a `tokio::task::
/// spawn_blocking` thread rather than the connection's own async task, so
/// it never stalls the ACP event loop.
///
/// `gate` (#1684 Packet 2) is the operator sign-off gate handler for any
/// step in `config`'s graph that declares `"gate": "operator"` (e.g. the
/// `pr-merge`/`pr-approve` example verbs — see `mission_config::StepConfig::
/// gate`). `acp.rs`'s `session/prompt` handler wires the ACP `session/
/// request_permission` handler here (a channel round-trip back to the
/// connection's async task — see `acp.rs`'s own doc on why that shape is
/// required from a `spawn_blocking` thread); `None` (no handler at all)
/// still fails CLOSED rather than silently ungated — a gated step with no
/// handler wired refuses itself via `crew::gate::resolve_gate`'s own
/// `None` fallback.
pub fn run_ephemeral(
    config: &MissionConfig,
    params: &[String],
    cwd: &Path,
    gate: Option<&mut crate::crew::gate::GateHandler<'_>>,
) -> Result<EphemeralOutcome> {
    // (#1685) The command allowlist gate — checked FIRST, before
    // validate()/interpret() ever run, so a blocked config never executes a
    // single step (not even a read-only gather step). See
    // `mission_config::check_cmd`'s own doc. Rendered the same way a
    // declined operator sign-off gate renders (a command-failed message,
    // never a hard `Err` across the ACP boundary), so the panel shows the
    // operator exactly why nothing ran.
    if let Some(reason) = mission_config::check_cmd(config) {
        return Ok(EphemeralOutcome { text: format!("darkmux: command failed:\n\n{reason}"), success: false });
    }

    // The inputs resolve through the function `mission launch` calls, so a
    // missing required input is refused with the CLI's own text.
    let inputs = match crate::mission_launch::resolve_inputs(config, None, params) {
        Ok(resolved) => resolved.collected,
        Err(e) => return Ok(EphemeralOutcome { text: format!("darkmux: command failed:\n\n{e:#}"), success: false }),
    };
    let args = inputs.get("args").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut config = config.clone();
    mission_config::inject_panel_args_task_if_referenced(&mut config, &args);

    // (#1684 QA finding — CONSIDER 7) `mission launch` runs
    // `MissionConfig::validate` at its consumption point before ever
    // calling `interpret` (contract 7: semantic validation is a separate,
    // explicit pass); the ephemeral path is a consumption point too and
    // gets the SAME gate — a zero-step task, for instance, would otherwise
    // either wedge (never reaches `Complete`, `task_status` never
    // resolves) or surface as an unhelpful "terminal task has no steps"
    // deep inside `render_ephemeral_result`, instead of a clear, named
    // validate()-time error. `known_kinds` is Tier 1 builtins only —
    // sufficient because ephemeral is procedural-only by construction
    // (the routing decision in `is_procedural_only` already excludes
    // anything else), and the reserved `PANEL_ARGS_TASK_ID` reads/depends_on
    // carve-out in `validate` means the just-injected args task never
    // trips a false dangling-reference finding here.
    let known_ids = StepKindRegistry::with_builtins().ids();
    let known_kinds: Vec<&str> = known_ids.iter().map(String::as_str).collect();
    let errors: Vec<_> = config
        .validate(&known_kinds)
        .into_iter()
        .filter(|f| f.severity == mission_config::FindingSeverity::Error)
        .collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n");
        anyhow::bail!("panel command config \"{}\" failed validation:\n{msg}", config.id);
    }

    let launch_params = LaunchParams { input_values: inputs, ..Default::default() };
    let (ordered_tasks, mut steps, interpret_warnings) =
        mission_config::interpret(&config, &launch_params).context("interpreting panel command graph")?;

    apply_default_cwd(&mut steps, cwd);

    let tasks: BTreeMap<String, Task> = ordered_tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let registry = StepKindRegistry::with_builtins();
    let facts = crate::crew::concurrent_dispatch::standing_facts(None);
    let est = FixedEstimator::default();

    // (#1684 QA finding — MUST-FIX 4) A per-INVOCATION run, the one every
    // record of this ephemeral run carries (the scheduler mints its sessions
    // in it). Without it, two concurrent invocations of the SAME config
    // would collide in the viewer with nothing to tell them apart. It is a
    // FLOW-RECORD correlation run only — no mission instance is minted for
    // it (rule D still holds: nothing under `<mission_id>/` is ever
    // written).
    let correlation = RunId::mission(mint_ephemeral_correlation_id(&config.id))?;

    // (#1877 QA must-fix 1, contract 8) The run bookend, the same one
    // `mission launch` opens and built by the same `run_bookend_record`: the
    // cmd configs a panel runs here (`pr-merge`, `pr-approve`) are runs too,
    // and one that hung would otherwise have no liveness signal. No mission
    // instance is minted (rule D): every record's run is the per-invocation
    // `correlation` id. Host samples are the machine-scoped sampler's job.
    let mut run_sink = |record: crate::flow::FlowRecord| {
        let _ = crate::flow::record(record);
    };
    let config_id_for_abort = config.id.clone();
    let correlation_for_abort = correlation.clone();
    // The guard's Drop writes `run.error` for any exit between `open` and a
    // matching `close` that this function does not reach explicitly (a
    // panic): the backstop, as in `launch`.
    let mut bookend = crate::flow::BookendGuard::new(&mut run_sink, move |_id, _kind| {
        crate::mission_launch::run_bookend_record(
            crate::flow::Edge::Error,
            &config_id_for_abort,
            &correlation_for_abort,
            RunPayload::failed("ephemeral panel run terminated before completion (early return or panic)"),
        )
    });
    bookend.open(
        "run",
        "run",
        crate::mission_launch::run_bookend_record(crate::flow::Edge::Start, &config.id, &correlation, RunPayload::default()),
    );

    // (#1685) Track whether an operator sign-off gate was actually
    // CONFIRMED during this run, for the command-gate audit record emitted
    // below. `Rc<Cell<..>>` (not a plain captured `&mut`) so this closure
    // can wrap the caller's own handler while still letting the outer
    // function read the result AFTER `run_step_graph` returns — the
    // closure's last use is the call below, so the borrow checker is happy,
    // but a Cell keeps the intent obvious. `None` stays `None` for the
    // whole run when no gated step ever runs (a read-only verb like
    // `pr-list`/`pr-info`, or a config with no gate at all) — the audit
    // record then records "no gate" rather than fabricating a yes/no for a
    // decision that never happened.
    let gate_confirmed: std::rc::Rc<std::cell::Cell<Option<bool>>> =
        std::rc::Rc::new(std::cell::Cell::new(None));
    type BoxedGateHandler<'a> = Box<dyn FnMut(&Step, &BTreeMap<String, String>) -> crate::crew::gate::GateDecision + 'a>;
    let mut instrumented_gate: Option<BoxedGateHandler<'_>> = gate.map(|handler| {
        let flag = gate_confirmed.clone();
        Box::new(move |step: &Step, facts: &BTreeMap<String, String>| {
            let decision = handler(step, facts);
            flag.set(Some(matches!(decision, crate::crew::gate::GateDecision::Approved)));
            decision
        }) as BoxedGateHandler<'_>
    });

    let scheduler_result = crate::crew::scheduler::run_step_graph(
        &correlation,
        &mut steps,
        &tasks,
        &registry,
        &facts,
        &est,
        // (#1665 review CONSIDER 5, closes #2681) Was a hardcoded `1` —
        // now resolves the operator's real setting; behavior-preserving
        // since `remote_concurrent_cap()`'s default moved to `1` in the
        // same change (see that accessor's own doc).
        darkmux_types::config_access::remote_concurrent_cap() as usize,
        &crate::crew::concurrent_dispatch::lms_host_factory,
        // Every record already carries this run (`correlation`): the
        // scheduler and every step kind mint their sessions in it.
        &mut |record| {
            let _ = crate::flow::record(record);
        },
        &mut |_step: &Step| {
            // Deliberately a no-op — ephemeral runs mint no mission
            // instance, so there is no `<mission>/<phase>/steps/<id>.json`
            // to persist a Step transition into (rule D).
        },
        instrumented_gate.as_deref_mut(),
        None,
        &[],
    );

    // (#1877 QA must-fix 1) Explicit close on every KNOWN exit — a
    // scheduler-level error, a `render_ephemeral_result` error, or the
    // ordinary success finish — same discipline `launch`'s own three
    // explicit-close sites use; the `BookendGuard` Drop backstop above is
    // strictly the fallback for the unexpected case (a panic).
    let report = match scheduler_result {
        Ok(report) => report,
        Err(e) => {
            bookend.close(
                "run",
                crate::mission_launch::run_bookend_record(
                    crate::flow::Edge::Error,
                    &config.id,
                    &correlation,
                    RunPayload::failed(e.to_string()),
                ),
            );
            return Err(e.context("running panel command graph"));
        }
    };

    let outcome = match render_ephemeral_result(&ordered_tasks, &steps, &report, &interpret_warnings) {
        Ok(outcome) => outcome,
        Err(e) => {
            bookend.close(
                "run",
                crate::mission_launch::run_bookend_record(
                    crate::flow::Edge::Error,
                    &config.id,
                    &correlation,
                    RunPayload::failed(e.to_string()),
                ),
            );
            return Err(e);
        }
    };

    bookend.close(
        "run",
        crate::mission_launch::run_bookend_record(
            if outcome.success { crate::flow::Edge::Complete } else { crate::flow::Edge::Error },
            &config.id,
            &correlation,
            RunPayload::ended(outcome.success),
        ),
    );

    // (#1685) Flow-record audit — ONE record per executed gated command,
    // regardless of outcome: a blocked-by-gate or otherwise-failed attempt
    // is still "the operator's session tried to act as them," an
    // audit-worthy fact on its own (trail 6: "the audit trail of 'when I
    // allowed the tool to act as me' is a compliance artifact in its own
    // right"). Configs with no `cmd` (the ordinary case) never emit
    // this record at all.
    if let Some(verb) = config.cmd.as_deref() {
        emit_cmd_audit(verb, &args, cwd, gate_confirmed.get(), outcome.success, &correlation);
    }

    Ok(outcome)
}

/// (#1685) Emit the flow-record audit-trail entry for one executed cmd-gate
/// command — see `run_ephemeral`'s own doc for when THIS module fires it.
/// Follows the SAME audit-trail convention `darkmux flow note --source
/// adjudication` uses (`Category::Audit`, a distinguishing `source` string,
/// structured detail in `payload`), never a parallel record channel.
///
/// `pub(crate)` (#1685 QA MUST-FIX 2): `src/mission_launch.rs::launch`'s
/// own `darkmux mission launch <id>` entry point calls this SAME function
/// (via `emit_launch_cmd_audit`'s thin wrapper — see its doc) rather
/// than growing a second, drifting copy of the record shape. Before this,
/// only the ACP ephemeral route emitted `gh.verb.executed` at all: a bare
/// `darkmux mission launch pr-merge` from a terminal executed the merge
/// with ZERO audit trail, contradicting this feature's own docs page
/// ("Every EXECUTED gated command ... emits one flow record") and the
/// PR body that shipped it.
///
/// `pr` is a BEST-EFFORT extraction — the first whitespace-delimited token
/// of the raw args string, verbatim. darkmux core has no notion of what a
/// PR number IS or whether the operator typed one; it only records what
/// was typed after the slash command, exactly as `gh` itself received it.
pub(crate) fn emit_cmd_audit(verb: &str, args: &str, cwd: &Path, gate_confirmed: Option<bool>, success: bool, run: &RunId) {
    let pr = args.split_whitespace().next().map(str::to_string);
    let handle = match &pr {
        Some(pr) => format!("{verb} {pr}"),
        None => verb.to_string(),
    };
    let record = crate::flow::FlowRecord {
        tier: crate::flow::Tier::Operator,
        source: Some(darkmux_flow::FlowSource::CmdGateAudit),
        ..crate::flow::FlowRecord::for_session_with(
            &SessionId::run(run.clone()),
            if success { crate::flow::Level::Info } else { crate::flow::Level::Warn },
            crate::flow::Category::Audit,
            crate::flow::Stage::Review,
            darkmux_flow::Payload::GhVerbExecuted(GhVerbExecutedPayload {
                verb: verb.to_string(),
                pr,
                worktree: cwd.to_string_lossy().into_owned(),
                confirmed: gate_confirmed,
                success,
            }),
            handle,
        )
    };
    let _ = crate::flow::record(record);
}

/// Mint a per-invocation flow-record correlation id for an ephemeral
/// panel-command run — see `run_ephemeral`'s own doc for why this exists.
/// Shape mirrors `mission_launch::mint_run_id`'s spirit
/// (`<config-id>-<unix-nanos>-<counter>`) without pulling in `blake3` here
/// (`acp_panel.rs` has no other cryptographic need); a nanosecond
/// timestamp plus an in-process atomic counter is already collision-free
/// for concurrent ACP prompts within one process.
fn mint_ephemeral_correlation_id(config_id: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("acp-ephemeral-{config_id}-{nanos}-{n}")
}

/// Fill `cwd` into every `procedural.shell` step's config that doesn't
/// already declare a working directory of its own — never overrides one
/// the step's author wrote.
///
/// **(#2532) "A working directory of its own" is EITHER spelling.**
/// `procedural.shell` reads three tiers (`step_kinds::builtins::
/// resolve_shell_cwd`): its own `cwd` key, then `Task.workdir`, then a step
/// config `workdir`. `cwd` is the highest of the three, so checking only
/// `cwd` here meant a step authored with `workdir` got `cwd` INJECTED over
/// it and the authored value lost — one document running in two different
/// directories depending on whether it was launched from the panel or from
/// `mission launch`. That is not a cosmetic difference on this path:
/// `docs/guide/pr-flow.html` sells this injection as the safety property of
/// the shipped pr-flow verbs ("Two concurrent darkmux sessions in two
/// different worktrees never see each other's PRs by accident"), and those
/// verbs are `procedural.shell` steps running `gh`, which resolves its
/// repository from the working directory. An operator who spelled it
/// `workdir` would have had `gh pr merge` target whatever repository the
/// panel session happened to be sitting in.
///
/// A step naming EITHER key is left completely alone — including the
/// `workdir` case, where the authored value then resolves through tier 3
/// (the panel mints no `Task.workdir`, so nothing sits between them).
fn apply_default_cwd(steps: &mut BTreeMap<String, Step>, cwd: &Path) {
    let cwd_str = cwd.to_string_lossy().to_string();
    for step in steps.values_mut() {
        if step.kind != "procedural.shell" {
            continue;
        }
        // An authored key is an authored key, empty string included: a
        // `"cwd": ""` is a config defect the resolver names precisely
        // ("step config `cwd` is set but empty"), and silently substituting
        // the panel's directory for it would hide exactly that.
        if darkmux_crew::step_config::shell_names_a_directory(&step.config) {
            continue;
        }
        match &mut step.config {
            serde_json::Value::Object(map) => {
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
            }
            serde_json::Value::Null => {
                let mut map = serde_json::Map::new();
                map.insert("cwd".to_string(), serde_json::Value::String(cwd_str.clone()));
                step.config = serde_json::Value::Object(map);
            }
            _ => {}
        }
    }
}

/// [`run_ephemeral`]'s result: the rendered TEXT (identical rendering to
/// the pre-#1698-Packet-B `Ok(String)` contract — every caller that only
/// ever displayed the string, `src/acp.rs`'s ACP panel path, keeps
/// byte-identical output) PLUS a real typed success/failure signal.
///
/// **Retires the failure-prefix string-sniffing** (#1698 Packet B carry-
/// list item 5) that used to be the only way a caller with a real exit
/// code to decide (`src/radio_cli.rs`) could tell success from failure:
/// `EPHEMERAL_FAILURE_PREFIX`/`EPHEMERAL_INCOMPLETE_PREFIX`/
/// `ephemeral_output_is_failure` are gone — `success` is now computed
/// directly from the scheduler's own typed state in
/// [`render_ephemeral_result`], not re-derived by matching a literal
/// prefix on rendered prose.
///
/// **Fixes the documented "Known gap" as a direct consequence, not a
/// separate follow-up:** the terminal step can `Complete` cleanly while a
/// SIDE branch elsewhere in the graph errored (`report.errored` non-empty)
/// — the string-sniffing contract COULD NOT distinguish that case from a
/// genuine clean success (the warning was appended to the SAME
/// success-shaped text, no distinct prefix), so a caller with a real exit
/// code silently exited 0 for a partially-failed run. With a real typed
/// field, [`render_ephemeral_result`] now sets `success: false` for that
/// case directly — see its own doc.
#[derive(Debug)]
pub struct EphemeralOutcome {
    pub text: String,
    pub success: bool,
}

/// The sink task in DOCUMENT order — a task no other task's `depends_on`
/// OR `reads` names — and its last step's output, rendered as the final
/// panel message. `ordered_tasks` MUST be the original `Vec<Task>`
/// `interpret` returned (document order); a `BTreeMap`'s key order is
/// lexicographic by id, not document order, so it cannot substitute here.
///
/// `interpret_warnings` (#1695 merge-gate finding 3) are `interpret`'s own
/// non-fatal findings (e.g. an absent `expand.over` collection under a
/// pre-2.0 document — see `InterpretedGraph`'s doc) — `run_ephemeral`
/// previously bound and dropped them; every other production caller
/// (`mission_launch.rs`) prints them, so silently discarding them here was
/// the one place in the codebase where they went nowhere. These are
/// informational only (never flip `success` to `false` on their own — an
/// absent `expand.over` collection isn't a run failure) — same for the
/// multi-sink notice below.
///
/// `success` (#1698 Packet B carry-list item 5): `false` for a terminal
/// step that errored OR never completed, AND (the fixed "Known gap") for a
/// terminal step that completed cleanly while `report.errored` names a
/// SIDE branch that failed elsewhere in the graph — `report.errored` is
/// scheduler-wide, not scoped to the chain that fed the terminal step, so
/// a clean-looking terminal output over a partially-failed run is exactly
/// the "silence reads as success" failure this project's own doctrine
/// (CLAUDE.md's "no blind runs") warns against.
fn render_ephemeral_result(
    ordered_tasks: &[Task],
    steps: &BTreeMap<String, Step>,
    report: &SchedulerReport,
    interpret_warnings: &[String],
) -> Result<EphemeralOutcome> {
    let referenced: BTreeSet<&str> = ordered_tasks
        .iter()
        .flat_map(|t| t.depends_on.iter().chain(t.reads.iter()))
        .map(|s| s.as_str())
        .collect();
    // (#1684 QA finding — CONSIDER 6) Every SINK task (no other task's
    // `depends_on`/`reads` names it) in document order, not just the last
    // one — a fan-out graph with more than one sink loses every branch but
    // the last silently otherwise. The LAST sink in document order is
    // still what becomes the "final message" (rule E is singular by
    // design), but the others are named in a warning rather than dropped
    // with no trace.
    let sinks: Vec<&Task> = ordered_tasks.iter().filter(|t| !referenced.contains(t.id.as_str())).collect();
    let sink = sinks
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph has no terminal (sink) task"))?;
    let last_step_id = sink
        .step_ids
        .last()
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal task `{}` has no steps", sink.id))?;
    let terminal = steps
        .get(last_step_id)
        .ok_or_else(|| anyhow::anyhow!("panel command graph's terminal step `{last_step_id}` vanished"))?;

    let output = terminal.output.clone().unwrap_or_default();
    if terminal.status == NodeStatus::Error {
        return Ok(EphemeralOutcome {
            text: format!("darkmux: command failed:\n\n{output}"),
            success: false,
        });
    }
    if terminal.status != NodeStatus::Complete {
        // The terminal task never completed — its dependency chain
        // stranded on an earlier error (`step_is_ready` never schedules a
        // task whose dependency didn't reach Complete). Name what errored
        // rather than returning an empty/misleading message.
        return Ok(EphemeralOutcome {
            text: format!(
                "darkmux: command did not reach its final step (status: {:?}) — step(s) errored: {}",
                terminal.status,
                if report.errored.is_empty() { "(none recorded)".to_string() } else { report.errored.join(", ") }
            ),
            success: false,
        });
    }

    // (#1684 QA finding — CONSIDER 6, fixed by #1698 Packet B carry-list
    // item 5) The terminal step can `Complete` cleanly while a SIDE branch
    // (a non-terminal sink, or any other step off the terminal's own
    // dependency chain) errored — `report.errored` is scheduler-wide, not
    // scoped to the chain that fed the terminal step. A clean-looking
    // final message over a partially-failed run is exactly the
    // "silence reads as success" failure CLAUDE.md's "no blind runs"
    // warns against; name it in the text AND flip `success` to `false` —
    // no longer just a warning appended to a success-shaped string.
    let mut warnings: Vec<String> = interpret_warnings.to_vec();
    let mut success = true;
    if !report.errored.is_empty() {
        warnings.push(format!("step(s) errored elsewhere in the graph: {}", report.errored.join(", ")));
        success = false;
    }
    if sinks.len() > 1 {
        let other_ids: Vec<&str> = sinks[..sinks.len() - 1].iter().map(|t| t.id.as_str()).collect();
        warnings.push(format!(
            "graph has {} terminal branches; only `{}`'s output is shown (also ran: {})",
            sinks.len(),
            sink.id,
            other_ids.join(", ")
        ));
    }
    if warnings.is_empty() {
        return Ok(EphemeralOutcome { text: output, success });
    }
    Ok(EphemeralOutcome {
        text: format!("{output}\n\n---\n⚠ {}", warnings.join("\n⚠ ")),
        success,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crew::mission_config::{MissionInput, PhaseConfig, StepConfig, TaskConfig, PANEL_ARGS_TASK_ID};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), config, gate: None, enabled: None, extras: Map::new() }
    }

    fn gated_step(id: &str, kind: &str, config: serde_json::Value, gate: &str) -> StepConfig {
        StepConfig { gate: Some(gate.to_string()), ..step(id, kind, config) }
    }

    fn task(id: &str, depends_on: &[&str], reads: &[&str], steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            enabled: None,
            excludes: Vec::new(),
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: reads.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            run_on: None,
            steps,
            grow: None,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, enabled: None, extras: Map::new() }
    }

    fn config(id: &str, phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: id.to_string(),
            name: id.to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            cmd: None,
            outcome_from: None,
            source_input: None,
            ticket: None,
            extras: Map::new(),
        }
    }

    // ── apply_default_cwd ────────────────────────────────────────────

    /// (#2532) **An authored `workdir` survives the panel path.**
    /// `procedural.shell` reads `cwd`, then `Task.workdir`, then a step
    /// config `workdir` — so a step that named `workdir` and got `cwd`
    /// injected over it ran in the PANEL's directory, not its own, while
    /// the same document launched through `mission launch` ran in the
    /// authored one. `docs/guide/pr-flow.html` sells this injection as the
    /// safety property of the pr-flow verbs ("two concurrent darkmux
    /// sessions in two different worktrees never see each other's PRs by
    /// accident"), and those verbs run `gh`, which reads its repository off
    /// the working directory — so the pre-fix behavior pointed
    /// `gh pr merge` at the panel session's repository.
    ///
    /// Runs the steps for real through the SAME registry the panel uses,
    /// so this asserts the directory the command actually ran in, not just
    /// the shape of the config. Red-proved by restoring the `cwd`-only
    /// check: the authored step then prints the panel directory.
    #[test]
    fn apply_default_cwd_leaves_a_step_authored_workdir_alone() {
        let panel_dir = tempfile::tempdir().unwrap();
        let authored_dir = tempfile::tempdir().unwrap();

        let mut steps: BTreeMap<String, Step> = serde_json::from_value(serde_json::json!({
            "authored": {
                "id": "authored", "task_id": "t", "kind": "procedural.shell",
                "config": {"command": "pwd", "workdir": authored_dir.path().to_str().unwrap()},
            },
            "bare": {
                "id": "bare", "task_id": "t", "kind": "procedural.shell",
                "config": {"command": "pwd"},
            },
        }))
        .unwrap();

        apply_default_cwd(&mut steps, panel_dir.path());

        let task: Task = serde_json::from_value(serde_json::json!({
            "id": "t", "phase_id": "p", "description": "", "step_ids": ["authored", "bare"],
        }))
        .unwrap();
        let registry = StepKindRegistry::with_builtins();
        let kind = registry.get("procedural.shell").unwrap();

        let ran_in = |id: &str| {
            let out = kind.run(&steps[id], &task, &BTreeMap::new(), &crate::crew::step_kinds::StepRunCtx::solo(crate::test_run())).unwrap();
            std::fs::canonicalize(out.output.trim()).unwrap()
        };
        // The behavioral claim first, so the mutation that reds this test
        // reds it on WHERE THE COMMAND RAN, not on the shape of a config.
        assert_eq!(
            ran_in("authored"),
            std::fs::canonicalize(authored_dir.path()).unwrap(),
            "the authored `workdir` is where the command runs"
        );
        assert_eq!(
            ran_in("bare"),
            std::fs::canonicalize(panel_dir.path()).unwrap(),
            "and a step naming NO directory still gets the panel session's — the property pr-flow.html sells"
        );
        assert!(
            steps["authored"].config.get("cwd").is_none(),
            "a step that already names its own working directory must not be injected into"
        );
    }

    // ── parse_command ────────────────────────────────────────────────

    #[test]
    fn parse_command_strips_slash_and_splits_name_from_args() {
        assert_eq!(parse_command("/pr-view 42"), Some(("pr-view".to_string(), "42".to_string())));
        assert_eq!(parse_command("/review"), Some(("review".to_string(), String::new())));
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command(""), None);
    }

    /// (#1698 Packet B — the mode bit) The retirement itself: a bare
    /// command name with NO leading slash must never match, even when it
    /// spells an advertised command id exactly, and even when it's the
    /// first word of an otherwise ordinary sentence — the live-repro'd
    /// ambiguity ("review this with me when you have a sec" routing as a
    /// bare `review` invocation) this packet's own investigation confirmed
    /// against the PRE-fix parser. Paired with the test above (a leading
    /// slash still resolves) so the fix can't regress into "nothing
    /// matches anymore" either.
    #[test]
    fn parse_command_retires_bare_word_invocation() {
        assert_eq!(parse_command("pr-view 42"), None);
        assert_eq!(parse_command("review"), None);
        assert_eq!(
            parse_command("review this with me when you have a sec"),
            None,
            "the exact sentence this retirement's own live investigation confirmed \
             mis-fired under the pre-fix slash-optional parser"
        );
    }

    // ── is_procedural_only (rule D routing test) ────────────────────────

    #[test]
    fn a_config_with_only_procedural_steps_is_ephemeral() {
        let cfg = config(
            "echo-test",
            vec![phase(
                "p1",
                vec![task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))])],
            )],
        );
        assert!(is_procedural_only(&cfg));
    }

    #[test]
    fn a_config_with_a_dispatch_step_is_not_ephemeral() {
        // (Required test) A `dispatch.*` step must route AWAY from the
        // ephemeral path — this only asserts the ROUTING decision, never
        // actually dispatches a model.
        let cfg = config(
            "coder-verb",
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))]),
                    task("t2", &["t1"], &[], vec![step("s2", "dispatch.internal", serde_json::Value::Null)]),
                ],
            )],
        );
        assert!(!is_procedural_only(&cfg), "a graph with any dispatch.* step must not be ephemeral");
    }

    #[test]
    fn a_config_with_zero_steps_is_not_ephemeral() {
        // A freeform config (every phase manual) — nothing for an
        // in-process runner to execute; `mission launch` handles this path.
        let cfg = config("freeform", vec![phase("p1", vec![])]);
        assert!(!is_procedural_only(&cfg));
    }

    // ── the /mission grammar ────────────────────────────────────────────

    #[test]
    fn parse_mission_verb_reads_each_verb_and_keeps_id_case() {
        assert_eq!(parse_mission_verb("list"), Ok(MissionVerb::List));
        assert_eq!(parse_mission_verb("  LIST  "), Ok(MissionVerb::List), "the verb word is case-insensitive");
        assert_eq!(
            parse_mission_verb("launch Pr-View 42 head=abc"),
            Ok(MissionVerb::Launch { config_id: "Pr-View".to_string(), rest: "42 head=abc".to_string() }),
            "a config id keeps its case; everything after it is the raw rest"
        );
        assert_eq!(
            parse_mission_verb("launch review"),
            Ok(MissionVerb::Launch { config_id: "review".to_string(), rest: String::new() })
        );
        assert_eq!(parse_mission_verb("show m-1"), Ok(MissionVerb::Show { id: "m-1".to_string() }));
    }

    #[test]
    fn parse_mission_verb_refuses_malformed_input_naming_the_usage() {
        for bad in ["", "launch", "show", "show a b", "list extra", "finalize m-1", "abort"] {
            let reply = parse_mission_verb(bad).expect_err(bad);
            assert!(reply.contains("/mission launch <config>"), "`{bad}` must name the usage: {reply}");
        }
    }

    #[test]
    fn parse_invocation_accepts_only_the_mission_command() {
        assert_eq!(parse_invocation("/mission list"), Some(PanelInvocation::Mission(MissionVerb::List)));
        assert_eq!(parse_invocation("/MISSION list"), Some(PanelInvocation::Mission(MissionVerb::List)));
        assert_eq!(parse_invocation("   "), None);
        assert_eq!(parse_invocation("/"), None);
        assert_eq!(parse_invocation("mission list"), None, "no slash is never a command");
    }

    /// The retired per-config commands are refused, naming what replaces them.
    #[test]
    fn a_retired_per_config_command_is_refused_naming_mission_launch() {
        match parse_invocation("/review") {
            Some(PanelInvocation::Refused(reply)) => {
                assert!(reply.contains("/mission launch <config>"), "{reply}");
                assert!(reply.contains("`/mission`"), "{reply}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ── argument mapping ────────────────────────────────────────────────

    fn input(name: &str) -> MissionInput {
        MissionInput { name: name.to_string(), description: None, required: Some(false), default: None, ignored: None, ignored_reason: None, extras: Map::new() }
    }

    fn config_with(inputs: &[&str], reads_args: bool) -> MissionConfig {
        let reads: &[&str] = if reads_args { &[PANEL_ARGS_TASK_ID] } else { &[] };
        let mut cfg = config(
            "mapped",
            vec![phase("p1", vec![task("t1", &[], reads, vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        cfg.inputs = inputs.iter().map(|n| input(n)).collect();
        cfg
    }

    #[test]
    fn a_declared_name_value_token_becomes_a_param_and_the_rest_is_free_text() {
        let cfg = config_with(&["rules"], true);
        assert_eq!(
            map_launch_args(&cfg, "rules=a,b 42 --squash").unwrap(),
            vec!["rules=a,b".to_string(), "args=42 --squash".to_string()]
        );
    }

    #[test]
    fn a_quoted_declared_value_may_hold_spaces_and_loses_its_quotes() {
        let cfg = config_with(&["test_command", "rules"], true);
        assert_eq!(
            map_launch_args(&cfg, r#"test_command="cargo test -p x" rules='a b' 42"#).unwrap(),
            vec!["test_command=cargo test -p x".to_string(), "rules=a b".to_string(), "args=42".to_string()]
        );
        // The value itself may hold the other quote, and text may follow the close.
        assert_eq!(
            map_launch_args(&cfg, r#"test_command="echo 'hi there'"x"#).unwrap(),
            vec!["test_command=echo 'hi there'x".to_string()]
        );
    }

    #[test]
    fn an_unclosed_quote_on_a_declared_value_is_refused_naming_it() {
        let cfg = config_with(&["test_command"], true);
        let err = map_launch_args(&cfg, r#"test_command="cargo test"#).unwrap_err().to_string();
        assert!(err.contains("`test_command=`") && err.contains("never closes"), "{err}");
    }

    #[test]
    fn a_backslash_before_a_quote_in_a_quoted_value_is_refused_naming_the_input() {
        let cfg = config_with(&["name"], true);
        let err = map_launch_args(&cfg, r#"name="say \"hi\" now""#).unwrap_err().to_string();
        assert!(err.contains("`name=`") && err.contains("no escapes"), "{err}");
        // The inverse: a backslash elsewhere in the value is ordinary text.
        assert_eq!(map_launch_args(&cfg, r#"name="a\b c""#).unwrap(), vec![r"name=a\b c".to_string()]);
    }

    #[test]
    #[serial_test::serial]
    fn a_control_character_in_a_planned_input_is_refused_naming_it() {
        let err = plan_launch("review", "rules=a\u{1b}[31mb").err().expect("refused").to_string();
        assert!(err.contains("`rules`") && err.contains("control character"), "{err}");
        // The inverse: a plain value plans.
        assert!(plan_launch("review", "rules=a,b").is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn a_format_character_in_a_planned_input_is_refused_naming_it() {
        for (label, ch) in [("bidi override", '\u{202e}'), ("zero-width space", '\u{200b}'), ("BOM", '\u{feff}')] {
            let err = plan_launch("review", &format!("rules=a{ch}b")).err().unwrap_or_else(|| panic!("{label} accepted")).to_string();
            assert!(err.contains("`rules`") && err.contains("control character"), "{label}: {err}");
        }
        // The inverse: ordinary non-ASCII text plans.
        assert!(plan_launch("review", "rules=caf\u{e9},\u{65e5}\u{672c}").is_ok());
    }

    /// The inverse: quotes outside a declared value are ordinary characters,
    /// so an apostrophe in free text neither errors nor vanishes.
    #[test]
    fn quotes_in_free_text_or_an_undeclared_value_are_left_alone() {
        let cfg = config_with(&["rules"], true);
        assert_eq!(map_launch_args(&cfg, "fix don't crash").unwrap(), vec!["args=fix don't crash".to_string()]);
        assert_eq!(map_launch_args(&cfg, r#"title="a b""#).unwrap(), vec![r#"args=title="a b""#.to_string()]);
    }

    #[test]
    fn an_undeclared_name_value_token_is_free_text_not_a_param() {
        // The inverse: a token that merely contains `=` is a param only when
        // the config declares that input, or free text with an `=` in it
        // would be swallowed into an undeclared `--param`.
        let cfg = config_with(&["rules"], true);
        assert_eq!(map_launch_args(&cfg, "title=x").unwrap(), vec!["args=title=x".to_string()]);
    }

    #[test]
    fn free_text_for_a_config_that_reads_no_args_is_refused_naming_its_inputs() {
        let cfg = config_with(&["rules", "draws"], false);
        let err = map_launch_args(&cfg, "please look closely").unwrap_err().to_string();
        assert!(err.contains("takes no free text"), "{err}");
        assert!(err.contains("rules, draws"), "the refusal names the declared inputs: {err}");
        let bare = config_with(&[], false);
        assert!(map_launch_args(&bare, "x").unwrap_err().to_string().contains("takes no arguments"));
    }

    #[test]
    fn the_refusal_does_not_offer_an_input_the_launcher_fills() {
        let cfg = config_with(&["mission_id", "rules"], false);
        let err = map_launch_args(&cfg, "please look").unwrap_err().to_string();
        assert!(err.contains("rules"), "{err}");
        assert!(!err.contains("mission_id"), "the launcher overwrites mission_id: {err}");
        // Only launcher-supplied inputs declared: nothing left to offer.
        let only = config_with(&["mission_id"], false);
        let err = map_launch_args(&only, "x").unwrap_err().to_string();
        assert!(err.contains("takes no arguments"), "{err}");
    }

    #[test]
    fn declared_params_alone_need_no_args_reader() {
        let cfg = config_with(&["rules"], false);
        assert_eq!(map_launch_args(&cfg, "rules=a").unwrap(), vec!["rules=a".to_string()]);
        assert!(map_launch_args(&cfg, "").unwrap().is_empty());
    }

    // ── launch planning over the registry ───────────────────────────────

    struct HomeGuard(Option<String>);

    impl HomeGuard {
        fn set(path: &Path) -> Self {
            let prev = std::env::var("DARKMUX_HOME").ok();
            // SAFETY: every caller is #[serial_test::serial].
            unsafe { std::env::set_var("DARKMUX_HOME", path) };
            HomeGuard(prev)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: every caller is #[serial_test::serial].
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
    }

    fn write_config(home: &Path, file_stem: &str, body: serde_json::Value) {
        let dir = home.join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{file_stem}.json")), serde_json::to_string(&body).unwrap()).unwrap();
    }

    /// THE PROMISE: the panel lists a config exactly when `mission launch`
    /// accepts its id, and plans every one it lists, including a config whose
    /// body id differs from its file stem. An id `mission launch` refuses
    /// (an uppercase letter) is not offered, so `/mission list` never names a
    /// config the CLI cannot start.
    #[test]
    #[serial_test::serial]
    fn the_panel_lists_exactly_the_configs_launch_accepts_and_plans_each() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let shell = serde_json::json!([{"id": "p1", "tasks": [{"id": "t1", "steps": [
            {"id": "s1", "kind": "procedural.shell", "config": {"command": "echo hi"}}]}]}]);
        write_config(tmp.path(), "pr-view", serde_json::json!({"id": "pr-view", "name": "PR View", "phases": shell}));
        write_config(tmp.path(), "renamed", serde_json::json!({"id": "body-id-differs", "name": "Renamed", "phases": shell}));
        write_config(tmp.path(), "Upper-Case", serde_json::json!({"id": "Upper-Case", "name": "Upper", "phases": shell}));

        let listed: Vec<String> = list_launchable().unwrap().into_iter().map(|c| c.id).collect();
        for id in mission_config::list_ids() {
            let launchable = crate::mission_launch::resolve_config(&id).is_ok();
            assert_eq!(listed.contains(&id), launchable, "`{id}`: listed must equal launchable, listed = {listed:?}");
            if launchable {
                plan_launch(&id, "").unwrap_or_else(|e| panic!("`{id}` is listed, so it must plan: {e:#}"));
            }
        }
        for id in ["pr-view", "renamed", "review", "machine-status", "crawl", "coder-phase"] {
            assert!(listed.contains(&id.to_string()), "`{id}` must be listed: {listed:?}");
        }
        assert!(!listed.contains(&"Upper-Case".to_string()), "an id `mission launch` refuses is not offered");
        assert_eq!(plan_launch("renamed", "").unwrap().config_id, "renamed", "the plan carries the id the operator typed");
    }

    #[test]
    #[serial_test::serial]
    fn a_procedural_only_config_plans_ephemeral_and_a_dispatching_one_plans_a_launch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let shell = |kind: &str| serde_json::json!([{"id": "p1", "tasks": [{"id": "t1", "steps": [{"id": "s1", "kind": kind}]}]}]);
        write_config(tmp.path(), "local-only", serde_json::json!({"id": "local-only", "name": "L", "phases": shell("procedural.noop")}));
        write_config(tmp.path(), "seated", serde_json::json!({"id": "seated", "name": "S", "phases": shell("dispatch.internal")}));
        assert_eq!(plan_launch("local-only", "").unwrap().route, LaunchRoute::Ephemeral);
        assert_eq!(plan_launch("seated", "").unwrap().route, LaunchRoute::Launch);
        assert_eq!(
            plan_launch("machine-status", "").unwrap().route,
            LaunchRoute::Ephemeral,
            "the built-in machine-status still runs in-process as `/mission launch machine-status`"
        );
    }

    /// An unknown config is refused with the words `mission launch` uses.
    #[test]
    #[serial_test::serial]
    fn an_unknown_config_is_refused_with_the_launch_refusal_text() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let panel = match plan_launch("no-such-config", "") {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("an unknown config must be refused"),
        };
        let cli = format!("{:#}", crate::mission_launch::resolve_config("no-such-config").map(|_| ()).expect_err("the CLI refuses it"));
        assert_eq!(panel, cli, "one refusal text on both surfaces");
        assert!(panel.contains("loading mission config \"no-such-config\""), "{panel}");
    }

    /// The retired `panel` block is refused by the same gate `mission launch`
    /// runs first, naming the replacement.
    #[test]
    #[serial_test::serial]
    fn a_config_still_carrying_a_panel_block_is_refused_naming_mission_launch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        write_config(
            tmp.path(),
            "old-style",
            serde_json::json!({"id": "old-style", "name": "Old", "panel": {"description": "d"}, "phases": []}),
        );
        let err = format!("{:#}", plan_launch("old-style", "").map(|_| ()).expect_err("a panel block must be refused"));
        assert!(err.contains("/mission launch <id>"), "{err}");
    }

    /// THE PROMISE: `/mission list` and radio's catalog offer a config only
    /// if a launch would get past its own first gate. One stale user-tier
    /// file (a retired `panel` key) blocks every launch, so nothing is
    /// offered and the refusal, word for word the launch's, is the answer.
    #[test]
    #[serial_test::serial]
    fn a_stale_user_config_empties_the_listing_and_the_catalog_with_the_launch_refusal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let shell = serde_json::json!([{"id": "p1", "tasks": [{"id": "t1", "steps": [
            {"id": "s1", "kind": "procedural.shell", "config": {"command": "echo hi"}}]}]}]);
        write_config(tmp.path(), "fine", serde_json::json!({"id": "fine", "name": "Fine", "phases": shell}));
        write_config(
            tmp.path(),
            "old-style",
            serde_json::json!({"id": "old-style", "name": "Old", "panel": {"description": "d"}, "phases": shell}),
        );

        let launch_refusal = format!("{:#}", crate::mission_launch::resolve_config("fine").map(|_| ()).expect_err("launch refuses"));
        let listing_refusal = format!("{:#}", list_launchable().map(|_| ()).expect_err("the listing refuses too"));
        assert_eq!(listing_refusal, launch_refusal, "one preflight, one text");
        assert!(mission_list_text().contains(&launch_refusal), "{}", mission_list_text());
        assert!(!mission_list_text().contains("`fine`"), "nothing is offered while launches are blocked");
        assert!(crate::radio::compile_catalog().is_err(), "radio's catalog is empty for the same reason");
    }

    /// The recovery case: delete the stale file and the listing returns.
    #[test]
    #[serial_test::serial]
    fn removing_the_stale_config_restores_the_listing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        write_config(tmp.path(), "old-style", serde_json::json!({"id": "old-style", "name": "Old", "panel": {}, "phases": []}));
        assert!(list_launchable().is_err());
        std::fs::remove_file(tmp.path().join("mission-configs/old-style.json")).unwrap();
        assert!(list_launchable().unwrap().iter().any(|c| c.id == "review"));
    }

    // ── /mission list ───────────────────────────────────────────────────

    #[test]
    fn render_mission_list_gives_one_line_per_config() {
        let configs = vec![
            LaunchableConfig { id: "a".to_string(), summary: "Does A.".to_string(), accepts_args: false },
            LaunchableConfig { id: "b".to_string(), summary: "Does B.".to_string(), accepts_args: true },
        ];
        let text = render_mission_list(&configs);
        assert!(text.contains("- `a`: Does A.") && text.contains("- `b`: Does B."), "{text}");
        assert!(render_mission_list(&[]).contains("No mission configs"));
    }

    #[test]
    fn config_summary_is_the_first_sentence_else_the_name() {
        let mut cfg = config("x", vec![]);
        cfg.name = "The Name".to_string();
        assert_eq!(config_summary(&cfg), "The Name");
        cfg.description = Some("Says what it does. Then a long paragraph of provenance.".to_string());
        assert_eq!(config_summary(&cfg), "Says what it does.");
    }

    /// The shipped configs lead with a plain sentence: it is what the router
    /// reads and what `/mission list` prints.
    #[test]
    fn the_builtin_summaries_name_what_they_do_in_plain_words() {
        let review = config_summary(&embedded_review());
        assert!(review.to_ascii_lowercase().contains("review"), "{review}");
        assert!(!review.contains("#2310"), "no issue numbers in a router-facing summary: {review}");
        let status = config_summary(&mission_config::load("machine-status").unwrap().config);
        assert!(status.to_ascii_lowercase().contains("loaded"), "{status}");
        assert!(!status.contains("#2918"), "{status}");
    }

    /// (F13) The router matches a rule-shaped goal ("look for swallowed errors
    /// in the ui code") to a config only if the summary line it reads names the
    /// rules, in words a user would use. The cap must not clip the line.
    #[test]
    fn crawl_and_review_summaries_name_their_rules_in_plain_words() {
        let crawl = config_summary(&mission_config::load("crawl").unwrap().config).to_ascii_lowercase();
        for word in ["swallowed error", "unnamed", "contradict", "stale"] {
            assert!(crawl.contains(word), "crawl summary must name `{word}`: {crawl}");
        }
        let review = config_summary(&embedded_review()).to_ascii_lowercase();
        for word in ["swallowed error", "test", "duplicate", "caller", "intent"] {
            assert!(review.contains(word), "review summary must name `{word}`: {review}");
        }
        assert!(!crawl.ends_with("..."), "{crawl}");
        assert!(!review.ends_with("..."), "{review}");
    }

    // ── prepare_launch: diff synthesis keyed on declared inputs ─────────

    fn ready(prepared: Prepared) -> PreparedLaunch {
        match prepared {
            Prepared::Ready(p) => p,
            Prepared::Nothing(msg) => panic!("expected Ready, got Nothing({msg})"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_renamed_copy_of_review_is_synthesized_because_it_declares_a_required_diff_file() {
        let repo = temp_repo(2);
        let mut copy = embedded_review();
        copy.id = "my-review-copy".to_string();
        let prepared = ready(prepare_launch(&copy, Vec::new(), repo.path()).unwrap());
        for key in ["diff_file", "workspace", "head_sha"] {
            assert!(supplies(&prepared.params, key), "`{key}` must be synthesized: {:?}", prepared.params);
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_prepared_review_names_the_inputs_it_synthesized_and_a_supplied_one_names_none() {
        let repo = temp_repo(2);
        let prepared = ready(prepare_launch(&embedded_review(), Vec::new(), repo.path()).unwrap());
        assert_eq!(prepared.synthesized_keys(), ["diff_file", "workspace", "head_sha"]);
        let own = ready(prepare_launch(&embedded_review(), vec!["diff_file=x.diff".to_string()], repo.path()).unwrap());
        assert!(own.synthesized_keys().is_empty());
    }

    /// An operator-supplied `diff_file` is never overridden by the cwd's.
    #[test]
    #[serial_test::serial]
    fn a_supplied_diff_file_suppresses_synthesis() {
        let not_a_repo = tempfile::TempDir::new().unwrap();
        let given = vec!["diff_file=/tmp/mine.diff".to_string()];
        let prepared = ready(prepare_launch(&embedded_review(), given.clone(), not_a_repo.path()).unwrap());
        assert_eq!(prepared.params, given, "no synthesis, so a non-repo cwd is not even consulted");
    }

    /// The operator's own `workspace=` or `head_sha=` is never overridden by
    /// the cwd's: supplying any one of the three synthesized inputs means
    /// synthesizing none, and the launch's required-input refusal names what
    /// is still missing.
    #[test]
    #[serial_test::serial]
    fn any_supplied_synthesized_input_suppresses_all_synthesis() {
        let repo = temp_repo(2);
        for key in ["workspace", "head_sha", "diff_file"] {
            let given = vec![format!("{key}=/mine")];
            let prepared = ready(prepare_launch(&embedded_review(), given.clone(), repo.path()).unwrap());
            assert_eq!(prepared.params, given, "`{key}` supplied: nothing may be appended after it");
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_config_without_a_diff_input_passes_its_params_through_untouched() {
        let repo = temp_repo(2);
        let given = vec!["rules=a".to_string()];
        let prepared = ready(prepare_launch(&config_with(&["rules"], false), given.clone(), repo.path()).unwrap());
        assert_eq!(prepared.params, given);
    }

    #[test]
    #[serial_test::serial]
    fn a_repo_with_nothing_to_review_reports_nothing_through_prepare_launch() {
        let repo = temp_repo(1);
        assert!(matches!(
            prepare_launch(&embedded_review(), Vec::new(), repo.path()).unwrap(),
            Prepared::Nothing(_)
        ));
    }

    // ── run_ephemeral (required test: two-step procedural.shell chain) ──

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_chains_two_procedural_shell_steps_via_step_input_env_var() {
        let cfg = config(
            "echo-chain",
            vec![phase(
                "p1",
                vec![
                    task(
                        "producer",
                        &[],
                        &[],
                        vec![step("producer-step", "procedural.shell", serde_json::json!({"command": "echo hello-from-producer"}))],
                    ),
                    task(
                        "consumer",
                        &["producer"],
                        &[],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT_PRODUCER"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &[], &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "got: hello-from-producer");
        assert!(out.success);
    }

    #[test]
    #[serial_test::serial]
    fn ephemeral_run_never_mints_a_mission_instance_directory() {
        // (Required test) Assert NO mission instance directory was
        // created — isolate DARKMUX_HOME so this test can inspect the
        // (empty) missions dir without racing any other test's real state.
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: this test is #[serial_test::serial].
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };

        let cfg = config(
            "noop-test",
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        let cwd = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &[], &cwd, None).expect("ephemeral run succeeds");
        // `procedural.noop` with no `output` override defaults to its own
        // step id (see `ProceduralNoopStepKind::run`) — proves the run
        // actually executed, not just that no directory appeared.
        assert_eq!(out.text, "s1");
        assert!(out.success);

        let missions_dir = crate::crew::loader::missions_dir();
        let entries: Vec<_> = std::fs::read_dir(&missions_dir).map(|d| d.collect()).unwrap_or_default();
        assert!(entries.is_empty(), "ephemeral run must mint zero mission instance directories, found {entries:?}");

        // SAFETY: this test is #[serial_test::serial].
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    // ── #1877 QA must-fix 1 — `run_ephemeral` gets the same run bookend ──
    // `mission_launch::launch` gives every other config.
    //
    // RED PROVED: against the pre-fix `run_ephemeral` (no bookend at all),
    // `read_all_flow_records()` in both tests below returned only the
    // step's own `step result` record, and both `assert_eq!(..., 1, ...)`
    // calls failed on `0`.

    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_a_run_bookend_pair_on_success() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = config(
            "bookend-noop-test",
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "procedural.noop", serde_json::Value::Null)])])],
        );
        let cwd = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &[], &cwd, None).expect("ephemeral run succeeds");
        assert!(out.success);

        let records = read_all_flow_records();
        let run_records: Vec<&serde_json::Value> = records.iter().filter(|r| is_run_bookend(r)).collect();
        let starts: Vec<&&serde_json::Value> = run_records.iter().filter(|r| r["action"] == "run.start").collect();
        let completes: Vec<&&serde_json::Value> =
            run_records.iter().filter(|r| r["action"] == "run.complete").collect();
        assert_eq!(starts.len(), 1, "an ephemeral run opens exactly one run bookend, as launch() does: {run_records:#?}");
        assert_eq!(completes.len(), 1, "a successful ephemeral run closes as `run.complete`: {run_records:#?}");
        assert!(
            !records.iter().any(|r| r["action"] == "dispatch.start"),
            "a procedural run has no role execution, so no `dispatch.start`: {records:#?}"
        );
        assert_eq!(starts[0]["handle"], serde_json::json!("bookend-noop-test"));
        let mission_id = starts[0]["mission_id"].as_str().expect("mission_id present").to_string();
        assert!(
            mission_id.starts_with("acp-ephemeral-bookend-noop-test-"),
            "the bookend's mission_id must be the minted per-invocation correlation id: {mission_id}"
        );
        assert_eq!(starts[0]["session_id"], serde_json::json!(format!("{mission_id}.run")));
        assert_eq!(completes[0]["mission_id"], serde_json::json!(mission_id));
        assert_eq!(completes[0]["payload"]["gate"], serde_json::Value::Null, "no coder-phase gate on this path");

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// (#1918) Two ephemeral panel dispatches of the SAME document (e.g.
    /// every `pr-view` invocation, which reuses the reserved task id
    /// `__panel_args__`) must NOT share a scheduler-emitted step-lifecycle
    /// `session_id` — the dominant real-world collision case named in
    /// #1918's own report (49 missions folded into one `session_id`
    /// bucket on the reporting machine). Sibling of
    /// `mission_launch.rs`'s `two_launches_of_the_same_config_produce_
    /// distinct_step_lifecycle_session_ids` — same fix, same assertions,
    /// proving the run reaches this SEPARATE `run_step_graph` entry point
    /// (`run_ephemeral`'s own), not just the generic `mission launch` path.
    #[test]
    #[serial_test::serial]
    fn two_ephemeral_runs_of_the_same_document_produce_distinct_step_lifecycle_session_ids() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = config(
            "panel-args-test",
            vec![phase(
                "p1",
                vec![task(
                    "__panel_args__",
                    &[],
                    &[],
                    vec![step("s1", "procedural.noop", serde_json::Value::Null)],
                )],
            )],
        );
        let cwd = std::env::temp_dir();

        let out1 = run_ephemeral(&cfg, &[], &cwd, None).expect("first ephemeral run succeeds");
        assert!(out1.success);
        let out2 = run_ephemeral(&cfg, &[], &cwd, None).expect("second ephemeral run succeeds");
        assert!(out2.success);

        let records = read_all_flow_records();
        let step_records: Vec<&serde_json::Value> = records
            .iter()
            .filter(|r| {
                let action = r.get("action").and_then(|v| v.as_str()).unwrap_or("");
                let source = darkmux_flow::reader::source_of(r);
                source == Some(darkmux_flow::FlowSource::Scheduler) && (action == "step.start" || action == "step.complete")
            })
            .collect();
        assert!(
            step_records.len() >= 4,
            "expected `step start`+`step complete` for `s1` from BOTH runs, got {}: {step_records:#?}",
            step_records.len()
        );

        let mission_ids: std::collections::BTreeSet<&str> = step_records
            .iter()
            .filter_map(|r| r.get("mission_id").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(
            mission_ids.len(),
            2,
            "both ephemeral runs must be represented by distinct mission ids, got {mission_ids:?}"
        );

        // Within each run, the step-lifecycle records still share ONE
        // session_id (grouping intact); across the two runs, that id must
        // now differ — the #1918 collision surface closed.
        let sessions_for = |mid: &str| -> std::collections::BTreeSet<&str> {
            step_records
                .iter()
                .filter(|r| r.get("mission_id").and_then(|v| v.as_str()) == Some(mid))
                .filter_map(|r| r.get("session_id").and_then(|v| v.as_str()))
                .collect()
        };
        let mut all_sessions: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for mid in &mission_ids {
            let sessions = sessions_for(mid);
            assert_eq!(
                sessions.len(),
                1,
                "one run's own step-lifecycle records must share exactly ONE session_id, got {sessions:?}"
            );
            for sid in &sessions {
                assert!(sid.contains(mid), "session id `{sid}` must carry its own run's mission id `{mid}`");
            }
            all_sessions.extend(sessions);
        }
        assert_eq!(
            all_sessions.len(),
            2,
            "the two ephemeral runs of the SAME document must mint DIFFERENT session ids \
             for the same reused task id `__panel_args__` — this is the #1918 collision surface"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn run_ephemeral_closes_the_run_as_run_error_when_the_gate_declines() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();
        let mut decline = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut decline))
            .expect("a declined gate still renders a command-failed message, not an Err");
        assert!(!out.success);

        let records = read_all_flow_records();
        let run_records: Vec<&serde_json::Value> = records.iter().filter(|r| is_run_bookend(r)).collect();
        let starts: Vec<&&serde_json::Value> = run_records.iter().filter(|r| r["action"] == "run.start").collect();
        let errors: Vec<&&serde_json::Value> = run_records.iter().filter(|r| r["action"] == "run.error").collect();
        assert_eq!(starts.len(), 1, "{run_records:#?}");
        assert_eq!(errors.len(), 1, "a declined gate is a command FAILURE: the run closes as `run.error`: {run_records:#?}");

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_seeds_args_when_a_task_reads_the_synthetic_args_task() {
        // `procedural.shell`'s `sanitize_env_key` uppercases alnum bytes
        // and maps everything else (including `_`) to `_`, so the reserved
        // id `__panel_args__` becomes the env var name below verbatim
        // (case-folded).
        let cfg = config(
            "args-echo",
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &["args=hello world".to_string()], &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "arg: hello world");
    }

    fn greeting_config(required: bool) -> MissionConfig {
        let mut cfg = config(
            "greet",
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[],
                    vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo greeting: {{greeting}}"}))],
                )],
            )],
        );
        cfg.inputs = vec![MissionInput { required: Some(required), ..input("greeting") }];
        cfg
    }

    /// A declared input reaches the step's `{{name}}` placeholder on the
    /// ephemeral route, as it does under `mission launch --param`.
    #[serial_test::serial]
    #[test]
    fn ephemeral_run_substitutes_a_declared_input_into_its_step() {
        let out = run_ephemeral(&greeting_config(true), &["greeting=hi".to_string()], &std::env::temp_dir(), None)
            .expect("ephemeral run succeeds");
        assert!(out.success, "{}", out.text);
        assert_eq!(out.text.trim(), "greeting: hi");
    }

    /// The same run without the required input is refused, in the words
    /// `mission launch` uses for a missing required input, and no step runs.
    #[serial_test::serial]
    #[test]
    fn ephemeral_run_refuses_a_missing_required_input_with_the_launch_text() {
        let cfg = greeting_config(true);
        let out = run_ephemeral(&cfg, &[], &std::env::temp_dir(), None).expect("a refusal is an outcome, not an Err");
        assert!(!out.success);
        let launch_text = format!("{:#}", crate::mission_launch::resolve_inputs(&cfg, None, &[]).map(|_| ()).expect_err("launch refuses too"));
        assert!(out.text.contains(&launch_text), "one refusal text on both surfaces:\n{}\nvs\n{launch_text}", out.text);
        assert!(!out.text.contains("greeting: "), "no step may have run: {}", out.text);
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_with_empty_args_still_resolves_a_task_that_reads_the_reserved_id() {
        // (#1684 QA context) The reserved id is injected with an EMPTY
        // string when the command was invoked with no arguments — the
        // config must still interpret/run cleanly, not dangle.
        let cfg = config(
            "args-echo-empty",
            vec![phase(
                "p1",
                vec![task(
                    "t1",
                    &[],
                    &[PANEL_ARGS_TASK_ID],
                    vec![step(
                        "s1",
                        "procedural.shell",
                        serde_json::json!({"command": "echo arg:[$DARKMUX_STEP_INPUT___PANEL_ARGS__]"}),
                    )],
                )],
            )],
        );
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &[], &tmp, None).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "arg:[]");
    }

    // ── (#1695 merge-gate finding 1) reserved-id collision ──────────────

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_skips_injection_when_the_document_already_declares_the_reserved_task_id() {
        // The document itself owns a task literally named "__panel_args__"
        // — injection must be skipped (never double-inject, which would
        // collide at interpret()'s own duplicate-id check), and the
        // reading task must receive the DOCUMENT's own task's real
        // output, never the synthetic args string.
        let cfg = config(
            "reserved-collision",
            vec![phase(
                "p1",
                vec![
                    task(
                        PANEL_ARGS_TASK_ID,
                        &[],
                        &[],
                        vec![step(
                            "producer-step",
                            "procedural.noop",
                            serde_json::json!({"output": "operator-owned-value"}),
                        )],
                    ),
                    task(
                        "consumer",
                        &[],
                        &[PANEL_ARGS_TASK_ID],
                        vec![step(
                            "consumer-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo got: $DARKMUX_STEP_INPUT___PANEL_ARGS__"}),
                        )],
                    ),
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        // A NON-EMPTY args string — if injection had run anyway (ignoring
        // the collision), the reading task would see THIS value instead
        // of the document's own task output.
        let out = run_ephemeral(&cfg, &["args=this-should-be-ignored".to_string()], &tmp, None).expect("ephemeral run succeeds, no duplicate-id bail");
        assert_eq!(out.text.trim(), "got: operator-owned-value");
    }

    // ── (#1684 QA finding — CONSIDER 7) validate() runs before interpret ──

    #[test]
    fn ephemeral_run_rejects_a_zero_step_task_at_validate_time_not_a_confusing_runtime_error() {
        let cfg = config(
            "hollow",
            vec![phase(
                "p1",
                vec![
                    task("t1", &[], &[], vec![]), // zero steps — a real MissionConfig::validate() Error
                ],
            )],
        );
        let tmp = std::env::temp_dir();
        let err = run_ephemeral(&cfg, &[], &tmp, None).expect_err("a zero-step task must fail validate(), not run");
        assert!(err.to_string().contains("failed validation"), "{err:#}");
    }

    // ── (#1695 merge-gate finding 3) interpret() warnings surfaced ──────

    #[test]
    fn render_ephemeral_result_appends_interpret_warnings_to_the_output() {
        // `run_ephemeral` used to bind and drop `interpret`'s own non-fatal
        // warnings; `render_ephemeral_result` must surface them in the
        // final message the same way it already surfaces scheduler-level
        // findings (errored steps, multi-sink branches) — a direct unit
        // test since `interpret()` itself has no live producer of a
        // non-empty warnings Vec today (see `InterpretedGraph`'s doc).
        let t = Task {
            run_on: darkmux_crew::types::default_run_on(),
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut steps = BTreeMap::new();
        steps.insert(
            "s1".to_string(),
            Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: "procedural.noop".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: Some("done".to_string()),
            },
        );
        let report = SchedulerReport::default();
        let interpret_warnings = vec!["absent expand.over collection for task foo".to_string()];

        let out = render_ephemeral_result(&[t], &steps, &report, &interpret_warnings).expect("renders");
        assert!(out.text.contains("done"), "{}", out.text);
        assert!(out.text.contains("absent expand.over collection for task foo"), "{}", out.text);
        // Informational only (an absent expand.over collection isn't a run
        // failure) — `success` stays true (#1698 Packet B carry-list item 5).
        assert!(out.success);
    }

    /// (#1698 Packet B carry-list item 5 — the "Known gap" fix) A terminal
    /// step that `Complete`s cleanly while `report.errored` names a SIDE
    /// branch that failed elsewhere in the graph must report `success:
    /// false`, not just append a warning to a success-shaped text. Before
    /// the typed `EphemeralOutcome`, the string-sniffing contract
    /// (`ephemeral_output_is_failure`) could not distinguish this case from
    /// a genuine clean success — this is the direct regression test for
    /// that fix, paired with the inverted case above (no errored steps ->
    /// `success: true`) so a change that makes EVERY run `false` can't
    /// silently pass either.
    #[test]
    fn render_ephemeral_result_flips_success_false_when_a_side_branch_errored() {
        let t = Task {
            run_on: darkmux_crew::types::default_run_on(),
            id: "t1".to_string(),
            phase_id: "p1".to_string(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["s1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let mut steps = BTreeMap::new();
        steps.insert(
            "s1".to_string(),
            Step {
                id: "s1".to_string(),
                task_id: "t1".to_string(),
                gate: None,
                kind: "procedural.noop".to_string(),
                status: NodeStatus::Complete,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: Some("terminal step's own clean output".to_string()),
            },
        );
        let report = SchedulerReport {
            errored: vec!["side-branch-step".to_string()],
            ..SchedulerReport::default()
        };

        let out = render_ephemeral_result(&[t], &steps, &report, &[]).expect("renders");
        assert!(
            !out.success,
            "a Complete terminal step alongside an errored side branch must not report success"
        );
        assert!(out.text.contains("terminal step's own clean output"), "{}", out.text);
        assert!(out.text.contains("side-branch-step"), "{}", out.text);
    }

    // ── (#1684 Packet 2) run_ephemeral + operator sign-off gate ────────

    /// A two-step procedural config — a `gather` task feeding a gated
    /// `executor` task — is the shape every documented gated panel verb
    /// (`pr-merge`, `pr-approve`) actually has: a gather step assembles the
    /// facts, the gated step is the consequential action. This test
    /// exercises BOTH decisions through the real `run_ephemeral` path (not
    /// just `gate::resolve_gate`'s own unit tests) and asserts the
    /// handler's facts map is literally the gather task's output — the
    /// dialog-body contract the #1685 spec depends on.
    fn gather_then_gated_config() -> MissionConfig {
        config(
            "gather-then-gated",
            vec![phase(
                "p1",
                vec![
                    task(
                        "gather",
                        &[],
                        &[],
                        vec![step(
                            "gather-step",
                            "procedural.shell",
                            serde_json::json!({"command": "echo 42 open PRs"}),
                        )],
                    ),
                    task(
                        "executor",
                        &["gather"],
                        &[],
                        vec![gated_step(
                            "executor-step",
                            "procedural.noop",
                            serde_json::json!({"output": "merged"}),
                            "operator",
                        )],
                    ),
                ],
            )],
        )
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_gate_handler_receives_the_gather_tasks_output_and_approving_runs_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut received: Option<Map<String, String>> = None;
        let mut approve = |_s: &Step, f: &Map<String, String>| {
            received = Some(f.clone());
            crate::crew::gate::GateDecision::Approved
        };
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged", "an approved gate must let the executor step actually run");
        assert!(out.success);
        assert_eq!(
            received.as_ref().and_then(|f| f.get("gather")).map(|s| s.trim()),
            Some("42 open PRs"),
            "the gate handler must receive the gather task's output as its facts map — the \
             dialog-body contract: {received:?}"
        );
    }

    #[serial_test::serial]

    #[test]
    fn ephemeral_run_gate_handler_declining_fails_the_command_without_running_the_executor() {
        let cfg = gather_then_gated_config();
        let tmp = std::env::temp_dir();

        let mut decline = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        // `run_ephemeral` still returns `Ok` — a declined gate is a command
        // FAILURE (rendered as such, mirroring `render_ephemeral_result`'s
        // existing Error-terminal handling), never a hard `Err` propagated
        // across the ACP boundary.
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut decline))
            .expect("a declined gate still renders a command-failed message, not an Err");
        assert!(!out.success, "a declined gate must report success: false");
        assert!(out.text.contains("darkmux: command failed"), "{}", out.text);
        assert!(out.text.contains("operator declined"), "{}", out.text);
    }

    // ── (#1685) cmd-gate allowlist + audit record ────────────────────────

    fn gather_then_gated_config_with_verb(verb: &str) -> MissionConfig {
        let mut cfg = gather_then_gated_config();
        cfg.cmd = Some(verb.to_string());
        cfg
    }

    /// Env isolation for the `DARKMUX_CMD_ENABLED`/`DARKMUX_CMD_ALLOWED` pair
    /// — restores both on drop, mirroring the restore blocks the rest of
    /// this file's tests hand-roll inline (kept as a tiny guard here since
    /// this cluster of tests uses it four times).
    struct GhEnvGuard {
        prev_enabled: Option<String>,
        prev_allowed: Option<String>,
    }
    impl GhEnvGuard {
        fn off() -> Self {
            let g = Self {
                prev_enabled: std::env::var("DARKMUX_CMD_ENABLED").ok(),
                prev_allowed: std::env::var("DARKMUX_CMD_ALLOWED").ok(),
            };
            unsafe {
                std::env::remove_var("DARKMUX_CMD_ENABLED");
                std::env::remove_var("DARKMUX_CMD_ALLOWED");
            }
            g
        }
        fn on(allowed: &str) -> Self {
            let g = Self {
                prev_enabled: std::env::var("DARKMUX_CMD_ENABLED").ok(),
                prev_allowed: std::env::var("DARKMUX_CMD_ALLOWED").ok(),
            };
            unsafe {
                std::env::set_var("DARKMUX_CMD_ENABLED", "true");
                std::env::set_var("DARKMUX_CMD_ALLOWED", allowed);
            }
            g
        }
    }
    impl Drop for GhEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_enabled {
                    Some(v) => std::env::set_var("DARKMUX_CMD_ENABLED", v),
                    None => std::env::remove_var("DARKMUX_CMD_ENABLED"),
                }
                match &self.prev_allowed {
                    Some(v) => std::env::set_var("DARKMUX_CMD_ALLOWED", v),
                    None => std::env::remove_var("DARKMUX_CMD_ALLOWED"),
                }
            }
        }
    }

    /// Every flow record written to the isolated `DARKMUX_FLOWS_DIR` so far
    /// — mirrors `mission_launch.rs`'s own `read_all_flow_records` test
    /// helper (same on-disk shape, read raw off disk).
    /// Whether a written record is a run bookend (`run.*`).
    fn is_run_bookend(r: &serde_json::Value) -> bool {
        darkmux_flow::reader::action_of(r).and_then(|a| a.bookend()).is_some_and(|b| b.grain == darkmux_flow::Grain::Run)
    }

    fn read_all_flow_records() -> Vec<serde_json::Value> {
        let dir = std::env::var("DARKMUX_FLOWS_DIR").expect("DARKMUX_FLOWS_DIR must be set by an active guard");
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return out,
        };
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let contents = std::fs::read_to_string(&path).unwrap_or_default();
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    out.push(v);
                }
            }
        }
        out
    }

    /// The allowlist check happens BEFORE `validate()`/`interpret()` ever
    /// run — with the gate off, NOT ONE step of the graph executes, gated
    /// or not (proved two ways: the gate handler, which would only ever be
    /// invoked for the GATED `executor` step, panics if called at all; and
    /// zero flow records land on disk — the UNGATED `gather` step would
    /// otherwise have emitted step-lifecycle records even though it needs
    /// no sign-off).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_blocks_a_cmd_config_when_the_allowlist_gate_is_off() {
        let _gh = GhEnvGuard::off();
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let tmp = std::env::temp_dir();
        let mut never_called = |_s: &Step, _f: &Map<String, String>| {
            panic!("the gate handler must never be invoked — the allowlist check refuses the whole config first")
        };
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut never_called))
            .expect("a blocked cmd-gate config still renders a command-failed message, not an Err");
        assert!(!out.success, "{}", out.text);
        assert!(out.text.contains("pr-merge"), "names the verb: {}", out.text);
        assert!(out.text.contains("cmd.enabled"), "points at the fix: {}", out.text);
        assert!(
            read_all_flow_records().is_empty(),
            "a blocked config must run zero steps, not even the ungated gather step"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// The inverted case (required by the #1685 spec): a verb that IS
    /// allowlisted, with its gate approved, actually runs — proving the
    /// allowlist gate is not fail-closed by accident (it also opens).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_runs_a_cmd_config_once_allowlisted_and_gate_approved() {
        let _gh = GhEnvGuard::on("pr-list,pr-merge");
        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let tmp = std::env::temp_dir();
        let mut approve = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Approved;
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged", "allowlisted + approved must actually run the executor");
        assert!(out.success);
    }

    /// The permission-dialog facts contract this whole feature depends on:
    /// the gathered CI/verdict/adjudication text the operator sees in the
    /// `session/request_permission` dialog IS the cmd-gate config's own
    /// `gather` task output — exercised end to end through `run_ephemeral`
    /// (the ACP wiring in `src/acp.rs::acp_gate_handler` renders this exact
    /// map with `render_gate_facts`, one `key: value` line per fact).
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_cmd_gate_handler_sees_the_gathered_ci_and_verdict_facts() {
        let _gh = GhEnvGuard::on("pr-merge");
        let cfg = config(
            "pr-merge",
            vec![phase(
                "p1",
                vec![
                    task(
                        "gather",
                        &[],
                        &[],
                        vec![step(
                            "gather-step",
                            "procedural.shell",
                            serde_json::json!({
                                "command": "echo 'ci: SUCCESS\nreview: no confirmed findings\nadjudication: advisory only, no higher-tier review\npr: 123 -> main'"
                            }),
                        )],
                    ),
                    task(
                        "executor",
                        &["gather"],
                        &[],
                        vec![gated_step(
                            "executor-step",
                            "procedural.noop",
                            serde_json::json!({"output": "merged"}),
                            "operator",
                        )],
                    ),
                ],
            )],
        );
        let mut cfg = cfg;
        cfg.cmd = Some("pr-merge".to_string());
        let tmp = std::env::temp_dir();

        let mut received: Option<Map<String, String>> = None;
        let mut approve = |_s: &Step, f: &Map<String, String>| {
            received = Some(f.clone());
            crate::crew::gate::GateDecision::Approved
        };
        let out = run_ephemeral(&cfg, &[], &tmp, Some(&mut approve)).expect("ephemeral run succeeds");
        assert_eq!(out.text.trim(), "merged");
        let facts = received.expect("the gate handler must have been invoked");
        let gathered = facts.get("gather").expect("the gather task's output must be a fact");
        assert!(gathered.contains("ci: SUCCESS"), "CI status by conclusion, not just completed: {gathered}");
        assert!(gathered.contains("no confirmed findings"), "the review verdict: {gathered}");
        assert!(gathered.contains("advisory only"), "the adjudication state: {gathered}");
        assert!(gathered.contains("123 -> main"), "PR/branch provenance: {gathered}");
    }

    /// One flow-record audit entry per EXECUTED gated command — verb, PR
    /// (best-effort from the raw args), worktree (the session cwd), and
    /// whether the operator confirmed it.
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_one_audit_flow_record_per_executed_cmd() {
        let _gh = GhEnvGuard::on("pr-merge");
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = gather_then_gated_config_with_verb("pr-merge");
        let worktree = std::env::temp_dir();
        let mut approve = |_s: &Step, _f: &Map<String, String>| crate::crew::gate::GateDecision::Approved;
        let out = run_ephemeral(&cfg, &["args=123".to_string()], &worktree, Some(&mut approve)).expect("ephemeral run succeeds");
        assert!(out.success);

        let records = read_all_flow_records();
        let audit = records
            .iter()
            .find(|r| r["action"] == "gh.verb.executed")
            .expect("exactly one gh.verb.executed audit record must be emitted");
        assert_eq!(audit["category"], "audit");
        let payload = &audit["payload"];
        assert_eq!(payload["verb"], "pr-merge");
        assert_eq!(payload["pr"], "123", "best-effort PR extraction from the raw args");
        assert_eq!(payload["worktree"], worktree.to_string_lossy().to_string());
        assert_eq!(payload["confirmed"], true, "the operator approved the gate");
        assert_eq!(payload["success"], true);

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// A config with NO `cmd` (the ordinary case — every panel command
    /// that isn't a GitHub-CLI verb) never emits this audit record at all.
    #[test]
    #[serial_test::serial]
    fn run_ephemeral_emits_no_audit_record_for_a_non_cmd_config() {
        let tmp_flows = tempfile::TempDir::new().unwrap();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path()) };

        let cfg = config(
            "echo-test",
            vec![phase(
                "p1",
                vec![task("t1", &[], &[], vec![step("s1", "procedural.shell", serde_json::json!({"command": "echo hi"}))])],
            )],
        );
        assert!(cfg.cmd.is_none());
        let tmp = std::env::temp_dir();
        let out = run_ephemeral(&cfg, &[], &tmp, None).expect("ephemeral run succeeds");
        assert!(out.success);
        assert!(
            read_all_flow_records().iter().all(|r| r["action"] != "gh.verb.executed"),
            "no cmd declared → no audit record, ever"
        );

        unsafe {
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    // ── (#2310 P4d) diff synthesis for the panel/radio surfaces ────────

    /// A git repo with `n` commits, each adding one line to `src/a.rs`.
    pub(crate) fn temp_repo(commits: usize) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for i in 0..commits {
            let mut body = String::new();
            for k in 0..=i {
                body.push_str(&format!("fn f{k}() {{}}\n"));
            }
            std::fs::write(dir.path().join("src/a.rs"), body).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", &format!("c{i}")]);
        }
        dir
    }

    fn embedded_review() -> MissionConfig {
        mission_config::load("review").expect("the embedded review config loads").config
    }

    /// (#2404 P4d round 3) A no-remote repo whose base branch is only
    /// reachable LOCALLY (no `origin/*` refs at all) used to fall straight
    /// to the `HEAD~1` last resort, which reviews only the single most
    /// recent commit — silently under-reviewing every commit before it.
    /// `main` gets 3 commits, then `feat` branches off and gets 3 more;
    /// the merge-base chain's local `main` candidate must resolve the
    /// base to where `feat` diverged, so the diff covers all 3 of
    /// `feat`'s commits, not just its last one.
    #[test]
    #[serial_test::serial]
    fn no_remote_repo_finds_the_local_main_branch_and_reviews_every_commit() {
        // Isolate from the operator's real ~/.gitconfig: if it happens to
        // set `init.defaultBranch = main` globally, that candidate alone
        // would mask a broken local-branch fallback and this test would
        // stay green for the wrong reason. `GIT_CONFIG_GLOBAL`/`_SYSTEM`
        // pointed at `/dev/null` makes `git config init.defaultBranch`
        // resolve to nothing here, so only the LOCAL `main`/`master`
        // candidates this test means to prove can make it pass.
        let prev_global = std::env::var("GIT_CONFIG_GLOBAL").ok();
        let prev_nosystem = std::env::var("GIT_CONFIG_NOSYSTEM").ok();
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            // Xcode's bundled git ships its OWN system gitconfig
            // (`init.defaultbranch=main`) that `GIT_CONFIG_SYSTEM=/dev/null`
            // does not suppress on this platform — `GIT_CONFIG_NOSYSTEM`
            // is the flag that actually disables it.
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
        let repo = temp_repo(3);
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["checkout", "-q", "-b", "feat"]);
        for k in 3..6 {
            std::fs::write(repo.path().join(format!("src/f{k}.rs")), format!("fn f{k}() {{}}
")).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", &format!("feat c{k}")]);
        }
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            other => panic!("expected Ready, got {}", match other {
                DiffLaunchInputs::NotNeeded => "NotNeeded",
                DiffLaunchInputs::Nothing(_) => "Nothing",
                DiffLaunchInputs::Ready(_) => unreachable!(),
            }),
        };
        let diff_path = synth
            .params()
            .iter()
            .find_map(|p| p.strip_prefix("diff_file=").map(String::from))
            .expect("diff_file param present");
        let diff = std::fs::read_to_string(diff_path).unwrap();
        for k in 3..6 {
            assert!(diff.contains(&format!("f{k}.rs")), "commit for f{k}.rs missing from the diff — reviewed less than 3 of 3:\n{diff}");
        }
        assert!(synth.excluded_note.is_none(), "a local main WAS found, so no last-resort note should print: {:?}", synth.excluded_note);
        unsafe {
            match prev_global {
                Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
                None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
            }
            match prev_nosystem {
                Some(v) => std::env::set_var("GIT_CONFIG_NOSYSTEM", v),
                None => std::env::remove_var("GIT_CONFIG_NOSYSTEM"),
            }
        }
    }

    /// RED before this packet's synthesis existed: the panel typed no
    /// params, `review.json` declares `diff_file` REQUIRED, and the launch
    /// bailed on the missing input — `/review` in the editor was broken.
    #[test]
    #[serial_test::serial]
    fn a_diff_scoped_config_gets_its_diff_workspace_and_head_synthesized_from_the_cwd() {
        let repo = temp_repo(2);
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            other => panic!("expected Ready, got {}", match other {
                DiffLaunchInputs::NotNeeded => "NotNeeded",
                DiffLaunchInputs::Nothing(_) => "Nothing",
                DiffLaunchInputs::Ready(_) => unreachable!(),
            }),
        };
        let by_key = |k: &str| {
            synth
                .params()
                .iter()
                .find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from))
                .unwrap_or_else(|| panic!("no `{k}=` among {:?}", synth.params()))
        };
        let diff = std::fs::read_to_string(by_key("diff_file")).expect("the diff file is written");
        assert!(diff.contains("src/a.rs"), "the synthesized diff must cover the commit: {diff}");
        let spec: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(by_key("workspace")).unwrap()).unwrap();
        let sources = spec["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1, "exactly one source: {spec}");
        assert_eq!(
            sources[0]["path"].as_str().unwrap(),
            repo.path().to_string_lossy(),
            "the source is a `path` origin at THIS checkout — no clone from a remote: {spec}"
        );
        let head = by_key("head_sha");
        assert_eq!(sources[0]["ref"].as_str().unwrap(), head, "the source is pinned to the diff's head");
    }

    /// The tempdir is removed when the guard drops — every exit path, since
    /// the caller holds it for the whole spawn.
    #[test]
    #[serial_test::serial]
    fn dropping_the_synthesized_inputs_removes_their_tempdir() {
        let repo = temp_repo(2);
        let dir = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => {
                let d = s.params()[0]
                    .strip_prefix("diff_file=")
                    .map(|p| std::path::PathBuf::from(p).parent().unwrap().to_path_buf())
                    .unwrap();
                assert!(d.exists());
                d
            }
            _ => panic!("expected Ready"),
        };
        assert!(!dir.exists(), "the synthesized-input dir must be gone once the guard drops");
    }

    /// A repo whose HEAD has nothing to review against its base is a
    /// rendered outcome, never an error or a launch that bails on a
    /// missing input.
    #[test]
    #[serial_test::serial]
    fn a_repo_with_no_reviewable_commit_reports_nothing_rather_than_launching() {
        let repo = temp_repo(1);
        match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Nothing(msg) => assert!(msg.contains("Nothing committed"), "{msg}"),
            _ => panic!("expected Nothing"),
        }
    }

    /// A config that declares no required `diff_file` is untouched — the
    /// decision is STRUCTURAL, never `id == "review"`.
    #[test]
    #[serial_test::serial]
    fn a_config_without_a_required_diff_file_gets_nothing_synthesized() {
        let repo = temp_repo(2);
        let cfg = config(
            "plain",
            vec![phase("p1", vec![task("t1", &[], &[], vec![step("s1", "dispatch.internal", serde_json::Value::Null)])])],
        );
        assert!(matches!(
            synthesize_diff_launch_inputs(&cfg, repo.path()).unwrap(),
            DiffLaunchInputs::NotNeeded
        ));
    }

    /// The end the operator actually cares about: the params the panel
    /// synthesizes make `mission launch review --dry-run` resolve every
    /// input and mint the graph. Stubbed home/profiles/`lms` — no model,
    /// no network, no mutation of the operator's real state.
    #[test]
    #[serial_test::serial]
    fn the_panel_synthesized_params_dry_run_green() {
        use assert_cmd::prelude::*;
        let repo = temp_repo(2);
        let home = tempfile::TempDir::new().unwrap();
        std::fs::write(home.path().join("profiles.json"), r#"{"profiles":{},"default_profile":null}"#).unwrap();
        let synth = match synthesize_diff_launch_inputs(&embedded_review(), repo.path()).unwrap() {
            DiffLaunchInputs::Ready(s) => s,
            _ => panic!("expected Ready"),
        };
        let mut cmd = std::process::Command::cargo_bin("darkmux").unwrap();
        darkmux_types::test_isolation::neutralize_state_vars(&mut cmd);
        cmd.args(["mission", "launch", "review"]);
        for p in synth.params() {
            cmd.args(["--param", p]);
        }
        let out = cmd
            .arg("--dry-run")
            .env("DARKMUX_HOME", home.path())
            .env("DARKMUX_FLOWS_DIR", home.path().join("flows"))
            .env("DARKMUX_PROFILES", home.path().join("profiles.json"))
            .env("DARKMUX_LMS_BIN", "/usr/bin/true")
            .output()
            .expect("mission launch review --dry-run runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "dry run must be green.\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("plan.sites"), "the graph must mint the plan steps:\n{stdout}");
        assert!(
            !stdout.contains("diff_file") || stdout.contains("diff_file = "),
            "diff_file must resolve, never be reported missing:\n{stdout}"
        );
    }

}
