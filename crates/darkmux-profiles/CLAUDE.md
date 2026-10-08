# Namespace convention

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## Namespace convention (darkmux state in shared systems)

When darkmux maintains state in a system other consumers also use — LMStudio loaded instances, anything operator-managed — **darkmux-owned entries are namespaced** so they can be recognized at a glance and so darkmux's own state-mutating operations can scope themselves to only the namespaced subset. User state is then off-limits by construction, not by careful coding.

### Current namespaces

| System | Form | Example |
|---|---|---|
| LMStudio loaded identifier (visible in `lms ps`) | `darkmux:<model-id>` | `darkmux:qwen3.6-35b-a3b` |

(A previous namespace, `darkmux/<role>` for openclaw agent ids, was retired along with the openclaw shell-out path in #1405.)

### Why this matters

Without the namespace, darkmux's operations have to fall back on heuristics or persistent state files to know "did I bring this up, or did the user?" Heuristics are fragile (the user might happen to use the same naming convention); state files go stale (user force-quits, LMStudio restarts, manual unloads). The namespace IS the state — durable, visible, self-describing. If `lms ps` shows `darkmux:qwen3.6-35b-a3b`, that's a darkmux load and `darkmux machine eject` can unload it. If it shows `qwen3.6-35b-a3b` with no prefix, that's user state and darkmux leaves it alone.

### The namespace is on the wire for a local dispatch, not just in `lms ps` (#2240)

When darkmux loads a model under `darkmux:<id>`, the underlying LMStudio model key is unchanged — `lms ps` shows `identifier=darkmux:foo, modelKey=foo`. Earlier revisions of this section said the namespace was "invisible at dispatch time" because a bare `model: "foo"` in the chat-completions call still resolved via the `modelKey` match. That was true only in the single-resident world, and it stopped being safe the moment the planner started deliberately creating co-residency (`darkmux:foo` loaded *alongside* a foreign, user-loaded `foo` — a sanctioned outcome, not an edge case): LMStudio's own bug tracker documents bare-key resolution across multiple same-key residents as undocumented/ambiguous, so which instance answers a bare `foo` under co-residency is not a contract darkmux can rely on. Dispatching against the wrong one is exactly the #1135 ghost — a user-loaded copy of the right model has unknown load configuration (context window, TTL, quant), and a confidently-wrong response from it looks identical to a correct one until something silently truncates.

**So a dispatch against a LOCAL LMStudio instance puts the namespaced identifier on the wire, not the bare key.** `resolve_dispatch_model_internal`'s internal dispatch path (CLI `dispatch`, radio, acp, crawl, the lab providers — everything riding it) puts `darkmux:<id>` (or the profile's explicit `identifier` opt-out) on the HTTP `model` field and the container's `--model` flag, the SAME identifier the residency preflight just loaded (or reused) it under (#2240; `dispatch_wire_model_id`). This is the mechanism LMStudio itself documents for addressing one of several resident copies of a base model.

**Two paths are deliberately exempt and stay bare, so the claim above is scoped, not global.** A dispatch staffed on an UNMANAGED endpoint never routes through that function at all — its `model` is the provider's own deployment name, and there is no LMStudio residency to namespace against. And a dispatch pointed at a non-LMStudio base URL (the mock-model harness, `skip_lmstudio_residency`) stays bare for the same reason: darkmux loaded nothing there, so there is no instance to address. Reading the bolded sentence as "every wire `model` in darkmux is namespaced" would be wrong in both directions. The generic `dispatch.single_shot` / `dispatch.map` StepKinds sit on the same split from the other side — their `config.model` is *already* the namespaced wire identifier and `config.model_key` carries the bare loadable key the wave loader's `lms load` needs (`crates/darkmux-crew/src/step_kinds/builtins.rs`, #1442 ship-2b) — so those kinds pass `config.model` through untouched by design rather than re-deriving it. Existing dispatcher configs need no migration — the identifier is a load-time detail this layer now carries through, not a new operator-facing field.

**The trade, stated because it is a real behavior change.** A namespaced identifier exists only while darkmux's own instance is resident, and LMStudio answers a request naming an absent identifier with a hard 400 rather than loading anything. So a dispatch now FAILS if that instance disappears between the residency preflight and the call — a TTL expiry, an `lms unload` / `darkmux machine eject` from another shell, an LMStudio restart. Pre-#2240 a bare key always resolved, worst case by silently JIT-loading a fresh copy at LMStudio's 4096 default. The loud failure is the better answer (a confidently-truncated response is worse than an error), and `residency_lost_detail` re-words it so the operator reads "darkmux's instance went away", not "you named a model that does not exist".

### Conventions for new code

When writing a new feature that mutates LMStudio state on the operator's behalf:

1. **Generate the namespaced form** at the point of write. See `darkmux_profiles::ownership::namespaced_identifier`.
2. **Filter on the namespace** at the point of read/cleanup. See `darkmux_profiles::ownership::is_darkmux_owned`.
3. **Pass-through explicit overrides** — if the operator sets an explicit identifier in their profile, don't override it. The namespace is the *default*; the operator can opt out.

### Operator-facing commands

- `darkmux machine status` — list `lms ps` results grouped by ownership (darkmux-managed vs user state). Read-only.
- `darkmux machine eject [--dry-run]` — unload everything in the `darkmux:` namespace; never touches user state. Use to release darkmux's RAM footprint without disturbing other tools.
- `darkmux dispatch <role-id> <text>` — dispatch a single turn to the named role. Looks up the role manifest + `.md` system prompt, then runs the role through the **internal runtime** (per-dispatch `darkmux-runtime` Docker container, mounted workspace tempdir, in-house Rust agent loop with streamed flow records). Pass `--image <tag>` (#703) to dispatch into a specific environment. By default darkmux runs the slim (python + node) runtime image built for its own version: a local `darkmux-runtime:latest` only when its `org.opencontainers.image.version` label matches the binary, otherwise the version-pinned `ghcr.io/kstrat2001/darkmux-runtime:<version>`, pulled on first use, and never an unlabeled or mismatched image (#2923). Naming a `darkmux-runtime:<tag>` (or GHCR) image runs that image after the same check, refused on mismatch. Naming any OTHER Linux image (e.g. `rust:slim`, the operator's own CI image) makes darkmux **inject** its static runtime binary into that image (bind-mount + entrypoint override) so the coder runs in that environment and can `cargo check`/`test` in-sandbox — the inner verify loop. darkmux ships NO per-language images (it brings the agent; you bring the environment). The image needs `bash` + coreutils (debian/ubuntu-family work as-is; bare-alpine needs them added). **For Rust in-sandbox lint** (`cargo clippy`), name an image that includes the clippy component — `rust:latest` ships it; bare `rust:slim` may not, and a missing clippy slips lint to the frontier gate. The coder role makes one bounded `rustup component add clippy` attempt when cargo is present but clippy isn't (the single exception to its no-toolchain-setup rule), but the reliable fix is the operator's image choice — BYO-environment, so bring clippy if you want in-sandbox lint. On a `--profile <p>@<machine>` dispatch the tag is sent along, and the other machine runs it only if its allow-list entry for this machine lists it.

(A previous entry here, the `crew sync` verb — reconciling an openclaw agent registry with the crew role manifests — was removed along with the openclaw shell-out path in #1405; the internal runtime reads role manifests directly, so there is no registry left to sync.)

Tracked alongside operator sovereignty (#44) and issues [#52](https://github.com/kstrat2001/darkmux/issues/52) (LMStudio namespace), [#55](https://github.com/kstrat2001/darkmux/issues/55) (full pre-flight checklist — partial coverage in `dispatch` today), and the `qa-review` migration that brought these verbs into the dispatch path.
