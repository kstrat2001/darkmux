//! The one execution identity (4.0).
//!
//! An execution is one role running until it stops (CLAUDE.md contract 8).
//! Every record about it carries the same [`ExecutionId`]: its `dispatch.*`
//! bookends, turns, tool calls, usage and budget records. A session says
//! which run a record belongs to; the execution says which role execution
//! inside that session it is about, so a task session that holds several
//! (a `dispatch.map`'s items) still tells them apart.
//!
//! [`ExecutionId::mint`] is the only way to a new identity; the host entries
//! mint exactly one per execution, and a resumed execution reads its id
//! back rather than minting another. [`ExecutionId::legacy`] is the one
//! spelling of an execution a pre-4.0 record never named: the reader
//! synthesizes it, and no file is rewritten.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session_id::IdError;

/// Process-local counter: two mints in one microsecond still differ.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// What every synthesized identity starts with. A minted id never does, so
/// the two cannot collide.
const LEGACY_PREFIX: &str = "legacy:";

/// One role execution's identity: an opaque, non-empty string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionId(String);

impl ExecutionId {
    /// A fresh identity: unique across processes (wall-clock microseconds
    /// and the process id) and within one (a counter).
    pub fn mint() -> Self {
        let micros = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
        let seq = MINTED.fetch_add(1, Ordering::Relaxed);
        ExecutionId(format!("exec-{micros:x}-{:x}-{seq:x}", std::process::id()))
    }

    /// The identity of an execution a pre-4.0 record never named.
    ///
    /// The finest identity such a record carried is its session (and
    /// mission: pre-4.0 task and step sessions recurred across missions), so
    /// every record of one session and mission is one legacy execution. A
    /// record with no session names itself: its `ts`, `handle` and machine.
    pub fn legacy(session: Option<&str>, mission: Option<&str>, ts: &str, handle: &str, machine_uid: &str) -> Self {
        let mission = mission.unwrap_or("");
        match session.filter(|s| !s.is_empty()) {
            Some(session) => ExecutionId(format!("{LEGACY_PREFIX}{session}:{mission}")),
            None => ExecutionId(format!("{LEGACY_PREFIX}:{mission}:{ts}:{handle}:{machine_uid}")),
        }
    }

    /// Read an identity back from its wire string.
    pub fn parse(wire: &str) -> Result<Self, IdError> {
        if wire.is_empty() {
            return Err(IdError("an execution id must be non-empty".to_string()));
        }
        Ok(ExecutionId(wire.to_string()))
    }

    /// Read back an identity [`ExecutionId::mint`] could have produced:
    /// `exec-<hex>-<hex>-<hex>` and nothing else. For a wire string an
    /// untrusted writer may have planted (a model-writable file), where
    /// [`ExecutionId::parse`]'s "any non-empty string" would let it claim a
    /// `legacy:` identity, carry control characters or name another execution.
    pub fn parse_minted(wire: &str) -> Result<Self, IdError> {
        let is_hex = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let minted = wire
            .strip_prefix("exec-")
            .map(|rest| rest.split('-').collect::<Vec<_>>())
            .is_some_and(|parts| parts.len() == 3 && parts.iter().all(|p| is_hex(p)));
        if !minted {
            return Err(IdError("not a minted execution id (exec-<hex>-<hex>-<hex>)".to_string()));
        }
        Ok(ExecutionId(wire.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The wall-clock second this identity was minted, from the microseconds
    /// [`ExecutionId::mint`] encodes. `None` for a synthesized (legacy) or
    /// otherwise foreign spelling, which carries no time.
    pub fn minted_at_secs(&self) -> Option<i64> {
        let micros = u128::from_str_radix(self.0.strip_prefix("exec-")?.split('-').next()?, 16).ok()?;
        i64::try_from(micros / 1_000_000).ok()
    }

    /// Whether this identity was synthesized by the reader for a pre-4.0
    /// record, rather than minted by a host entry.
    pub fn is_legacy(&self) -> bool {
        self.0.starts_with(LEGACY_PREFIX)
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ExecutionId {
    type Error = IdError;

    fn try_from(wire: String) -> Result<Self, IdError> {
        ExecutionId::parse(&wire)
    }
}

impl From<ExecutionId> for String {
    fn from(id: ExecutionId) -> String {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_mint_is_distinct_and_never_legacy() {
        let ids: HashSet<ExecutionId> = (0..5_000).map(|_| ExecutionId::mint()).collect();
        assert_eq!(ids.len(), 5_000);
        assert!(ids.iter().all(|i| !i.is_legacy()));
    }

    #[test]
    fn a_legacy_id_is_the_session_and_mission_and_names_a_sessionless_record_by_itself() {
        let a = ExecutionId::legacy(Some("task-t"), Some("m1"), "2026-08-20T01:00:00Z", "coder", "u1");
        let b = ExecutionId::legacy(Some("task-t"), Some("m1"), "2026-08-20T02:00:00Z", "judge", "u2");
        assert_eq!(a, b, "one session and mission is one legacy execution, whatever the record's ts or handle");
        assert_eq!(a.as_str(), "legacy:task-t:m1");
        let other_mission = ExecutionId::legacy(Some("task-t"), Some("m2"), "2026-08-20T01:00:00Z", "coder", "u1");
        assert_ne!(a, other_mission, "a pre-4.0 task session recurred across missions");
        let s1 = ExecutionId::legacy(None, None, "2026-08-20T01:00:00Z", "coder", "u1");
        let s2 = ExecutionId::legacy(Some(""), None, "2026-08-20T01:00:01Z", "coder", "u1");
        let s3 = ExecutionId::legacy(None, None, "2026-08-20T01:00:00Z", "coder", "u2");
        assert!(s1 != s2 && s1 != s3, "a sessionless record names itself: its time, handle and machine");
        assert_eq!(s1.as_str(), "legacy:::2026-08-20T01:00:00Z:coder:u1");
        assert!(a.is_legacy() && s1.is_legacy());
    }

    #[test]
    fn parse_minted_accepts_only_the_minted_grammar() {
        let id = ExecutionId::mint();
        assert_eq!(ExecutionId::parse_minted(id.as_str()).unwrap(), id);
        for bad in ["", "legacy:s:m", "exec-", "exec-1-2", "exec-1-2-3-4", "exec-1-2-", "exec-G-2-3", "exec-1-2-3\n", "exec-1/2-3-4", "EXEC-1-2-3", "exec-1-2-\u{1b}"] {
            assert!(ExecutionId::parse_minted(bad).is_err(), "{bad:?} is not a minted id");
        }
    }

    #[test]
    fn a_minted_id_names_the_second_it_was_minted_and_a_legacy_one_names_none() {
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let minted = ExecutionId::mint().minted_at_secs().expect("a minted id carries its time");
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((before..=after).contains(&minted), "{before} <= {minted} <= {after}");
        assert_eq!(ExecutionId::parse_minted("exec-65c8243026c00-1a2b-0").unwrap().minted_at_secs(), Some(1_790_564_400));
        let legacy = ExecutionId::legacy(Some("task-t"), Some("m1"), "2026-08-20T01:00:00Z", "coder", "u1");
        assert_eq!(legacy.minted_at_secs(), None);
    }

    #[test]
    fn serde_carries_the_string_and_refuses_an_empty_one() {
        let id = ExecutionId::mint();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ExecutionId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ExecutionId>("\"\"").is_err());
    }
}
