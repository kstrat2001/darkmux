# The internal runtime (`runtime/`)

Claude Code loads this file when it works in this directory.

- This crate is NOT a workspace member: `cargo t-all` and workspace clippy never see it. Test it with `cargo t-runtime`; lint it with `cargo clippy --manifest-path runtime/Cargo.toml --all-targets -- -D warnings`. CI runs both.
- It builds into the `darkmux-runtime` Docker image. A dispatch runs the image whose version label matches the binary, so a runtime change needs `docker build -t darkmux-runtime:latest runtime/` before a live dispatch can exercise it.
- Text the runtime sends to a model (feedback-injection templates in `src/feedback.rs`, telemetry messages such as `STALL_NUDGE_MESSAGE`) follows the model-facing prompt rules in `templates/builtin/CLAUDE.md`.
