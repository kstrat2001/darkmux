//! Turn-boundary checkpoint (#2114).
//!
//! After each completed turn boundary (a full turn, or a #1221 reasoning-
//! checkpoint hand-back — see `loop_runner`'s `resuming_after_checkpoint`),
//! AND after each individual tool result within a turn (#2114 finding 2),
//! the loop writes `<out_dir>/checkpoint.json` atomically (temp file +
//! rename onto the same filesystem) so a killed container always leaves a
//! checkpoint from its last completed step, never a torn write. `out_dir`
//! is the container's `/darkmux-out` mount (`trajectory::RUNTIME_OUT_BASE`)
//! — the same always-writable, never-`:ro` bookkeeping dir trajectory and
//! findings already use, NOT `<workspace>/.darkmux`: `/workspace` is
//! read-only for crawl-kind dispatches (#1959) and, when writable, is the
//! operator's own repo tree. `--resume <path>` (or
//! `DARKMUX_RESUME_CHECKPOINT`) reloads one of these instead of starting
//! from the system prompt.
//!
//! **Resume's real guarantee:** at most ONE tool call may be re-executed
//! on a resume — the one that was in flight at kill time (see
//! `RunCheckpoint::pending_tool_calls`'s own doc for why). Every tool a
//! role can call must be safe to run twice with the same arguments.

use crate::lmstudio::{Message, ToolCall};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Schema version for `RunCheckpoint`. Bump on a breaking field change so
/// an older runtime resuming a newer checkpoint (or vice versa) fails
/// loud at load time instead of silently misreading fields.
///
/// v2 (#2114 finding 2) adds `pending_tool_calls` — v1 checkpoints have no
/// way to represent an in-progress tool-call batch, so a v1 checkpoint
/// resumed by a v2+ runtime would silently look like a clean turn
/// boundary. Bumped rather than defaulted so that ambiguity fails loud
/// instead of re-running (or losing) mid-turn tool calls.
///
/// v3 (security audit, #2114 resume follow-up) adds `role_id` — a required
/// field, deliberately with NO `#[serde(default)]`, so a checkpoint that
/// predates this field fails to PARSE at all rather than silently
/// defaulting to an empty role a forger could trivially match. The host
/// (`dispatch_internal::stage_resume_checkpoint`) reads this field back to
/// refuse a resume whose recorded role differs from the resuming role,
/// BEFORE the container ever spawns — see that function's own doc. This is
/// the third, host-side leg of a three-part defense; `validate_for_resume`
/// below is the other two (tool-allowlist + system-message match),
/// enforced runtime-side.
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 3;

/// (#1221) The reasoning-checkpoint hand-back pending on the turn in
/// flight, if the checkpoint was written mid-continuation rather than at
/// a clean turn boundary — captured so a resumed dispatch reconstructs
/// the SAME prefill continuation instead of restarting the turn (and
/// re-running any tool calls it already made) from scratch. Mirrors
/// `loop_runner::TurnAccum`'s fields; kept as a separate public type
/// since `TurnAccum` itself is private to `loop_runner` and carries
/// loop-internal invariants that don't belong in a serialized artifact.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PendingHandBack {
    pub thought: String,
    pub answer: String,
    pub think_closed: bool,
    pub is_reasoning: bool,
    /// The accumulated thought already begins with the model's own
    /// `<think>` opener — see `TurnAccum::carries_own_opener`'s own doc.
    /// Must round-trip or a resumed prefill can double-wrap the opener.
    #[serde(default)]
    pub carries_own_opener: bool,
}

/// Everything a resumed dispatch needs to continue from this checkpoint's
/// turn boundary instead of starting over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub schema_version: u32,
    /// (v3, security audit) The role id THIS run was dispatched as
    /// (`--role-id`, host-supplied — the runtime has no other concept of
    /// "role"). Stamped on every write so a LATER `--resume-from` can
    /// refuse, host-side, to resume a checkpoint recorded under a
    /// different role than the one it's about to run as — before the
    /// container even spawns. Never validated by the runtime itself
    /// (`validate_for_resume` below checks tool-allowlist + system-message
    /// instead); this field is read-and-compared entirely on the host.
    pub role_id: String,
    /// Full conversation so far, in order. On a CLEAN turn boundary
    /// (`pending_tool_calls: None`, `pending_hand_back: None`) the
    /// tool-call cursor IS this vector's length/contents — there's no
    /// separate index to drift from it. Mid-turn (`pending_tool_calls:
    /// Some`), the tail of this vector is the in-flight assistant message
    /// followed by whichever of ITS tool results had completed as of this
    /// write; `pending_tool_calls` names what's left.
    pub messages: Vec<Message>,
    /// Completed turns (model calls that advanced the loop's `turns`
    /// counter; a #1221 hand-back continuation does NOT advance it). A
    /// mid-turn checkpoint (`pending_tool_calls: Some`) still reports the
    /// turn IN PROGRESS here — its model call already happened and
    /// advanced this counter before any of that turn's tool calls ran.
    pub turns: u32,
    pub total_prompt_tokens: u32,
    pub total_completion_tokens: u32,
    /// darkmux compaction rewrites `messages` itself (the middle is
    /// replaced with a summary message), so the compacted history IS
    /// `messages` above — this counter is the only side state
    /// compaction carries across a resume.
    pub compactions: u32,
    pub rest_ms: u64,
    pub rests: u32,
    /// Pending #1221 hand-back, if the loop was mid-checkpoint-
    /// continuation when this was written. `None` on a clean turn
    /// boundary (the common case).
    pub pending_hand_back: Option<PendingHandBack>,
    /// (#2114 finding 2) Tool calls from the CURRENT turn's assistant
    /// message that had not yet been dispatched as of this write —
    /// `messages` already carries the assistant message plus every
    /// result recorded before the write; this names what's left. `None`
    /// on a clean turn boundary or a #1221 hand-back boundary (neither
    /// has a tool-call batch in flight). A resume with this `Some` finishes
    /// dispatching exactly these calls before requesting the next turn.
    ///
    /// **The real guarantee (#2114 finding N2): a resume may re-execute AT
    /// MOST ONE tool call — the one in flight at kill time.** Every
    /// COMPLETED call's result is recorded (in `messages`) and stamped out
    /// of `pending_tool_calls` before the next call starts, so a kill
    /// between tool N and tool N+1 never re-runs tool N. But a kill WHILE
    /// tool N is still executing (inside `dispatch(...)`, before its
    /// result lands) catches it mid-flight: nothing was recorded for it
    /// yet, so it's still the head of `pending_tool_calls` and DOES get
    /// re-dispatched on resume. Every tool a role can call must therefore
    /// be safe to run twice with the same arguments — this is not a new
    /// requirement `checkpoint.rs` introduces, it's the same
    /// idempotent-under-retry expectation the cycle/failure-rate detectors
    /// already assume of the tool surface, just now also exercised by a
    /// kill-and-resume instead of only by the model repeating itself.
    ///
    /// NOT covered by this pass: detector state (cycle/failure-rate/
    /// reasoning-loop windows), `checkpoints_used`/`stall_recoveries_used`,
    /// and queued-but-undrained feedback-injection signals are all reset
    /// to fresh/empty on resume rather than restored — a resumed dispatch
    /// gets a clean detector slate instead of the exact pre-kill state.
    /// This is a scope cut, not an oversight; residue tracked on #2114.
    #[serde(default)]
    pub pending_tool_calls: Option<Vec<ToolCall>>,
    /// (#2114 finding N6) The `tool_seq` the FIRST entry of
    /// `pending_tool_calls` will get when it's dispatched — i.e. how many
    /// of this turn's tool calls were already completed as of this write.
    /// Without this, a resume's catch-up pass would renumber its calls
    /// from 0, so the SAME call could show two different `tool_seq`
    /// values across a kill-and-resume (once from the original run, once
    /// from the catch-up) in `trajectory.jsonl`. Meaningless (left at 0)
    /// when `pending_tool_calls` is `None`.
    #[serde(default)]
    pub pending_tool_calls_seq_base: u32,
    pub written_at_unix_ms: u64,
}

/// `<out_dir>/checkpoint.json` — `out_dir` is the container's
/// `/darkmux-out` mount in production (`trajectory::RUNTIME_OUT_BASE`), a
/// tempdir in tests. See the module doc for why this moved off
/// `<workspace>/.darkmux`.
pub fn checkpoint_file_path(out_dir: &Path) -> PathBuf {
    out_dir.join("checkpoint.json")
}

/// Write atomically: serialize to a sibling temp file (named with this
/// process's pid AND a nanosecond timestamp — pid alone repeats across a
/// container restart on the same mount, which is exactly the moment two
/// writes are most likely to race), fsync the temp file's contents, then
/// `rename` over the real path and fsync the containing directory so the
/// rename itself survives a crash (a rename can be atomic yet still be
/// lost from a directory entry that was never flushed). Rename is atomic
/// on POSIX when both paths share a filesystem, which they do here (both
/// under the same mounted out-dir) — a reader never observes a
/// partially-written checkpoint, and a container killed mid-write leaves
/// the PREVIOUS complete checkpoint in place, never a torn one.
pub fn write_checkpoint(out_dir: &Path, checkpoint: &RunCheckpoint) -> std::io::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let final_path = checkpoint_file_path(out_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = out_dir.join(format!("checkpoint.json.tmp.{}-{nanos}", std::process::id()));
    let body = serde_json::to_vec_pretty(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    {
        let file = std::fs::File::create(&tmp_path)?;
        use std::io::Write;
        (&file).write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    if let Ok(dir_handle) = std::fs::File::open(out_dir) {
        // Best-effort: fsyncing a directory handle isn't supported on
        // every platform (notably Windows), so a failure here doesn't
        // fail the write — the rename itself already landed.
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

/// Read + parse a checkpoint written by `write_checkpoint`. Errors
/// propagate (not swallowed): resuming is an explicit operator/host
/// action (`--resume <path>`), so a missing or corrupt checkpoint should
/// fail the dispatch loudly rather than silently starting fresh under a
/// name that looked like a resume.
pub fn read_checkpoint(path: &Path) -> anyhow::Result<RunCheckpoint> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading checkpoint {}: {e}", path.display()))?;
    let checkpoint: RunCheckpoint = serde_json::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("parsing checkpoint {}: {e}", path.display()))?;
    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION {
        anyhow::bail!(
            "checkpoint {} has schema_version={}, this runtime supports {}",
            path.display(),
            checkpoint.schema_version,
            CHECKPOINT_SCHEMA_VERSION
        );
    }
    Ok(checkpoint)
}

/// (Security audit, #2114 resume follow-up) Refuse a loaded checkpoint
/// whose CONTENT could have been shaped by a prompt-injected model rather
/// than genuinely produced by THIS run's own history — `/darkmux-out`
/// stays read-write even when `/workspace` is `:ro` (crawl-kind
/// dispatches), so a role with any file-write tool reaching that mount
/// could, in principle, forge or edit `checkpoint.json` outright, and even
/// without that, the model's OWN legitimately-recorded tool calls are
/// exactly what a resume blindly re-executes. `read_checkpoint` above only
/// validates the SHAPE (parses, schema version matches); this validates
/// SAFETY, against context this run actually has, not the file's own
/// claims. Called from `main.rs` once `tools`/`initial_messages` are
/// resolved — `read_checkpoint` alone stays callable standalone (tests,
/// the "missing/corrupt" early-fail path) without needing that context.
///
/// Two independent checks, BOTH must hold — never "fixes up" a failing
/// checkpoint, only refuses outright:
///
/// 1. Every `pending_tool_calls[].function.name` must be a tool THIS run
///    actually granted (`allowed_tool_names`, this run's OWN resolved
///    tool-allowlist, never the checkpoint's). Catches a checkpoint naming
///    a tool call this run never granted — most acutely a checkpoint
///    written under a MORE permissive role/tool set and resumed under a
///    less permissive one.
/// 2. The checkpoint's own system message (`messages[0]`, `role ==
///    "system"`) must be BYTE-IDENTICAL to `fresh_system_message` — the
///    system prompt THIS run was launched with. Compaction never rewrites
///    index 0 (`compaction::PRESERVE_HEAD >= 1` keeps it in place across
///    every compaction generation), so this holds on every LEGITIMATE
///    resume of the SAME role. Catches both a role swap (different role →
///    different system prompt) and an outright forged/edited system
///    message.
pub fn validate_for_resume(
    checkpoint: &RunCheckpoint,
    allowed_tool_names: &[&str],
    fresh_system_message: &str,
) -> anyhow::Result<()> {
    if let Some(pending) = &checkpoint.pending_tool_calls {
        for call in pending {
            if !allowed_tool_names.contains(&call.function.name.as_str()) {
                anyhow::bail!(
                    "RESUME CHECKPOINT REFUSED — pending tool call `{}` names a tool this run \
                     did not grant; refusing to re-execute it (the checkpoint may have been \
                     written under a different, more permissive role or tool set)",
                    call.function.name
                );
            }
        }
    }
    let checkpoint_system = checkpoint
        .messages
        .first()
        .filter(|m| m.role == "system")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("");
    if checkpoint_system != fresh_system_message {
        anyhow::bail!(
            "RESUME CHECKPOINT REFUSED — the checkpoint's system message is not byte-identical \
             to this run's own system prompt; refusing to resume a checkpoint that may have \
             been forged, edited, or written under a different role"
        );
    }
    Ok(())
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunCheckpoint {
        RunCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            role_id: "coder".to_string(),
            messages: vec![Message::system("sys"), Message::user("hi")],
            turns: 1,
            total_prompt_tokens: 10,
            total_completion_tokens: 5,
            compactions: 0,
            rest_ms: 0,
            rests: 0,
            pending_hand_back: None,
            pending_tool_calls: None,
            pending_tool_calls_seq_base: 0,
            written_at_unix_ms: unix_ms(),
        }
    }

    #[test]
    fn round_trips_through_write_and_read() {
        let out_dir = tempfile::tempdir().unwrap();
        let checkpoint = sample();
        write_checkpoint(out_dir.path(), &checkpoint).unwrap();
        let loaded = read_checkpoint(&checkpoint_file_path(out_dir.path())).unwrap();
        assert_eq!(loaded.turns, 1);
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn write_is_atomic_no_tmp_file_left_behind() {
        let out_dir = tempfile::tempdir().unwrap();
        write_checkpoint(out_dir.path(), &sample()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(out_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["checkpoint.json".to_string()]);
    }

    #[test]
    fn read_rejects_mismatched_schema_version() {
        let out_dir = tempfile::tempdir().unwrap();
        let mut checkpoint = sample();
        checkpoint.schema_version = 999;
        write_checkpoint(out_dir.path(), &checkpoint).unwrap();
        let err = read_checkpoint(&checkpoint_file_path(out_dir.path())).unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }

    #[test]
    fn pending_tool_calls_round_trips() {
        use crate::lmstudio::{FunctionCall, ToolCall};
        let out_dir = tempfile::tempdir().unwrap();
        let mut checkpoint = sample();
        checkpoint.pending_tool_calls = Some(vec![ToolCall {
            id: "call_2".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: r#"{"command":"echo hi"}"#.into(),
            },
            extra_content: None,
        }]);
        write_checkpoint(out_dir.path(), &checkpoint).unwrap();
        let loaded = read_checkpoint(&checkpoint_file_path(out_dir.path())).unwrap();
        let pending = loaded.pending_tool_calls.expect("pending_tool_calls survives round-trip");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "call_2");
        assert_eq!(pending[0].function.name, "bash");
    }

    #[test]
    fn pending_hand_back_round_trips() {
        let ws = tempfile::tempdir().unwrap();
        let mut checkpoint = sample();
        checkpoint.pending_hand_back = Some(PendingHandBack {
            thought: "still thinking".into(),
            answer: String::new(),
            think_closed: false,
            is_reasoning: true,
            carries_own_opener: false,
        });
        write_checkpoint(ws.path(), &checkpoint).unwrap();
        let loaded = read_checkpoint(&checkpoint_file_path(ws.path())).unwrap();
        assert_eq!(
            loaded.pending_hand_back,
            Some(PendingHandBack {
                thought: "still thinking".into(),
                answer: String::new(),
                think_closed: false,
                is_reasoning: true,
                carries_own_opener: false,
            })
        );
    }

    // ─── security audit, #2114 resume follow-up: validate_for_resume ──────

    use crate::lmstudio::FunctionCall;

    fn tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: "{}".into() },
            extra_content: None,
        }
    }

    #[test]
    fn validate_for_resume_refuses_a_pending_tool_call_outside_the_allowlist() {
        let mut checkpoint = sample();
        checkpoint.pending_tool_calls = Some(vec![tool_call("bash")]);
        // "sys" (sample()'s system message) is byte-identical to what we
        // pass as fresh_system_message below, so ONLY the tool-allowlist
        // guard is under test here — "read" is granted, "bash" is not.
        let err = validate_for_resume(&checkpoint, &["read", "edit"], "sys").unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT REFUSED"),
            "expected a named RESUME CHECKPOINT REFUSED error, got: {err:#}"
        );
        assert!(format!("{err:#}").contains("bash"), "error should name the offending tool");
    }

    #[test]
    fn validate_for_resume_refuses_when_the_system_message_differs_by_one_byte() {
        let checkpoint = sample(); // system message is "sys"
        let err = validate_for_resume(&checkpoint, &["read", "edit"], "sys!").unwrap_err();
        assert!(
            format!("{err:#}").contains("RESUME CHECKPOINT REFUSED"),
            "expected a named RESUME CHECKPOINT REFUSED error, got: {err:#}"
        );
    }

    #[test]
    fn validate_for_resume_refuses_when_system_message_matches_but_tool_call_does_not() {
        // Proves the two checks are independent — a matching system prompt
        // alone is not enough to pass.
        let mut checkpoint = sample();
        checkpoint.pending_tool_calls = Some(vec![tool_call("exec")]);
        let err = validate_for_resume(&checkpoint, &["read"], "sys").unwrap_err();
        assert!(format!("{err:#}").contains("RESUME CHECKPOINT REFUSED"));
    }

    #[test]
    fn validate_for_resume_allows_the_happy_path_same_role_same_prompt_allowed_tools() {
        let mut checkpoint = sample();
        checkpoint.pending_tool_calls = Some(vec![tool_call("read")]);
        // Byte-identical system message, and "read" is in the allowlist —
        // both checks must pass; this is the resume the whole feature
        // exists to allow.
        validate_for_resume(&checkpoint, &["read", "edit", "bash"], "sys")
            .expect("a same-role, same-prompt, allowed-tool checkpoint must resume cleanly");
    }

    #[test]
    fn validate_for_resume_allows_a_clean_turn_boundary_with_no_pending_tool_calls() {
        // pending_tool_calls: None (the common case, no mid-turn kill) —
        // the tool-allowlist check has nothing to check and must not
        // spuriously refuse.
        let checkpoint = sample();
        validate_for_resume(&checkpoint, &[], "sys")
            .expect("no pending tool calls means the allowlist check is a no-op");
    }
}
