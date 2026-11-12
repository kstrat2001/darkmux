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
- **Which backend runs is `DARKMUX_SANDBOX` for now** (`docker`, the default; `seatbelt` on macOS; `landlock` on Linux), refused where the host cannot run it; it becomes the `runtime.sandbox` setting with a per-platform default. A caller that needs to know whether its sandbox runs this host's own programs on its loopback (no image, no user to switch to) asks `BackendKind::on_host`, never compares a kind.
- **Landlock (`src/landlock.rs`) is Seatbelt's Linux twin, built differently.** The kernel pieces (user, mount, pid and network namespaces, Landlock) live in `darkmux_platform::confine`, since only that crate may name an OS. The confined process is started by a helper, the executable that planned it run with `__darkmux-landlock` first: `darkmux`'s `main` hands its arguments to `landlock::helper_if_asked` before anything else, and a test binary that starts Landlock sandboxes does it before `main` (`darkmux_platform::confine_before_main!`). Its doc lists what it cannot do.

## The ratchet (`docker-calls.allow`, `src/docker_calls.rs`)

Outside `src/docker.rs`, no production line spells the `docker` program (`"docker"` as a string literal): the allow list is empty since step 1 of #3112. The walk covers every Rust package the repo builds (the workspace, `runtime/`, `plugins/`, `tools/`) and every `build.rs`. Test code is not counted (`tests/`, `tests.rs`, `*_tests.rs`, `#[cfg(test)]` items). The test fails if a file gains one (the fix is the seam) and if a file loses one until its count is lowered, so the list only shrinks.

## Conformance

What every backend must do is one suite, `crates/darkmux-crew/src/sandbox_conformance_tests.rs`, instantiated per backend by its `conformance!` line. It starts real sandboxes, so it is opt-in: `cargo nextest run -p darkmux-crew sandbox_conformance --run-ignored only`. A new backend adds one `conformance!` line and passes every case before any caller uses it. On Linux, run Landlock's alone: `cargo nextest run -p darkmux-crew --lib sandbox_conformance_tests::landlock --run-ignored only` (its own extra cases, `landlock_only`, match that filter too).

## Migration (step 1 of #3112): done

Every caller is on the seam: contained runs and the mods gate (#3548), the reapers and client previews (#3551), the dispatch container, its kills and the watchdog's probe, and Docker's image and engine commands. Next is step 2, a Seatbelt backend that passes the conformance suite; its first question is how a backend that cannot remap paths honors `Mount.at` (`/workspace`).
