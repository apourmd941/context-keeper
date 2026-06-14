//! Core types shared across the context-keeper workspace.
//!
//! Every other crate depends on these. Keep this crate small, dependency-light,
//! and free of I/O.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use thiserror::Error;
use typeshare::typeshare;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid id: {0}")]
    InvalidId(String),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;

// ---------- ID newtypes ----------

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId(pub String);

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TopicId(pub String);

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeId(pub String);

#[typeshare]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProjectId(pub String);

impl ChunkId {
    pub fn new(session: &SessionId, turn_index: u32, sub_index: u32) -> Self {
        Self(format!("{}:{}:{}", session.0, turn_index, sub_index))
    }
}

impl TopicId {
    /// Stable across reruns when membership is stable.
    pub fn from_members(member_chunk_ids: &[ChunkId]) -> Self {
        let mut sorted: Vec<&str> = member_chunk_ids.iter().map(|c| c.0.as_str()).collect();
        sorted.sort_unstable();
        let mut hasher = Sha256::new();
        for id in sorted {
            hasher.update(id.as_bytes());
            hasher.update(b"\n");
        }
        Self(hex_encode_truncated(hasher.finalize().as_slice(), 16))
    }
}

impl EdgeId {
    pub fn new(kind: &str, a: &str, b: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        hasher.update(b":");
        hasher.update(a.as_bytes());
        hasher.update(b":");
        hasher.update(b.as_bytes());
        Self(hex_encode_truncated(hasher.finalize().as_slice(), 16))
    }
}

// ---------- ProjectNamespace ----------

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectNamespace {
    pub id: ProjectId,
    /// Best-known absolute path. Resolved from session `cwd` records when
    /// available, since the directory-name-to-path mapping is lossy.
    pub original_path: String,
    pub session_ids: Vec<SessionId>,
    pub topic_ids: Vec<TopicId>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

// ---------- Session ----------

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMeta {
    pub agent_type: String,
    pub description: String,
}

#[typeshare]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub text: String,
    pub bullets: Vec<String>,
    pub decisions: Vec<String>,
    pub artifacts: Vec<String>,
    pub generated_by: String,
    pub generated_at: DateTime<Utc>,
    pub input_hash: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub is_sidechain: bool,
    pub parent_session_id: Option<SessionId>,
    pub agent_meta: Option<AgentMeta>,
    pub source_file: String,
    pub source_file_mtime_ms: i64,
    pub source_file_sha256: String,
    pub content_hash: String,
    pub first_prompt: Option<String>,
    pub ai_title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub message_count: u32,
    pub model_usage: Vec<ModelUsage>,
    pub git_branch: Option<String>,
    pub cwd: Option<String>,
    pub summary: Option<SessionSummary>,
    pub chunk_ids: Vec<ChunkId>,
    pub topic_ids: Vec<TopicId>,
}

// ---------- Chunk ----------

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkRole {
    User,
    Assistant,
    ToolUse,
    ToolResult,
    System,
}

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    UserPrompt,
    AssistantText,
    ToolCall,
    ToolResult,
    CommandMessage,
    SkillInvocation,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRef {
    pub model: String,
    pub sha256: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub turn_index: u32,
    pub role: ChunkRole,
    pub kind: ChunkKind,
    pub text: String,
    pub token_count: u32,
    pub start_uuid: String,
    pub end_uuid: String,
    pub started_at: DateTime<Utc>,
    pub tool_name: Option<String>,
    pub tool_input_preview: Option<String>,
    pub embedding_ref: Option<EmbeddingRef>,
}

// ---------- Topic ----------

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub id: TopicId,
    pub label: String,
    pub description: String,
    pub member_chunk_ids: Vec<ChunkId>,
    pub session_ids: Vec<SessionId>,
    pub project_ids: Vec<ProjectId>,
    pub centroid: Vec<f32>,
    pub size: u32,
    pub created_at: DateTime<Utc>,
    pub last_updated_at: DateTime<Utc>,
}

// ---------- TopicLink ----------

#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TopicLinkKind {
    TopicSimilarity,
    SharedFile,
    SessionContinuation,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicLink {
    pub id: EdgeId,
    pub kind: TopicLinkKind,
    pub from_id: String,
    pub to_id: String,
    pub weight: f32,
    pub evidence: Vec<String>,
    pub created_at: DateTime<Utc>,
}

// ---------- Helpers ----------

/// Convert raw bytes to lowercase hex.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

fn hex_encode_truncated(bytes: &[u8], chars: usize) -> String {
    let mut s = hex_encode(bytes);
    s.truncate(chars);
    s
}

/// SHA-256 of `bytes` rendered as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_encode(h.finalize().as_slice())
}

/// Inverse of `sanitize_project_path` is intentionally NOT provided: the
/// `/` -> `-` replacement is lossy when the original path contains hyphens.
/// Resolve original paths from session `cwd` records instead.
pub fn sanitize_project_path(abs_path: &str) -> String {
    abs_path.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_id_is_stable_across_member_order() {
        let a = ChunkId("s1:0:0".into());
        let b = ChunkId("s2:0:0".into());
        let id1 = TopicId::from_members(&[a.clone(), b.clone()]);
        let id2 = TopicId::from_members(&[b, a]);
        assert_eq!(id1.0, id2.0);
        assert_eq!(id1.0.len(), 16);
    }

    #[test]
    fn edge_id_is_deterministic() {
        let id1 = EdgeId::new("topic-similarity", "abc", "def");
        let id2 = EdgeId::new("topic-similarity", "abc", "def");
        assert_eq!(id1.0, id2.0);
        assert_eq!(id1.0.len(), 16);
    }

    #[test]
    fn sanitize_round_trip_is_lossy_so_we_dont_attempt_it() {
        let p = "/Users/me/Documents/same-table";
        assert_eq!(sanitize_project_path(p), "-Users-me-Documents-same-table");
    }

    #[test]
    fn sha256_hex_known_vector() {
        let h = sha256_hex(b"abc");
        assert_eq!(
            h,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
