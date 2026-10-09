# Configuration and environment variables

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## Configuration (`config.json`)

darkmux's canonical config surface is **`~/.darkmux/config.json`** (#661), written by `darkmux init`. Every setting resolves with one precedence — **`env(DARKMUX_*) > config.json > built-in default`** — and that precedence lives in exactly ONE place: `darkmux_types::config_access` (the env tier is read **live per-access**, so a `set_var` in a test or a power-user export still wins). A reader never has to wonder where a setting came from; `darkmux doctor` surfaces the resolved value + source.

**The file is self-documenting by design.** `init` writes the common knobs *visible* (not hidden as code-defaults), so the operator tunes the file, not the source. Off-by-default integrations are **feature blocks gated by an `enabled` field, not by field-presence** — `init` writes the whole block with `enabled: false` and the sub-defaults populated, so the surface is discoverable and one flip from on:

```json
{
  "schema_version": "2.0",
  "machine_id": "studio",
  "lms_bin": "lms",
  "lmstudio_url": "http://localhost:1234",
  "redis":   { "enabled": false, "host": "127.0.0.1", "port": 6379, "stream": "darkmux:flow", "maxlen": 10000, "telemetry_maxlen": 10000 },
  "audit":   { "enabled": false, "dir": "~/.darkmux/audit" },
  "runtime": { "inactivity_timeout_seconds": 600, "strict_selection": false, "feedback_injection": true, "check_updates": true },
  "serve":   { "port": 8765, "bind": "127.0.0.1", "token_keychain": false, "read_auth": false },
  "power":   { "min_battery_pct": 50, "refuse_start_below_min": true, "pause_running_below_min": true },
  "fleet":   { "mode": "standalone" }
}
```

When proposing a config change to an operator, write the visible field; don't reach for an env var as the primary mechanism. The mechanism is **`darkmux config set <key> <value>`** (#937) — it validates the dotted key (a typo is surfaced with a suggestion, never silently written) and coerces the value to the field's type; `darkmux config get <key>` / `darkmux config list` read it back (`darkmux doctor` shows the fully *resolved* value with env/config/default provenance). **Secrets are NOT config** — `config set` refuses the known secret keys (Redis password, serve token) and points at the `security add-generic-password` Keychain form. **Deliberately NOT written by `init`** (because a literal would be wrong, not because they're hidden): `dirs.*` (derived from the root — `darkmux doctor` shows the resolved path) and caps like `runtime.max_turns` (absent = uncapped, a real behavior).

**Carve-outs — the ONLY things NOT plaintext config:**
- **Redis password → macOS Keychain** (item `darkmux-redis`, the same item the Homebrew wrapper populates). `config.redis` holds only non-secret bits (`enabled`/`host`/`port`/`db`/`stream`/`maxlen`/`telemetry_maxlen`); the password is read at runtime via `security find-generic-password` and never logged — every URL is wrapped in `RawRedisUrl` (redacted `Display` + `Debug`; raw bytes only via `expose_for_probe`). Non-macOS uses the full-URL env override. `redis_url()` resolves `env(DARKMUX_REDIS_URL) verbatim > config.redis.enabled + Keychain > off`.
- **Serve-daemon bearer token → macOS Keychain** (item `darkmux-serve-token`) — #881, same carve-out shape as the Redis password. `config.serve` holds only the non-secret `token_keychain` gate; the token is read at runtime via `security find-generic-password`, wrapped in `RawServeToken` (redacted `Display` + `Debug`; raw bytes only via `expose_for_compare`), and lives in `darkmux-flow` beside the Redis-secret machinery. `serve_token()` resolves `env(DARKMUX_SERVE_TOKEN) verbatim > serve.token_keychain + Keychain > off`. **Read auth and execution auth are separate switches (#2988).** EXECUTION (fleet work submission, the only surface that starts work) always requires the token plus the network-verified sender, whatever else is set. READS (the viewer, every JSON route, the live SSE stream) are governed by `serve.read_auth` (`DARKMUX_SERVE_READ_AUTH`), default `false`: reads stay tailnet-open by design, token or no token. With `serve.read_auth` on, only a request from THIS machine stays open: a loopback peer carrying no reverse-proxy header (`is_local_request` in `darkmux-serve`, the one predicate for every local decision); a request proxied to loopback, e.g. by `tailscale serve` (it sets `X-Forwarded-For` / `Tailscale-User-*`), is NOT treated as loopback and needs `Authorization: Bearer <token>`; a request is also local only when its `Host` header names this daemon: `localhost`, `127.0.0.1`, `[::1]` or the bound address, with no port or the bound port. A DNS-rebound page (loopback peer, the attacker's Host) and a browser reaching the daemon through a header-less `tailscale serve --tcp` proxy (the tailnet name as Host) are therefore not local, and a request with no `Host` is not local. Known limit: `Host` is client-set, so a non-browser client behind a TCP forward that adds no headers can send `Host: localhost` and cannot be told apart from this machine; for that setup use the HTTPS `tailscale serve` (it adds headers) or keep read auth on with a non-loopback bind. An IPv4-mapped loopback peer (`::ffff:127.0.0.1`) is loopback. Every console panel follows the read posture and none is refused for being remote (5.0, operator decision 2026-10-07: a console is a command line, its output names this machine's own facts, and the full output is for this machine or for whoever runs the CLI there over ssh). A caller that is neither local nor holding the token is served each panel REDACTED (`darkmux_types::panel_audience`): the daemon spawns the verb with `DARKMUX_PANEL_AUDIENCE=remote` and the verb renders its remote form (`doctor` runs every check and keeps every row, status and remedy, but withholds the fleet listener, identity and allow-list rows' detail; `config-list` withholds every value that names an address, path, URL or credential pointer, the listener's port, the busy policy, and each allow-list entry beyond its machine name; `flow-status` withholds its directories, the Redis URL and each hook's target); `lab-fixture-list` withholds each fixture's path); then the daemon applies, to stdout and stderr alike (stderr is redacted, not dropped), the shared redaction plus every address, path, endpoint URL and credential pointer this machine is configured with (`darkmux_serve::panel_withheld`: every location `config_access::LOCATION_ACCESSORS` resolves, a test failing on an accessor that is in no list, plus `config.json` as written, the profile and fixture registries and the Redis URL; a value spelled like a machine, profile or endpoint name is spared), and puts one calm notice in `withheld` naming the command to run on this machine, or over ssh, for the full output. A local caller and a token holder read every panel unchanged; `/health` likewise gives a non-local caller only the fleet listener's coarse state. `darkmux serve` also runs the same config gate every other entry point runs, so a wrong-typed value (`serve.read_auth: "true"`) or a retired key refuses the start instead of dropping the file to defaults. `serve` refuses to start with read auth on and no token, and refuses a non-loopback `--bind` unless read auth is on.
- **`DARKMUX_HOME`** — the bootstrap pointer that *locates* the config root (`<root>/config.json`); it can't live inside the config it finds, so it stays an env var. It is the ONE relocation of the darkmux root: a `./.darkmux/` in the working directory is never adopted, nor is a cwd `profiles.json` / `.darkmux.json` registry (only a repo's `lessons.db` and `conventions.json` are read from it; `darkmux doctor` warns about anything else in it), and `dirs.crew` / `DARKMUX_CREW_DIR` are retired (a set retired env var is refused by every command except `doctor` and `config`, through the one check at the top of `run` in `src/main.rs`, and failed by doctor).

**Loading never bricks, consuming refuses** (CONFIG 2.0, contract 7): all-`Option` + `#[serde(flatten)] extras` overflow means a partial, hand-edited or malformed config never bricks the CLI and `darkmux doctor` always runs, but a key the schema does not know is refused at every entry point's preflight and failed by doctor, naming the closest valid key. So an older binary refuses a newer config's new key: add fields as a minor bump, and upgrade the binary before writing them. `CONFIG_SCHEMA_VERSION` lives in `darkmux-types/src/config.rs`.

**Don't confuse `config.json` with the profiles registry.** `~/.darkmux/profiles.json` (the model profiles) is a SEPARATE file, overridden by `--profiles-file` / `DARKMUX_PROFILES` — **renamed in #661 from the misleading `--config` / `DARKMUX_CONFIG`** (those names are retired, not reused, because a real `config.json` now exists).

## Environment variables

Every `DARKMUX_*` var is the top tier of **`env > config.json > built-in
default`**, resolved in one place (`darkmux_types::config_access`), with the
env tier read live per access.

**The full table — every variable, its default, its effect, and the
`config.json` field it maps to — is in [`docs/ENVIRONMENT.md`](../../docs/ENVIRONMENT.md).**
Read it when you need to know what a knob means. To find out what a setting
resolves to *right now*, run `darkmux doctor`, which prints the resolved value
with its provenance — that is the better answer to that question, and it cannot
go stale.

Two rules worth carrying without looking anything up:

- **Secrets are never `config.json`.** The Redis password and the serve token
  live in the macOS Keychain, read at runtime, wrapped so `Debug`/`Display`
  redact them. `darkmux config set` refuses those keys outright.
- **When proposing a setting change to an operator, write the visible
  `config.json` field** via `darkmux config set <key> <value>` — do not reach
  for an env var as the primary mechanism. Env is for per-shell, CI, and test
  overrides.
- **The battery gate is INERT on a machine with no battery (#2706).**
  `power.min_battery_pct` / `.refuse_start_below_min` / `.pause_running_below_min`
  gate whether a run starts and whether one already going pauses. A desktop
  reports no battery at all, and absence is `None` all the way down — not
  "treated as 0%", not "treated as 100%", not defaulted either way. In this
  fleet the always-on hub is a desktop and the battery-bearing laptop is the
  inference peer, so getting that wrong would either block the hub
  permanently or silently disable the gate on the one machine it protects.
  The start policy and the in-flight policy are two config items rather than
  one mode, because refusing to start and interrupting work in progress are
  different decisions with different costs. Every refusal names the charge,
  the floor and the config field that decided — darkmux enforces a threshold
  the OPERATOR wrote and never advises about battery health.
- **A `0` on a darkmux bound means UNBOUNDED, never "instantly".** It reads
  that way for `redis.maxlen`, and (#2361/#2310) for
  `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS` → `runtime.step_command_timeout_seconds`
  (default `600`) — the bound on ONE shell command a step runs (`mods.gate`'s
  `test_command`, `procedural.shell`'s `command`), on whose expiry the
  command's whole process group is killed and the step reports the timeout.
  `darkmux doctor` prints the resolved value and which reading it got.
  (#2902 step 5, #3035) An endpoint's `limits.tokens_per_dispatch: 0` is no
  per-dispatch cap (no warning, nothing metered); an endpoint budget's `limits.window.tokens: 0`
  (or `calls: 0`) is REFUSED ("0 is not a budget; set policy off to turn it
  off"), because read either way it would be an eternal wait.
- **Two concurrency bounds, and they are not interchangeable (#2394, #3035).**
  An endpoint's `limits.concurrent_calls` (in `profiles.json`, on an
  endpoint darkmux does NOT manage; absent, its calls run one at a time and
  darkmux says so once per launch; `0` = unbounded; refused on a managed
  endpoint, whose parallelism the scheduler owns) bounds the calls to that
  endpoint, and on a fleet receiver the jobs other machines send it;
  `DARKMUX_DISPATCH_FREE_CONCURRENCY` → `runtime.dispatch_free_concurrency`
  (default `8`) bounds steps that speak to no model at all
  (`procedural.shell`, `mods.gate`, `records.gather`,
  `deliver.github_review`). They were one cap only because a dispatch-free step
  had no way to say what it consumed, so at the old machine-wide hosted cap
  of 1 six independent shell waits ran strictly one at a time. The
  `remote.*` settings are retired (#3035): `tokens_per_dispatch`, `policy`
  and `concurrent_calls` live on the endpoint. See "Seat classes" in
  `DESIGN.md`.
