# darkmux-agent (spike)

A minimal in-house agent runtime for darkmux. **This is a spike, not a
production component.** The goal is to prove that darkmux can replace its
dependency on openclaw with a lean kernel-isolated runtime that ships
inside a container.

## Why this exists

darkmux currently delegates agent execution to
[openclaw](https://github.com/openclaw/openclaw). openclaw is a
Discord-first agent framework — darkmux uses about 20% of its surface and
inherits the rest (workspace bootstrap, multi-channel routing, persona
management) as overhead. That worked while darkmux was a profile
multiplexer wrapping an existing runtime; it stopped fitting once the
crew abstraction landed.

The specific failure mode that motivated this spike is captured in
`feedback_no_public_repo_for_personal_engagements.md`: a persistent
`--workdir` symlink inside an openclaw workspace dir survived across
unrelated dispatches and produced a publish-to-public-OSS leak. The
deeper architectural smell is *darkmux state lives inside another tool's
directory tree.*

A container-bounded in-house runtime replaces both layers:

- darkmux owns the runtime end-to-end (no more 20%-of-openclaw)
- The container is the kernel-enforced boundary (no more soft workspace
  isolation that depends on code discipline)

## Spike phases

| Phase | Status | Goal |
|---|---|---|
| 1 | ← here | Alpine image builds; binary runs inside; `/workspace` mount is visible |
| 2 | pending | LMStudio chat-completions client + tool-call loop |
| 3 | pending | Three tools (Bash, Read, Write) with workspace-root path enforcement |
| 4 | pending | `darkmux crew dispatch --runtime=internal <role>` wires it up |
| 5 | pending | Side-by-side comparison vs openclaw runtime; honest writeup |

Phase 5 decides whether the spike graduates to production design or what
the next iteration needs to address.

## Build + run

From this directory:

```
# build the image
docker build -t darkmux-agent-spike .

# default CMD runs the phase-1 sanity check
docker run --rm darkmux-agent-spike

# explicit version probe
docker run --rm darkmux-agent-spike --version

# mount a host directory as the workspace
docker run --rm -v "$(pwd):/workspace" darkmux-agent-spike
```

## What's NOT in this spike

- Multi-turn chat history persistence — single dispatch in, structured
  result out
- Compaction support — openclaw handles that today; if the spike
  graduates, this is the next thing to design (or deliberately leave out)
- Streaming responses — single-shot completions only
- More than 3 tools — adding more is mechanical once the loop works
- Migration of existing crew roles to the new runtime
- Audit-chain wiring for in-container writes

## What the production design will need to address if the spike validates

- Image build + distribution (where does it live? Docker Hub? GHCR?)
- Operator workflow: `darkmux init` → ensures Docker is present + the
  image is pulled
- Migration story for operators with existing openclaw configurations
- The `DARKMUX_RUNTIME_CMD` env var (lab harness) stays runtime-pluggable;
  crew dispatch default flips to the in-house runtime; opt back to
  openclaw via the same flag if needed
- Audit-chain coverage at the volume-mount boundary
