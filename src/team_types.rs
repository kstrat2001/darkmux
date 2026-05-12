//! Schema types for the darkmux team architecture.
//!
//! These are scaffolding types — consumed by downstream phases (crew orchestration,
//! mission management) but not yet wired into any CLI command.
//!
//! User files at `~/.darkmux/<entity-type>/` take precedence over bundled templates.
//! Bundled templates are starting points, never the source-of-truth.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A named capability with keyword-based relevance scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub keywords: Vec<KeywordWeight>,
}

/// A keyword paired with a relevance weight (0.0–1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeywordWeight {
    pub keyword: String,
    pub weight: f32,
}

/// Role positioning within a crew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Lead,
    Support,
}

/// Escalation contract — what a role does when it can't solve an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationContract {
    BailWithExplanation,
    RetryWithHint,
    HandOffTo(String), // role id to hand off to
}

/// A single role definition: capabilities, tool palette, escalation behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub capabilities: Vec<String>, // capability ids
    pub tool_palette: ToolPalette,
    pub escalation_contract: EscalationContract,
    /// Path to the sibling `<role-id>.md` prompt file if present. The loader
    /// resolves this from the same directory as the role's JSON manifest; it
    /// does NOT read the prompt content — just stores the resolvable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_path: Option<PathBuf>,
}

/// Which tool operations a role is allowed or denied.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPalette {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// A single crew member: which role they play and their position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewMember {
    pub role_id: String,
    pub position: Position,
}

/// A crew — a named collection of role assignments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crew {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub members: Vec<CrewMember>,
}

/// Status of a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MissionStatus {
    #[default]
    Active,
    Closed,
    Paused,
}

/// A mission — a named objective tying sprints together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: MissionStatus,
    #[serde(default)]
    pub sprint_ids: Vec<String>,
    pub created_ts: u64,
}

/// Status of a sprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SprintStatus {
    #[default]
    Planned,
    Running,
    Complete,
    Abandoned,
}

/// A sprint — a time-boxed work unit within a mission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sprint {
    pub id: String,
    pub mission_id: String,
    pub description: String,
    #[serde(default)]
    pub status: SprintStatus,
    /// IDs of other sprints this depends on (must complete before running).
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub created_ts: u64,
}
