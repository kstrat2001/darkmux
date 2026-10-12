# Platform strategies

Claude Code loads this file when it works in this directory. The root `CLAUDE.md` keeps a one-line pointer to it.

## The rule

**An OS difference is a `Platform` method, never a branch at the call site.** darkmux grew up on macOS; a second OS (Linux, first on Omarchy under Asahi) must not mean `if linux` scattered through business logic. So:

- **Callers ask, they don't check.** `darkmux_platform::current().container_host_access()` returns data (`ContainerHostAccess::HostGateway`); the caller decides what that means for it (a docker-run flag). A caller never sees an OS name, so it can't branch on one.
- **Each OS answers in its own module:** `macos.rs`, `linux.rs`, `other.rs` (every answer is what darkmux did before this crate existed, so an OS with no strategy changes nothing). `fake.rs` is for tests: every answer set by the test.
- **`current()` is the only place a `cfg(target_os)` chooses between them.**
- **Every strategy module compiles on every OS**, so each one's answers are unit-tested in CI on Linux and macOS alike. An OS-only call inside a module (a sysctl, a `/proc` read) is `cfg`-gated at that leaf, with its parsing compiled everywhere (the `gestalt_host::mac_probe` shape).
- **Answers are facts or capabilities, not OS names.** `container_host_access` answers *how a container reaches this machine*, not *is this Linux*: Docker Desktop for Linux and Docker Engine are both Linux, and the question is what the engine needs.

## Adding a question

1. A method on `Platform` returning a type that says what the caller needs (an enum, a struct of facts, `Option` where the platform may not know). Doc it as the question.
2. An answer in `macos.rs`, `linux.rs`, `other.rs` and `fake.rs`. On macOS, the answer is the code darkmux already runs there, **moved, not rewritten**: macOS behavior must not change.
3. Tests in `src/tests.rs` for each strategy's answer, and at the call site against `FakePlatform` for both answers.
4. The call site asks `current()`. If it replaced an OS gate, lower that file's line in `os-gates.allow` in the same commit.

## The ratchet (`os-gates.allow`, `src/os_gates.rs`)

Outside this crate, a line naming the OS (`target_os`, `consts::OS`) is allowed only where one already was when this crate arrived. The test fails if a file gains one (the fix is a `Platform` method) and if a file loses one until its count is lowered, so the list records the migration and only shrinks. `runtime/` is outside the workspace and outside the ratchet: it only ever runs in the Linux container.

## Migration order

What blocks Linux use first: container host access and container user (done; the user is overridable as `runtime.container_user`, the platform answering only when it is unset), then machine uid (done: `/etc/machine-id` on Linux, one uid per install; a dual-boot Mac's two OSes are grouped as one machine by a separate hashed hardware id, not by this uid), cpu brand (done: the fleet card's hardware line; on Asahi the chip is named from the device tree the way macOS names it, so a dual-boot Mac reads the same on both sides), the host probe (machine lens, observation only: CPU percent, clusters and memory done on Linux via `cpu_clusters`, `cpu_ticks` and `memory_snapshot`; the macOS mach FFI still lives in darkmux-crew's host_probe, and its strategy answers nothing until that moves here; battery (`battery`, from /sys/class/power_supply) and cluster MHz (`cpu_cluster_mhz`, from cpufreq) done too; power needs the flow payload to carry a total without per-rail figures, a schema change of its own; thermal state and GPU utilization have no Asahi source), then the memory snapshot for planning (the residency planner and scheduler read empty pools off macOS; a conservative figure, decided separately), secrets (Keychain), service management (launchd / systemd).
