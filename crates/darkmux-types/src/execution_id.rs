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
//! back rather than minting another. A record written before 4.0 names no
//! execution, and none is invented for it (#3036).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session_id::IdError;

/// Process-local counter: two mints in one microsecond still differ.
static MINTED: AtomicU64 = AtomicU64::new(0);

/// One role execution's identity: an opaque, non-empty string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionId(String);

/// A non-empty string in the wire form [`ExecutionId::parse`] reads.
impl schemars::JsonSchema for ExecutionId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ExecutionId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "minLength": 1})
    }

    fn inline_schema() -> bool {
        true
    }
}

impl ExecutionId {
    /// A fresh identity: unique across processes (wall-clock microseconds
    /// and the process id) and within one (a counter).
    pub fn mint() -> Self {
        let micros = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
        let seq = MINTED.fetch_add(1, Ordering::Relaxed);
        ExecutionId(format!("exec-{micros:x}-{:x}-{seq:x}", std::process::id()))
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
    /// [`ExecutionId::parse`]'s "any non-empty string" would let it carry control
    /// characters or name another execution.
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
    /// [`ExecutionId::mint`] encodes. `None` for a foreign spelling,
    /// which carries no time.
    pub fn minted_at_secs(&self) -> Option<i64> {
        let micros = u128::from_str_radix(self.0.strip_prefix("exec-")?.split('-').next()?, 16).ok()?;
        i64::try_from(micros / 1_000_000).ok()
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
    fn every_mint_is_distinct() {
        let ids: HashSet<ExecutionId> = (0..5_000).map(|_| ExecutionId::mint()).collect();
        assert_eq!(ids.len(), 5_000);
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
    fn a_minted_id_names_the_second_it_was_minted() {
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let minted = ExecutionId::mint().minted_at_secs().expect("a minted id carries its time");
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((before..=after).contains(&minted), "{before} <= {minted} <= {after}");
        assert_eq!(ExecutionId::parse_minted("exec-65c8243026c00-1a2b-0").unwrap().minted_at_secs(), Some(1_790_564_400));
        assert_eq!(ExecutionId::parse("not-minted").unwrap().minted_at_secs(), None, "a foreign spelling names no time");
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
