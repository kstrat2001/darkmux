# Engagements — the long form

The operative rules live in `CLAUDE.md` ("Engagements (operator-defined
dreamscapes)"). This file is the reasoning behind them: the full range of shapes
an engagement can take, the orchestrator's bridging role, and the
lost-in-translation argument for why engagement context must never be
compressed into a CLI argument.

Moved out of `CLAUDE.md` (2026-08-13) as part of a process audit. It was ~10% of
a file loaded into context on every turn, and it is read-once material — the
rule it justifies is three sentences, and that stayed behind. Nothing here is
retired; it is the same text, in a place that costs nothing until someone wants
the argument.

Tracked as #49.

An engagement is operator-defined, never system-defined. The system doesn't enumerate engagements, doesn't impose a directory shape, doesn't have an `engagement` config file format. The operator decides what's an engagement and how much to describe it.

An engagement can be:

- *"It's just a repo at `~/my-project`"* — one-line; the orchestrator uses the path
- *"I'm planning a 10-day Japan trip with a food focus"* — fuller context; the orchestrator may capture it in a `dreamscape.md` with tilts and constraints
- *"Our wedding site is at knot.com/our-wedding"* — engagement lives at a URL; not a local dir; the orchestrator notes the URL and maps planning sub-tasks to missions
- *"It's a Lovable.dev app I'm prototyping"* — hosted SaaS; the orchestrator references the workspace URL
- *"My personal training goal is sub-5-minute mile"* — life goal; the orchestrator captures the aspiration as missions
- *"I'm running a substack about local AI"* — long-form writing engagement; the orchestrator helps with drafts, editorial calendar, cross-post threading
- *"I'm authoring a book on systems engineering"* — multi-month writing project; the orchestrator scaffolds chapters and tracks research threads
- *"It's classified work I can't describe"* — the orchestrator respects opacity; engagement is named but content is operator-private
- Unwritten entirely — operator carries it in their head; the orchestrator works from conversation

If the operator is unsure what their engagement *is*, the orchestrator can offer a few of the above as starting shapes — picking a medium is itself one of the bridging moves the orchestrator is here to help with.

**The orchestrator's bridging role.** When working on a mission within an engagement:

- Read (or ask for) the engagement context — whatever form it takes
- Capture it durably as an `.md` if the operator wants — location is operator's call (engagement repo root, `de-lab`, a private notes file, etc.)
- Translate the soft free-form context into the structured concepts darkmux supports in code (Mission, Phase, role tilts, preferences) — proposing this translation when it'd help the operator move forward is the orchestrator's by-design job, not a thing to withhold
- Don't pry for structure the operator didn't volunteer — offer a suggestion once, let it land or get redirected, then drop it

Engagements should not be well-defined. They are open-ended dreamscapes where ideas are meant to flourish. darkmux supports the engagements it can support (local dirs, local code work) and stays out of the way for the rest (SaaS, hosted, conceptual, classified). The Rust-level data model in the schema PR (#45) names Role, Crew, Mission, Phase — concepts the system CAN model uniformly. Engagement isn't in that schema by design; it's the layer above where operator judgment lives.

This is operator sovereignty (above) applied at the project-shape level: the operator decides what their projects look like; the system doesn't impose a schema.

Tracked as #49.

### Engagement never enters CLI arg surface

Concrete doctrine that follows from the above: **engagement context lives in the frontier orchestrator layer (CLAUDE.md files, skills, conversation). It never becomes a `--engagement <hint>`-style CLI arg on any `darkmux` verb.**

Three reasons the rule is load-bearing:

- **CLI args quantize.** A `--engagement <hint>` field forces the operator to compress a dreamscape into a single string-token. *"wife time"* as a token is worse than *"this is my marriage time, not a work trip — focus on relaxation, no aggressive sightseeing"* threaded through the actual intent text. The frontier carries that nuance natively; the CLI surface cannot.
- **Utility agents are the wrong layer for engagement interpretation.** A 4B mission-compiler asked to *"interpret the operator's relationship to this engagement"* is the capability mismatch the utility-vs-specialist split (role-families, defined below) exists to prevent. Engagement nuance interpretation is judgment-bearing work that belongs to the frontier — never to a utility agent and never to a CLI arg the utility agent will read.
- **The frontier already handles it.** *"Plan our Japan trip — focus on relaxation, no aggressive sightseeing, this is for my marriage"* reads richer than `--engagement "wife time"` + `"plan Japan trip"` because the nuance threads through prose, not into a separate enum. A frontier-orchestrator-driven workflow gets engagement-shaping for free; a bare CLI invocation gets it by the operator putting context in the input text itself.

For new CLI verbs that would benefit from "context-aware" output: the operator carries that context into the verb's primary input. No separate `--engagement`, no `--context`, no `--vibe`. If the operator has no frontier orchestrator and wants context-shaping, they write the context into the input prose where the utility agent reads it as part of its bounded structuring job.

### Why the line matters at scale — the lost-in-translation problem

The mechanical reasons above (quantization, capability mismatch, etc.) are downstream of a deeper principle. **The pattern is older than AI:** in any organization, when admin staff translate vision → tasks, the vision quietly dies in the translation. The admin role IS narrower — that's why an admin layer can absorb volume — but applying that layer to vision-bearing work is the antipattern. Same dynamic in the AI stack: darkmux's *utility* layer is the AI analog of the org-world admin layer; pushing engagement-bearing work into it produces the same lost-in-translation failure mode.

What makes the line load-bearing:

- **Engagement is where the *why* lives.** The frontier orchestrator can hold engagements because it can sit in operator context, hold contradictions, and carry nuance across turns. A 4B utility agent can't hold contradictions — it'll resolve them. That resolution is where vision gets lost. A `--engagement "wife time"` flag forces the utility agent to do that resolution before it has the context to do it well.
- **The utility AI is the basic planning layer, not the strategic layer.** Capacity-matched to its actual job (bounded inputs, structured outputs, throughput). Asking it to ALSO carry *"what does this mean for the operator's broader life / org / book / engagement"* loads it past its capacity. Even when it produces something, that something is the small-picture compression of the big picture.
- **The cost scales with org size.** A solo operator can correct utility output in the next turn — the loop is tight enough that drift gets caught. An organization where the admin layer is making decisions BEFORE the operator/frontier sees them is the scenario where *big dreams get eaten alive by small bugs written by admin staff who don't have capacity yet to hold the big picture vision.* darkmux's utility layer can have exactly that pathology if its scope leaks into engagement territory; the line drawn here is what prevents it.

The frontier orchestrator's role in this layering is named **vision guard** — the layer that protects the operator's engagement-level intent from being compressed before it has been translated into structure the utility layer can handle. The cultivation discipline (how operators *shape* their frontier to actually hold their vision — CLAUDE.md files, skills, memory, conversation history) is the next-order concern; tracked separately as [#130](https://github.com/kstrat2001/darkmux/issues/130).

Surfaced 2026-05-14: Sprint 3 of #113 originally added `--engagement` to `darkmux mission propose`; operator caught it pre-merge as a doctrine violation against #49. Removed in the same PR, and the rule made explicit here so future verbs don't re-introduce it. The lost-in-translation framing came from the same exchange — codified here because the *why* is harder to reconstruct from the rule alone, and future verbs that look context-shaped will tempt the same drift.
