# The sandbox seam

Claude Code loads this file when it works in this directory.

## The rule

**Code that runs something isolated builds a `SandboxPolicy` and spawns what a `SandboxBackend` returns. It never spells a container engine's command line.** darkmux grew up on Docker; a second backend (Apple's `container`, Seatbelt) must not mean a second argv builder at every call site (#3112).

- **The policy says what the sandbox may see and do:** its mounts (read-only or read-write, a later nested mount layered over an earlier one, like a checkout's `.git` read-only over its tree), its network (`None` or `Default`), its limits, its environment, its user, its labels, its image and command. Nothing outside it is granted.
- **Some things are not in the policy because no caller may turn them off:** every sandbox is removed when it exits, has an init as its first process, drops every capability and cannot gain privileges.
- **A backend plans processes; it runs nothing.** Bounding a run (deadline, interrupt, process-group kill) stays with the caller's runner (`darkmux-crew`'s `bounded_command`). A run cut short is followed by `remove(handle)`, because killing the client need not stop the sandbox.
- **A `SandboxHandle` is what a record stores** (a reaper's, a resume's): the backend and the sandbox's id there.
- **Which backend runs on which host is a `darkmux-platform` question, later.** Today every caller uses `DockerBackend`.

## The ratchet (`docker-calls.allow`, `src/docker_calls.rs`)

Outside `src/docker.rs`, a production line spelling the `docker` program (`"docker"` as a string literal) is allowed only where one already was when the seam arrived. Test code is not counted (`tests/`, `tests.rs`, `*_tests.rs`, `#[cfg(test)]` items). The test fails if a file gains one (the fix is the seam) and if a file loses one until its count is lowered, so the list only shrinks.

## Conformance

What every backend must do is one suite, `crates/darkmux-crew/src/sandbox_conformance_tests.rs`, instantiated per backend by its `conformance!` line. It starts real sandboxes, so it is opt-in: `cargo nextest run -p darkmux-crew sandbox_conformance --run-ignored only`. A new backend adds one `conformance!` line and passes every case before any caller uses it.

## Migration order

Contained runs (a mod's test command, a lab verify, a grader) and the mods gate: done. Then client previews (`darkmux-serve`'s `client_preview.rs`, `docker create` / `start -a`), then the dispatch container with its resume records, then the reapers (`orphan_reaper.rs`, `dispatch_reaper.rs`), then the image and doctor probes.
