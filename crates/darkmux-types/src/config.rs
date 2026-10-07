//! (#661) Versioned config file — `~/.darkmux/config.json`.
//!
//! The canonical, `darkmux init`-written configuration surface. Every setting
//! resolves with precedence **`env > config.json > built-in default`** — see
//! [`crate::config_access`], the single place that precedence lives.
//!
//! This module owns only the *shape* + *load* of the file; the accessors
//! (which layer env over these fields over the built-in defaults) live in
//! `config_access`. A missing or malformed file is non-fatal — it loads as
//! the empty default and every accessor falls through to its env/built-in
//! tiers, so a bad config never bricks the CLI.
//!
//! **Carve-outs (NOT in this file by design):**
//! - the Redis **password** lives in the macOS Keychain, never plaintext —
//!   `RedisConfig` holds only non-secret connection bits.
//! - the config-file location is found via the `DARKMUX_HOME` bootstrap
//!   pointer + `paths::resolve` (`<root>/config.json`), not from inside the
//!   config itself.
//!
//! Schema shape mirrors `RuntimeCompactionConfig` (typed `Option`s +
//! `#[serde(flatten)] extras`, so a load survives an unknown key); see its
//! round-trip invariant tests for the pattern this file's tests copy. An
//! unknown key still loads, but every entry point refuses it at preflight
//! and `darkmux doctor` fails it (`crate::user_files`, CONFIG 2.0).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Semver of the `config.json` shape. Since 2.0 an unknown key is refused
/// (`crate::user_files`), so a config written for a newer darkmux is refused
/// by an older one: a field/section add is a **minor** bump (a newer binary
/// reads every older file), renaming/retyping/removing a field is **major**.
/// The history below records, for 1.x, what an older binary did with each
/// addition then (it ignored it). Mirrors the `FLOW_SCHEMA_VERSION`
/// discipline (`crates/darkmux-flow/src/schema.rs`).
// 1.1 (#933): additive `fleet{}` block (fleet.mode). Minor bump — an older
// binary tolerates it (all-Option + `extras` overflow), per the lenient-read
// doctrine.
// 1.2 (#1260/#1177): additive `remote{}` block (remote.max_tokens_per_execution
// — the per-pipeline-stage remote token allowance for endpoint-staffed crew
// seats). Minor bump, same lenient-read reasoning.
// 1.3 (#1230 Packet 5): additive `mission{}` block (mission.stale_active_days
// — the staleness threshold `darkmux mission status`'s drift detector uses
// to flag an Active mission with zero Complete phases). Minor bump, same
// lenient-read reasoning.
// 1.4 (#1349): additive `review{}` block (review.judge_concurrency — the
// PR-review pipeline judge step's bounded-concurrency cap, moved off a bare
// `DARKMUX_FUNNEL_JUDGE_CONCURRENCY` env read onto the standard precedence
// chain as part of the funnel->review rename). Minor bump, same
// lenient-read reasoning.
// 1.5 (#1475 packet 1): additive `role_profiles{}` map (a machine-local
// `role-id -> profile-name` binding — profiles stay role-agnostic + reusable,
// the map welds a role to a profile on THIS machine). Resolution is role ->
// map -> profile -> model, an unmapped role falling back to `default_profile`.
// Minor bump — an older binary tolerates it (all-Option + `extras` overflow),
// per the lenient-read doctrine.
// 1.6 (#1585): additive `dirs.lab` field — the lab-run scan root, previously
// the ONE directory setting with an env var (`DARKMUX_LAB_DIR`) and no config
// tier, which is why unset resolved to nothing and 247 on-disk lab runs were
// invisible to `/lab/runs` and `/runs`. Minor bump — `DirsConfig` carries its
// own `extras` overflow, so an older binary shunts the key there and falls to
// its own default. First FIELD-level add under this rule (the nine sibling
// `dirs.*` entries predate it, landing in the 1.0 scaffold).
// 1.7 (#1698 Packet B2): additive `radio{}` block (router_profile /
// answerer_profile / humor — the radio interpreter's own staffing + persona
// knobs) plus `runtime.acp_idle_exit_minutes` (the `darkmux acp` process's
// idle self-exit budget). Minor bump, same lenient-read reasoning.
// 1.8 (#1758): REMOVED `orchestrator` — write-only, machine-scoped
// provenance stamped at record-write time to describe an invocation-scoped
// fact (which frontier orchestrator drove the work), so every record on a
// machine carried the same value regardless of what actually drove that
// invocation. Nothing ever read it. An older binary's `~/.darkmux/config.json`
// still carrying the key loads fine — `extras` overflow absorbs the now-
// unknown top-level key, same lenient-read guarantee as an additive bump.
// 1.9 (#1685): additive `gh{}` block, RENAMED to `cmd{}` in 1.11 (#2004)
// (`cmd.enabled` / `cmd.allowed` — the
// per-verb allowlist gating an operator-authored panel command's shell-out
// to their OWN `gh` CLI, e.g. the `pr-approve`/`pr-merge` example verbs in
// the PR-flow guide). darkmux holds no credentials of its own; this
// block only says which verb NAMES the operator has opted into running.
// Minor bump, same lenient-read reasoning as every other additive block.
// 1.12 (#2093): additive `hooks{}` block (`enabled` / `outbox_dir` / `rules`
// — the flow-record hook sink: match a record, POST it to a loopback
// receiver). Minor bump, same lenient-read reasoning as every other
// additive block.
// 1.13 (#2094): additive `runtime.turn_delay_ms` (a global rest, in
// milliseconds, the internal runtime sleeps between inference turns on
// every LOCAL dispatch — GPU thermal/power relief for sustained runs, see
// that field's own doc). Minor bump, same lenient-read reasoning.
// 1.14 (#2107, #1833): additive `runtime.host_sampler_interval_ms` — the
// daemon-side continuous host sampler `darkmux serve` runs for the machine
// stats drawer's live `/machine/resources` `load` block (cpu/mem/gpu on a
// fixed cadence, kept in an in-memory ring; no flow records). `0` disables
// the sampler entirely. Minor bump, same lenient-read reasoning.
// 1.15 (#2110/#2109): additive `runtime.thermal{}` block — the thermal
// governor's pause/resume hysteresis thresholds (`pause_at`/`resume_at`/
// `resume_hold_ms`/`max_pause_ms`) and the breaker's throttle floor
// (`min_cpu_speed_limit_pct`), gated by `enabled` (default `true` — this is
// hardware-safety behavior, not an opt-in integration, so it follows
// `strict_selection`/`check_updates`'s always-on-unless-told-otherwise
// convention rather than `redis`/`audit`'s off-by-default one). Minor
// bump, same lenient-read reasoning.
// 1.16 (#2111): additive `runtime.telemetry_record_every_samples` — how
// many dispatch-sampler ticks between `machine.telemetry` SAMPLE records
// (the periodic host-pressure curve alongside `machine.thermal`'s
// TRANSITION events). Visible default `5` (≈10s at the sampler's 2s
// cadence); `0` disables the periodic curve without touching the sampler
// itself (thermal governor + `host_window` summary are unaffected). Minor
// bump, same lenient-read reasoning.
// 1.17 (#2135 option 2): additive per-rule
// `hooks.rules[].signing_secret_keychain_item` (a Keychain item NAME, never
// the secret itself) — the HMAC-SHA256 secret a rule signs its tailnet/
// loopback deliveries with. Minor bump, same lenient-read reasoning.
// 1.18 (#2183): additive per-rule `hooks.rules[].transform` (a jq adapter
// NAME, resolved inside `~/.darkmux/hooks/adapters/`), `hooks.rules[]
// .headers` (a name -> literal-string-or-`{keychain_item}` map — auth
// headers for a non-tailnet target), `hooks.rules[].file` (a directory —
// the no-network `file` transport, mutually exclusive with `http`), and
// `hooks.rules[].attribution_headers` (opt the `X-Darkmux-*` headers out,
// since a SaaS endpoint may reject unknown headers); plus block-level
// `hooks.jq_timeout_ms` / `hooks.jq_max_output_bytes` (the transform's
// wall-clock + output-size bounds). Minor bump, same lenient-read
// reasoning as every other additive block — an older binary's config with
// these keys still loads (`extras` overflow); an older binary reading a
// rule that names ONLY `transform`/`headers`/`file` with no `http` simply
// sees a rule its own `resolve_rules` refuses at load (no `http`), same as
// any other misconfigured rule today.
//   1.19 (#2200, #2171 — bookkeeping): two `RuntimeBehaviorConfig` fields
//           shipped without a bump of their own and are credited here.
//           `runtime.max_stall_recoveries` (#2200) landed AFTER 1.18 was
//           set, so 3.5.0 would have declared a schema that did not cover
//           a field it ships; `runtime.generation_checkpoint_interval_
//           tokens` (#2171) landed one commit before the 1.16 bump and
//           was never named by any entry. Both are `Option<u32>` caps —
//           additive, lenient-on-read, absent meaning "uncapped" — so no
//           reader breaks either way. The bump exists so the declared
//           version and the shipped shape agree, which is the whole point
//           of the contract. Neither is written by `init` (a literal
//           would be wrong: absent is a real behavior), same carve-out as
//           `runtime.max_turns`.
//   1.20 (#2361, swarm S4-4): additive `runtime.step_command_timeout_
//           seconds` — the bound on ONE operator-supplied shell command a
//           step runs (`mods.gate`'s `test_command`, `procedural.shell`),
//           written VISIBLY by `init` at its 600s default like its
//           `model_load_timeout_seconds` sibling, because unlike a cap
//           whose absence is a real behavior this one always has a value.
//           `Option<u64>`, lenient-on-read: an older binary ignores it and
//           runs unbounded, exactly as it did before.
//   1.21 (#2394): additive `runtime.dispatch_free_concurrency` — how many
//           DISPATCH-FREE steps (`procedural.shell`, `procedural.noop`,
//           `mods.gate`, `records.gather`, `deliver.github_review`) the
//           scheduler runs at once. These used to ride the hosted-endpoint
//           track and queue behind `remote.concurrent_cap`, which a
//           mission launch sets to 1 — six independent shell waits ran
//           strictly one at a time. They now have their own track and
//           their own ceiling. Written VISIBLY by `init` at its default of
//           8, like its `step_command_timeout_seconds` sibling, because
//           this one always has a value (absence is not a distinct
//           behavior). `Option<u32>`, lenient-on-read: an older binary
//           ignores the field and keeps its old serialized behavior.
// 1.22 (#2404 P4d round 3): REMOVED the `review{}` block
//           (`judge_concurrency` / `judge_fail_on_any_skip`) added in 1.4.
//           The funnel driver those knobs tuned was deleted in #2310 — the
//           `review` mission config is now an ordinary on-disk mission
//           config run through the generic launch path, with no judge
//           step left to bound. Pre-1.0, no-compat-baggage posture: the
//           field is removed outright rather than deprecated in place. An
//           older config's `review` key still loads fine — it lands in
//           top-level `extras` overflow, same lenient-read guarantee as
//           every other removal (see 1.8's `orchestrator` precedent);
//           `darkmux doctor` names it and tells the operator to delete it.
//
//   (#2413) `runtime.telemetry_record_every_samples` is ALSO retired as of
//           this same version — the per-dispatch `machine.telemetry` curve
//           it configured is gone (one machine-scoped sampler now owns
//           that emission; see `FLOW_SCHEMA_VERSION` 1.42.0). No separate
//           bump: flattening one more retired key into `extras` doesn't
//           change the config SHAPE a consumer has to learn, so it rides
//           the 1.22 line above rather than minting 1.23. Lenient-on-read
//           as always: an older config carrying the field just flattens it
//           into `extras` harmlessly, and a binary at this version never
//           reads it. `darkmux doctor` names it and tells the operator to
//           delete it, same as the `review{}` block above.
//   1.23 (#2706): additive top-level `power{}` block —
//           `min_battery_pct` / `refuse_start_below_min` /
//           `pause_running_below_min`, the battery-charge gate on run
//           START and on in-flight CONTINUATION. Top-level rather than
//           under `runtime` because it governs whether work starts at
//           all, not how the runtime behaves once it has. Written
//           VISIBLY by `init` with all three defaults populated (50 /
//           true / true), per the visible-defaults doctrine: the surface
//           is discoverable and one edit from changed. `Option`-typed
//           and lenient-on-read as always — an older binary ignores the
//           block into `extras` and simply does not gate, which is
//           exactly its pre-#2706 behavior. NOT an `enabled`-gated
//           feature block: the two boolean policies ARE the gates, one
//           per decision, and a master switch would make "off"
//           expressible two ways.
//   1.24 (#2678): additive `runtime.mission_wall_clock_timeout_seconds` —
//           the run-level wall-clock bound on `darkmux mission launch`
//           (see the field's own doc for the mechanism). Written VISIBLY
//           by `init` at its default of `0` (UNBOUNDED, the same reading
//           every other darkmux zero-knob has), so an existing mission's
//           behavior is unchanged until an operator opts in.
//           `Option<u64>`, lenient-on-read: an older binary ignores the
//           field and keeps its old (never-bounded) behavior.
//   1.25 (#2772): additive `runtime.local_dispatch_concurrency` — an
//           explicit override for the per-resident-instance LOCAL model
//           dispatch concurrency cap (`darkmux_crew::concurrent_dispatch::
//           run_local_waves`). Absent by default and NOT written by
//           `init`, unlike 1.23's `power{}` block: the built-in default
//           here is derived live per instance from that instance's own
//           declared `PARALLEL` (`lms ps --json`), so a written literal
//           would freeze a value meant to track whatever model is
//           resident — same reasoning `max_turns` etc. already document.
//           `Option<u32>`, lenient-on-read: an older binary ignores the
//           field and keeps deriving the cap from the live declaration.
//   1.26 (#2765, #2775): two additive top-level blocks.
//           `serve{}` — `port` / `bind`, the daemon's listen address.
//           Before this the port lived ONLY in the launch command, so a
//           restart that forgot `--port` silently reverted to the
//           built-in 8765 while the machine's proxy still pointed
//           somewhere else; the daemon stayed healthy and every client
//           looked in the wrong place. Written VISIBLY by `init` at the
//           built-in defaults (8765 / 127.0.0.1). NOT an `enabled`-gated
//           feature block — the daemon is not a feature you turn on, it
//           is a process you start, and the block only says where.
//           `serve.token` remains a SECRET and is never a field here
//           (Keychain item `darkmux-serve-token`).
//           `machine_rollup{}` — `enabled` / `period_seconds`, the
//           periodic `machine.rollup` flow record carrying the whole
//           machine-lens aggregate. An `enabled`-gated feature block in
//           the redis/audit/hooks mold: written visibly with
//           `enabled: false` and `period_seconds: 60`, because it adds
//           steady-state volume to the flow stream nobody should pay for
//           unsubscribed.
//           Both `Option`-typed and lenient-on-read: an older binary
//           ignores either block into `extras` and behaves exactly as it
//           did before.
//   1.27 (#2846): one additive block under `runtime`.
//           `detection{}` — per-detector policy. `degeneracy.policy` is
//           `enforce` (act) / `observe` (measure, never act) / `off` (do
//           not measure); renamed in 1.31 (#2947). Written visibly by
//           `init` at `enforce`, so an
//           older binary ignoring the block behaves exactly as the
//           default does.
//           The value is stored as a STRING, not a derived enum: this
//           file's loader ends in `unwrap_or_default()`, so a derived
//           enum's hard error on an unknown variant discarded the WHOLE
//           config on one typo. Same reason `fleet.mode` is a string.
//           Parsing happens at the accessor, which reports an
//           unrecognized value as `config-invalid` rather than coercing
//           it silently. (Superseded by the #2947 note below: an
//           unrecognized value is now refused, not reported and armed.)
//   1.28 (#2914, darkmux 4.0): REMOVED `radio.router_profile` (added in
//           1.7). The radio ROUTING seat runs on the machine's one utility
//           model (`internal.utility` in profiles.json) and is never staffed
//           through a profile, so the knob has no meaning; the interim
//           `role_profiles.radio-router` binding it superseded is refused by
//           `config set` for the same reason. Clean break, no deprecation
//           read (the 4.0 posture): an older config still carrying the key
//           loads fine (it lands in `radio.extras`, same lenient-read
//           guarantee as 1.8's `orchestrator` and 1.22's `review`), has no
//           effect, and `darkmux doctor` names it with the fix.
//           `radio.answerer_profile` stays: answering the user is work.
//   1.29 (#2916, darkmux 4.0): three additive keys under `fleet{}`, the
//           secure work-submission surface. `fleet.identity.provider` names
//           the overlay network that verifies which machine is on the other
//           end of a connection (a VALUE, `"tailscale"` today; no vendor name
//           appears in any field). `fleet.listener{}` (`enabled` / `port`) is
//           the dedicated submission listener, bound to the address that
//           provider reports for this machine and nowhere else. `fleet.
//           accept_work{}` is the receiver's allow-list, keyed by machine
//           name, each entry carrying the `node_id` `darkmux machine trust`
//           resolved (never typed), the `profiles` that machine may run here,
//           and `workspace` (a receiver path grant, see 2.2).
//           `init` writes `identity` and `listener` visibly (`enabled:
//           false`) and `accept_work` empty. Lenient-on-read as always: an
//           older binary ignores all three into `fleet.extras`.
//           Also recorded here, since it shipped in the same 4.0 cycle with
//           no bump of its own: REMOVED `dirs.notebook` (#2913, with the
//           `lab notebook` verb and the `scribe` role). `config set` rejects
//           the key; a config still carrying it loads fine (serde drops the
//           unknown key; `dirs` has no overflow map), has no effect, and
//           `darkmux doctor` names it with the fix, alongside
//           `DARKMUX_NOTEBOOK_DIR`.
//           Likewise REMOVED in 5.0 with no bump of its own: `dirs.ack`
//           (#3036, with the licensed-adjacent acknowledgment gate); doctor
//           names it with the fix, alongside `DARKMUX_ACK_DIR`.
//   1.30 (#2928, darkmux 4.0): additive `runtime.live_sample_ms` — the cadence of the
//           LIVE channel: model state sampled from a running execution and
//           pushed to the local daemon's viewers, never written to the flow
//           log. Written VISIBLY by `init` at its default of 250. `0` turns
//           the live channel off (the zero-means-off convention
//           `host_sampler_interval_ms` uses); a non-zero value is clamped to
//           100..=1000 at the accessor, which reports the clamp. `Option<u64>`,
//           lenient-on-read: an older binary ignores it and has no live
//           channel, exactly as before.
//   1.31 (#2947, darkmux 4.0): VALUE change, no field change.
//           `runtime.detection.degeneracy.policy` values now name the
//           action: `off` / `record` / `warn` / `conclude` (was `off` /
//           `observe` / `enforce`). `init` writes `conclude`. Bumped although
//           no field changed, because a config written by `init` at 1.30 or earlier
//           carries `"policy": "enforce"`, which this binary REFUSES (the
//           retired spelling is refused with its replacement named, never
//           read as `conclude`): the file still loads (lenient read), but every
//           dispatch, mission launch and lab run refuses until the value is
//           changed, and `darkmux doctor` prints the exact `config set`.
//           Also added under the same rule: `hooks.rules[].match.level` /
//           `.category` are validated against the flow vocabulary (a typo
//           used to match nothing); no shape change there either.
//           The same release makes every enum-valued key (the policy,
//           `runtime.thermal.pause_at` / `resume_at`, `fleet.mode`,
//           `fleet.identity.provider`, the hook `match` fields) follow one
//           rule (`config_enum`): still read leniently as strings, but an
//           unregistered value is refused where it is consumed and reported
//           as Fail by `darkmux doctor`, never resolved to a fallback.
//           (The acting value was briefly spelled `cut` on the #2947
//           branch and renamed to `conclude` before anything shipped, so
//           `cut` is not a retired spelling.)
//   1.32 (#2902 step 5, darkmux 4.0): budgets are the operator's, never
//           darkmux's, and the per-step cap speaks darkmux's step vocabulary
//           (CLAUDE.md contract 8). Changes to `remote{}`:
//           - RENAMED `remote.max_tokens_per_execution` ->
//             `remote.max_tokens_per_step` (env
//             `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` ->
//             `DARKMUX_REMOTE_MAX_TOKENS_PER_STEP`): a per-step cap on hosted
//             tokens. Clean break, no alias: `config set` refuses the old key
//             naming the new one, and `darkmux doctor` names a leftover old
//             key (in config.json, where it now lands in `remote.extras` and
//             is read by nothing, or in the env). It also LOSES its built-in
//             500000: unset means no per-step cap, and `init` writes it
//             visibly as `null`, never a number.
//           - additive `remote.step_budget_policy` (env
//             `DARKMUX_REMOTE_STEP_BUDGET_POLICY`; `off` / `warn`, a
//             registered `ConfigEnum`, absent = `warn`; `wait` is an
//             endpoint budget's value only and is refused here): what a step
//             that reaches its cap does. A step used to STOP its hosted
//             calls at the cap; nothing stops a step now. `init` writes it
//             visibly as `null`.
//           Folded into this minor bump by operator decision (the rename
//           would be a major by the rule above; 4.0 is the clean-break
//           release). An older binary reading this file ignores both new
//           keys and falls back to its own 500000 default.
//   1.33 (#2916 stage 2, darkmux 4.0): additive `fleet.busy_policy`, the
//           receiver's answer to a fleet job that finds its seat busy:
//           `refuse` (answer at once, naming what is running) or `queue`
//           (hold it and tell the sender it is waiting). A job on a local
//           model has capacity one per model; a job on a hosted endpoint
//           runs concurrently up to this machine's `remote.concurrent_cap`,
//           and the policy applies past that cap. Written VISIBLY by `init`
//           as `refuse`. A string read leniently and refused where it is
//           consumed (#2947): an older binary ignores the field and keeps
//           its one-job-at-a-time listener.
//   2.0 (darkmux 4.0, MAJOR): an unknown key is refused. `config.json` still
//           loads with one (`extras` catches it, so one typo never discards
//           the rest), but every entry point that reads the file refuses it
//           at preflight and `darkmux doctor` fails it, naming the key's
//           dotted path and the closest valid key (`crate::user_files`, the
//           same gate as every other user file). The valid keys are derived
//           from this type's JSON schema, never listed. A retired key is
//           named with what replaced it: `remote.max_tokens_per_execution`
//           (renamed) and every other key a past
//           `DarkmuxConfig` had (`RETIRED_SETTINGS`, built from `git log`). A leftover
//           of any of the three used to be warned about and ignored; now it
//           refuses. A value of the wrong type (`"port": "x"`) is refused the
//           same way, naming the expected type and what it got: one such
//           value used to fail the typed load and silently drop EVERY setting
//           to its default (Redis and audit off). The breaking change is the
//           reading rule, not the shape.
//           Also in 2.0 (#2988): `runtime.daemon_auth_enabled` is retired,
//           replaced by `serve.token_keychain` (the same Keychain gate) and
//           the new `serve.read_auth` (reads need the token, default off),
//           both written visibly by `init` as `false`.
//   2.1 (#3022, darkmux 4.0): additive `fleet.defaults{}`, what a fleet hub
//           hands to machines with no setting of their own. First key:
//           `fleet.defaults.radio.answerer_profile`, a `<profile>@<machine>`
//           address (`config set` refuses a bare profile name). Meaningful
//           only where `fleet.mode` is `hub`; the hub serves it in its
//           machine card, and a card that does not declare `hub` has it
//           refused by every reader. `init` writes it visibly, empty (empty
//           means no default). An older binary refuses the unknown key.
//   2.2 (#3035, #3036, #755, darkmux 5.0): REMOVED `runtime.log_level` (and
//           `DARKMUX_LOG`; it only switched on one debug line on the hosted
//           single-shot path) and the whole `machine_rollup{}` block (and its
//           two env vars; the `machine.rollup` flow record is gone). Both are
//           `RETIRED_SETTINGS` entries: an env var warns, a leftover config
//           key is named by the unknown-key gate and judged by its value
//           (`LeftoverValue`, #3057). Additive
//           `fleet.accept_work.<name>.repos`, the names of repos in the
//           receiver's future registry that the peer may hand work off
//           against; absent or empty means none. Reserved for git workspace
//           handoff (#755) and read by nothing in this darkmux, so it changes
//           no admission decision. `workspace` stays a receiver PATH grant
//           only.
//   2.3 (#3035, darkmux 5.0): REMOVED the whole `remote{}` block:
//           `remote.max_tokens_per_step`, `remote.step_budget_policy` and
//           `remote.concurrent_cap` (and their three env vars, plus the
//           4.0-renamed `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`). "Remote"
//           was the wrong axis: a local server on the same machine is an
//           endpoint too, and what matters is whether darkmux MANAGES it.
//           Limits live per endpoint in `profiles.json`
//           (`endpoints.<id>.limits.tokens_per_dispatch`,
//           `.concurrent_calls`, `.window`, `.policy`, `.warn_at`). Each is a
//           `RETIRED_SETTINGS` entry naming its replacement: an env var
//           warns, a leftover config key is named by the unknown-key gate
//           and judged by its value (`LeftoverValue`, #3057).
//           Nothing is carried over: limits are off until set per endpoint.
//           Folded into a minor bump like 2.2's removals.
//           Also ADDED `redis.telemetry_maxlen` (the retention cap of the
//           hub's machine-telemetry stream; unreleased, so folded into 2.3).
//           An older binary refuses a config carrying it as an unknown key.
pub const CONFIG_SCHEMA_VERSION: &str = "2.3";

/// A `config.json` key an older darkmux read (and `init` may have written)
/// that this one does not. The unknown-key gate (`user_files`) names it with
/// `line` instead of guessing a near-miss. Built from `git log` of this file
/// (every field a past `DarkmuxConfig` carried that this one does not);
/// `every_historical_config_key_is_named_as_retired` pins the set.
#[derive(Debug, Clone, Copy)]
pub struct RetiredSetting {
    /// The dotted key; a block (`review`) covers every key inside it.
    pub key: &'static str,
    /// The env var that set it, when it had one ([`retired_env_leftovers`]).
    pub env: Option<&'static str>,
    /// What a still-set `env` does at CLI entry. `Refuse` when ignoring it
    /// would quietly change behavior; `Warn` when nothing reads it and nothing
    /// is lost. Meaningless without an `env`. One rule on both channels, refuse only when
    /// ignoring is unsafe: a `Warn` setting's `leftover` is `Any`, a `Refuse` one's is not
    /// (`a_retired_settings_env_and_config_channels_agree_on_whether_ignoring_it_is_unsafe`).
    pub env_policy: LeftoverPolicy,
    /// How a leftover `config.json` key's VALUE is judged (#3057): at the
    /// default `darkmux init` once wrote (or any value, where ignoring it
    /// changes nothing) it only warns; anything else is refused.
    pub leftover: LeftoverValue,
    /// What replaced it, or that nothing did, and what to do.
    pub line: &'static str,
}

/// A value `darkmux init` once wrote for a retired key, or one a retired key
/// held by default: ignoring it loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldDefault {
    Null,
    Bool(bool),
    Int(i64),
    Str(&'static str),
    EmptyArray,
}

impl OldDefault {
    fn matches(self, value: &serde_json::Value) -> bool {
        use serde_json::Value;
        match (self, value) {
            (OldDefault::Null, Value::Null) => true,
            (OldDefault::Bool(want), Value::Bool(got)) => want == *got,
            (OldDefault::Int(want), Value::Number(n)) => n.as_i64() == Some(want),
            (OldDefault::Str(want), Value::String(got)) => want == got,
            (OldDefault::EmptyArray, Value::Array(a)) => a.is_empty(),
            _ => false,
        }
    }
}

/// How a retired `config.json` key's leftover value is judged (#3057, 5.0):
/// a value ignoring which changes nothing only WARNS and every command still
/// starts; a value ignoring which would change something (a spend cap that
/// was set, a feature that was turned on) is REFUSED until the operator
/// moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftoverValue {
    /// No value is judged harmless: the key is refused, as before.
    Refuse,
    /// Any value is harmless: ignoring it only makes things slower or quieter.
    Any,
    /// Harmless only at one of these old defaults.
    Default(&'static [OldDefault]),
    /// A block: harmless when it is an object and every key inside it is
    /// (judged by that key's own entry, `<block>.<key>`; a key with no entry
    /// is not).
    Block,
}

/// Whether a retired `config.json` key holding `value` is harmless to ignore
/// ([`LeftoverValue`]). A key that is not retired is not.
pub fn leftover_is_harmless(key: &str, value: &serde_json::Value) -> bool {
    let Some(entry) = RETIRED_SETTINGS.iter().find(|r| r.key == key) else { return false };
    match entry.leftover {
        LeftoverValue::Refuse => false,
        LeftoverValue::Any => true,
        LeftoverValue::Default(defaults) => defaults.iter().any(|d| d.matches(value)),
        LeftoverValue::Block => value
            .as_object()
            .is_some_and(|map| map.iter().all(|(k, v)| leftover_is_harmless(&format!("{key}.{k}"), v))),
    }
}

/// Every retired or renamed `config.json` key (a rename's `line` names the new key).
pub const RETIRED_SETTINGS: &[RetiredSetting] = &[
    RetiredSetting {
        key: "remote",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Block,
        line: "removed in 5.0 (#3035): \"remote\" was the wrong axis (a local server on the same machine is an \
               endpoint too), so limits are declared per endpoint in profiles.json, under \
               `endpoints.<id>.limits`. Nothing is carried over: delete the block, then set the limits you want \
               on the endpoints that need them",
    },
    RetiredSetting {
        key: "remote.max_tokens_per_step",
        env: Some("DARKMUX_REMOTE_MAX_TOKENS_PER_STEP"),
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Null]),
        line: "removed in 5.0 (#3035): the cap is per DISPATCH (one role execution) now, set per endpoint as \
               `endpoints.<id>.limits.tokens_per_dispatch` in profiles.json. A whole-run budget is that \
               endpoint's rolling `limits.window`. Nothing is carried over: limits are off until you set them",
    },
    RetiredSetting {
        key: "remote.max_tokens_per_execution",
        env: Some("DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION"),
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Null, OldDefault::Int(500_000)]),
        line: "removed in 5.0 (#3035; `remote.max_tokens_per_step` was its brief successor): set \
               `endpoints.<id>.limits.tokens_per_dispatch` on the endpoint in profiles.json instead. Nothing is \
               carried over: limits are off until you set them (500000 was darkmux's old default, not a \
               recommendation)",
    },
    RetiredSetting {
        key: "remote.step_budget_policy",
        env: Some("DARKMUX_REMOTE_STEP_BUDGET_POLICY"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3035): what reaching a limit does is `endpoints.<id>.limits.policy` (`off`, `warn` \
               or `wait`) in profiles.json, one policy for the endpoint's whole `limits`",
    },
    RetiredSetting {
        key: "remote.concurrent_cap",
        env: Some("DARKMUX_REMOTE_CONCURRENT_CAP"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3035): concurrency is per endpoint. On an endpoint darkmux does not manage, set \
               `endpoints.<id>.limits.concurrent_calls` in profiles.json (absent, its calls run one at a time); \
               on a managed endpoint the scheduler owns parallelism and the field is refused",
    },
    RetiredSetting {
        key: "dirs.ack",
        env: Some("DARKMUX_ACK_DIR"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3036): the licensed-adjacent acknowledgment gate and its roles retired, so \
               nothing writes or reads an acknowledgment file. Delete it",
    },
    RetiredSetting {
        key: "runtime.log_level",
        env: Some("DARKMUX_LOG"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3035): it only ever switched on one debug line on the tool-less hosted dispatch \
               path and nothing else read it. Delete it",
    },
    RetiredSetting {
        key: "machine_rollup",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Block,
        line: "removed in 5.0 (#3036): the periodic `machine.rollup` flow record is gone; the machine lens reads \
               `GET /machine/resources`. Delete the block",
    },
    RetiredSetting {
        key: "machine_rollup.enabled",
        env: Some("DARKMUX_MACHINE_ROLLUP_ENABLED"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3036) with the rest of `machine_rollup`. Delete it",
    },
    RetiredSetting {
        key: "machine_rollup.period_seconds",
        env: Some("DARKMUX_MACHINE_ROLLUP_PERIOD_SECONDS"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#3036) with the rest of `machine_rollup`. Delete it",
    },
    RetiredSetting {
        key: "dirs.notebook",
        env: Some("DARKMUX_NOTEBOOK_DIR"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in 5.0 (#2913): the notebook verbs retired; the bundled `darkmux-lab-notebook` skill writes \
               an entry wherever your own instructions say. Delete it",
    },
    RetiredSetting {
        // flow-action-guard:allow — a retired config key, refused by name
        key: "radio.router_profile",
        env: Some("DARKMUX_RADIO_ROUTER_PROFILE"),
        env_policy: LeftoverPolicy::Warn,
        leftover: LeftoverValue::Any,
        line: "removed in CONFIG 1.28: radio routing runs on the machine's utility model, `internal.utility` in \
               profiles.json. Delete it",
    },
    RetiredSetting {
        key: "dirs.openclaw_config",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Refuse,
        line: "removed with the openclaw runtime (#1405): nothing reads it. Delete it",
    },
    RetiredSetting {
        key: "dirs.runtime_agents",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Refuse,
        line: "removed with the openclaw runtime (#1405): nothing reads it. Delete it",
    },
    RetiredSetting {
        key: "gh",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Block,
        line: "renamed to `cmd` (#2003): move `gh.enabled` / `gh.allowed` to `cmd.enabled` / `cmd.allowed`",
    },
    RetiredSetting {
        key: "orchestrator",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Null, OldDefault::Str("")]),
        line: "removed in #1766 (`init` wrote it from #663): flow records no longer carry an orchestrator. \
               Delete it",
    },
    RetiredSetting {
        key: "remote.stage_budget_policy",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Null, OldDefault::Str("warn")]),
        line: "removed in 5.0 (#3035; `remote.step_budget_policy` was its brief successor): the policy is \
               `endpoints.<id>.limits.policy` in profiles.json",
    },
    RetiredSetting {
        key: "review",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Block,
        line: "removed with the review funnel (#2310): `review` runs as a mission config now, and its judge knobs \
               went with the funnel. Delete the block",
    },
    RetiredSetting {
        key: "runtime.daemon_auth_enabled",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Bool(false)]),
        line: "replaced in 5.0 (#2988) by `serve.token_keychain` (read the serve token from the Keychain; the \
               fleet's execution credential) and `serve.read_auth` (whether reads from off this machine need \
               it, default off). Move your value to `serve.token_keychain`, and set `serve.read_auth true` if \
               you want reads closed",
    },
    RetiredSetting {
        key: "runtime.telemetry_record_every_samples",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Int(5)]),
        line: "removed in #2413: one machine-scoped host sampler replaced the per-dispatch curve; its cadence is \
               `runtime.host_sampler_interval_ms`. Delete it",
    },
    RetiredSetting {
        key: "dirs.crew",
        env: Some("DARKMUX_CREW_DIR"),
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Refuse,
        line: "removed in 5.0: \"crew\" is a retired concept. `DARKMUX_HOME` (or `~/.darkmux`) is the one root, and \
               roles, missions, phases, crews and skills live directly under it. Unset it, and to relocate \
               darkmux set `DARKMUX_HOME`; the autonomous-dispatch preamble override is \
               `<root>/AUTONOMOUS_DISPATCH_PREAMBLE.md`",
    },
    RetiredSetting {
        key: "review.judge_concurrency",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Int(1)]),
        line: "removed with the review funnel (#2310): `review` runs as a mission config now. Delete the block",
    },
    RetiredSetting {
        key: "review.judge_fail_on_any_skip",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Bool(false)]),
        line: "removed with the review funnel (#2310): `review` runs as a mission config now. Delete the block",
    },
    RetiredSetting {
        // flow-action-guard:allow — a retired config key, refused by name
        key: "gh.enabled",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::Bool(false)]),
        line: "renamed to `cmd.enabled` (#2003)",
    },
    RetiredSetting {
        // flow-action-guard:allow — a retired config key, refused by name
        key: "gh.allowed",
        env: None,
        env_policy: LeftoverPolicy::Refuse,
        leftover: LeftoverValue::Default(&[OldDefault::EmptyArray]),
        line: "renamed to `cmd.allowed` (#2003)",
    },
];

/// What a leftover env var does at CLI entry (operator, 2026-10-01): a
/// leftover whose silent loss would change behavior (a renamed cap, a
/// state-location dir) refuses every command but `doctor` and `config`; one
/// that nothing reads and whose loss changes nothing is ignored with one
/// warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftoverPolicy {
    Refuse,
    Warn,
}

/// A retired or renamed setting whose env var is still set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredLeftover {
    pub setting_old_key: &'static str,
    pub policy: LeftoverPolicy,
    /// `env var ...`.
    pub found_in: String,
    /// The value the env var holds (which may name a path).
    pub value: String,
    /// The operator line: what is refused, what replaced it, and what to do.
    pub line: String,
}

/// Every renamed or retired setting whose env var is still set. (A leftover
/// `config.json` key is an unknown key, which `user_files` refuses.)
pub fn retired_env_leftovers(env: &dyn Fn(&str) -> Option<String>) -> Vec<RetiredLeftover> {
    let retired = RETIRED_SETTINGS.iter().filter_map(|r| Some((r.key, r.env?, r.env_policy, r.line.to_string())));
    retired
        .filter_map(|(key, var, policy, what)| {
            let v = env(var).filter(|v| !v.trim().is_empty())?;
            let found_in = format!("env var {var} ({v})");
            let verdict = match policy {
                LeftoverPolicy::Refuse => "is refused",
                LeftoverPolicy::Warn => "is ignored",
            };
            Some(RetiredLeftover { setting_old_key: key, policy, line: format!("{found_in} {verdict}: {what}"), found_in, value: v })
        })
        .collect()
}

/// What the CLI-entry check refuses with: every leftover env var whose policy
/// is [`LeftoverPolicy::Refuse`], one operator line each. Its `Display` is the
/// whole message; `doctor` and `config` are the only commands that run past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredEnvRefusal(pub Vec<RetiredLeftover>);

impl std::fmt::Display for RetiredEnvRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refusing to start: bad config")?;
        for l in &self.0 {
            write!(f, "\n  {}", l.line)?;
        }
        Ok(())
    }
}

impl std::error::Error for RetiredEnvRefusal {}

/// The `~/.darkmux/config.json` document. All fields optional + skipped when
/// `None`, so a fresh/empty config serializes to `{}` and any field absent
/// from the file falls through to its env/built-in default at the accessor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DarkmuxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,

    // ── Provenance / identity ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,

    // ── External tooling ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lms_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lmstudio_url: Option<String>,

    // ── Sections ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirs: Option<DirsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis: Option<RedisConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeBehaviorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetConfig>,
    /// (#2706) The battery-charge policy gate — see [`PowerConfig`]'s own
    /// doc. Top-level rather than under `runtime` because it governs
    /// whether work STARTS at all, not how the runtime behaves once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerConfig>,
    // Serde field name stays `mission` — only the Rust type was renamed
    // MissionConfig -> MissionBoardConfig (#1284; see that struct's doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission: Option<MissionBoardConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio: Option<RadioConfig>,
    /// (#1685) The `gh`-verb allowlist — see [`CmdConfig`]'s own doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<CmdConfig>,
    /// (#2093) Flow-record hooks — the filter → HTTP-POST outcome sink. See
    /// [`HooksConfig`]'s own doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfig>,
    /// (#2765) Where the `darkmux serve` daemon listens — see
    /// [`ServeConfig`]'s own doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<ServeConfig>,

    /// (#1475 packet 1) The machine-local **role → profile** map — the binding
    /// that welds an abstract role id (e.g. `judge`, `probe-high`) to a
    /// role-agnostic profile (e.g. `qwen35b`) on THIS machine. Many roles may
    /// name one profile. Deliberately lives in `config.json` (machine config),
    /// NOT in `profiles.json`, so profiles stay pure, reusable model configs.
    ///
    /// Resolution is `role -> this map -> profile -> model`; an unmapped role
    /// falls back to `default_profile` (the fresh-user single-model floor). A
    /// mapping to a profile name absent from the registry is a **loud** doctor
    /// warning + a clear resolution error (config-leniency contract 7: semantic
    /// validation at resolution + doctor, never the hot load path), never a
    /// silent fallback. `darkmux init` writes it as a visible empty `{}` per the
    /// visible-defaults doctrine, so the surface is discoverable and one
    /// `darkmux config set role_profiles.<role> <profile>` from bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_profiles: Option<BTreeMap<String, String>>,

    /// Forward-compat overflow — unknown top-level keys land here and
    /// re-serialize flat (a newer config read by an older binary).
    #[serde(flatten)]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Directory/path overrides. Each layers `env(DARKMUX_*) > config.dirs.X >
/// the `DarkmuxPaths` built-in` at the accessor (path unification lands in
/// #661 Slice 3). Values support `~` expansion.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DirsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub flows: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub audit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub templates: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub fleet_file: Option<String>,
    /// (#1585) Where lab-run artifacts live — the scan root behind `/lab/runs`
    /// and the lab arm of `/runs`.
    ///
    /// Added late, and for a reason worth keeping: this was the ONE directory
    /// setting with an env var (`DARKMUX_LAB_DIR`) and no config tier, because
    /// #1247 made the lab lens deliberately opt-in while lab was a SEPARATE
    /// side-lens — unset then honestly meant "you aren't using the lab lens."
    /// #1508 promoted lab into `/runs`, the unified read-model, which silently
    /// changed what unset MEANS: one of three sources missing from the primary
    /// view, with nothing saying so. 247 real runs went invisible. An
    /// optionality that is fine for a side-lens is a data-completeness hole
    /// once the same source feeds a consolidated view.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub lab: Option<String>,
    /// (#2265) Where finding records live — `<root>/findings` by default, one
    /// `<execution>/<seq>/finding.json` per accepted `create_finding` call.
    /// The flow stream stays the audit trail; this directory is the queryable
    /// copy `finding list` / `finding show` read, the same way roles are JSON
    /// on disk rather than a derived-only view.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub findings: Option<String>,
    /// (#2265) Where mod records live — `<root>/mods` by default, one
    /// `<key>/mod.json` per `mod create`, plus that mod's own `attachments/`.
    /// A mod is a KIT: instructions plus data, opaque to darkmux. Its key is
    /// MINTED per mod, never derived from a finding, so two agents proposing
    /// different changes for the same observation produce two records rather
    /// than the second overwriting the first.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mods: Option<String>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The shipped default for `redis.maxlen` (`XADD MAXLEN ~ N` retention) — the
/// value BOTH `Config::with_defaults()` (what `darkmux init` writes into
/// `config.json`) and `config_access::redis_maxlen()`'s built-in tier resolve
/// to, so an operator who has never touched retention gets exactly this number
/// through either path.
///
/// (#1715) Named rather than repeated as a literal because a DOWNSTREAM
/// invariant now rests on it: `darkmux-flow`'s near-maxlen warning is silent
/// only while this default is `>= FLOW_READ_CAP_RECORDS` (at or above the read
/// cap, raising retention buys a reader nothing, so the warning would be
/// unactionable — the permanent-warning bug #1715 removed). That relationship
/// is pinned by a `const _: () = assert!(...)` sitting beside
/// `FLOW_READ_CAP_RECORDS` in `darkmux_flow::status`, which reads this
/// constant: moving either number without the other FAILS THE BUILD instead of
/// silently reviving the warning for every operator on the shipped default.
pub const DEFAULT_REDIS_MAXLEN: usize = 10_000;

/// The shipped default for `redis.telemetry_maxlen`: the retention of the hub's
/// SECOND stream, `<redis.stream>:telemetry`, which carries only the machine
/// samples (`machine.telemetry`). Telemetry is most of the hub's records by
/// volume, so on one shared stream it flushed work records out of their
/// window (#2101: a relayed run's usage was gone from the hub within two
/// days). It has its own cap so neither kind can evict the other.
pub const DEFAULT_REDIS_TELEMETRY_MAXLEN: usize = 10_000;

/// The Redis flow-coordination sink — a **feature block gated by `enabled`**,
/// not by field-presence. `darkmux init` writes the whole block with
/// `enabled: false` and every connection knob populated to its sensible
/// default, so the operator sees the full surface and turns it on by flipping
/// one field (the knobs are already there to tweak). The on/off *gating* wires
/// in #661 Slice 5; this is the visible schema + the written defaults.
///
/// The **password is NEVER here** — it lives in the macOS Keychain (item
/// `darkmux-redis`), assembled at runtime (Slice 5). `DARKMUX_REDIS_URL` (full
/// URL, password inline) still wins as the env override regardless of `enabled`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RedisConfig {
    /// The gate: `true` → assemble + connect; `false`/absent → off (unless the
    /// `DARKMUX_REDIS_URL` env override is set). Declared first so it reads at
    /// the top of the block.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub db: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub maxlen: Option<usize>,
    /// Retention of the machine-telemetry stream (`<stream>:telemetry`),
    /// separate from `maxlen` so samples never evict work records. `0` is
    /// unbounded, as for `maxlen`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub telemetry_maxlen: Option<usize>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// The hash-chained audit sink (#163) — a **feature block gated by `enabled`**,
/// same pattern as `RedisConfig`. `darkmux init` writes it with `enabled:
/// false` + the default `dir`. Today's env equivalent (`DARKMUX_AUDIT_DIR`
/// presence) still wins as the override; the config gating wires in #661.
/// POSIX-only sink (the env var is recognized but skipped on Windows).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuditConfig {
    /// The gate: `true` → the AuditFileSink writes a hash-chained (BLAKE3)
    /// per-day JSONL that `darkmux flow integrity-check` walks to detect chain
    /// breaks; `false`/absent → off. Declared first.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub dir: Option<String>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Per-dispatch runtime behavior knobs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuntimeBehaviorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub inactivity_timeout_seconds: Option<u64>,
    /// (#1276) Bounded model-load/unload phase for gestalt host-port calls:
    /// the `LmsHost` adapter hard-kills the `lms load`/`lms unload` child at
    /// expiry and surfaces a typed timeout naming the phase — a wrong model
    /// id can no longer hang a dispatch until the workflow's outer kill.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub model_load_timeout_seconds: Option<u64>,
    /// (#2310/#2361, swarm finding S4-4) Bound on ONE operator-supplied
    /// shell command a step runs — `mods.gate`'s `test_command` and
    /// `procedural.shell`'s `command`. The child is spawned in its own
    /// process group, polled, and killed (the whole group) at expiry, so a
    /// hung suite can no longer pin a mission open with no way to interrupt
    /// it. Sibling of `model_load_timeout_seconds`, which bounds a host
    /// model load the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub step_command_timeout_seconds: Option<u64>,
    /// (#2678) Wall-clock bound, in seconds, on ONE `darkmux mission
    /// launch` run as a WHOLE — distinct from `inactivity_timeout_seconds`
    /// (per-dispatch, resets on activity) and `step_command_timeout_seconds`
    /// (per shell command): this one bounds the RUN's total elapsed time
    /// regardless of how much progress any individual dispatch is making.
    /// Nothing in the scheduler enforced this before #2678 — a grinding
    /// `mission launch review` was stopped only by the CI job's own
    /// `timeout-minutes`, which kills the whole process tree with nothing
    /// rendered. `0` (the default) means UNBOUNDED, the same reading every
    /// other darkmux zero-knob has (see `step_command_timeout_seconds`
    /// above) — this must never become a surprise new limit on an existing
    /// mission. Enforced by a background watchdog `mission_launch.rs::
    /// launch` spawns right after arming its signal handlers
    /// (`launch_guard::spawn_wall_clock_watchdog`, a no-op when this reads
    /// `0`): at expiry it calls the SAME `darkmux_types::interrupt::
    /// mark_interrupted()` a real SIGTERM would, so a bound-triggered stop
    /// renders through the identical graceful-abort path a real operator
    /// signal already uses (`launch_guard::wall_clock_exceeded` is the one
    /// extra bit that lets the abort writer report the honest, specific
    /// reason instead of collapsing both into "aborted").
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mission_wall_clock_timeout_seconds: Option<u64>,
    /// (#2394) How many DISPATCH-FREE steps the scheduler runs
    /// concurrently — `procedural.shell`, `procedural.noop`, `mods.gate`,
    /// `records.gather`, `deliver.github_review`, every step whose
    /// `StepKind::seat` claims `SeatClaim::NoModel`. Its own ceiling
    /// because these consume no model: an endpoint's `limits.concurrent_calls`
    /// exists to protect that endpoint's rate limit and a shell command is not
    /// one. Not unbounded, though — `mods.gate` runs a `test_command` per
    /// mod, and each such step is individually bounded by
    /// `step_command_timeout_seconds` above, not by anything global.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub dispatch_free_concurrency: Option<u32>,
    /// (#2772) Explicit override for how many LOCAL-MODEL dispatches run
    /// at once against ONE resident instance — its own ceiling, distinct
    /// from `dispatch_free_concurrency` above (that one bounds steps that
    /// consume no model at all) and from an endpoint's `limits.concurrent_calls`
    /// (that endpoint's rate limit). Absent = the runtime derives it PER
    /// INSTANCE, per WAVE, from that resident's own declared `PARALLEL` as
    /// `lms ps --json` reports it, falling back to 1 (never unbounded)
    /// when that can't be read — see
    /// `config_access::local_dispatch_concurrency`'s own doc for the full
    /// measured reasoning. Deliberately absent, not a written literal: a
    /// fixed number here would freeze a value that is supposed to track
    /// whichever model happens to be resident, so — same reasoning as
    /// `max_turns`/`max_tokens`/`max_stall_recoveries` above — `init`
    /// never writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub local_dispatch_concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens: Option<u32>,
    /// (#1221) Per-CALL completion-token cap (reasoning + content of one
    /// model turn). Absent = the runtime's built-in default (10000) — which
    /// E19 measured truncating PRODUCTIVE reasoning on thinking-family
    /// models, so benches raise it explicitly per run.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_tokens_per_call: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub reasoning_checkpoint_interval_tokens: Option<u32>,
    /// (#2171) The GENERATION check-in — bounds every call that does NOT
    /// carry the reasoning bound above, not just reasoning ones. Absent =
    /// the runtime's built-in default (4000). See `loop_runner::
    /// GENERATION_CHECKPOINT_INTERVAL`'s doc for the incident this fixes
    /// (a non-thinking model's single 10000-token call outlasting the
    /// inactivity watchdog).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub generation_checkpoint_interval_tokens: Option<u32>,
    /// (#2190) Per-dispatch budget for intra-turn stall recoveries — how
    /// many times the runtime drops a useless turn (empty `tool_calls`, or
    /// a runaway-reasoning cut) and nudges before escalating out of local-
    /// tier. Absent = the runtime's built-in default (`MAX_STALL_RECOVERIES`
    /// = 2). Surfaced live: a Devstral dispatch hit the SAME shape three
    /// turns running at ~19k context and died with a hard-coded budget of
    /// 2 with no operator override available.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_stall_recoveries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub strict_selection: Option<bool>,
    /// (#1548) Whether the runtime injects feedback (nudge) messages into a
    /// struggling dispatch's next turn. Resolved via
    /// `config_access::feedback_injection()` — env, then this field, then
    /// `true` by default; the docker-spawn site forwards the resolved value
    /// into the container, which is the ONLY thing `runtime/src/feedback.rs`
    /// actually reads (it can't depend on `config_access` directly — the
    /// runtime crate isn't a workspace member).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub feedback_injection: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub default_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub check_updates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub daemon_cors_origins: Option<String>,
    // (#1011) Fraction (0–1) of the dispatch model's context window budgeted for
    // the injected-context blocks (detector cautions + authored lessons + prior
    // corrections) in the coder brief. A fraction auto-scales across profiles
    // from one value — a large-window profile gets proportionally more room.
    // `env(DARKMUX_INJECTED_CONTEXT_FRACTION) > this > 0.15`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub injected_context_fraction: Option<f64>,
    /// (#1698 Packet B2, #1684 session-hygiene addendum) How many CONSECUTIVE
    /// idle minutes `darkmux acp` waits — zero sessions with any live
    /// activity, zero commands/routes in flight — before self-exiting
    /// (`std::process::exit(0)`). Lives under `runtime` rather than a new
    /// `acp{}` block or the `radio{}` block: idle self-exit is a PROCESS
    /// lifecycle behavior of the `darkmux acp` binary invocation itself, not
    /// specific to the radio no-slash channel (a session doing nothing but
    /// slash-command dispatches idles out identically) — the honest home is
    /// alongside the other per-dispatch/per-process runtime budgets
    /// (`inactivity_timeout_seconds`, `model_load_timeout_seconds`). Default
    /// 30 (documented in the issue's session-hygiene addendum: "most swaps
    /// find no process running").
    #[serde(default, skip_serializing_if = "Option::is_none")] pub acp_idle_exit_minutes: Option<u64>,
    /// (#2094) A global rest, in milliseconds, the internal runtime sleeps
    /// between inference turns on EVERY local dispatch — not a per-workload
    /// "duty cycle" (that shape was proposed for the crawler, #1959, and
    /// rejected by the operator as both too narrow and the wrong grain: heat
    /// is about sustained load, and the right rest is after each inference
    /// burst, on every dispatch). Resolved HOST-side via
    /// `config_access::turn_delay_ms()` and forwarded into the container as
    /// `-e DARKMUX_TURN_DELAY_MS=<n>` — the #1548 pattern (the runtime crate
    /// can't depend on `config_access`; the host does the tier resolution
    /// and always forwards its result). Default `0` (no rest — the pre-
    /// existing behavior). Applied in `runtime/src/loop_runner.rs` between
    /// turns, never before the first turn; clamped below the inactivity
    /// timeout with a loud warning if configured at or above it.
    ///
    /// (#2094 finding 10) "Between turns" means between LOGICAL turns, not
    /// between every individual inference call: a checkpoint continuation
    /// (the model resuming mid-thought after hitting the per-call token
    /// cap) and the compactor's own call are both part of the SAME turn by
    /// design (`resuming_after_checkpoint` / the compactor's separate
    /// client), so neither one triggers a rest — only the boundary between
    /// one completed turn and the next model-facing turn does. This keeps
    /// the knob's cost proportional to actual GPU inference bursts rather
    /// than to how finely one burst happens to get checkpointed. Local
    /// dispatches only. The unmanaged-endpoint single-shot path never forwards this at
    /// all (it never builds a `DockerRunConfig`). An agentic-REMOTE
    /// dispatch (a tool-granting role on an endpoint profile, which DOES
    /// run the same container/`loop_runner.rs` local dispatches use) is
    /// force-overridden to `0` HOST-side regardless of this setting
    /// (`dispatch_internal.rs`'s `DockerRunConfig` construction, #2094
    /// finding 4) — an endpoint has no local GPU on this host to rest, so
    /// honoring an operator's configured rest there would only add real
    /// latency the per-execution remote token allowance pays for nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub turn_delay_ms: Option<u64>,
    /// (#2107, #1833, #2108) Cadence, in milliseconds, of `darkmux serve`'s
    /// daemon-side continuous host sampler — the background thread that
    /// reads cpu/mem/gpu/power/thermal (via its OWN privately-owned
    /// `darkmux_crew::host_probe::HostProbe`, constructed inside the
    /// sampler thread — NOT `telemetry_sampler::sample_host`'s shared,
    /// `Mutex`-guarded singleton; the two are separate `HostProbe`
    /// instances with separate cadences, on purpose, the same way
    /// `dispatch_internal`'s per-dispatch sampler owns its own) into an
    /// in-memory ring so the machine stats drawer (phone bottom tab,
    /// desktop modal) has live numbers between dispatches
    /// instead of reading "idle · no samples" until one starts. `0`
    /// disables the sampler entirely (an explicit opt-out, mirroring
    /// the pre-4.0 per-execution remote budget's `0`-means-hard-off convention).
    /// The sampler writes NO flow records (CLAUDE.md "the observer must not
    /// join the observed" — zero model dispatches, and this must not double
    /// the fleet stream's size); it only feeds the `/machine/resources`
    /// `load` block, which stamps its own measured cost
    /// (`sampler_cost_ms_mean`) and the measured (not nominal) sample
    /// interval into the payload.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub host_sampler_interval_ms: Option<u64>,
    /// (#2928) Cadence, in milliseconds, of the LIVE channel: how often a
    /// running execution's model state (and every utility job's start and
    /// end) is sampled and pushed to the local `darkmux serve` daemon's
    /// viewers. The samples never reach a flow record, the day file, Redis
    /// or the audit chain; the durable `dispatch.turn.heartbeat` stays at
    /// its own 2 s coalescing. `0` turns the channel off. Resolved (and
    /// clamped to 100..=1000) by `config_access::live_cadence`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub live_sample_ms: Option<u64>,
    /// (#2110/#2109) The thermal governor + breaker's tuning block. See
    /// [`ThermalConfig`]'s own doc for the pause/resume/breaker semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub thermal: Option<ThermalConfig>,
    /// (#2846) Per-detector policy block. The runtime carries several
    /// detectors that WATCH a dispatch and ACT on what they find; this is
    /// where an operator says how much authority each one has. See
    /// [`DetectionConfig`] and [`DetectionPolicy`].
    #[serde(default, skip_serializing_if = "Option::is_none")] pub detection: Option<DetectionConfig>,
    /// (#2653) Retention window, in hours, for
    /// `<darkmux-home>/liveness/<pid>.log` per-dispatch heartbeat files
    /// (`darkmux_types::dispatch_liveness`) — a file older than this is
    /// pruned the next time any dispatch writes a marker. Absent = the
    /// module's own built-in default (168h / 7 days). This field exists for
    /// OPERATOR VISIBILITY and for `darkmux doctor`'s resolved-value row
    /// (`config_access::liveness_retention_hours`); `dispatch_liveness`
    /// itself deliberately does NOT resolve through `config_access` (see
    /// that module's doc — it must work before config/Redis/audit/flow are
    /// touched), so it does its own minimal raw peek at this same key.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub liveness_retention_hours: Option<u64>,
    /// (5.0) Print informational stderr lines (liveness markers, sink
    /// banners, dispatch progress headers) even on an interactive terminal.
    /// Absent or `false`: they print only when stderr is not a terminal (logs,
    /// pipes), under `--verbose`, or with `DARKMUX_VERBOSE=1`. Warnings and
    /// errors always print. Resolved by `darkmux_types::diagnostics`, which
    /// peeks this key raw because the liveness floor runs before config.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub verbose: Option<bool>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2110/#2109) The thermal governor's pace/breaker tuning — a **feature
/// block gated by `enabled`**, same shape as `RedisConfig`/`AuditConfig`,
/// but defaulting to **`enabled: true`**: unlike an optional integration
/// with an external system, this is hardware-safety behavior on the
/// operator's own machine, so `darkmux init` writes it already on (mirrors
/// `strict_selection`/`check_updates`, not `redis`/`audit`).
///
/// **Governor (#2110):** on each host thermal sample, when the OS-reported
/// state is at or above `pause_at`, the host writes `<out_dir>/pace.json`
/// (the mounted `/darkmux-out` bookkeeping dir, NOT `/workspace` — a crawl
/// unit mounts that read-only and a coder run's workspace is the
/// operator's own repo tree; `runtime/src/pace.rs`'s schema, #2114) with
/// `pause: true, reason: "thermal", state: "<state>"` — the in-flight
/// dispatch rests at its next turn boundary. The pause clears
/// (`pause: false`) once the state has held at or below `resume_at` for
/// `resume_hold_ms` continuously (hysteresis — no flapping on a state that's
/// bouncing right at the threshold). If a single pause episode runs past
/// `max_pause_ms` without recovering, the governor hands off to the breaker
/// instead of resting forever.
///
/// **Breaker (#2109):** at `critical`, or when `cpu_speed_limit_pct` drops
/// below `min_cpu_speed_limit_pct`, the host writes the pace file with
/// `pause: true, reason: "thermal-critical"` — the in-flight unit pauses
/// with its checkpoint persisted (#2114), never killed — and, for a crawl
/// mission, also drops the crawl's `STOP` file. **Read side (#2454):**
/// `dispatch.unit`'s own step kind checks that file (via
/// `thermal_governor::stop_file_path_from_record_context`, never a raw
/// re-join) before preparing each unit's dispatch, so a unit that has not
/// started yet is skipped rather than dispatched — reported in the crawl's
/// summary as `stopped_by: "thermal"` with a per-unit `thermal_stop` result,
/// never silently folded into `error`. A unit already in flight is
/// unaffected here; that one keeps resting on the pace file above. Known
/// gap: a crawl spec with an explicit `root:` override cannot be
/// reconstructed from the dispatch's `record_context` alone, so the STOP
/// file is neither written nor read for that case (see
/// `stop_file_path_from_record_context`'s own doc) — symmetric, so such a
/// crawl gets no breaker rather than a half-working one.
///
/// **Scope of the STOP file (#2454).** Nothing removes it — not the
/// retired launcher, not the governor — and its path
/// (`<root>/crawl/<manifest>/STOP`) is keyed to the WORKSPACE, not to a
/// run. So the breaker STAMPS the mission it fired in
/// (`thermal_governor::stop_file_body`) and a unit honors the file only
/// when it names its OWN mission: the stop binds the run the breaker was
/// protecting, and a LATER crawl on the same workspace is not refused by
/// an older run's thermal event. Without that scoping one transient
/// thermal event would disable every future crawl on that workspace
/// permanently. A STOP file naming NO mission — hand-written, or from a
/// build older than #2454 — is still honored by every mission, and only a
/// human removes it; the skip message names the exact `rm` for it.
///
/// Resume within a run is the operator's call (`darkmux dispatch
/// --resume`); the breaker does not un-pause itself on recovery.
/// (#2846) What authority one detector has over the dispatch it watches.
///
/// A boolean was the first shape proposed and was rejected: the runtime has
/// FOUR detectors (degeneracy, tool-call cycles, repeated reasoning,
/// consecutive tool failures), and "on/off" cannot express the state that is
/// actually most useful for diagnosing one of them, which is *keep measuring
/// but stop acting*. A policy enum says that directly and leaves room for
/// per-detector policies that are not yet written.
///
/// Only `degeneracy` reads this today. The other three detectors are
/// deliberately NOT given config keys until they read them: a key that
/// nothing consumes is indistinguishable, to an operator, from one that does.
/// Deliberately does NOT derive `Deserialize`, matching [`FleetMode`]'s
/// convention in this same file and for the same reason, which a review
/// proved is not theoretical here (#2846):
///
/// `DarkmuxConfig::load_from` ends in `unwrap_or_default()`, so ANY
/// deserialization error discards the ENTIRE config silently. A derived
/// enum hard-errors on an unknown variant, which meant one typo in this one
/// value (`"obserev"`, or merely `"Observe"` capitalized) dropped
/// `machine_id`, the Redis sink, the audit sink, the thermal block and every
/// other setting back to built-in defaults with no error anywhere in the
/// path. The `#[serde(flatten)] extras` maps guard unknown KEYS and do
/// nothing for unknown VALUES.
///
/// So the field on [`DetectorConfig`] is a plain `Option<String>` and the
/// token is resolved at the accessor, where an unrecognized value can be
/// reported against the raw string instead of taking the document with it.
///
/// (#2947, operator 2026-09-27) The values NAME THE ACTION: `off`, `record`,
/// `warn`, and the rule's own verb (`conclude` for the degeneracy
/// detector: it closes the model's thought so it answers from what it has,
/// and escalates if the output keeps repeating; nothing is discarded).
/// `enforce` and `observe` are retired in 4.0: `enforce` hid different
/// actions per rule, and `observe` read like "warns" when it only recorded.
/// Both are refused with the new word (`config_enum!`'s `retired` list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetectionPolicy {
    /// Do not run the detector at all. Zero CPU, measures nothing.
    Off,
    /// Detect and RECORD, silently, what concluding would have done; never act
    /// and never warn. The setting that makes a controlled comparison
    /// possible: the check-in cadence, the per-call token cap and therefore
    /// the usable prompt budget are all unchanged, so the only variable is
    /// whether the verdict is obeyed. (Was `observe`.)
    Record,
    /// Detect, and on a finding SURFACE a warning (a stderr line for the
    /// dispatch, a `dispatch.degeneracy.warning` flow record the viewer
    /// shows, and the run envelope's `degeneracy_warnings`), without
    /// concluding anything.
    Warn,
    /// Detect, and on repeating output conclude: close the model's thought
    /// so it answers from what it has, escalating if it keeps repeating.
    /// The shipped behavior, unchanged. (Was `enforce`.)
    #[default]
    Conclude,
}

impl DetectionPolicy {
    /// Whether the detector should run its measurement.
    pub fn measures(self) -> bool {
        !matches!(self, DetectionPolicy::Off)
    }
    /// Whether a finding may change what the dispatch does.
    pub fn acts(self) -> bool {
        matches!(self, DetectionPolicy::Conclude)
    }
    /// Whether a finding is surfaced as a warning without acting.
    pub fn warns(self) -> bool {
        matches!(self, DetectionPolicy::Warn)
    }
    pub fn as_str(self) -> &'static str {
        crate::config_enum::ConfigEnum::token(self)
    }
}

// (#2947) The value table: tokens, meanings, retired spellings, and
// (through the macro's exhaustive match) the parser. An unknown or retired
// value is refused at the accessor
// (`config_access::detection_degeneracy_policy`), never resolved to a
// default: see `config_enum`'s module doc for the rule.
crate::config_enum!(DetectionPolicy, "detection policy", [
    Off = "off" => "the detector does not run (zero CPU, measures nothing)",
    Record = "record" => "measure and record what concluding would have done, silently; never act",
    Warn = "warn" => "measure; on a finding surface a warning (stderr, flow record, envelope); never conclude",
    Conclude = "conclude" => "measure; on repeating output close the thought so the model answers, escalating if it keeps repeating (the shipped behavior)",
], retired: [
    "enforce" => Conclude,
    "observe" => Record,
]);

/// (#2947 review C2) The flow-record `level` a hook rule's `match.level`
/// names. The config vocabulary of `darkmux_flow::Level` (declared here so
/// the registry, which lives in this leaf crate, can hold it; darkmux-flow's
/// `hook_match_vocabulary_matches_the_flow_schema` pins the two together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

crate::config_enum!(HookLevel, "flow record level", [
    Error = "error" => "records at error level",
    Warn = "warn" => "records at warn level",
    Info = "info" => "records at info level",
    Debug = "debug" => "records at debug level",
    Trace = "trace" => "records at trace level",
]);

/// (#2947 review C2) The flow-record `category` a hook rule's
/// `match.category` names; the config vocabulary of
/// `darkmux_flow::Category` (same arrangement as [`HookLevel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCategory {
    Work,
    Machinery,
    Audit,
    Review,
    Telemetry,
}

crate::config_enum!(HookCategory, "flow record category", [
    Work = "work" => "records of the work itself (dispatches, missions)",
    Machinery = "machinery" => "records of darkmux's own machinery",
    Audit = "audit" => "audit-trail records (decisions, notes)",
    Review = "review" => "review records",
    Telemetry = "telemetry" => "per-dispatch instrument samples",
]);

/// (#2846) One detector's settings. Split per detector rather than one global
/// policy because the detectors are independent: an engine whose reasoning
/// repeats is not necessarily one whose tool calls cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct DetectorConfig {
    /// The raw token. `Option<String>`, not `Option<DetectionPolicy>` — see
    /// [`DetectionPolicy`]'s own doc for the config-discarding failure that
    /// forces this. Resolved via
    /// `config_access::detection_degeneracy_policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub policy: Option<String>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2846) The detector block. Only `degeneracy` is wired today.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct DetectionConfig {
    /// The repeated-output gate (`runtime/src/reasoning_loop.rs`). Under
    /// `observe` the check-in still fires on exactly the same cadence and the
    /// tail ratio is still computed and recorded; what stops is the cut.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub degeneracy: Option<DetectorConfig>,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    #[schemars(skip)]
    pub extras: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThermalConfig {
    /// The gate: `true`/absent → governor + breaker active; `false` → the
    /// sampler still reads the OS thermal state (for telemetry) but never
    /// writes the pace file or the crawl `STOP` file. Declared first.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    /// OS thermal state (`nominal`/`fair`/`serious`/`critical`) at or above
    /// which the governor pauses. Default `"serious"`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub pause_at: Option<String>,
    /// OS thermal state at or below which the governor is eligible to
    /// resume, once held for `resume_hold_ms`. Default `"fair"`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub resume_at: Option<String>,
    /// How long (ms) the state must hold at or below `resume_at`, continuously,
    /// before the governor clears the pause. Default `60000` (60s).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub resume_hold_ms: Option<u64>,
    /// Cap (ms) on one continuous pause episode before the governor hands
    /// off to the breaker instead of resting indefinitely. Default `900000`
    /// (15 minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_pause_ms: Option<u64>,
    /// Breaker floor: `cpu_speed_limit_pct` (from `IOPMCopyCPUPowerStatus`)
    /// below this triggers the breaker even if the named thermal state
    /// hasn't reached `critical` yet. Default `50`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub min_cpu_speed_limit_pct: Option<u64>,
    /// (#2110/#2109 review finding 7) Consecutive samples below
    /// `min_cpu_speed_limit_pct` required before the breaker trips on that
    /// signal — a lone sample below the floor is noise (a brief DVFS dip),
    /// not a sustained condition. Does NOT apply to the `critical` state
    /// check, which still trips immediately. Default `3`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub speed_limit_hold_samples: Option<u32>,
    /// (#2774 tier 2) Base duty-cycle turn delay (ms) applied between turns
    /// once the state has held at or above `resume_at` (and below
    /// `pause_at`) for `resume_hold_ms` — the SAME hysteresis hold used to
    /// clear a pause, reused here for symmetry rather than adding a second
    /// hold knob for what is conceptually the same "sustained at fair"
    /// question. Default `15000` (15s), per the operator's own suggestion.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub duty_delay_ms: Option<u64>,
    /// (#2774 tier 3) Multiplier applied to the duty-cycle delay every time
    /// a `serious` episode recovers back to `resume_at` — one-way for the
    /// life of the run; recovering never restores the pre-doubling value.
    /// Default `2`. A configured `0` is coerced to `1` (no growth) rather
    /// than zeroing the delay on the first escalation, which would defeat
    /// the ratchet entirely — see `thermal_ratchet_factor`'s own doc.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub ratchet_factor: Option<u32>,
    /// (#2774 tier 4) How many `serious` EPISODES (transitions into
    /// `serious`, not samples at it) this run tolerates before the Nth one
    /// escalates straight to an indefinite, operator-gated pause instead of
    /// the ordinary tier-3 pause/resume. Default `2`. `0` means unbounded —
    /// never escalate to tier 4 — matching darkmux's existing convention
    /// that a `0` bound means unbounded, never "instantly" (see
    /// `runtime.step_command_timeout_seconds`'s own doc for the same rule
    /// applied elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub episode_threshold: Option<u32>,
    /// (#2774 tier 4) Whether the episode-count escalation is active at
    /// all. `false` keeps tiers 1-3 (and the pre-existing tier-5-adjacent
    /// breaker) exactly as before, with every `serious` episode handled as
    /// an ordinary tier-3 pause/resume regardless of how many have
    /// happened this run. Default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub tier4_enabled: Option<bool>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2706) **The battery-charge policy** — three knobs, and the start
/// policy is deliberately separate from the in-flight policy.
///
/// ```json
/// "power": {
///   "min_battery_pct": 50,
///   "refuse_start_below_min": true,
///   "pause_running_below_min": true
/// }
/// ```
///
/// Refusing to START and interrupting work IN PROGRESS are different
/// decisions with different costs — an operator may reasonably want one
/// without the other — so they are two config items rather than one mode.
/// Written visibly by `darkmux init` with its defaults populated, so the
/// surface is discoverable and one edit from changed.
///
/// **A machine with no battery is never gated.** This is the sharpest
/// correctness requirement of the feature and it is enforced at the
/// POLICY, not here: `power_policy::start_decision` and
/// `power_policy::BatteryGovernor` are inert on an absent reading — not
/// "treated as 0%", not "treated as 100%", not defaulted either way. In
/// this fleet the always-on hub is a desktop and the battery-bearing
/// laptop is the inference peer, so a gate that misread absence would
/// either block the hub permanently or silently disable itself on the one
/// machine it exists to protect.
///
/// **Describing versus adjudicating.** A gate is an ACTION, so this stays
/// inside darkmux's describe-don't-adjudicate posture only under a strict
/// reading: darkmux enforces a threshold THE OPERATOR WROTE and never
/// invents a policy of its own. Every refusal names what was observed, the
/// floor, and the config field that produced the decision — so the
/// operator never has to wonder where it came from. It does not say the
/// battery is unhealthy, does not recommend a charge policy, and does not
/// suggest a different threshold.
///
/// Deliberately NOT an `enabled`-gated feature block like `redis`/`audit`:
/// `refuse_start_below_min` and `pause_running_below_min` ARE the gates,
/// one per policy, and a third master switch would make "off" expressible
/// two ways.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PowerConfig {
    /// The charge floor, in percent. Default `50`.
    ///
    /// `u64`, not `u8`, matching `runtime.thermal.min_cpu_speed_limit_pct`:
    /// the lenient-read contract says a hand-written out-of-range value must
    /// never fail the whole-config parse (which would brick every other
    /// setting). A `u8` field would reject `"min_battery_pct": 300` at
    /// DESERIALIZE time; the wide type accepts it and
    /// `config_access::power_min_battery_pct` clamps it at RESOLUTION time,
    /// which is where semantic validation belongs (config-leniency
    /// contract 7).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub min_battery_pct: Option<u64>,
    /// A new run will not start below the floor. Default `true`; set
    /// `false` to allow runs under the threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub refuse_start_below_min: Option<bool>,
    /// A run already in flight PAUSES when charge crosses the floor.
    /// Default `true`; set `false` to let it run to completion.
    ///
    /// Pausing means pausing: it composes with #2114's pace-file
    /// contract — checkpoint and resume, never a hard stop. A run type
    /// whose pause cannot be resumed refuses to pause and says so rather
    /// than pausing into a state it cannot leave (see
    /// `power_policy::BatteryEvent::PauseUnsupported`).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub pause_running_below_min: Option<bool>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// Fleet position (#933) — the machine's declared place in a multi-node fleet,
/// a `fleet{}` block beside `redis{}`/`audit{}`/`runtime{}`. The operator
/// **declares** `mode`; detection (a machine running Redis + the always-on
/// daemon looks like a hub) is only a `darkmux doctor` cross-check that flags
/// declared ≠ observed — never the source of truth (operator sovereignty).
/// The mode changes only which address a viewer link names (see
/// [`FleetMode`]); it gates no fleet feature. `darkmux init` writes
/// `mode: "standalone"` visible, so the fleet surface is discoverable and one
/// edit from `hub`/`peer`.
///
/// `mode` is stored as a **string, not a typed enum**, deliberately: the
/// lenient-read doctrine says a typo'd value must never fail the whole-config
/// parse (which would brick every setting). The raw token is kept so `darkmux
/// doctor` can flag it against what the operator actually wrote (#934);
/// `FleetMode::parse` does the typed interpretation at the accessor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FleetConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mode: Option<String>,
    /// (#2916) Which overlay network verifies a connecting machine. See
    /// [`FleetIdentityConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")] pub identity: Option<FleetIdentityConfig>,
    /// (#2916) The work-submission listener. See [`FleetListenerConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")] pub listener: Option<FleetListenerConfig>,
    /// (#2916) The receiver's allow-list. The map key is a LABEL: `machine
    /// trust` writes the peer's `machine_id` there, but a connection is
    /// matched to an entry by the entry's `node_id` alone, so renaming a
    /// machine never breaks its trust (the key is only what a refusal or a
    /// record calls the peer). Written by `darkmux machine trust` /
    /// `untrust`, never by `config set`: its `node_id` is resolved through
    /// the identity provider, never typed. An absent or empty map accepts no
    /// work.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub accept_work: Option<BTreeMap<String, AcceptWorkEntry>>,
    /// (#2916 stage 2) What this machine's fleet listener does with a job
    /// whose seat is busy: `refuse` or `queue`. See [`BusyPolicy`]. A
    /// string, read leniently and refused where it is consumed (#2947).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub busy_policy: Option<String>,
    /// (#3022) Fleet defaults, meaningful only on the machine that declares
    /// `fleet.mode` `hub`. See [`FleetDefaultsConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")] pub defaults: Option<FleetDefaultsConfig>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#3022) The defaults a fleet hub hands to the machines that have no
/// setting of their own. Meaningful only where `fleet.mode` is `hub`: a
/// machine that is not the hub carries no defaults on its card, and a card
/// that is not the hub's has its defaults refused by every reader. They
/// travel in the hub's machine card only, never through Redis.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FleetDefaultsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub radio: Option<FleetDefaultsRadioConfig>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#3022) The fleet default for radio. `answerer_profile` is a
/// `<profile>@<machine>` address: a bare profile name would be read against
/// each receiving machine's own registry, where it means something else.
/// Empty or absent means the hub states no default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FleetDefaultsRadioConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub answerer_profile: Option<String>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2916) The identity source for fleet work submission: the overlay
/// network whose own daemon answers "which node is on the other end of this
/// connection". `provider` is a VALUE (`"tailscale"` is the one this darkmux
/// knows); an unknown value is bad config (#2947): fleet work submission
/// refuses it at preflight on both sides, and `darkmux doctor` reports
/// Fail. Stored as a string, like `fleet.mode`, so a typo never fails the
/// whole-config parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FleetIdentityConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub provider: Option<String>,
    /// Path to the provider's command-line tool, when it is not on `PATH`
    /// under its usual name (a daemon started by launchd may have a short
    /// `PATH`). Absent = the provider's usual command name. Not written by
    /// `init`: a literal would be wrong on most machines.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub bin: Option<String>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2916) The dedicated work-submission listener `darkmux serve` opens
/// beside its viewer port. It binds only to the address the identity
/// provider reports for this machine (never `0.0.0.0`, never a LAN address),
/// so every connection it accepts can be asked "which node is this".
/// An `enabled`-gated feature block: `init` writes `enabled: false` with the
/// default port visible.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FleetListenerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    /// TCP port on the overlay address. Built-in default `8766`. The fleet
    /// uses ONE port: a sender dials the roster host of the target on its
    /// own resolved port, so set the same value on every machine.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub port: Option<u16>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2916) One allow-list entry: a machine this machine takes work from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct AcceptWorkEntry {
    /// The overlay network's stable id for the peer's node, resolved by
    /// `darkmux machine trust`. An entry without one never matches.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub node_id: Option<String>,
    /// The work-class profiles (this machine's own profile names) the peer
    /// may run here. A profile outside this list is refused, and so is a
    /// profile that resolves only to the machine's utility model.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub profiles: Option<Vec<String>>,
    /// The roles (this machine's role ids) the peer may dispatch here. An
    /// explicit list, and absent or empty means NONE: a role is a tool
    /// palette and a system prompt, so granting "any role" would grant every
    /// tool palette this machine has.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub roles: Option<Vec<String>>,
    /// Docker images the peer may name with `--image`, matched exactly. A
    /// job naming no image runs on darkmux's own pinned runtime image and is
    /// always allowed; any other image (a custom one, or a pull from an
    /// arbitrary registry) must be listed. Absent = none.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub images: Option<Vec<String>>,
    /// Whether the peer may name a working directory on this machine.
    /// `true` lets a job mount any directory under this machine's darkmux
    /// worktrees base READ-WRITE as its workspace (symlinks resolved; nothing
    /// outside the base). `false` or absent: a job carrying a `workdir` is
    /// refused. `workspace` grants a receiver PATH only. It never authorizes
    /// a fetch, a checkout or a push. Git handoff (#755) gets its own grant,
    /// and its checkouts live outside the worktrees base, so this grant
    /// cannot reach them.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub workspace: Option<bool>,
    /// The names of repos in this machine's future repo registry that the
    /// peer may hand work off against. Absent or empty means none.
    /// Reserved for git workspace handoff (#755); read by nothing in this
    /// darkmux.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub repos: Option<Vec<String>>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2765) Where the `darkmux serve` daemon listens — and, just as
/// importantly, where every CLIENT that looks for it expects to find it.
///
/// **The defect this block closes was an ASYMMETRY, not a missing knob.**
/// The port lived in exactly one place — the launch command — so a restart
/// that forgot `--port` (a reboot, a `brew services` restart, a hand-typed
/// `darkmux serve` while debugging) silently reverted to the built-in 8765
/// while whatever proxies the machine (`tailscale serve`, an nginx block)
/// still pointed at the port the operator had chosen. Nothing was
/// misconfigured and nothing reported an error: the daemon was healthy and
/// serving, just not where anything was looking. Observed 2026-09-16 —
/// a real dispatch printed `darkmux serve isn't reachable on
/// 127.0.0.1:8765` while the daemon was answering on the operator's
/// configured port, so flow records did not stream to the live view.
///
/// So a fix that only moved the SERVER's default here would have fixed
/// nothing. Both halves resolve through `config_access::serve_port` /
/// `serve_bind`: the `serve` command, and every client that probes for the
/// daemon (the per-dispatch reachability nudge, `darkmux doctor`'s daemon
/// check, the portless-address default for a peer base URL).
///
/// **Not an `enabled`-gated feature block.** The redis/audit/hooks pattern
/// exists for integrations that are OFF until opted into; a daemon is not
/// a feature you turn on, it is a process you start, and this block only
/// says where it listens when you do. There is nothing for an `enabled`
/// field to mean here that `darkmux serve` not running does not already.
///
/// **`serve.token` is deliberately absent.** The daemon's bearer token is a
/// SECRET and lives in the macOS Keychain (item `darkmux-serve-token`); the
/// non-secret gate for reading it is `serve.token_keychain`. `config set`
/// refuses `serve.token` with the `security add-generic-password` form.
///
/// **Two auth switches, two surfaces (#2988).** The token is the EXECUTION
/// credential: the fleet listener requires it on every work submission,
/// whatever `read_auth` says. `read_auth` alone decides whether READS (the
/// viewer and every JSON route) need it from a request that is not from
/// this machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ServeConfig {
    /// TCP port the daemon listens on. Built-in default `8765`. A
    /// `--port` on the command line still wins outright, matching the
    /// CLI-beats-config convention every other flag here follows.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub port: Option<u16>,
    /// Address the daemon binds. Built-in default `127.0.0.1`
    /// (loopback-only). A non-loopback bind is refused unless `read_auth`
    /// is on with a resolved serve token (`darkmux-serve`'s
    /// `serve_auth_preflight`).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub bind: Option<String>,
    /// Whether the daemon may read the serve token from the macOS Keychain
    /// item `darkmux-serve-token`. Default `false`. The env token
    /// `DARKMUX_SERVE_TOKEN` needs no gate (its presence is the opt-in).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub token_keychain: Option<bool>,
    /// Whether a READ that is not from this machine needs the serve token.
    /// Default `false`: reads are open to whatever reaches the daemon (the
    /// tailnet, behind `tailscale serve`). When `true`, a request is exempt
    /// only if it arrives on loopback with no reverse-proxy header, and
    /// `darkmux serve` refuses to start unless a token resolves. A
    /// non-loopback bind requires it. Never governs execution (#2988).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub read_auth: Option<bool>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#1230 Packet 5) Mission-board drift-detection knobs — consumed by
/// `darkmux mission status`'s `detect_drift`.
///
/// Renamed from `MissionConfig` (#1284 Packet 1 review round): the
/// mission-registry arc makes `darkmux_crew::mission_config::MissionConfig`
/// (a mission GRAPH document — phases/tasks/steps) the arc's headline
/// concept, and two unrelated `MissionConfig`s in one workspace invited
/// exactly the confusion the review caught. This one is the mission BOARD's
/// config block. Rust-only rename — the serde field name stays `mission`
/// (see `DarkmuxConfig::mission`), so operator `config.json` files are
/// untouched; pre-1.0 no-compat-baggage applies to the type name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MissionBoardConfig {
    /// How many days an Active mission may sit with zero `Complete` phases
    /// before `mission status` flags it as stale (default 14). The concrete
    /// motivating case: `doom-loop-m4` sat at 0/4 phases for ~20 days with
    /// no drift surfaced at all, because the pre-#1230-Packet-5 detector
    /// only checked Closed+non-terminal and Active+all-terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub stale_active_days: Option<u64>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#1698 Packet B2) The radio interpreter's own staffing + persona knobs —
/// separate from `role_profiles` because these are radio-specific overrides
/// with radio-specific defaults (an EMPTY profile name falls through to the
/// ordinary role-profile/default-profile precedence, not an error), and
/// separate from `RuntimeBehaviorConfig` because they're specific to the
/// `radio` interpreter (routing + answering), not general dispatch behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RadioConfig {
    // (#2914, CONFIG 1.28) `router_profile` REMOVED. The ROUTING seat runs on
    // the machine's utility model (`internal.utility` in profiles.json), not
    // on a profile, so there is nothing to bind it to; `role_profiles.
    // radio-router` is refused by `config set` for the same reason. An older
    // config still carrying `router_profile` lands it in `extras` below
    // (lenient-on-read), and `darkmux doctor` names it with the fix.
    /// Explicit profile override for the ANSWERING seat (`radio-host`).
    /// Empty/absent falls through to `role_profiles.radio-host` (if bound)
    /// then `default_profile` — the fresh-install floor, per the issue's
    /// "answerer_profile empty = default_profile."
    #[serde(default, skip_serializing_if = "Option::is_none")] pub answerer_profile: Option<String>,
    /// The RADIO persona's humor dial (0-100), substituted into the
    /// answering seat's `{{humor}}` template placeholder at assembly time
    /// (`src/radio_answer.rs`). Default `RADIO_HUMOR_DEFAULT` (50; was 65, the value the operator's manual
    /// TARS-persona override file carried before this config knob existed.
    ///
    /// **Deliberately `u64`, not `u8`** — `config set` coerces every `Uint`
    /// key through one shared parse (`Ty::Uint`, `src/config_cmd.rs`) that
    /// always produces a `u64` JSON number; a `u8` field here would make an
    /// operator's `darkmux config set radio.humor 300` fail the WHOLE
    /// config-file write with "the resulting config.json would not parse"
    /// (`config_cmd.rs::set_at`), and — worse — a hand-edited `"humor": 300`
    /// in config.json would silently reset the ENTIRE config to defaults on
    /// next load (`DarkmuxConfig::load_from`'s lenient
    /// `unwrap_or_default()`), taking every OTHER setting down with it. The
    /// accessor (`config_access::radio_humor`) already clamps to `0..=100`
    /// after parsing, so widening this field costs nothing and removes both
    /// hazards.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub humor: Option<u64>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#1685) The operator's own `gh` CLI credential gate — a **feature block
/// gated by `enabled`**, same pattern as `RedisConfig`/`AuditConfig`.
/// The allowlist a mission config's `cmd` field is checked against before
/// that config may run at all.
///
/// (#2004) Named `gh{}` / `gh_verb` until schema 1.11. The mechanism was
/// always forge-agnostic — this block holds no knowledge of any particular
/// tool, just a list of operator-chosen COMMAND NAMES — but the NAME said
/// otherwise, so a GitLab user allowlisted `mr-merge` under `gh.allowed`,
/// and a config gating `terraform apply` or `kubectl delete` had to declare
/// a GitHub-shaped field to get a gate that has nothing to do with GitHub.
/// `cmd` is neutral across forges and across domains, which is what the
/// mechanism always was. The old name is a loud validation Error, never a
/// silent overflow — see `MISSION_CONFIG_SCHEMA`'s doc for why that matters
/// more here than for an ordinary rename.
///
/// darkmux never authenticates to anything itself; the PR-flow panel
/// commands (`pr-list`/`pr-info`/`pr-approve`/`pr-merge` — see the PR-flow
/// guide) are operator-authored `procedural.shell` mission configs that
/// shell out to whatever tool the OPERATOR already has signed in, exactly
/// like the `lms`/`zed` shell-outs elsewhere in this binary. The gated tool
/// never enters darkmux core: this block is just a list of operator-chosen
/// COMMAND NAMES,
/// checked against the `cmd` an operator's own mission config declares
/// (`darkmux_crew::mission_config::MissionConfig::cmd` /
/// `check_cmd`) before that config is allowed to run at all, on either
/// entry point (`darkmux acp`'s ephemeral panel route or a direct `darkmux
/// mission launch <id>`).
///
/// `darkmux init` writes this block visible with `enabled: false` and an
/// EMPTY `allowed` list — darkmux ships no opinion about which verbs
/// exist; the operator's own configs name their own verbs, and the
/// operator opts each one in by listing it here. Fails closed on both
/// counts: `enabled: false` blocks every verb regardless of `allowed`, and
/// a verb absent from `allowed` is blocked even with `enabled: true`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CmdConfig {
    /// The gate: `true` → the `allowed` list is consulted at all;
    /// `false`/absent → every `cmd`-declaring config is refused,
    /// regardless of `allowed`. Declared first so it reads at the top of
    /// the block.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    /// The allowlisted verb names — e.g. `["pr-list", "pr-info", "pr-approve",
    /// "pr-merge"]`, matching each config's own `cmd` field verbatim.
    /// `darkmux config set cmd.allowed <comma-separated-list>` replaces the
    /// whole list (there is no incremental add today — see the PR-flow guide).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub allowed: Option<Vec<String>>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2093) Flow-record hooks — a fourth `FlowSink` kind: match → POST. A
/// **feature block gated by `enabled`**, same pattern as `RedisConfig` /
/// `AuditConfig` / `CmdConfig`. `darkmux init` writes the whole block with
/// `enabled: false`, the default `outbox_dir` populated, and an EMPTY
/// `rules` array — darkmux ships no opinion about which hooks an operator
/// wants; the surface is discoverable and one flip + one rule from live.
///
/// Every `write()` is filtered against `rules` (see `HookRule`/`HookMatch`);
/// a match appends the record verbatim to that rule's outbox file, and a
/// background drainer POSTs it to `http`, retrying 5xx, 408, 429 and network
/// failures without a cap (1s doubling to 60s) and giving up on other 4xx
/// after 3 and on a redirect at once. The write
/// path never blocks on the network — see `darkmux_flow::hooks` for the
/// sink implementation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HooksConfig {
    /// The gate: `true` → the drainer thread starts and `rules` are
    /// consulted; `false`/absent → off, regardless of `rules`. Declared
    /// first so it reads at the top of the block. The ONLY env override is
    /// the whole-feature gate (`DARKMUX_HOOKS_ENABLED`) — individual rules
    /// are config-only (a rule is a structured object, not a scalar an env
    /// var can carry).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub enabled: Option<bool>,
    /// Where per-rule outbox + cursor files live. Each rule gets
    /// `<outbox_dir>/<host-port>-<hash16>.outbox.jsonl` (the pending
    /// queue, append-only — keyed by a content hash of the rule's
    /// `match`+`http`, NOT its position in `rules`, so reordering rules
    /// in config never orphans an in-flight outbox) and a sibling
    /// `.cursor` file (the byte offset of the first undelivered line) —
    /// durable across restarts, so a down receiver never loses a firing.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub outbox_dir: Option<String>,
    /// The filter → target list. Evaluated in order against every record
    /// this process writes; a record may match more than one rule (each
    /// gets its own outbox line). An empty `match` on a rule matches
    /// nothing (`darkmux doctor` warns — that rule is dead weight, not a
    /// catch-all, to keep "matches everything" an explicit `*` action
    /// rather than an easy-to-write-by-accident empty object). Records
    /// whose `action` starts with `hook.` (the sink's own firing/failure
    /// records) never match any rule — loop prevention.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub rules: Option<Vec<HookRule>>,
    /// (#2093 merge-gate finding 5) Hard cap, in MiB, on UNDELIVERED bytes
    /// a single rule's outbox may hold before appends for that rule stop.
    /// A down/unreachable receiver never blocks `write()` (the outbox
    /// exists precisely so it doesn't have to), but without a ceiling an
    /// indefinitely-down receiver turns "buffer while down" into
    /// "consume disk without bound." Past this cap, new records for that
    /// rule are dropped (counted, surfaced in `flow status` and
    /// `doctor`, and named in a rate-limited `hook.failed`) rather than
    /// grown further — other rules and every other sink are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_outbox_mb: Option<u64>,
    /// (#2183) Wall-clock cap, in milliseconds, on ONE `transform`
    /// evaluation (compile + run). A jq filter that never terminates
    /// (`def rec: rec; rec`) is bounded here rather than hanging the
    /// drainer — see `hook_transform::apply_transform`'s doc. Visible
    /// default `5000` (5s).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub jq_timeout_ms: Option<u64>,
    /// (#2183) Hard cap, in bytes, on a `transform`'s produced body. An
    /// adapter that builds an unbounded string (`"x" * 10000000`) is a
    /// TERMINAL failure past this cap, not a giant POST. Visible default
    /// `1048576` (1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub jq_max_output_bytes: Option<u64>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// (#2183) One `headers` map entry's value: either a plain (non-secret)
/// literal string, or a reference to a macOS Keychain generic-password
/// item holding the COMPLETE header value (`Basic <base64(...)>`, `Bearer
/// <token>`, `X-Api-Key: <key>` — darkmux stays scheme-agnostic; the item
/// holds whatever string the header needs, never a raw token darkmux
/// would have to format itself). `#[serde(untagged)]` — a bare JSON string
/// is `Literal`; an object with a `keychain_item` key is `Keychain`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum HeaderValue {
    Literal(String),
    Keychain { keychain_item: String },
}

/// One hook rule: a predicate (`match`) plus an HTTP outcome (`http`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HookRule {
    /// Renamed `match` on the wire (a Rust keyword) — see `HookMatch`.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "match")]
    pub r#match: Option<HookMatch>,
    /// The target URL a matching record is POSTed to (`Content-Type:
    /// application/json`, body = the record verbatim). Accepted by URL
    /// policy alone (#2135 option 2) — either loopback (`127.0.0.1`,
    /// `[::1]`, `localhost`) or a genuine Tailscale address (an IPv4 in
    /// `100.64.0.0/10`, or a hostname ending in `.ts.net`, MagicDNS).
    /// Refused at config load (the whole hooks sink degrades, loudly)
    /// when the host is neither — a token-bearing remote (non-tailnet)
    /// hook is a later packet (#2093's own "out of scope").
    ///
    /// **Delivery is AT-LEAST-ONCE, not exactly-once** (#2093 merge-gate
    /// finding 13): the cursor advances only AFTER a successful POST, so
    /// a crash (or process kill) between the receiver returning 2xx and
    /// that cursor write redelivers the same record on the next restart.
    /// This receiver MUST be idempotent — in practice, that means keying
    /// on the record's own identity rather than treating arrival as the
    /// event. The tracker's finding-identity key already does this by
    /// construction, so a redelivered `hook.fired`/matched record is a
    /// safe no-op for it, not a duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub http: Option<String>,
    /// (#2183) The no-network testing tier: a directory a matching
    /// delivery writes ONE JSON file into (`{delivery_id, target_would_be,
    /// headers, body}`, secret header values redacted) instead of
    /// POSTing — `hook.dry_run` instead of `hook.fired`. Mutually
    /// exclusive with `http`: a rule naming BOTH, or NEITHER, is a
    /// load-time refusal (`resolve_rules`) — a rule must have exactly one
    /// destination.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub file: Option<String>,
    /// (#2183) The NAME of a jq adapter (e.g. `"jira-issue.jq"`), resolved
    /// inside `~/.darkmux/hooks/adapters/` — never a path (`/`, `\`, `..`,
    /// or an absolute string are refused at load AND at delivery). Absent
    /// → today's behavior, the record verbatim (byte-identical). Applied
    /// at DELIVERY time, not enqueue — the outbox keeps raw records, so a
    /// corrected adapter re-drains the SAME lines. See
    /// `hook_transform`'s module doc for the full design.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub transform: Option<String>,
    /// (#2183) Extra headers this rule's `http` deliveries carry, name ->
    /// literal string or `{"keychain_item": "..."}`. This is what makes a
    /// non-tailnet (external, `https`) target real — an auth header a
    /// SaaS receiver requires. Every value is sanitized the same way the
    /// `X-Darkmux-*` headers already are (`sanitize_header_value`); a
    /// Keychain-resolved value is NEVER written to a flow record, a log
    /// line, `doctor` output, or the `file` transport's dump — those
    /// render `"<redacted>"` instead. Ignored on a `file`-transport rule
    /// (nothing goes on the wire — the dump shows literal headers as
    /// configured and `"<redacted>"` for Keychain ones, same redaction
    /// rule, no actual header is ever "sent").
    #[serde(default, skip_serializing_if = "Option::is_none")] pub headers: Option<BTreeMap<String, HeaderValue>>,
    /// (#2183) `false` drops the `X-Darkmux-Delivery`/`-Event`/`-Sender`/
    /// `-Timestamp`/`-Machine-Id`/`-Machine-Uid`/`-Signature` attribution
    /// headers from this rule's deliveries — a SaaS endpoint may reject
    /// unknown headers. Absent/`true` → today's behavior (every delivery
    /// carries them). No effect on a `file`-transport rule (the dump
    /// always shows what WOULD have gone on the wire, attribution headers
    /// included, since there's no receiver to reject them).
    #[serde(default, skip_serializing_if = "Option::is_none")] pub attribution_headers: Option<bool>,
    /// (#2135 option 2) The name of a macOS Keychain generic-password
    /// item (`security add-generic-password -a $USER -s <item> -w`)
    /// holding the HMAC-SHA256 secret this rule signs its deliveries
    /// with. The secret NEVER lives in config or in a log line — only
    /// this item NAME does. When set, every delivery for this rule
    /// carries `X-Darkmux-Signature: sha256=<hex hmac>`; when absent,
    /// deliveries go out unsigned (fine inside a trusted loopback or
    /// tailnet, `darkmux doctor` warns for a tailnet target). Non-macOS:
    /// set `DARKMUX_HOOK_SECRET_<RULE-INDEX>` instead (e.g.
    /// `DARKMUX_HOOK_SECRET_0` for `rules[0]`) — that env var, when set,
    /// wins over this field on every platform.
    #[serde(default, skip_serializing_if = "Option::is_none")] pub signing_secret_keychain_item: Option<String>,
    #[serde(flatten)] #[schemars(skip)] pub extras: serde_json::Map<String, serde_json::Value>,
}

/// A hook rule's match predicate — every field is an independent AND'd
/// condition; a field left `None` doesn't gate. `action` is a small glob
/// (`*` within a segment, or a trailing `*` segment matching one-or-more
/// further dot-separated segments — e.g. `dispatch.*` matches
/// `dispatch.tool` but not `dispatched`; a bare `*` matches every action).
/// The rest are exact
/// matches against the record's own fields — `session_id`/`mission_id`/
/// `machine_id` compare as plain strings, `category`/`level` compare
/// against the record's serialized (lowercase) enum value.
///
/// **An all-`None` match is deliberately NOT a catch-all** — it matches
/// nothing, and `darkmux doctor` warns (a rule an operator forgot to fill
/// in should look broken, not silently subscribe to everything).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HookMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub machine_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub level: Option<String>,
    /// (#1959) Every OTHER top-level key on the wire lands here —
    /// including the payload predicates this struct doesn't declare a
    /// typed field for: `"payload.tool_name"`, `"payload.ok"`, or a
    /// deeper `"payload.detections.count"`, each a literal DOTTED KEY
    /// (never a nested `{"payload": {...}}` object — `#[serde(flatten)]`
    /// permits exactly one flattened field per struct, so a genuinely
    /// nested second field would collide with this one). See
    /// [`HookMatch::payload_predicates`] for the accessor that reads
    /// them back out.
    #[serde(flatten)] pub extras: serde_json::Map<String, serde_json::Value>,
}

impl HookMatch {
    /// True when every field is `None`/empty — the "matches nothing" state
    /// a half-filled-in rule leaves. `darkmux doctor` uses this to warn.
    pub fn is_empty(&self) -> bool {
        self.action.is_none()
            && self.session_id.is_none()
            && self.mission_id.is_none()
            && self.machine_id.is_none()
            && self.category.is_none()
            && self.level.is_none()
            && self.payload_predicates().next().is_none()
    }

    /// (#1959) Every `"payload.<dotted path>"` key on this match, with the
    /// `payload.` prefix stripped — the exact-match predicates
    /// `hooks::hook_match` evaluates against a record's OWN `payload`
    /// field, e.g. `{"action": "dispatch.tool", "payload.tool_name":
    /// "create_finding", "payload.ok": true}` yields `("tool_name",
    /// "create_finding")` and `("ok", true)`. A remaining `extras` key that
    /// does NOT start with `payload.` is unrelated forward-compat overflow
    /// and is not a predicate — see the struct doc.
    pub fn payload_predicates(&self) -> impl Iterator<Item = (&str, &serde_json::Value)> {
        self.extras
            .iter()
            .filter_map(|(k, v)| k.strip_prefix("payload.").map(|rest| (rest, v)))
    }
}

/// A machine's declared fleet position. Its one consumer is
/// `darkmux_doctor::viewer_link_base`: `Standalone` (default) makes viewer
/// links name this machine's own address; `Hub` and `Peer` make them prefer
/// the tailnet address when `tailscale serve` proxies to this daemon. It
/// gates no fleet feature: the fleet listener, the roster and dispatch to
/// peers are separate settings and work in any mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FleetMode {
    #[default]
    Standalone,
    Hub,
    Peer,
}

impl FleetMode {
    /// The canonical lowercase token — the `config.json` value and the
    /// `DARKMUX_FLEET_MODE` env token.
    pub fn as_str(self) -> &'static str {
        crate::config_enum::ConfigEnum::token(self)
    }
}

// (#2947) Parsing (trimmed, case-insensitive) comes from `ConfigEnum::parse`;
// an unrecognized token is refused at `config_access::fleet_mode`, never
// read as `standalone`.
crate::config_enum!(FleetMode, "fleet position", [
    Standalone = "standalone" => "stated on the card, presence and telemetry; viewer links name this machine's own address (the fleet listener and roster are separate settings, unaffected)",
    Hub = "hub" => "stated on the card, presence and telemetry, which gives the Fleet lens its HUB badge and lets the card hand out `fleet.defaults`; viewer links prefer the tailnet address when `tailscale serve` proxies to this daemon (the fleet listener and roster are separate settings, unaffected)",
    Peer = "peer" => "stated on the card, presence and telemetry; viewer links prefer the tailnet address when `tailscale serve` proxies to this daemon (the fleet listener and roster are separate settings, unaffected)",
]);

/// A machine's declared fleet position as it travels on the wire: in its card,
/// its presence beat and its `machine.telemetry` records. [`FleetMode`] is the
/// config value and refuses an unregistered token; this is what a READER of
/// another machine's record meets, so it also has `Unknown` for a value a
/// newer darkmux states (never read as any known mode), and it is the
/// declared mode of a machine whose own `fleet.mode` is bad config: that
/// machine cannot say which mode it declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum DeclaredFleetMode {
    #[default]
    Standalone,
    Hub,
    Peer,
    #[serde(other)]
    Unknown,
}

impl From<FleetMode> for DeclaredFleetMode {
    fn from(mode: FleetMode) -> Self {
        match mode {
            FleetMode::Standalone => Self::Standalone,
            FleetMode::Hub => Self::Hub,
            FleetMode::Peer => Self::Peer,
        }
    }
}

/// (#2947) An OS thermal state, as `runtime.thermal.pause_at` / `resume_at`
/// name one. Declared in severity order, mildest first: the governor ranks a
/// state by its position (`darkmux_crew::host_probe::thermal::THERMAL_STATES`
/// is this enum's token list), so the order is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalState {
    Nominal,
    Fair,
    Serious,
    Critical,
}

impl ThermalState {
    pub fn as_str(self) -> &'static str {
        crate::config_enum::ConfigEnum::token(self)
    }
}

crate::config_enum!(ThermalState, "thermal state", [
    Nominal = "nominal" => "no thermal pressure",
    Fair = "fair" => "slightly elevated; the OS may start to throttle",
    Serious = "serious" => "high; the OS is throttling",
    Critical = "critical" => "the OS is throttling hard; the breaker's own threshold",
]);

/// (#2947) The overlay network that verifies which machine is on the other
/// end of a fleet connection (`fleet.identity.provider`). A VALUE, never a
/// field or type name, per the no-vendor-names-in-identifiers rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityProvider {
    Tailscale,
}

impl IdentityProvider {
    pub fn as_str(self) -> &'static str {
        crate::config_enum::ConfigEnum::token(self)
    }
}

crate::config_enum!(IdentityProvider, "identity provider", [
    Tailscale = "tailscale" => "the tailnet's own daemon answers who is connecting (`whois`)",
]);

/// (#2916 stage 2) What a machine's fleet listener does with a submitted job
/// whose seat is already in use (`fleet.busy_policy`). A job on a LOCAL model
/// holds that model for its whole run (one request at a time per instance);
/// a job on an endpoint darkmux does not manage runs beside others on that
/// endpoint up to its `limits.concurrent_calls` (one at a time when it
/// declares none). Past either limit, this policy decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "lowercase")]
pub enum BusyPolicy {
    Refuse,
    Queue,
}

impl BusyPolicy {
    pub fn as_str(self) -> &'static str {
        crate::config_enum::ConfigEnum::token(self)
    }
}

crate::config_enum!(BusyPolicy, "busy policy", [
    Refuse = "refuse" => "answer at once that the seat is busy, naming what is running",
    Queue = "queue" => "hold the job until its seat frees, telling the sender it is waiting",
]);

impl DarkmuxConfig {
    /// The full, self-documenting default config that `darkmux init` writes —
    /// every common knob present and visible, so the operator tunes the *file*,
    /// not the code, and can *see* the surface without digging. Scalar defaults
    /// are written explicitly; the integration features (`redis`, `audit`) are
    /// written as complete blocks with `enabled: false`, so their whole surface
    /// is discoverable and one flip from on.
    ///
    /// Deliberately omitted — NOT hidden defaults, but fields where a written
    /// literal would be *wrong*:
    /// - `dirs` — defaults are derived from the root (`<root>/flows`); there is
    ///   no fixed literal to write without freezing the derivation. The
    ///   discovery surface is `darkmux doctor` (resolved path, overridable).
    /// - caps (`max_turns`/`max_tokens`/`max_tokens_per_call`/
    ///   `reasoning_checkpoint_interval_tokens`/
    ///   `generation_checkpoint_interval_tokens`/`max_stall_recoveries`), `default_role`,
    ///   `daemon_cors_origins` — absent is a real behavior (uncapped / the
    ///   runtime's built-in per-call default), not a value to default.
    /// - `local_dispatch_concurrency` (#2772) — absent means "derive it live
    ///   from the resident instance's own declared `PARALLEL`"; a written
    ///   literal would freeze that derivation to whatever happened to be
    ///   resident at `init` time, which is exactly the class of wrong-literal
    ///   `dirs` avoids above.
    ///
    /// Single source of truth for the written defaults: `init` writes this and
    /// `config.example.json` is asserted equal to its pretty form (a drift
    /// guard), so the docs reference and the code can't diverge. `machine_id`
    /// is a placeholder here — `init` overrides it with the machine's name.
    pub fn with_defaults() -> Self {
        DarkmuxConfig {
            schema_version: Some(CONFIG_SCHEMA_VERSION.to_string()),
            machine_id: Some("my-machine".to_string()),
            lms_bin: Some("lms".to_string()),
            lmstudio_url: Some("http://localhost:1234".to_string()),
            dirs: None,
            redis: Some(RedisConfig {
                enabled: Some(false),
                host: Some("127.0.0.1".to_string()),
                port: Some(6379),
                db: None,
                stream: Some("darkmux:flow".to_string()),
                maxlen: Some(DEFAULT_REDIS_MAXLEN),
                telemetry_maxlen: Some(DEFAULT_REDIS_TELEMETRY_MAXLEN),
                extras: Default::default(),
            }),
            audit: Some(AuditConfig {
                enabled: Some(false),
                dir: Some("~/.darkmux/audit".to_string()),
                extras: Default::default(),
            }),
            runtime: Some(RuntimeBehaviorConfig {
                inactivity_timeout_seconds: Some(600),
                model_load_timeout_seconds: Some(600),
                step_command_timeout_seconds: Some(600),
                // (#2678) Visible `0` — UNBOUNDED, the pre-existing
                // no-run-level-deadline behavior, discoverable and one edit
                // from a real bound.
                mission_wall_clock_timeout_seconds: Some(0),
                dispatch_free_concurrency: Some(8),
                local_dispatch_concurrency: None,
                max_turns: None,
                max_tokens: None,
                max_tokens_per_call: None,
                reasoning_checkpoint_interval_tokens: None,
                generation_checkpoint_interval_tokens: None,
                max_stall_recoveries: None,
                strict_selection: Some(false),
                // (#1548) Now wired end-to-end (config_access accessor +
                // docker-spawn forwarding) — a visible `true` default, same
                // treatment as strict_selection/check_updates above.
                feedback_injection: Some(true),
                default_role: None,
                check_updates: Some(true),
                daemon_cors_origins: None,
                injected_context_fraction: Some(0.15),
                acp_idle_exit_minutes: Some(30),
                // (#2094) Visible `0` — the pre-existing no-rest behavior,
                // discoverable + one edit from a thermal-friendly value.
                turn_delay_ms: Some(0),
                // (#2107, #1833) Visible `5000` — the machine stats
                // drawer's daemon-side sampler cadence, discoverable and
                // one edit from `0` (disabled) or a tighter/looser value.
                host_sampler_interval_ms: Some(5000),
                // (#2928) Visible `250` — the live channel's cadence,
                // discoverable and one edit from `0` (off).
                live_sample_ms: Some(250),
                // (#2110/#2109) Visible on-by-default block — see
                // `ThermalConfig`'s own doc for why this defaults to
                // `enabled: true` rather than the redis/audit off-by-default
                // pattern.
                // (#2846) Visible on-by-default block, same rationale as
                // `thermal`: the operator tunes the file, not the source.
                detection: Some(DetectionConfig {
                    degeneracy: Some(DetectorConfig {
                        policy: Some(DetectionPolicy::Conclude.as_str().to_string()),
                        extras: Default::default(),
                    }),
                    extras: Default::default(),
                }),
                thermal: Some(ThermalConfig {
                    enabled: Some(true),
                    pause_at: Some("serious".to_string()),
                    resume_at: Some("fair".to_string()),
                    resume_hold_ms: Some(60_000),
                    max_pause_ms: Some(900_000),
                    min_cpu_speed_limit_pct: Some(50),
                    speed_limit_hold_samples: Some(3),
                    // (#2774) Tiers 2-4 of the operator's thermal escalation
                    // ladder — see `ThermalConfig`'s own field docs.
                    duty_delay_ms: Some(15_000),
                    ratchet_factor: Some(2),
                    episode_threshold: Some(2),
                    tier4_enabled: Some(true),
                    extras: Default::default(),
                }),
                // (#2653) Visible `168` (7 days) — the built-in default,
                // discoverable and one edit from a tighter/looser window.
                liveness_retention_hours: Some(168),
                verbose: Some(false),
                extras: Default::default(),
            }),
            fleet: Some(FleetConfig {
                mode: Some("standalone".to_string()),
                // (#2916) Visible, so the submission surface is discoverable:
                // the provider value, and the listener one flip from on.
                identity: Some(FleetIdentityConfig {
                    provider: Some(crate::config_access::FLEET_IDENTITY_PROVIDER_DEFAULT.to_string()),
                    bin: None,
                    extras: Default::default(),
                }),
                listener: Some(FleetListenerConfig {
                    enabled: Some(false),
                    port: Some(crate::config_access::FLEET_LISTENER_PORT_DEFAULT),
                    extras: Default::default(),
                }),
                accept_work: Some(BTreeMap::new()),
                // (#2916 stage 2) Visible, so the busy answer is discoverable.
                busy_policy: Some(crate::config_access::FLEET_BUSY_POLICY_DEFAULT.to_string()),
                // (#3022) Visible and empty: only a hub's values are served.
                defaults: Some(FleetDefaultsConfig {
                    radio: Some(FleetDefaultsRadioConfig {
                        answerer_profile: Some(String::new()),
                        extras: Default::default(),
                    }),
                    extras: Default::default(),
                }),
                extras: Default::default(),
            }),
            // (#2706) Visible block with every default populated — the
            // config philosophy's "init writes the knobs the operator
            // would otherwise have to know to add by hand". Both policies
            // ship ON: the operator's stated intent is that a laptop below
            // half charge neither starts nor continues sustained local
            // inference.
            power: Some(PowerConfig {
                min_battery_pct: Some(50),
                refuse_start_below_min: Some(true),
                pause_running_below_min: Some(true),
                extras: Default::default(),
            }),
            mission: Some(MissionBoardConfig {
                stale_active_days: Some(14),
                extras: Default::default(),
            }),
            // (#1698 Packet B2) Written visible with an empty (unset)
            // answering-seat override: an empty string, not an absent field,
            // falls through to `role_profiles.radio-host` then
            // `default_profile`. (#2914) The routing seat has no knob here.
            radio: Some(RadioConfig {
                answerer_profile: Some(String::new()),
                humor: Some(crate::config_access::RADIO_HUMOR_DEFAULT),
                extras: Default::default(),
            }),
            // (#1475 packet 1) Written as a visible empty `{}` — the operator
            // discovers the role->profile surface and binds a role with
            // `darkmux config set role_profiles.<role> <profile>`. Empty (not
            // absent) so it shows up in `config list` / the example file.
            role_profiles: Some(BTreeMap::new()),
            // (#1685) Written visible with `enabled: false` and an empty
            // `allowed` list — see `CmdConfig`'s own doc. darkmux ships no
            // opinion about which gh verbs exist; the operator opts each
            // one in by naming it here once they've authored the config.
            cmd: Some(CmdConfig {
                enabled: Some(false),
                allowed: Some(Vec::new()),
                extras: Default::default(),
            }),
            // (#2093) Written visible with `enabled: false`, the default
            // outbox dir populated, and an empty `rules` array — the
            // operator's own rules are structured objects with no sensible
            // literal default, so the surface is the empty array itself.
            hooks: Some(HooksConfig {
                enabled: Some(false),
                outbox_dir: Some("~/.darkmux/hooks".to_string()),
                rules: Some(Vec::new()),
                max_outbox_mb: Some(256),
                // (#2183) Visible by design (this project's config
                // philosophy — `init` writes the common knobs the
                // operator would otherwise have to know to add by hand).
                jq_timeout_ms: Some(5_000),
                jq_max_output_bytes: Some(1_048_576),
                extras: Default::default(),
            }),
            // (#2765) Written visible at the built-in defaults. The whole
            // point of the issue is that this knob was invisible: the
            // operator could not `config set` it because it did not exist,
            // and could not SEE that the daemon's address was a decision
            // anyone had made. Visible-at-default is what makes a restart
            // that forgets `--port` land on the operator's own value
            // instead of silently reverting.
            serve: Some(ServeConfig {
                port: Some(crate::config_access::SERVE_PORT_DEFAULT),
                bind: Some(crate::config_access::SERVE_BIND_DEFAULT.to_string()),
                // (#2988 follow-up) Both auth switches visible at `false`:
                // no Keychain read, reads open to whatever reaches the
                // daemon. One `config set` from on.
                token_keychain: Some(false),
                read_auth: Some(false),
                extras: Default::default(),
            }),
            extras: Default::default(),
        }
    }

    /// Load the config.json at the USER-scope location (`~/.darkmux/config.json`
    /// or `$DARKMUX_HOME`), never a project-local one. Missing or malformed →
    /// default-empty (loud validation belongs to `darkmux doctor`, not the hot
    /// load path; a bad config must never brick the CLI — accessors fall through
    /// to env/built-in defaults).
    ///
    /// config.json carries user/machine-level state (redis/audit/lms/machine_id):
    /// there is no per-project config, and a `<cwd>/.darkmux/` never shadows it.
    pub fn load_resolved() -> Self {
        let path = crate::paths::resolve(crate::paths::ResolveScope::ForceUser).config;
        Self::load_from(&path)
    }

    /// Load from an explicit path (used by tests + `load_resolved`). Silent
    /// default on missing/unreadable/unparseable file.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_enum::ConfigEnum;

    /// One rule across both channels: refuse only when ignoring a leftover is unsafe. A setting
    /// whose env var is merely warned about must not refuse its `config.json` twin at any value,
    /// and one whose env var refuses must not let any value of its key through unjudged.
    #[test]
    fn a_retired_settings_env_and_config_channels_agree_on_whether_ignoring_it_is_unsafe() {
        for r in RETIRED_SETTINGS.iter().filter(|r| r.env.is_some()) {
            let config_ignores_any_value = r.leftover == LeftoverValue::Any;
            match r.env_policy {
                LeftoverPolicy::Warn => {
                    assert!(config_ignores_any_value, "{}: env warns but a config leftover can still refuse", r.key)
                }
                LeftoverPolicy::Refuse => {
                    assert!(!config_ignores_any_value, "{}: env refuses but any config leftover value passes", r.key)
                }
            }
        }
    }

    /// (#1323) The config seam's self-defending conformance test: a project-local
    /// `.darkmux/config.json` (created for missions/phases/lessons) must NEVER
    /// shadow the user-scope config. `DARKMUX_HOME` is UNSET on purpose — with it
    /// set, `paths::resolve` short-circuits to the same root for every scope, so
    /// the project and user scopes wouldn't diverge and this guard would be
    /// hollow. If `load_resolved` regresses to `ResolveScope::ForceProject`, it
    /// reads the project shadow → the marker → this fails.
    #[serial_test::serial]
    #[test]
    fn config_load_resolved_ignores_project_darkmux_shadow() {
        use std::env;
        let proj = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(proj.path().join(".darkmux")).unwrap();
        std::fs::write(
            proj.path().join(".darkmux").join("config.json"),
            r#"{"machine_id":"PROJECT-SHADOW-MUST-NOT-LOAD"}"#,
        )
        .unwrap();

        let prev_home = env::var("DARKMUX_HOME").ok();
        let prev_cwd = env::current_dir().unwrap();
        unsafe { env::remove_var("DARKMUX_HOME") };
        env::set_current_dir(proj.path()).unwrap();

        // Sanity: in THIS setup the project and user scopes genuinely diverge, so the guard below actually exercises the choice.
        let auto = crate::paths::resolve(crate::paths::ResolveScope::ForceProject).config;
        let force_user = crate::paths::resolve(crate::paths::ResolveScope::ForceUser).config;
        let cfg = DarkmuxConfig::load_resolved();

        // Restore env FIRST so a failed assert can't poison other serial tests.
        env::set_current_dir(prev_cwd).unwrap();
        match prev_home {
            Some(h) => unsafe { env::set_var("DARKMUX_HOME", h) },
            None => unsafe { env::remove_var("DARKMUX_HOME") },
        }

        assert_ne!(
            auto, force_user,
            "sanity: with a project .darkmux/ and no DARKMUX_HOME, the project scope must diverge from ForceUser"
        );
        // The real guard: under the project scope, load_resolved reads the
        // project shadow → the marker → FAIL. Under `ForceUser` it never does.
        assert_ne!(
            cfg.machine_id.as_deref(),
            Some("PROJECT-SHADOW-MUST-NOT-LOAD"),
            "#1323: load_resolved must ignore a project-local .darkmux/config.json"
        );
    }

    /// (#2094) `turn_delay_ms` is written visible at `0` (no-rest, the
    /// pre-existing behavior) and round-trips a populated value losslessly.
    #[test]
    fn turn_delay_ms_visible_default_and_round_trips() {
        let cfg = DarkmuxConfig::with_defaults();
        assert_eq!(cfg.runtime.as_ref().unwrap().turn_delay_ms, Some(0));
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"turn_delay_ms\":0"), "visible in the serialized default: {json}");

        let populated = r#"{ "runtime": { "turn_delay_ms": 3000 } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(populated).unwrap();
        assert_eq!(cfg.runtime.as_ref().unwrap().turn_delay_ms, Some(3000));
        let back: DarkmuxConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.runtime.unwrap().turn_delay_ms, Some(3000), "lossless round-trip");
    }

    /// `with_defaults()` is the full, self-documenting config `init` writes:
    /// feature blocks present + gated off, scalar defaults explicit, derived/
    /// advanced fields absent, and `enabled` serialized first in each block.
    #[test]
    fn with_defaults_is_full_visible_and_round_trips() {
        let cfg = DarkmuxConfig::with_defaults();
        // Integration features: present as `enabled: false` blocks (visible
        // surface, off) — not absent, so the operator can see + flip them.
        let redis = cfg.redis.as_ref().unwrap();
        assert_eq!(redis.enabled, Some(false));
        assert_eq!(redis.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(redis.maxlen, Some(10_000));
        assert_eq!(cfg.audit.as_ref().unwrap().enabled, Some(false));
        // Scalar defaults written explicitly (not hidden in code).
        assert_eq!(cfg.lms_bin.as_deref(), Some("lms"));
        // Fields where a written literal would be wrong stay absent.
        assert!(cfg.dirs.is_none(), "dirs are derived → surfaced by doctor, not frozen");
        assert!(cfg.runtime.as_ref().unwrap().max_turns.is_none(), "uncapped, not defaulted");
        // (#1548) Now fully wired (config_access accessor + docker-spawn
        // forwarding) — a visible `true` default, same as strict_selection.
        assert_eq!(
            cfg.runtime.as_ref().unwrap().feedback_injection,
            Some(true),
            "feedback_injection is config_access-backed as of #1548 → written visibly, default on"
        );
        // `enabled` reads at the TOP of each feature block.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            json.find("\"enabled\"").unwrap() < json.find("\"host\"").unwrap(),
            "enabled must precede the connection knobs"
        );
        // (#933) The fleet block is written visible at the standalone default,
        // so the fleet surface is discoverable + one edit from hub/peer.
        assert_eq!(cfg.fleet.as_ref().unwrap().mode.as_deref(), Some("standalone"));
        // (#3035) The `remote` block is gone: `init` writes nothing for it.
        assert!(!json.contains("\"remote\""), "no remote block is written: {json}");
        // Lossless round-trip.
        let back: DarkmuxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.redis.as_ref().unwrap().enabled, Some(false));
        assert_eq!(back.audit.as_ref().unwrap().dir.as_deref(), Some("~/.darkmux/audit"));
        assert_eq!(back.fleet.as_ref().unwrap().mode.as_deref(), Some("standalone"));
    }

    /// The retired env vars nothing reads and whose loss changes nothing.
    const RETIRED_WARN_VARS: [&str; 8] = [
        "DARKMUX_REMOTE_STEP_BUDGET_POLICY",
        "DARKMUX_REMOTE_CONCURRENT_CAP",
        "DARKMUX_ACK_DIR",
        "DARKMUX_NOTEBOOK_DIR",
        "DARKMUX_RADIO_ROUTER_PROFILE",
        "DARKMUX_LOG",
        "DARKMUX_MACHINE_ROLLUP_ENABLED",
        "DARKMUX_MACHINE_ROLLUP_PERIOD_SECONDS",
    ];

    /// (#3035) Each of the `remote.*` settings 5.0 retired is named with the
    /// `endpoints.<id>.limits.*` field that replaces it, in its config-key
    /// entry and its env-var leftover alike, and nothing is carried over. A
    /// leftover old `config.json` key is not a leftover here: it is an
    /// unknown key, which `user_files` refuses with the same line.
    #[test]
    fn the_remote_settings_are_retired_with_their_replacements() {
        let replacement = [
            ("remote.max_tokens_per_step", "DARKMUX_REMOTE_MAX_TOKENS_PER_STEP", "limits.tokens_per_dispatch"),
            ("remote.max_tokens_per_execution", "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION", "limits.tokens_per_dispatch"),
            ("remote.step_budget_policy", "DARKMUX_REMOTE_STEP_BUDGET_POLICY", "limits.policy"),
            ("remote.concurrent_cap", "DARKMUX_REMOTE_CONCURRENT_CAP", "limits.concurrent_calls"),
        ];
        for (key, var, field) in replacement {
            let entry = RETIRED_SETTINGS.iter().find(|r| r.key == key).unwrap_or_else(|| panic!("{key} is not retired"));
            assert_eq!(entry.env, Some(var), "{key}");
            // Ignoring a spend cap removes the cap, so its env var is REFUSED;
            // ignoring the concurrency or policy var only makes things slower
            // or quieter, so those warn (the `LeftoverPolicy` rule).
            let spend_cap = field == "limits.tokens_per_dispatch";
            let want = if spend_cap { LeftoverPolicy::Refuse } else { LeftoverPolicy::Warn };
            assert_eq!(entry.env_policy, want, "{key}");
            assert!(entry.line.contains(field) && entry.line.contains("endpoints.<id>"), "{key}: {}", entry.line);
            let one = |k: &str| (k == var).then(|| "9".to_string());
            let found = retired_env_leftovers(&one);
            assert_eq!(found.len(), 1, "{var}: {found:?}");
            assert_eq!(found[0].policy, want, "{var}");
            assert!(found[0].line.contains(var) && found[0].line.contains(field), "{found:?}");
        }
        let block = RETIRED_SETTINGS.iter().find(|r| r.key == "remote").expect("the block is retired");
        assert!(block.line.contains("endpoints.<id>.limits"), "{}", block.line);
        let doc = serde_json::json!({ "remote": { "concurrent_cap": 2 } });
        let issues = crate::user_files::key_issues::<DarkmuxConfig>(&doc, &crate::user_files::config_retired);
        assert!(issues.iter().any(|i| i.to_string().contains("endpoints.<id>.limits")), "a leftover block is refused: {issues:?}");
    }

    #[test]
    fn retired_env_leftovers_are_found_in_the_env() {
        assert!(retired_env_leftovers(&|_| None).is_empty());
        let blank = |_: &str| Some("  ".to_string());
        assert!(retired_env_leftovers(&blank).is_empty(), "an empty env value reads as unset");
        let crew = |k: &str| (k == "DARKMUX_CREW_DIR").then(|| "/x".to_string());
        let found = retired_env_leftovers(&crew);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].line.contains("DARKMUX_CREW_DIR") && found[0].line.contains("DARKMUX_HOME"), "{found:?}");
        for var in RETIRED_WARN_VARS {
            let one = |k: &str| (k == var).then(|| "/x".to_string());
            let found = retired_env_leftovers(&one);
            assert_eq!(found.len(), 1, "{var} is a retired env var: {found:?}");
            assert!(found[0].line.contains(var), "{found:?}");
        }
    }

    /// (operator, 2026-10-01) Each leftover's policy is decided by whether
    /// ignoring it is safe: the state-location `DARKMUX_CREW_DIR` refuses
    /// (silently ignoring it changes behavior);
    /// `DARKMUX_NOTEBOOK_DIR`, `DARKMUX_RADIO_ROUTER_PROFILE` and
    /// `DARKMUX_ACK_DIR` warn (nothing reads them and nothing is lost).
    #[test]
    fn each_leftover_refuses_or_warns_by_whether_ignoring_it_is_safe() {
        let policy_of = |var: &str| {
            let one = |k: &str| (k == var).then(|| "/x".to_string());
            let found = retired_env_leftovers(&one);
            assert_eq!(found.len(), 1, "{var}: {found:?}");
            (found[0].policy, found[0].line.clone())
        };
        for var in ["DARKMUX_CREW_DIR", "DARKMUX_REMOTE_MAX_TOKENS_PER_STEP", "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION"] {
            let (policy, line) = policy_of(var);
            assert_eq!(policy, LeftoverPolicy::Refuse, "{var}");
            assert!(line.contains("is refused"), "{line}");
        }
        for var in RETIRED_WARN_VARS {
            let (policy, line) = policy_of(var);
            assert_eq!(policy, LeftoverPolicy::Warn, "{var}");
            assert!(line.contains("is ignored"), "{line}");
        }
    }

    /// (#2914) `radio.router_profile` is REMOVED (CONFIG 1.28): routing runs
    /// on the machine's utility model. `with_defaults()` no longer writes it,
    /// and an older config still carrying it loads leniently into
    /// `radio.extras`; the unknown-key gate refuses it (`RETIRED_SETTINGS`).
    #[test]
    fn radio_router_profile_is_removed_and_a_leftover_lands_in_extras() {
        let cfg = DarkmuxConfig::with_defaults();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("router_profile"), "with_defaults must not write the removed key: {json}");
        assert!(json.contains("answerer_profile"), "the answering seat's knob stays: {json}");

        let old: DarkmuxConfig =
            serde_json::from_str(r#"{"schema_version":"1.27","radio":{"router_profile":"radio","answerer_profile":""}}"#)
                .unwrap();
        let radio = old.radio.as_ref().unwrap();
        assert_eq!(radio.extras.get("router_profile").and_then(|v| v.as_str()), Some("radio"), "lenient-on-read");
        assert_eq!(radio.answerer_profile.as_deref(), Some(""));
    }

    /// (#1475 packet 1) `role_profiles` is written by `init` as a visible empty
    /// map, round-trips a populated map losslessly, and an absent map deserializes
    /// to `None` (lenient — a fresh/older config never carries it).
    #[test]
    fn role_profiles_map_visible_default_and_round_trips() {
        // `init`/`with_defaults` writes a VISIBLE empty `{}` (discoverable, off).
        let cfg = DarkmuxConfig::with_defaults();
        assert_eq!(
            cfg.role_profiles.as_ref().map(|m| m.is_empty()),
            Some(true),
            "with_defaults writes a visible empty role_profiles map"
        );
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"role_profiles\":{}"), "empty map serializes visible, got: {json}");

        // A populated map round-trips losslessly (many roles -> one profile ok).
        let populated = r#"{
            "role_profiles": {
                "probe-high": "qwen27b",
                "probe-mid": "devstral",
                "probe-low": "qwen4b",
                "judge": "qwen35b",
                "verify": "qwen35b"
            }
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(populated).unwrap();
        let map = cfg.role_profiles.as_ref().unwrap();
        assert_eq!(map.get("judge").map(String::as_str), Some("qwen35b"));
        assert_eq!(map.get("verify").map(String::as_str), Some("qwen35b"), "many roles -> one profile");
        assert_eq!(map.get("probe-low").map(String::as_str), Some("qwen4b"));
        let back: DarkmuxConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.role_profiles, cfg.role_profiles, "lossless round-trip");

        // Absent map -> None (lenient; a fresh/older config never carries it).
        let cfg: DarkmuxConfig = serde_json::from_str(r#"{ "machine_id": "x" }"#).unwrap();
        assert!(cfg.role_profiles.is_none(), "absent role_profiles is None, not a brick");
    }

    /// (#2093) `with_defaults()` writes the `hooks` block visible, gated off,
    /// with an empty (but present) `rules` array — same feature-block shape
    /// as `redis`/`audit`: `enabled` declared first, sub-defaults populated
    /// where a literal is sensible (`outbox_dir`), and the array left empty
    /// because darkmux ships no opinion about which rules an operator wants.
    #[test]
    fn hooks_config_visible_default_and_round_trips() {
        let cfg = DarkmuxConfig::with_defaults();
        let hooks = cfg.hooks.as_ref().expect("hooks block is written visible");
        assert_eq!(hooks.enabled, Some(false));
        assert_eq!(hooks.outbox_dir.as_deref(), Some("~/.darkmux/hooks"));
        assert_eq!(hooks.rules.as_ref().map(|r| r.is_empty()), Some(true));
        // (#2093 merge-gate finding 5) the hard outbox cap is visible too —
        // an operator tuning it shouldn't have to know the field exists
        // before they can find it in their own config.json.
        assert_eq!(hooks.max_outbox_mb, Some(256), "default hard cap is visible in the written config");

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(json.contains("\"hooks\""), "hooks block visible in serialized config");
        assert!(json.contains("\"rules\": []"), "empty rules array is VISIBLE, not omitted: {json}");
        assert!(
            json.find("\"hooks\"").unwrap() > 0
                && json[json.find("\"hooks\"").unwrap()..].find("\"enabled\"").unwrap()
                    < json[json.find("\"hooks\"").unwrap()..].find("\"outbox_dir\"").unwrap(),
            "enabled precedes outbox_dir within the hooks block"
        );

        // Lossless round-trip, including a populated rule.
        let populated = r#"{
            "hooks": {
                "enabled": true,
                "outbox_dir": "~/.darkmux/hooks",
                "rules": [
                    { "match": { "action": "crawl.*" }, "http": "http://127.0.0.1:8790/events" },
                    { "match": { "action": "dispatch.error", "session_id": "abc" }, "http": "http://localhost:9000/alerts" }
                ]
            }
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(populated).unwrap();
        let hooks = cfg.hooks.as_ref().unwrap();
        assert_eq!(hooks.enabled, Some(true));
        let rules = hooks.rules.as_ref().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].http.as_deref(), Some("http://127.0.0.1:8790/events"));
        assert_eq!(rules[0].r#match.as_ref().unwrap().action.as_deref(), Some("crawl.*"));
        assert_eq!(rules[1].r#match.as_ref().unwrap().session_id.as_deref(), Some("abc"));
        let back: DarkmuxConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.hooks.as_ref().unwrap().rules.as_ref().unwrap().len(), 2);

        // Absent block -> None (lenient; an older config never carries it).
        let cfg: DarkmuxConfig = serde_json::from_str(r#"{ "machine_id": "x" }"#).unwrap();
        assert!(cfg.hooks.is_none(), "absent hooks block is None, not a brick");
    }

    /// (#2093) An unrecognized `hooks` sub-field (schema skew from a newer
    /// binary) overflows into `extras`, never bricking the parse — the
    /// lenient-on-read contract (registry entry 7).
    #[test]
    fn hooks_config_unknown_field_overflows_to_extras() {
        let json = r#"{ "hooks": { "enabled": false, "future_field": 42 } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        let hooks = cfg.hooks.as_ref().unwrap();
        assert_eq!(hooks.extras.get("future_field"), Some(&serde_json::json!(42)));
    }

    /// (#933) `FleetMode::parse` is lenient (trim + case-insensitive) and
    /// returns `None` for an unrecognized token so doctor can flag the typo
    /// rather than silently coercing it; `as_str` round-trips the canonical
    /// lowercase token.
    #[test]
    fn fleet_mode_parse_and_roundtrip() {
        assert_eq!(FleetMode::parse("hub"), Some(FleetMode::Hub));
        assert_eq!(FleetMode::parse("  PEER "), Some(FleetMode::Peer));
        assert_eq!(FleetMode::parse("standalone"), Some(FleetMode::Standalone));
        assert_eq!(FleetMode::parse("hubb"), None, "typo → None, not silently standalone");
        assert_eq!(FleetMode::default(), FleetMode::Standalone);
        for m in [FleetMode::Standalone, FleetMode::Hub, FleetMode::Peer] {
            assert_eq!(FleetMode::parse(m.as_str()), Some(m));
        }
    }

    /// (#933) A typo'd `fleet.mode` must NOT fail the whole-config parse (the
    /// lenient-read doctrine) — it lands as a plain string the accessor/doctor
    /// interpret, never bricking the other settings.
    #[test]
    fn bad_fleet_mode_does_not_brick_config() {
        let json = r#"{ "machine_id": "x", "fleet": { "mode": "hubb" } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("x"), "other fields still parse");
        assert_eq!(cfg.fleet.as_ref().unwrap().mode.as_deref(), Some("hubb"), "raw token preserved for doctor");
        assert_eq!(FleetMode::parse(cfg.fleet.unwrap().mode.as_deref().unwrap()), None);
    }

    /// Default serializes to `{}` and round-trips empty — the forward-compat
    /// guarantee (mirrors `runtime_compaction_config_default_round_trips_empty`).
    #[test]
    fn default_round_trips_empty() {
        let cfg = DarkmuxConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, "{}");
        let back: DarkmuxConfig = serde_json::from_str(&json).unwrap();
        assert!(back.machine_id.is_none());
        assert!(back.redis.is_none());
        assert!(back.dirs.is_none());
        assert!(back.runtime.is_none());
        assert!(back.extras.is_empty());
    }

    #[test]
    fn full_shape_round_trips() {
        let json = r#"{
            "schema_version": "1.0",
            "machine_id": "studio",
            "lms_bin": "/usr/local/bin/lms",
            "lmstudio_url": "http://localhost:1234",
            "dirs": { "flows": "~/dm/flows", "audit": "~/dm/audit" },
            "redis": { "host": "100.64.0.2", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000 },
            "runtime": { "inactivity_timeout_seconds": 600, "max_turns": 40, "strict_selection": true },
            "serve": { "token_keychain": true, "read_auth": true }
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("studio"));
        assert_eq!(cfg.redis.as_ref().unwrap().host.as_deref(), Some("100.64.0.2"));
        assert_eq!(cfg.redis.as_ref().unwrap().port, Some(6379));
        assert_eq!(cfg.dirs.as_ref().unwrap().flows.as_deref(), Some("~/dm/flows"));
        assert_eq!(cfg.runtime.as_ref().unwrap().max_turns, Some(40));
        assert_eq!(cfg.runtime.as_ref().unwrap().strict_selection, Some(true));
        // (#881, #2988) the serve auth switches deserialize from the config tier.
        assert_eq!(cfg.serve.as_ref().unwrap().token_keychain, Some(true));
        assert_eq!(cfg.serve.as_ref().unwrap().read_auth, Some(true));
        // Re-serialize → parse → still equal on the load-bearing fields.
        let round = serde_json::to_string(&cfg).unwrap();
        let back: DarkmuxConfig = serde_json::from_str(&round).unwrap();
        assert_eq!(back.machine_id, cfg.machine_id);
        assert_eq!(back.redis.as_ref().unwrap().port, Some(6379));
    }

    /// (#1758) An existing `~/.darkmux/config.json` written by a pre-1.8
    /// binary still carries `"orchestrator": "<value>"` on disk. Loading it
    /// on THIS binary must not error or brick the rest of the file — the
    /// now-unknown key lands in `extras` (the same forward-compat overflow
    /// a genuinely-future key would use) and every other field still parses.
    #[test]
    fn old_config_with_removed_orchestrator_field_still_loads() {
        let json = r#"{
            "schema_version": "1.7",
            "machine_id": "studio",
            "orchestrator": "claude-code",
            "lms_bin": "lms"
        }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("studio"), "sibling fields still parse");
        assert_eq!(cfg.lms_bin.as_deref(), Some("lms"), "sibling fields still parse");
        assert_eq!(
            cfg.extras.get("orchestrator").and_then(|v| v.as_str()),
            Some("claude-code"),
            "the removed field lands in extras, not a typed slot or a parse error"
        );
    }

    /// Unknown top-level keys land in `extras` and re-serialize flat (a newer
    /// config read by an older binary) — and the Redis section has NO
    /// password field, so a stray `password` key would land in `extras`, not
    /// a typed slot (the carve-out holds structurally).
    #[test]
    fn unknown_keys_land_in_extras_and_reserialize_flat() {
        let json = r#"{ "machine_id": "x", "future_knob": 7, "nested_future": {"a": 1} }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.machine_id.as_deref(), Some("x"));
        assert_eq!(cfg.extras.get("future_knob").and_then(|v| v.as_u64()), Some(7));
        let out: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        let obj = out.as_object().unwrap();
        assert!(!obj.contains_key("extras"), "extras must flatten, not nest");
        assert!(obj.contains_key("future_knob"), "unknown key re-serializes flat");
    }

    #[test]
    fn redis_password_is_not_a_typed_field() {
        // The carve-out, structurally: a config with a redis.password lands it
        // in the sub-struct's extras (forward-compat overflow), NOT a typed
        // slot darkmux reads — secrets never resolve from plaintext config.
        let json = r#"{ "redis": { "host": "h", "password": "leaked" } }"#;
        let cfg: DarkmuxConfig = serde_json::from_str(json).unwrap();
        let redis = cfg.redis.unwrap();
        assert_eq!(redis.host.as_deref(), Some("h"));
        assert!(redis.extras.contains_key("password"), "password is overflow, not typed");
    }

    #[test]
    fn load_from_missing_file_is_default() {
        let cfg = DarkmuxConfig::load_from(Path::new("/nonexistent/darkmux/config.json"));
        assert!(cfg.machine_id.is_none());
        assert!(cfg.extras.is_empty());
    }

    #[test]
    fn load_from_malformed_file_is_default_not_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "{ not valid json").unwrap();
        let cfg = DarkmuxConfig::load_from(tmp.path());
        assert!(cfg.machine_id.is_none(), "malformed config falls back to default, never panics");
    }
}
