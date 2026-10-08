# Model-facing prompt construction

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## Model-facing prompt construction (AI-convention defaults + term provenance)

Local-AI models under clean dispatch context have no harness history. They can't ground darkmux-internal vocabulary by induction. Every model-facing prompt — role `.md` files, skill descriptions, the autonomous-dispatch preamble, workload prompts under `templates/builtin/workloads/`, feedback-injection templates, runtime-telemetry message wording — defaults to **AI-convention terminology** the model already recognizes from its training. When a darkmux-specific term is genuinely needed, **provide provenance** so the model can ground it.

### Convention defaults

- "the user" (not "the operator", "the human user") — the universal message-role term; "operator" is darkmux-internal vocabulary
- "system message" / "system prompt" — canonical for the system-role text
- "tool calls" / "function calls" — canonical for agent loops
- "the assistant" / "your previous turn" — self-referential canonical
- XML structure (`<example>`, `<context>`, `<instructions>`) over ad-hoc section headers when content is hierarchically structured — Anthropic-trained models recognize the convention; other major-family models parse it cleanly
- Markdown inline code (`` `cmd` ``) and triple-fenced code blocks for commands

### Provenance options for darkmux-specific terms

When a darkmux term genuinely must appear in a model-facing prompt (e.g., a verb name the model invokes, or a structural identifier present in the workload), attach provenance via one of:

1. **Tag/marker block** at first use: `<darkmux-term name="role">a stance + tool palette + system prompt for one dispatch</darkmux-term>` — the model parses the XML structure and binds the term to the definition
2. **Supplied conceptual definition** before first use, framed as inline context the model can bind to subsequent uses
3. **Self-identifying prefix** (e.g., `[darkmux-runtime]`) when speaking AS the runtime — the bracketed prefix is the provenance

### Audit surface

When reviewing a model-facing change, ask: *"what does this read as to a fresh-context model with no darkmux history?"* If a term doesn't ground in AI-convention OR have inline provenance, fix one or the other before shipping.

Applies to: role `.md` files, skill manifest `description` fields, the autonomous-dispatch preamble, workload prompts in `templates/builtin/workloads/`, feedback-injection templates (`runtime/src/feedback.rs`), future per-role feedback templates, runtime-telemetry message wording (e.g., `STALL_NUDGE_MESSAGE` in `runtime/src/loop_runner.rs`).

### Origin

Surfaced 2026-05-28 during PR #454/#455 iteration. Auditing the coder role prompt revealed darkmux-internal terms (*"the frontier"*, *"the operator"*, *"brief"*) that a clean-context model couldn't ground. Pairs with operator sovereignty (above) — the operator owns the dispatch intent; the role prompt is how that intent is communicated to the model; the communication has to land.
