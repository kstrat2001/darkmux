Part of #1698 — closes the panel gap: "is this darkmux?" now gets a grounded answer.

## Summary

Packet B2, the last packet of the radio arc: the answering seat + the session layer.

- **A — Answering pipeline**: a router refusal (panel no-slash channel AND the `darkmux radio` CLI verb) now routes to the ANSWERING seat instead of printing the bare reason + listing. The answerer may itself decline open-domain text (persona-authored "ask your frontier orchestrator" line); the bare reason + listing is now the *last resort*, rendered only when the answering dispatch itself errors.
- **B — Grounding assembler** (`src/radio_answer.rs`, pure/deterministic/zero-model): compiles the command catalog, the live `config list` surface, a compact mission-board summary, the top-level `--help` text, the session's artifact shelf, and (when the text names one) a local mission deep-artifact — enforcing the pinned budget (char-approximated token caps, hard cap ~10K tokens ≈ 40K chars) by dropping help → shelf tail → board → deep artifact → config → catalog, in that order.
- **C — Artifact shelf**: a per-session ring buffer (last 3 rendered outputs) living in the ACP `Sessions` map, written on every slash-routed AND no-slash-routed execution, read only by the assembler.
- **D — The answering role**: `templates/builtin/roles/radio-host.{json,md}` — tool-less, prose-output, `role_family: specialist`. Persona template carries `{{humor}}`, substituted at assembly from `radio.humor` config.
- **E — `radio` config block**: `radio.router_profile` / `radio.answerer_profile` / `radio.humor` (default 65), config schema 1.6 → 1.7 (minor, additive).
- **F — Session config options**: `NewSessionResponse.config_options` advertises a "radio host" select (profile picker, plus a synthetic "use configured default" choice) and a "humor" select (preset ladder — the vendored v1 schema has no numeric/slider kind, only `select`/`boolean`; see the schema-finding section below). `session/set_config_option` applies the change session-scoped and echoes the updated option list.
- **G — Session hygiene pair** (from #1684): minimal `session/load` (`loadSession: true` capability, accepts the resume, restores cwd, replays nothing — commands are stateless views, empty shelf on resume is doctrine) + idle self-exit (`runtime.acp_idle_exit_minutes`, default 30 — the process exits when zero commands are in flight and the idle window has elapsed).
- **H — Carry**: the before-direction fence red-prove test from the #1702 gate (`validate_router_output_bare_fence_before_the_json_block_refuses`) — the total-delimiter-count check already handled this case; this packet just adds the specimen test.

## The mechanism this needed: `DispatchOpts.system_prompt_override`

The `{{humor}}` substitution has to land in the persona TEXT sent to the model, but the shared `dispatch_local_single_shot` primitive resolves the system prompt internally via the role loader with no hook for a pre-substituted override. Added one new optional field, `DispatchOpts.system_prompt_override: Option<String>` — `Some` sends the given text verbatim (and skips the specialist-preamble prepend, since an override caller is handing over the exact prompt it wants); `None` (every pre-existing call site) preserves today's behavior exactly. Mechanical fallout: ~21 existing `DispatchOpts { .. }` literals across the workspace each needed `system_prompt_override: None,` added — driven by `cargo check` until clean, zero behavior change at any of those sites.

## Schema finding (scope F)

The vendored `agent-client-protocol-schema` v1.5.0 has exactly two `SessionConfigKind` variants: `Select` (dropdown) and `Boolean`. There is no numeric/slider kind. The "humor" picker is therefore a `select` over a small preset ladder (`HUMOR_PRESETS = [10, 35, 65, 90]`), not a continuous 0-100 dial — an honest constraint of the protocol version, not an implementation shortcut. Whether Zed renders `config_options` at all is unverified — the live smoke below confirms the WIRE contract (advertised on `session/new`, applied via `session/set_config_option`), not the client-side UI. Flagged for operator dogfood.

## Deliberate scope narrowing (named, not silent)

- **The deep-artifact heuristic ships MISSION-id detection only** (local, `crew::loader::load_missions`/`load_phases`, zero network). A PR-number heuristic (`gh pr view <n>`) was scoped out — it would be the module's only network call and its only external-process spawn, a materially different risk/test profile from every other grounding source (all in-process reads). Named as a follow-up, not built.
- **No dedicated `/config` panel command.** The persona's own prompt instructs it to recommend the EXISTING `darkmux config set <key> <value>` CLI invocation as text (wall 5 — suggest, never execute) — satisfies the "config concierge" addendum's suggest-don't-execute contract without a new panel-command surface.
- Idle self-exit lives under `runtime.acp_idle_exit_minutes`, not a new `acp{}` block or under `radio{}` — it's a process-lifecycle behavior of the whole `darkmux acp` binary (a session doing nothing but slash-command dispatches idles out identically), not radio-specific. Documented inline on the field.
- **Flagged, not fixed: the grounding bundle follows the "radio host" picker to a remote endpoint.** The bundle includes `config.json` (machine_id, lmstudio_url, redis host/port, dirs, orchestrator), mission ids/descriptions, and the artifact shelf (which after a `/review` can hold rendered code-review output over the operator's private diff). This crate already gates an analogous case — `identity_augmentation_allowed(remote_brained)` withholds `identity.md` from remote dispatches — but the answering seat has no equivalent gate today, and any registry profile (including remote-endpoint ones) is one dropdown selection away via `available_profile_names()`. Surfaced per operator-sovereignty doctrine rather than silently shipped or silently blocked; needs an explicit operator decision on where the line goes (a data-boundary gate mirroring identity augmentation, a remote-specific grounding subset, or an accepted risk with a warning) — not built in this diff.

## Fixed from the fresh-context frontier review (before this PR opened)

A `code-reviewer` pass on the diff found one real correctness bug and several should-fix defects, all addressed here:

- **Critical:** `DispatchOpts.system_prompt_override` was honored in `dispatch_local_single_shot` but NOT in `try_resolve_remote_target` — a caller override silently reverted to the unsubstituted loader-resolved prompt (a literal `{{humor}}` in the persona) plus the specialist preamble the override explicitly opts out of, whenever the resolved profile targeted a remote endpoint. Now honored in both paths.
- **`radio.router_profile` was a dead, write-only config knob** — settable, visible in `config.example.json`, documented as taking precedence over `role_profiles.radio-router`, but nothing read it (`src/radio.rs` still passed `profile_name: None` unconditionally). Wired.
- **Idle self-exit could fire immediately after a long command finished** — `last_activity_unix` was stamped only at request receipt, never at completion, so a 45-minute `/review` followed by the very next 60s tick could exit the process while the operator was still reading the result. Now stamped on completion too.
- **`in_flight` could leak if `cx.spawn` itself returned `Err`** (the increment happened before the spawn call; the matching decrement lived inside the future, which then never ran) — permanently disabling idle self-exit. The increment now happens as the spawned future's own first action.
- **`radio.humor` widened `u8` → `u64`** in the config schema — a `u8` field meant `darkmux config set radio.humor 300` failed the WHOLE config write, and a hand-edited out-of-range value in `config.json` would silently reset the ENTIRE config to defaults on next load (the lenient-read fallback). The accessor already clamped to `0..=100`; widening the storage type removes both hazards for free.
- **`session/set_config_option` no longer materializes ghost sessions** for wire-supplied ids this process never minted (was `entry(...).or_default()`; now a no-op on an unknown id, matching the shelf-write convention elsewhere).
- **`session/load` no longer clobbers a live session's shelf/overrides** — now `and_modify`/`or_insert_with` (refresh `cwd`, preserve everything else if the session is already held) instead of a blind `insert`, correct for the "Zed reconnects to a still-running process" case as well as the "process restarted" case.
- **Registry disk I/O moved outside the sessions mutex** in the `session/set_config_option` handler, mirroring the pattern `session/new` already used.
- Renamed a test whose assertions had been replaced but whose name still claimed the pre-B2 behavior (`no_slash_refusal_renders_reason_and_listing_and_records_wall_4` → `no_slash_refusal_routes_to_the_answering_seat_and_records_wall_4`).
- Added the two round-trip tests the review flagged as missing: the shelf round-trip (a prior command's output reaching a later answering-seat dispatch) and the overrides round-trip (a `session/set_config_option` change reaching the dispatch).
- Documented (not deleted) that `Sections::enforce_budget`'s trim path is currently unreachable from `assemble_grounding` in production — the sum of every per-section cap is comfortably under the hard cap today; the order/termination are proven correct by direct tests, and the loop stays live for when a cap changes.

All fixes re-verified: `cargo check`/`clippy -D warnings` clean, full test suite green, and the live smoke re-run end to end (transcript below) after the fixes landed.

## Test plan

- [x] `cargo check --workspace --tests --all-targets` clean, zero warnings
- [x] `cargo test --workspace` — see transcript in the report
- [x] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [x] Live ACP smoke (real model dispatch, no mocks) — transcript in the report:
  - `initialize` advertises `loadSession: true`
  - `session/new` advertises `config_options` (`radio-host`, `humor`)
  - `session/prompt "is this darkmux?"` → router refuses → grounded, in-persona answer from the real `radio-host` dispatch (default profile `deep`, per `default_profile` fallthrough — `role_profiles.radio-host` unset)
  - `session/set_config_option` (humor=90) accepted
  - `session/load` round trip accepted
- [x] `darkmux radio "is this darkmux?"` (CLI path) — same grounded-answer behavior, empty (fresh-process) shelf

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01CW1oqeju57ukFasqevb1p2
