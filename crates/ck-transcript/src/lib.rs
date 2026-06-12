//! Defensive JSONL parser for Claude Code transcripts.
//!
//! Reads `~/.claude/projects/<sanitized>/<session-uuid>.jsonl` and nested
//! `subagents/agent-*.jsonl`. Records of unknown `type` are not errors; they
//! are tallied in [`ParseStats::unknown_types`] so format drift surfaces in
//! `ck doctor` instead of crashing the daemon.

use chrono::{DateTime, Utc};
use ck_core::{sha256_hex, ProjectId, SessionId};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, TranscriptError>;

/// One project directory under `~/.claude/projects/`.
#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub id: ProjectId,
    pub dir: PathBuf,
    pub session_files: Vec<PathBuf>,
    pub subagent_files: Vec<PathBuf>,
}

/// Walk the Claude Code projects root and list every project + session file.
pub fn discover_projects(projects_root: &Path) -> Result<Vec<DiscoveredProject>> {
    let mut out: Vec<DiscoveredProject> = Vec::new();
    if !projects_root.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(projects_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let name = match dir.file_name().and_then(|s| s.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_owned(),
            _ => continue,
        };
        let mut session_files = Vec::new();
        let mut subagent_files = Vec::new();
        for f in WalkDir::new(&dir).into_iter().filter_map(|r| r.ok()) {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let parent_name = p
                .parent()
                .and_then(|x| x.file_name())
                .and_then(|s| s.to_str());
            if matches!(parent_name, Some("subagents")) {
                subagent_files.push(p.to_path_buf());
            } else if p.parent() == Some(dir.as_path()) {
                session_files.push(p.to_path_buf());
            }
        }
        session_files.sort();
        subagent_files.sort();
        out.push(DiscoveredProject {
            id: ProjectId(name),
            dir,
            session_files,
            subagent_files,
        });
    }
    out.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    Ok(out)
}

#[derive(Debug, Default, Clone)]
pub struct ParseStats {
    /// Histogram of every record `type` value seen (known and unknown).
    pub types: HashMap<String, u32>,
    /// Subset of `types` whose key is not in [`KNOWN_TYPES`] (or was missing).
    pub unknown_types: HashMap<String, u32>,
    /// Lines that failed to parse as JSON. Counted, never failed.
    pub bad_lines: u32,
    pub total_lines: u32,
}

#[derive(Debug, Default, Clone)]
pub struct AggregateUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ParsedSession {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub is_sidechain: bool,
    pub parent_session_id: Option<SessionId>,
    pub agent_meta: Option<AgentMetaSidecar>,
    pub source_file: PathBuf,
    pub source_file_mtime_ms: i64,
    pub source_file_sha256: String,
    pub first_prompt: Option<String>,
    pub ai_title: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub message_count: u32,
    pub user_count: u32,
    pub assistant_count: u32,
    pub model_usage: HashMap<String, AggregateUsage>,
    pub git_branch: Option<String>,
    pub cwd: Option<String>,
    pub stats: ParseStats,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentMetaSidecar {
    #[serde(rename = "agentType")]
    pub agent_type: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Record `type` strings we currently recognize. Anything else is tallied
/// under `unknown_types` and surfaced by `ck doctor`.
pub const KNOWN_TYPES: &[&str] = &[
    "user",
    "assistant",
    "system",
    "attachment",
    "tool-result",
    "file-history-snapshot",
    "queue-operation",
    "ai-title",
    "agent-name",
    "last-prompt",
    "file-missing",
    "file-error",
    "permission-mode",
];

/// Parse a single session JSONL file end-to-end.
pub fn parse_session_file(project_id: &ProjectId, file: &Path) -> Result<ParsedSession> {
    let metadata = fs::metadata(file)?;
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let bytes = fs::read(file)?;
    let source_file_sha256 = sha256_hex(&bytes);

    let agent_meta = load_agent_meta_sidecar(file);
    let is_sidechain = agent_meta.is_some();

    let session_id_from_name = file
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_prefix("agent-").unwrap_or(s).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // For subagents the parent of `subagents/` is named after the parent session.
    let parent_session_id = if is_sidechain {
        file.parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|s| SessionId(s.to_string()))
    } else {
        None
    };

    let mut session = ParsedSession {
        session_id: SessionId(session_id_from_name),
        project_id: project_id.clone(),
        is_sidechain,
        parent_session_id,
        agent_meta,
        source_file: file.to_path_buf(),
        source_file_mtime_ms: mtime_ms,
        source_file_sha256,
        first_prompt: None,
        ai_title: None,
        started_at: None,
        ended_at: None,
        message_count: 0,
        user_count: 0,
        assistant_count: 0,
        model_usage: HashMap::new(),
        git_branch: None,
        cwd: None,
        stats: ParseStats::default(),
    };

    let (total, bad) = for_each_value(&bytes, |value| ingest_record(value, &mut session));
    session.stats.total_lines = total;
    session.stats.bad_lines = bad;

    Ok(session)
}

/// One JSONL record, decoded into the fields the chunker actually consumes.
/// Records of types we don't chunk (file-history-snapshot, queue-operation, …)
/// are still returned, just with empty `content_blocks` and `attachment_*`.
#[derive(Debug, Clone)]
pub struct RecordView {
    pub ty: String,
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub role: Option<String>,
    pub model: Option<String>,
    pub content_blocks: Vec<ContentBlock>,
    /// `attachment.type` for `attachment` records (e.g. `"tool-result"`).
    pub attachment_kind: Option<String>,
    /// `attachment.result` text for `attachment` records.
    pub attachment_text: Option<String>,
}

/// One slice of an assistant's `message.content` array.
///
/// `Thinking` carries no plaintext: Claude Code stores thinking blocks with
/// only a signature/hash, never the content itself.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_preview: String,
    },
    Thinking,
}

/// Re-iterate a JSONL file and return one [`RecordView`] per parsed record.
/// Bad lines are skipped silently; if you also need parse stats, call
/// [`parse_session_file`] alongside.
pub fn parse_session_records(file: &Path) -> Result<Vec<RecordView>> {
    let bytes = fs::read(file)?;
    let mut out = Vec::new();
    for_each_value(&bytes, |v| out.push(record_view_from(v)));
    Ok(out)
}

fn record_view_from(rec: &Value) -> RecordView {
    let ty = rec
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let uuid = rec
        .get("uuid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let parent_uuid = rec
        .get("parentUuid")
        .and_then(|v| v.as_str())
        .map(String::from);
    let timestamp = rec
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let role = rec
        .pointer("/message/role")
        .and_then(|v| v.as_str())
        .map(String::from);
    let model = rec
        .pointer("/message/model")
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut content_blocks = Vec::new();
    if let Some(content) = rec.pointer("/message/content") {
        if let Some(s) = content.as_str() {
            if !s.is_empty() {
                content_blocks.push(ContentBlock::Text {
                    text: s.to_string(),
                });
            }
        } else if let Some(arr) = content.as_array() {
            for item in arr {
                let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                content_blocks.push(ContentBlock::Text {
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                    "tool_use" => {
                        let id = item
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input_preview = item
                            .get("input")
                            .map(|v| {
                                let s = serde_json::to_string(v).unwrap_or_default();
                                truncate(&s, 300)
                            })
                            .unwrap_or_default();
                        content_blocks.push(ContentBlock::ToolUse {
                            id,
                            name,
                            input_preview,
                        });
                    }
                    "thinking" => content_blocks.push(ContentBlock::Thinking),
                    _ => {}
                }
            }
        }
    }

    let attachment_kind = rec
        .pointer("/attachment/type")
        .and_then(|v| v.as_str())
        .map(String::from);
    let attachment_text = rec
        .pointer("/attachment/result")
        .and_then(|v| v.as_str())
        .map(String::from);

    RecordView {
        ty,
        uuid,
        parent_uuid,
        timestamp,
        role,
        model,
        content_blocks,
        attachment_kind,
        attachment_text,
    }
}

/// Iterate JSONL bytes line-by-line, parsing each line with `simd-json` and
/// passing successful values to `f`. Returns `(total_lines, bad_lines)`.
fn for_each_value(bytes: &[u8], mut f: impl FnMut(&Value)) -> (u32, u32) {
    let mut total = 0u32;
    let mut bad = 0u32;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let rel = bytes[cursor..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| cursor + p)
            .unwrap_or(bytes.len());
        let line = &bytes[cursor..rel];
        cursor = rel + 1;
        if line.is_empty() {
            continue;
        }
        total += 1;
        let mut buf = line.to_vec();
        match simd_json::serde::from_slice::<Value>(&mut buf) {
            Ok(value) => f(&value),
            Err(_) => bad += 1,
        }
    }
    (total, bad)
}

fn ingest_record(rec: &Value, session: &mut ParsedSession) {
    let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if ty.is_empty() {
        *session
            .stats
            .unknown_types
            .entry("<missing>".to_string())
            .or_insert(0) += 1;
    } else {
        *session.stats.types.entry(ty.to_string()).or_insert(0) += 1;
        if !KNOWN_TYPES.contains(&ty) {
            *session
                .stats
                .unknown_types
                .entry(ty.to_string())
                .or_insert(0) += 1;
        }
    }

    if let Some(ts) = rec.get("timestamp").and_then(|v| v.as_str()) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
            let utc = dt.with_timezone(&Utc);
            session.started_at = Some(session.started_at.map_or(utc, |s| s.min(utc)));
            session.ended_at = Some(session.ended_at.map_or(utc, |e| e.max(utc)));
        }
    }
    if let Some(branch) = rec.get("gitBranch").and_then(|v| v.as_str()) {
        if !branch.is_empty() {
            session.git_branch = Some(branch.to_string());
        }
    }
    if let Some(cwd) = rec.get("cwd").and_then(|v| v.as_str()) {
        if !cwd.is_empty() {
            session.cwd = Some(cwd.to_string());
        }
    }

    match ty {
        "user" => {
            session.user_count += 1;
            session.message_count += 1;
            if session.first_prompt.is_none() {
                if let Some(content) = rec.pointer("/message/content") {
                    if let Some(text) = extract_text(content) {
                        session.first_prompt = Some(truncate(&text, 280));
                    }
                }
            }
        }
        "assistant" => {
            session.assistant_count += 1;
            session.message_count += 1;
            if let Some(model) = rec.pointer("/message/model").and_then(|v| v.as_str()) {
                let agg = session.model_usage.entry(model.to_string()).or_default();
                if let Some(usage) = rec.pointer("/message/usage") {
                    agg.input_tokens += usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    agg.output_tokens += usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    agg.cache_read_tokens += usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    agg.cache_creation_tokens += usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
            }
        }
        "ai-title" => {
            if let Some(t) = rec.get("aiTitle").and_then(|v| v.as_str()) {
                session.ai_title = Some(t.to_string());
            }
        }
        _ => {}
    }
}

fn extract_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut joined = String::new();
        for item in arr {
            if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                if !joined.is_empty() {
                    joined.push('\n');
                }
                joined.push_str(s);
            }
        }
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    None
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

fn load_agent_meta_sidecar(jsonl_path: &Path) -> Option<AgentMetaSidecar> {
    let parent = jsonl_path.parent()?;
    if parent.file_name().and_then(|s| s.to_str()) != Some("subagents") {
        return None;
    }
    let stem = jsonl_path.file_stem()?.to_str()?;
    let meta_path = parent.join(format!("{stem}.meta.json"));
    let bytes = fs::read(meta_path).ok()?;
    serde_json::from_slice::<AgentMetaSidecar>(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn parses_minimal_session_with_unknown_type() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("-proj");
        fs::create_dir_all(&project_dir).unwrap();
        let session_id = "abc-123";
        let file = project_dir.join(format!("{session_id}.jsonl"));
        let mut f = fs::File::create(&file).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-05-09T12:00:00Z","cwd":"/tmp/x","message":{{"role":"user","content":"hello"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-05-09T12:00:01Z","message":{{"model":"claude-opus-4-7","content":[{{"type":"text","text":"hi"}}],"usage":{{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}}}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"ai-title","aiTitle":"greeting"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"future_record_we_have_never_seen","payload":42}}"#
        )
        .unwrap();
        // a deliberately broken line — should be counted, not fatal.
        writeln!(f, r#"{{not even json"#).unwrap();
        drop(f);

        let parsed = parse_session_file(&ProjectId("-proj".into()), &file).unwrap();
        assert_eq!(parsed.session_id.0, session_id);
        assert_eq!(parsed.message_count, 2);
        assert_eq!(parsed.user_count, 1);
        assert_eq!(parsed.assistant_count, 1);
        assert_eq!(parsed.ai_title.as_deref(), Some("greeting"));
        assert_eq!(parsed.first_prompt.as_deref(), Some("hello"));
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/x"));
        assert!(parsed
            .stats
            .unknown_types
            .contains_key("future_record_we_have_never_seen"));
        assert_eq!(parsed.stats.bad_lines, 1);
        let usage = parsed.model_usage.get("claude-opus-4-7").unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_tokens, 7);
        assert_eq!(usage.cache_creation_tokens, 3);
    }

    #[test]
    fn parse_session_records_yields_typed_records() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("-proj");
        fs::create_dir_all(&project_dir).unwrap();
        let file = project_dir.join("s.jsonl");
        let mut f = fs::File::create(&file).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"u1","timestamp":"2026-05-09T12:00:00Z","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","uuid":"a1","timestamp":"2026-05-09T12:00:01Z","message":{{"role":"assistant","model":"claude-opus-4-7","content":[{{"type":"text","text":"sure"}},{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"ls"}}}},{{"type":"thinking","thinking":"…","signature":"sig"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"attachment","uuid":"x1","parentUuid":"a1","attachment":{{"type":"tool-result","result":"file1\nfile2\n"}}}}"#
        )
        .unwrap();
        drop(f);

        let records = parse_session_records(&file).unwrap();
        assert_eq!(records.len(), 3);

        // user record
        assert_eq!(records[0].ty, "user");
        assert_eq!(records[0].uuid, "u1");
        assert!(matches!(
            records[0].content_blocks[0],
            ContentBlock::Text { .. }
        ));

        // assistant record: text + tool_use + thinking
        assert_eq!(records[1].ty, "assistant");
        assert_eq!(records[1].model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(records[1].content_blocks.len(), 3);
        match &records[1].content_blocks[1] {
            ContentBlock::ToolUse {
                id,
                name,
                input_preview,
            } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "Bash");
                assert!(input_preview.contains("\"command\":\"ls\""));
            }
            _ => panic!("expected ToolUse"),
        }
        assert!(matches!(
            records[1].content_blocks[2],
            ContentBlock::Thinking
        ));

        // attachment with tool-result text
        assert_eq!(records[2].ty, "attachment");
        assert_eq!(records[2].parent_uuid.as_deref(), Some("a1"));
        assert_eq!(records[2].attachment_kind.as_deref(), Some("tool-result"));
        assert!(records[2]
            .attachment_text
            .as_deref()
            .unwrap()
            .contains("file1"));
    }

    #[test]
    fn discovers_subagents_separately() {
        let dir = tempdir().unwrap();
        let project_dir = dir.path().join("-proj");
        let session_id = "main-session";
        let subagents_dir = project_dir.join(session_id).join("subagents");
        fs::create_dir_all(&subagents_dir).unwrap();
        fs::write(project_dir.join(format!("{session_id}.jsonl")), b"").unwrap();
        fs::write(subagents_dir.join("agent-xyz.jsonl"), b"").unwrap();
        fs::write(
            subagents_dir.join("agent-xyz.meta.json"),
            br#"{"agentType":"Explore","description":"d"}"#,
        )
        .unwrap();

        let projects = discover_projects(dir.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].session_files.len(), 1);
        assert_eq!(projects[0].subagent_files.len(), 1);

        let parsed = parse_session_file(&projects[0].id, &projects[0].subagent_files[0]).unwrap();
        assert!(parsed.is_sidechain);
        assert_eq!(parsed.parent_session_id.as_ref().unwrap().0, session_id);
        assert_eq!(parsed.agent_meta.as_ref().unwrap().agent_type, "Explore");
    }
}
