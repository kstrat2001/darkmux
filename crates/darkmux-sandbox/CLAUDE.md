# The sandbox seam

Claude Code loads this file when it works in this directory.

## The rule

**Code that runs something isolated builds a `SandboxPolicy` and spawns what a `SandboxBackend` returns. It never spells a container engine's command line.** darkmux grew up on Docker; a second backend (Apple's `container`, Seatbelt) must not mean a second argv builder at every call site (#3112).

- **The policy says what the sandbox may see and do:** its mounts (read-only or read-write, a later nested mount layered over an earlier one, like a checkout's `.git` read-only over its tree), its network (`None` or `Default`), ports published on loopback only, whether it reads stdin, a name that reaches the host, its limits, its environment, its user, its labels, its image and command. Nothing outside it is granted.
- **A backend's lifecycle:** `run` (attached), or `create` then `start` (a created sandbox is found by `remove` before anything in it runs; `stop` does not end one that has not started); `stop(grace)` (SIGTERM, then the kill); `kill` (succeeds only against a running sandbox, so its success is proof it ran); `remove`; and `list`/`parse_list` by label, name prefix or exact name.
- **Docker's own image and engine commands** (inspect, pull, `version`, copying a file out of an image) are `DockerBackend` functions, not trait methods: a backend without images has nothing to do there.
- **Some things are not in the policy because no caller may turn them off:** every sandbox is removed when it exits, has an init as its first process, drops every capability and cannot gain privileges.
- **A backend plans processes; it runs nothing.** Bounding a run (deadline, interrupt, process-group kill) stays with the caller's runner (`darkmux-crew`'s `bounded_command`). A run cut short is followed by `remove(handle)`, because killing the client need not stop the sandbox.
- **A `SandboxHandle` is what a record stores** (a reaper's, a resume's): the backend and the sandbox's id there.
- **Landlock (`src/landlock.rs`) is Seatbelt's Linux twin, built differently.** The kernel pieces (user, mount, pid and network namespaces, Landlock) live in `darkmux_platform::confine`, since only that crate may name an OS. The confined process is started by a helper, the executable that planned it run with `__darkmux-landlock` first: in `darkmux` it is a row of the multi-call entry (`src/multicall.rs`, routed to `landlock::helper` before anything else), and a test binary that starts Landlock sandboxes does it before `main` (`darkmux_platform::confine_before_main!`). Its doc lists what it cannot do.
- **Which backend runs is the `runtime.sandbox` setting** (#3112), resolved by `darkmux_types::config_access::sandbox` (env `DARKMUX_SANDBOX` > `config.json` > the platform's answer, `Platform::default_sandbox`: Seatbelt on a Mac with `sandbox-exec`, Landlock on a Linux whose kernel can confine a process (`Platform::confine_support`), Docker elsewhere). `selected()` reads it and refuses what cannot run here. **Starting** a sandbox goes through `backend()`, which returns that refusal as an error: no caller gets Docker by accident. **An existing sandbox** is reached through its handle's backend (`for_handle`), whatever is configured now, and a reaper lists every backend this host runs (`available_backends`), so a sandbox left from before the setting changed is still found. A long-running process that reads its config per request uses `selected_in` with a fresh `config.json`. This crate parses no config itself. A test build (`test-support`) and every CLI test spawn resolve Docker on every host, so no outcome depends on the machine. Messages name the sandbox with `placed_in`, and a Docker remedy comes only from `docker_remedy`, which is `None` under any other sandbox.

## The ratchet (`docker-calls.allow`, `src/docker_calls.rs`)

Outside `src/docker.rs`, no production line spells the `docker` program (`"docker"` as a string literal): the allow list is empty since step 1 of #3112. The walk covers every Rust package the repo builds (the workspace, `runtime/`, `plugins/`, `tools/`) and every `build.rs`. Test code is not counted (`tests/`, `tests.rs`, `*_tests.rs`, `#[cfg(test)]` items), nor is a `config_enum!` row inside a `config_enum!` invocation in `crates/darkmux-types/src/config.rs` (`runtime.sandbox`'s `docker` token, a setting's value, not the program); the same row shape anywhere else counts. The test fails if a file gains one (the fix is the seam) and if a file loses one until its count is lowered, so the list only shrinks.

## Conformance

What every backend must do is one suite, `crates/darkmux-crew/src/sandbox_conformance_tests.rs`, instantiated per backend by its `conformance!` line. It starts real sandboxes, so it is opt-in: `cargo nextest run -p darkmux-crew sandbox_conformance --run-ignored only`. A new backend adds one `conformance!` line and passes every case before any caller uses it. On Linux, run Landlock's alone: `cargo nextest run -p darkmux-crew --lib sandbox_conformance_tests::landlock --run-ignored only` (its own extra cases, `landlock_only`, match that filter too).

## Migration (step 1 of #3112): done

Every caller is on the seam: contained runs and the mods gate (#3548), the reapers and client previews (#3551), the dispatch container, its kills and the watchdog's probe, and Docker's image and engine commands. Next is step 2, a Seatbelt backend that passes the conformance suite; its first question is how a backend that cannot remap paths honors `Mount.at` (`/workspace`).
