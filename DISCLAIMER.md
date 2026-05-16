# Disclaimer

darkmux is a personal project released under the MIT license. The license text already says "no warranty, use at your own risk" in legal language. This file says the same thing in plain English, with specifics, because darkmux does things that warrant a clear-eyed warning.

## What darkmux does to your machine

- **Reads and writes config files.** darkmux modifies `~/.openclaw/openclaw.json` when you swap stacks. If darkmux has a bug, if your config has an unusual shape, or if a swap is interrupted, your OpenClaw agent runtime can end up in a broken state. You are ultimately responsible for backing up anything you cannot afford to lose.
- **Talks to your local LMStudio server.** darkmux sends HTTP requests to `http://localhost:1234/v1` to load and unload models. It does not talk to any remote service on your behalf. (No telemetry, no analytics, no update checks.)
- **Runs AI-orchestrated workloads.** In lab mode, darkmux dispatches prompts to a local agent (OpenClaw) and can execute shell commands the agent produces — for example, `npm test` against AI-generated code. The lab working directory is **not a security boundary.** It is a regular directory on your filesystem. An AI that decides to run `rm -rf ~` is not stopped by darkmux. Treat lab mode the way you would treat running any untrusted script: only on a machine where that risk is acceptable, ideally on a separate user account or VM.

## About AI behaviour

AI models — local or hosted — can produce unexpected, incorrect, or unsafe output. They can hallucinate file paths, generate destructive shell commands, edit files in ways you did not intend, and confidently explain why the wrong thing was the right thing to do. darkmux is an orchestration layer; it does not police what the model does. If a model misbehaves, darkmux will faithfully execute the misbehaviour. Review agent output before letting it touch anything you care about.

## Licensed-adjacent role prompts

darkmux ships role prompts under `templates/builtin/crew/roles/` that operate in domains regulated by professional licensure: `health-research.md`, `legal-research.md`, and `fitness-coach.md`. These prompts are written explicitly as research and organization assistants — **not** as substitutes for a physician, attorney, registered dietitian, physical therapist, or other licensed professional. They contain explicit "You are NOT" framings, scope MUST-NOTs, and escalation rules.

Even with that framing:

- **The prompt is the only runtime boundary.** These prompts drive a local LLM. LLMs can deviate from their system prompt under adversarial, leading, or persistent prompting. darkmux does not enforce the doctrine at runtime — the prompt IS the boundary.
- **The MIT license permits anyone to modify these prompts.** A downstream fork that strips the "NOT a physician / NOT an attorney" framing and ships advice-shaped variants is the licensee's responsibility, not the author's. But an operator running this repo's prompts as shipped is still subject to the law of their jurisdiction.
- **Unauthorized practice of law (UPL) and medicine (UPM) statutes exist in every US state and most jurisdictions globally** (e.g., Cal. Bus. & Prof. Code §6125–6126 for law, §2052 for medicine; NY Jud. Law §478; analogous statutes elsewhere). They generally apply to anyone who holds out as providing those services, regardless of disclaimer. A solo operator using these prompts privately on their own materials is the intended use case. Re-distributing the prompts as a service to third parties, or modifying them into advice-shaped variants, may expose the operator to UPL/UPM liability.
- **Health privacy.** If you paste medical records, insurance documents, or symptom notes into the workspace for `health-research` to read, those files sit on your local disk in plaintext and pass through your local LLM. HIPAA does not apply to you-as-patient handling your own records, but it WILL apply if you are a covered entity (clinician, payer, business associate) reading a patient's records through this tool. Do not use these prompts on third-party PHI without your organization's HIPAA-compliance review.
- **Attorney-client privilege.** Pasting privileged communications into a local LLM workspace generally does not waive privilege (local execution is not third-party disclosure), but privilege analysis is jurisdiction-specific. If you are an attorney using this on client work, confirm with your bar's ethics opinion on generative-AI tools (see ABA Formal Opinion 512, July 2024). If you are a client using it on communications from your attorney, the privilege is yours to assert and yours to risk.

If you cannot accept these terms — including the residual risk that no prompt-level discipline is bulletproof against model misbehavior — do not use the licensed-adjacent roles.

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

## Two layers of liability

darkmux involves two distinct legal personas, and the MIT license addresses only one of them.

**The distributor** (the author of darkmux, Darkly Energized LLC) ships the binary and the prompts under MIT with no warranty. If darkmux corrupts a config, returns a wrong benchmark number, or produces an unexpected output, the author owes you nothing beyond the source you already have. The MIT "AS IS" clause is the contract.

**The operator** (anyone running darkmux on their own machine) is subject to the law of their own jurisdiction independently of the MIT grant. The license does NOT insulate the operator from: unauthorized practice of law or medicine if they re-publish licensed-adjacent role outputs as a service to third parties; HIPAA if they are a covered entity processing PHI through a local LLM; their professional ethics rules if they are a licensed attorney, physician, RD, PT, or trainer using the tool on client/patient work; data-protection rules (GDPR, PDPA, CCPA) if they process personal data of others. These are operator-side obligations. darkmux makes no representation that running it satisfies any of them.

## The MIT bit, in human words

If darkmux trashes your config, eats your project, makes your fans sound like a jet engine, gives you a number that turns out to be wrong, or in any other way ruins your afternoon: that is on you, not on the author. The author owes you nothing beyond the source code you already have. If you cannot accept those terms, do not use darkmux.

If you find a bug, please file an issue. If you find a security issue, please open a private security advisory on GitHub before disclosing publicly.

— Kain Osterholt, Darkly Energized LLC
