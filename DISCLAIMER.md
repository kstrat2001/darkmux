# Disclaimer

darkmux is a personal project, semver-stable as of v1.0.0, released under the MIT license. The license text already says "no warranty, use at your own risk" in legal language. This file says the same thing in plain English, with specifics, because darkmux does things that warrant a clear-eyed warning.

It's a personal project with a small audience. As of v1.0.0 darkmux follows semver: breaking changes (renames, removals, default changes) ship in a major version bump, signaled rather than silent. If you need stability, pin a version.

## What darkmux does to your machine

- **Reads and writes config files.** darkmux manages the LMStudio loadout internally (its residency arbiter, gestalt, loads and unloads models as each dispatch requires); darkmux also reads/writes its own `~/.darkmux/` state (`config.json`, `profiles.json`, flow records). It does not write to any other agent runtime's config file. If darkmux has a bug, if your config has an unusual shape, or if a write is interrupted, your `~/.darkmux/` state can end up in a broken state. You are ultimately responsible for backing up anything you cannot afford to lose.
- **Talks to network services — but ships none of its own.** darkmux is a conductor, not a provider: no model, no inference server, and no cloud endpoint ships with it. It sends HTTP requests to `http://localhost:1234/v1` to load and unload models on LMStudio, which you installed yourself. If you staff a crew seat with a hosted endpoint profile (OpenAI, Anthropic, or any API you configure), that seat sends the bundled workspace content to whatever vendor you named there — under that vendor's terms, which you accepted when you chose them. darkmux only routes to what your staffing configuration points at; which vendor to trust with your data, if any, is your call and your responsibility, the same way it would be if you called that vendor's API directly. The one thing darkmux reaches out to on its own, by default, is `darkmux doctor`'s update check against `api.github.com/repos/kstrat2001/darkmux/releases/latest` — set `DARKMUX_CHECK_UPDATES=0` (or `config.runtime.check_updates: false`) to turn it off. Outside that one call, darkmux has no telemetry or analytics channel of its own.
- **Uses Docker for the default dispatch + lab path.** `darkmux dispatch` and `darkmux lab run` default to darkmux's internal Rust runtime, which runs inside a per-invocation `darkmux-runtime` Docker container (image built from `runtime/`). Docker is therefore a runtime dependency for that path. darkmux builds and runs that image on your behalf.
- **Runs AI-orchestrated workloads.** In lab mode and on dispatch, darkmux runs an agent loop and can execute shell commands the agent produces, for example `npm test` against AI-generated code. Each dispatch runs in a per-invocation container with kernel-enforced workspace isolation: better than a bare directory, but still not a hardened sandbox (Docker on macOS is a VM boundary, not a security guarantee against a determined adversary). Treat any dispatch the way you would treat running any untrusted script: only on a machine where that risk is acceptable, ideally on a separate user account or VM.

## About AI behavior

AI models, local or hosted, can produce unexpected, incorrect, or unsafe output. They can hallucinate file paths, generate destructive shell commands, edit files in ways you did not intend, and confidently explain why the wrong thing was the right thing to do. darkmux is an orchestration layer; it does not police what the model does. If a model misbehaves, darkmux will faithfully execute the misbehavior. Review agent output before letting it touch anything you care about.

## Results vary by your frontier configuration

darkmux assumes a frontier orchestrator (Claude Code, Cursor, or similar) driving it. Its value depends on how *you* configure that frontier: the frontier models you use as the orchestrator need proper guidance to make the most of darkmux. Contradictory statements between your project's `CLAUDE.md`, the user guide, and other frontier configs will cause more harm than good. Configure to your own strategy and goals. darkmux cannot warrant outcomes that depend on your frontier configuration, which it neither controls nor sees. See [issue #112](https://github.com/kstrat2001/darkmux/issues/112) for the architectural reasoning.

## Flow records and the audit sink

darkmux writes a structured flow record for each dispatch, decision, and review. The always-on `LocalFileSink` writes these to `~/.darkmux/flows/` on your disk: casual provenance, no integrity guarantee.

An opt-in audit sink (enabled by setting `DARKMUX_AUDIT_DIR`; POSIX-only, meaning Linux/macOS) additionally writes those records into a BLAKE3 hash chain. `darkmux flow integrity-check` recomputes that chain and exits non-zero if it diverges, so most post-hoc edits to an audited record are **detectable** at the next integrity check — not all of them; see SECURITY.md for the known gaps. This is a detection property, not a prevention one: the chain surfaces tampering after the fact; it does not make records impossible to alter, and running it does not make you compliant with anything. See "Two layers of liability" below for where your obligations begin.

## About the performance numbers

Benchmarks, throughput claims, and "X tokens/sec" figures in this repository and in the accompanying article series at [substack.com/@DarklyEnergized](https://substack.com/@DarklyEnergized) were measured on the author's hardware: a MacBook Pro with the Apple M5 Max chip and 128 GB of unified memory. Your numbers will differ, sometimes by a lot, depending on chip generation, RAM, thermal conditions, model quantization, context length, and what else is running. Treat the numbers as one data point, not a guarantee.

## Third-party software

darkmux is not affiliated with, endorsed by, or supported by:

- **LMStudio**: a separate product with its own license and terms of service. You are responsible for complying with them, including any commercial-use restrictions.
- **Docker**: a separate product with its own license and terms. The dispatch path depends on a working Docker installation, which you provide and maintain.
- **Apple, Inc.**: "Apple Silicon" and "M5 Max" are Apple trademarks used here descriptively. No endorsement is implied.

darkmux is tested against specific versions of LMStudio. Future versions may break compatibility. When that happens, file an issue, but understand fixes ship on the author's schedule.

## Model licenses are your responsibility

darkmux helps you load models through LMStudio. It does not download, redistribute, or otherwise interact with the model files themselves beyond telling LMStudio which ones to load. Each model you use has its own license (Llama Community License, Qwen License, Gemma Terms, Apache-2.0, MIT, and so on), and those licenses have different rules about commercial use, attribution, derivative works, and acceptable use. Read them. Comply with them. darkmux cannot do that for you.

## Hardware compatibility

darkmux is developed and tested on Apple Silicon Macs, specifically on the author's M5 Max system. It should work on other M-series chips, but that is not validated. Linux compiles and passes CI's fleet tests, but nobody dogfoods it — treat it as unsupported in practice. Intel Macs and Windows are untested and unsupported.

## Two layers of liability

darkmux involves two distinct legal personas, and the MIT license addresses only one of them.

**The distributor** (the author of darkmux, Kain Osterholt / Darkly Energized LLC) ships the binary and the prompts under MIT with no warranty. If darkmux corrupts a config, returns a wrong benchmark number, or produces an unexpected output, the author owes you nothing beyond the source you already have. The MIT "AS IS" clause is the contract.

**The operator** (anyone running darkmux on their own machine) is subject to the law of their own jurisdiction independently of the MIT grant. The license does NOT insulate the operator from: unauthorized practice of law or medicine if they re-publish model outputs as a service to third parties; HIPAA if they are a covered entity processing PHI through a local LLM; their professional ethics rules if they are a licensed attorney, physician, RD, PT, or trainer using the tool on client/patient work; data-protection rules (GDPR, PDPA, CCPA, subject to each regime's own thresholds) if they process personal data of others. These are operator-side obligations. The audit sink can make most post-hoc tampering detectable (not all — see SECURITY.md), but it does not satisfy any of these obligations on its own. darkmux makes no representation that running it makes you compliant with anything.

## The MIT bit, in human words

If darkmux trashes your config, eats your project, makes your fans sound like a jet engine, gives you a number that turns out to be wrong, or in any other way ruins your afternoon: that is on you, not on the author. The author owes you nothing beyond the source code you already have. If you cannot accept those terms, do not use darkmux.

If you find a bug, please file an issue. If you find a security issue, please open a private security advisory on GitHub before disclosing publicly.

— Kain Osterholt, Darkly Energized LLC
