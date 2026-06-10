# Security Policy

darkmux is a personal, pre-1.0 project released under the MIT license with no
warranty (see [DISCLAIMER.md](./DISCLAIMER.md)). This file describes how to
report a vulnerability and what the project's threat model is — what darkmux
does and does not defend against — so you can make an informed decision about
where and how to run it.

## Reporting a vulnerability

**Please report security issues privately, before public disclosure.**

- Open a [private security advisory](https://github.com/kstrat2001/darkmux/security/advisories/new)
  on the repository (GitHub → Security → Report a vulnerability). This is the
  preferred channel.
- If you cannot use GitHub advisories, file a normal issue that says *only*
  "security issue, please enable private reporting" — without details — and the
  maintainer will open a private channel.

Please do not open a public issue or PR that describes an exploit before a fix
is available. This is a solo-maintained project; fixes ship on the maintainer's
schedule, but security reports are triaged first.

## Supported versions

darkmux is pre-1.0. Only the latest release (and `main`) receives fixes. There
are no backported security patches to older tags — pin a version only if you
accept that it will not receive fixes. Once 1.0 ships, this table will name the
supported `1.x` line.

| Version | Supported |
| ------- | --------- |
| latest release / `main` | ✅ |
| any older tag | ❌ — upgrade to get fixes |

## Threat model

darkmux is a **single-operator, local-first tool**. The trust boundary is *your
machine and the people who can reach it*. It is not a multi-tenant service and
makes no attempt to defend one user from another on a shared host. Within that
frame:

### What darkmux defends

- **The observability viewer escapes all record-derived content.** Flow records
  can carry attacker-influenced strings (a container tool can write its own
  trajectory events — see *Known limitations* below). The viewer
  (`darkmux serve`) HTML-escapes every record field it renders and routes all
  click handling through a single delegated listener with no inline event
  handlers, so a crafted identifier in a flow record cannot execute script in
  your browser. This is enforced by build-time guards in the serve crate.
- **Secrets stay out of plaintext config.** A Redis password is never written to
  `config.json` — it lives in the macOS Keychain, is read at runtime, and is
  never logged. Every Redis URL is wrapped so its `Debug`/`Display` redacts the
  password; the raw value is only exposed at the point of connection. On
  non-macOS, the full URL is supplied via `DARKMUX_REDIS_URL` (your
  responsibility to keep out of shell history / logs).
- **Daemon binds to loopback by default.** `darkmux serve` binds `127.0.0.1`
  unless you explicitly bind elsewhere. CORS is deny-by-default (only
  `null`/`file://` origins); extra origins are opt-in via
  `DARKMUX_DAEMON_CORS_ORIGINS`.
- **No shell interpolation of untrusted strings.** Dispatch invokes Docker and
  subprocesses through argument vectors (`Command::arg`), not a shell string, so
  record/identifier values cannot inject shell commands. Operator-supplied
  identifiers (e.g. `mission_id`) are validated against an identifier charset at
  the CLI boundary.
- **Path parameters are validated.** The daemon's date-scoped flow endpoints
  accept only a strict `YYYY-MM-DD` shape, so a request cannot traverse the
  filesystem.

### What darkmux does NOT defend (by design)

- **AI-generated code is not sandboxed for security.** The default internal
  runtime runs each dispatch in a per-invocation Docker container with
  kernel-enforced *workspace* isolation — better than a bare directory, but
  Docker on macOS is a VM boundary, not a guarantee against a determined
  adversary. The opt-in `--runtime openclaw` / `--runtime-cmd` path has **no
  container at all**: the working directory is a regular directory and an agent
  that runs `rm -rf ~` is not stopped by darkmux. Run only on a machine where
  that risk is acceptable. (See [DISCLAIMER.md](./DISCLAIMER.md).)
- **The daemon has no authentication.** Anyone who can reach the bind address
  can read your flow records and drive the viewer. Keep it on loopback, or put
  it behind your own authenticated reverse proxy / a private network
  (Tailscale) — do not expose `darkmux serve` to the public internet.
- **Flow records are not authenticated.** Any process that can write to the
  flows directory or the Redis stream can author records. The audit sink (below)
  makes *post-hoc tampering detectable*, but nothing makes the live stream
  unforgeable.
- **The audit sink is a detection substrate, not a prevention one.** The opt-in
  `DARKMUX_AUDIT_DIR` sink writes records into a BLAKE3 hash chain;
  `darkmux flow integrity-check` walks the chain and exits non-zero if it is
  broken, so a later edit to an audited record is **detectable at the next
  check**. It does not make records impossible to alter, and running it does not
  by itself make you compliant with any regulation.
- **Model behavior is not policed.** darkmux faithfully executes what the model
  produces. Review agent output before it touches anything you care about.

## Known limitations (tracked)

These are accepted, documented gaps — not undisclosed vulnerabilities:

- **Trajectory event injection ([#237](https://github.com/kstrat2001/darkmux/issues/237)).**
  Container tools can write to the dispatch trajectory file, which the host
  tailer turns into flow records. The identifier fields on those records
  (`session_id`, `machine_id`, `handle`, `model`, `mission_id`) are **stamped by
  the host** from the dispatch context, not read from the container — so a
  container influences only the record *payload* (tool name, finish reason,
  reasoning text, detector detail). That payload is (a) output-encoded at the
  viewer, so it can't execute script, and (b) **size-bounded at ingest** so a
  container can't inject a pathologically large field into the flow stream /
  audit chain / Redis (`reasoning_text` ≤ 256 KiB; `tool_name` / `finish_reason`
  / detector `detail` ≤ 4 KiB). The residual is that a container can still emit
  *plausible* (well-formed, in-bounds) telemetry about its own run — a
  self-affecting concern bounded by the operator's per-dispatch caps, not a
  cross-trust-boundary one.
- **CORS origin normalization ([#289](https://github.com/kstrat2001/darkmux/issues/289)).**
  The `DARKMUX_DAEMON_CORS_ORIGINS` allowlist matches origins by normalized
  string (lowercased scheme/host, trailing slash stripped) but does not collapse
  port-equivalent or hostname-alias forms (`http://localhost` vs
  `http://localhost:80`; `localhost` vs `127.0.0.1`). Configure origins
  consistently. The secure default (loopback bind, deny-by-default CORS) is
  unaffected.

## Scope

In scope: the darkmux binary, the `darkmux serve` daemon and viewer, the
internal runtime, and the dispatch path. Out of scope: vulnerabilities in
third-party software darkmux orchestrates or depends on (LMStudio, OpenClaw,
Docker, the models you load) — report those to their respective projects.
