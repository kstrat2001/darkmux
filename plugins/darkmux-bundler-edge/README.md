# darkmux-bundler-edge

The **second reference implementation** of darkmux's frozen `--bundler`
plugin contract ([#1319](https://github.com/kstrat2001/darkmux/issues/1319)),
proving the contract at N=2: a second language (Python, next to the first
reference plugin's Rust), for a template DSL rather than a systems
language, with zero dependencies. `darkmux-bundler-rust` (this directory's
sibling, `../darkmux-bundler-rust/`) proved the contract was implementable
by a Rust-source-aware bundler; this plugin proves it generalizes to a
domain the built-in bundler was never meant to reach — [Edge.js][edge]
templates — without reaching into any darkmux-internal crate.

[edge]: https://edgejs.dev/

## The contract

Invoked exactly as:

```
darkmux-bundler-edge --diff <file> [--worktree <dir>]
```

- `--diff` — a unified diff (as produced by `git diff` / `gh pr diff`).
  Required.
- `--worktree` — a local checkout to read full template contents from, for
  whole-file spans and accurate manifest resolution. Optional — omitted
  entirely when the caller has no checkout (e.g. a `--github` source with
  no local clone); the plugin still produces bundles from the diff's own
  hunk context alone, just without whole-file spans.

On success, it emits a `BundleSet` JSON document (`{"bundles": [...]}`) on
stdout and exits `0`. Each bundle:

| Field | Meaning |
|---|---|
| `id` | `"<template>@<path>"` — the template's basename stands in for a function name, since an Edge template is itself the review unit. |
| `code` | `[{path, start, end}]` — 1-indexed, inclusive line spans the review pipeline resolves against the worktree. |
| `facts` | Mechanical `differential`-family observations about the diff. |
| `fact_family` | Always `"differential"` (matches the Rust reference plugin's v1 scope). |
| `manifest` | *(optional)* External templates this one references. |
| `truncated` | *(optional, `true` when present)* The `code` spans are hunk windows, not the whole file. |

When the diff has no reviewable `.edge` hunks, the plugin fails **loudly**:
a clear message on stderr and a non-zero exit, never a silent pass. The
review pipeline's `external_bundles` caller (`crates/darkmux-lab/src/lab/bundle/external.rs`)
treats an empty bundle set the same way it treats a non-zero exit — as an
error to surface, not a quiet no-op.

## What it bundles

- **Code spans.** Templates at or under 400 lines get a whole-file span
  (Edge templates usually are this small, and the small-model reviewer
  benefits from full context). Larger templates get merged hunk windows
  (±30 lines of context around each changed region, adjacent windows
  merged) with `truncated: true` so the caller knows it's not seeing the
  whole file.
- **Facts** (the `differential` family): line/hunk counts; interpolation
  expressions (`{{ expr }}` / `{{{ expr }}}`) added or removed — Edge
  comments (`{{-- ... --}}`) are explicitly excluded from this count, since
  they are documentation, not template logic; directive line changes
  (`@if`/`@each`/`@component`/`@include`/... ); `class="..."` attribute
  churn.
- **Manifests.** `@include(...)` and `@!component(...)` targets the
  template references, so the review pipeline can pull in cross-template
  context. A template's own name appearing in its own source (e.g. inside
  a usage-example comment) is excluded — that's not an external reference.

## Install

Copy the script anywhere on `PATH` and make it executable:

```bash
cp plugins/darkmux-bundler-edge/darkmux-bundler-edge ~/.local/bin/
chmod 700 ~/.local/bin/darkmux-bundler-edge
```

Then either name it explicitly on a review launch:

```bash
darkmux mission launch review --param bundler=darkmux-bundler-edge
```

or wire it into an editor integration (e.g. an ACP client) that invokes
`--bundler` automatically for `.edge`-touching diffs.

Requires Python 3 only — no `pip install`, no virtualenv.

## Keeping a personal copy in sync

This repo copy is the **reference** — the canonical, tracked version the
darkmux project maintains and tests against. An operator's own installed
copy at `~/.local/bin/darkmux-bundler-edge` is theirs: free to diverge for
a house style, a repo-specific convention, or an extra fact family. Diverge
deliberately; if a fix or improvement belongs upstream, it's a normal PR
against this file.

## Origin

Written live during the [#1388](https://github.com/kstrat2001/darkmux/issues/1388)
ACP/Zed demo night, after the first live editor review closed degenerate on
an `.edge`-only diff ("3 skipped: 3 non-code extension") — the built-in
bundler is TypeScript-only, and Edge templates aren't source it can parse.
That was `--bundler`'s [loud-failure doctrine](https://github.com/kstrat2001/darkmux/issues/1319)
working exactly as designed: rather than silently reviewing nothing, the
gap was visible enough to fix on the spot. Graduated into the repo as the
second reference plugin per [#1686](https://github.com/kstrat2001/darkmux/issues/1686).
See the origin trail: [#1388, trail 2](https://github.com/kstrat2001/darkmux/issues/1388#issuecomment-5211504971).
