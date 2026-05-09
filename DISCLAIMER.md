# Disclaimer

darkmux is a personal project released under the MIT license. The license text already says "no warranty, use at your own risk" in legal language. This file says the same thing in plain English, with specifics, because darkmux does things that warrant a clear-eyed warning.

## What darkmux does to your machine

- **Reads and writes config files.** darkmux modifies `~/.openclaw/openclaw.json` when you swap stacks. If darkmux has a bug, if your config has an unusual shape, or if a swap is interrupted, your OpenClaw agent runtime can end up in a broken state. You are ultimately responsible for backing up anything you cannot afford to lose.
- **Talks to your local LMStudio server.** darkmux sends HTTP requests to `http://localhost:1234/v1` to load and unload models. It does not talk to any remote service on your behalf. (No telemetry, no analytics, no update checks.)
- **Runs AI-orchestrated workloads.** In lab mode, darkmux dispatches prompts to a local agent (OpenClaw) and can execute shell commands the agent produces — for example, `npm test` against AI-generated code. The lab working directory is **not a security boundary.** It is a regular directory on your filesystem. An AI that decides to run `rm -rf ~` is not stopped by darkmux. Treat lab mode the way you would treat running any untrusted script: only on a machine where that risk is acceptable, ideally on a separate user account or VM.

## About AI behaviour

AI models — local or hosted — can produce unexpected, incorrect, or unsafe output. They can hallucinate file paths, generate destructive shell commands, edit files in ways you did not intend, and confidently explain why the wrong thing was the right thing to do. darkmux is an orchestration layer; it does not police what the model does. If a model misbehaves, darkmux will faithfully execute the misbehaviour. Review agent output before letting it touch anything you care about.

## About the performance numbers

Benchmarks, throughput claims, and "X tokens/sec" figures in this repository and in the accompanying article series at [substack.com/@DarklyEnergized](https://substack.com/@DarklyEnergized) were measured on the author's hardware: a MacBook Pro with the Apple M5 Max chip and 128 GB of unified memory. Your numbers will differ — sometimes by a lot — depending on chip generation, RAM, thermal conditions, model quantization, context length, and what else is running. Treat the numbers as one data point, not a guarantee.

## Third-party software

darkmux is not affiliated with, endorsed by, or supported by:

- **LMStudio** — a separate product with its own license and terms of service. You are responsible for complying with them, including any commercial-use restrictions.
- **OpenClaw** — a separate open-source project. darkmux integrates with it but is not maintained by the OpenClaw authors.
- **Apple, Inc.** — "Apple Silicon" and "M5 Max" are Apple trademarks used here descriptively. No endorsement is implied.

darkmux is tested against specific versions of LMStudio and OpenClaw. Future versions of either may break compatibility. When that happens, file an issue — but understand fixes ship on the author's schedule.

## Model licenses are your responsibility

darkmux helps you load models through LMStudio. It does not download, redistribute, or otherwise interact with the model files themselves beyond telling LMStudio which ones to load. Each model you use has its own license — Llama Community License, Qwen License, Gemma Terms, Apache-2.0, MIT, and so on — and those licenses have different rules about commercial use, attribution, derivative works, and acceptable use. Read them. Comply with them. darkmux cannot do that for you.

## Hardware compatibility

darkmux is developed and tested on Apple Silicon Macs, specifically on the author's M5 Max system. It should work on other M-series chips, but that is not validated. Intel Macs are not supported. Linux and Windows are not supported.

## The MIT bit, in human words

If darkmux trashes your config, eats your project, makes your fans sound like a jet engine, gives you a number that turns out to be wrong, or in any other way ruins your afternoon: that is on you, not on the author. The author owes you nothing beyond the source code you already have. If you cannot accept those terms, do not use darkmux.

If you find a bug, please file an issue. If you find a security issue, please open a private security advisory on GitHub before disclosing publicly.

— Kain Osterholt, Darkly Energized LLC
