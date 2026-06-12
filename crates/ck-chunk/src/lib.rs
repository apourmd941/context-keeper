//! Pure-logic chunker.
//!
//! Turns a stream of [`RecordView`]s from `ck-transcript` into a `Vec<Chunk>`
//! suitable for embedding. Rules:
//!
//! - 1 chunk per `user` record (kind = `UserPrompt`, or `CommandMessage` when
//!   the text begins with `<command-name>` / `<command-message>`).
//! - Each `text` block in an `assistant` record is split at
//!   [`ASSISTANT_CHUNK_TOKENS`] BPE tokens with [`ASSISTANT_CHUNK_OVERLAP`]
//!   tokens of overlap.
//! - Each `tool_use` block in an `assistant` record becomes a single
//!   `ToolCall` chunk; the matching `tool-result` attachment text (paired by
//!   `parentUuid` plus ordinal of `tool_use` within the parent assistant) is
//!   appended to the chunk.
//! - All other record types and `Thinking` blocks are skipped — they have no
//!   plaintext we can embed.
//!
//! ### Why 400 tokens, not 1000
//!
//! BGE-small-en-v1.5 (the M2 default embedder) truncates inputs at 512
//! WordPiece tokens. `tiktoken-rs` BPE tokens are roughly 1.0–1.2× larger,
//! so a 1000-BPE-token chunk would have its tail silently truncated by the
//! embedder. 400-token windows fit comfortably under the limit while keeping
//! enough context to be useful.

use chrono::{DateTime, Utc};
use ck_core::{Chunk, ChunkId, ChunkKind, ChunkRole, ProjectId, SessionId};
use ck_transcript::{ContentBlock, RecordView};
use std::{collections::HashMap, sync::OnceLock};
use tiktoken_rs::CoreBPE;

pub const ASSISTANT_CHUNK_TOKENS: usize = 400;
pub const ASSISTANT_CHUNK_OVERLAP: usize = 50;
const TOOL_PREVIEW_CHARS: usize = 4_000;

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base load"))
}

fn count_tokens(text: &str) -> u32 {
    bpe().encode_with_special_tokens(text).len() as u32
}

/// Chunk one session.
pub fn chunk_session(
    session: &SessionId,
    project: &ProjectId,
    records: &[RecordView],
) -> Vec<Chunk> {
    let tool_results = collect_tool_results(records);
    let mut chunks = Vec::new();

    for (turn_index, r) in records.iter().enumerate() {
        let turn = turn_index as u32;
        let started_at = r.timestamp.unwrap_or_else(Utc::now);

        match r.ty.as_str() {
            "user" => {
                if let Some(text) = combined_text(&r.content_blocks) {
                    let kind = if looks_like_command_message(&text) {
                        ChunkKind::CommandMessage
                    } else {
                        ChunkKind::UserPrompt
                    };
                    chunks.push(make_chunk(
                        session,
                        project,
                        turn,
                        0,
                        ChunkRole::User,
                        kind,
                        text,
                        r.uuid.clone(),
                        r.uuid.clone(),
                        started_at,
                        None,
                        None,
                    ));
                }
            }
            "assistant" => {
                let mut sub: u32 = 0;
                let mut tool_iter = tool_results
                    .get(&r.uuid)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter();
                for block in &r.content_blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            for piece in split_by_tokens(
                                text,
                                ASSISTANT_CHUNK_TOKENS,
                                ASSISTANT_CHUNK_OVERLAP,
                            ) {
                                chunks.push(make_chunk(
                                    session,
                                    project,
                                    turn,
                                    sub,
                                    ChunkRole::Assistant,
                                    ChunkKind::AssistantText,
                                    piece,
                                    r.uuid.clone(),
                                    r.uuid.clone(),
                                    started_at,
                                    None,
                                    None,
                                ));
                                sub += 1;
                            }
                        }
                        ContentBlock::ToolUse {
                            name,
                            input_preview,
                            ..
                        } => {
                            let mut text = format!("Tool call: {name}\nInput: {}", input_preview);
                            if let Some(result) = tool_iter.next() {
                                text.push_str("\nResult:\n");
                                let trimmed = if result.chars().count() > TOOL_PREVIEW_CHARS {
                                    let i = result
                                        .char_indices()
                                        .nth(TOOL_PREVIEW_CHARS)
                                        .map(|(i, _)| i)
                                        .unwrap_or(result.len());
                                    let mut s = result[..i].to_string();
                                    s.push('…');
                                    s
                                } else {
                                    result
                                };
                                text.push_str(&trimmed);
                            }
                            chunks.push(make_chunk(
                                session,
                                project,
                                turn,
                                sub,
                                ChunkRole::ToolUse,
                                ChunkKind::ToolCall,
                                text,
                                r.uuid.clone(),
                                r.uuid.clone(),
                                started_at,
                                Some(name.clone()),
                                Some(input_preview.clone()),
                            ));
                            sub += 1;
                        }
                        ContentBlock::Thinking => {} // hashed-only; no plaintext to embed
                    }
                }
            }
            _ => {} // skip noise: file-history-snapshot, queue-operation, last-prompt,
                    //          permission-mode, agent-name, file-missing, file-error,
                    //          system, ai-title, attachment (already consumed above)
        }
    }

    chunks
}

fn collect_tool_results(records: &[RecordView]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for r in records {
        if r.ty == "attachment" && r.attachment_kind.as_deref() == Some("tool-result") {
            if let Some(parent) = &r.parent_uuid {
                if let Some(text) = r.attachment_text.clone() {
                    out.entry(parent.clone()).or_default().push(text);
                }
            }
        }
    }
    out
}

fn combined_text(blocks: &[ContentBlock]) -> Option<String> {
    let mut buf = String::new();
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(text);
        }
    }
    if buf.is_empty() {
        None
    } else {
        Some(buf)
    }
}

fn looks_like_command_message(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<command-name>") || t.starts_with("<command-message>")
}

fn split_by_tokens(text: &str, max: usize, overlap: usize) -> Vec<String> {
    debug_assert!(overlap < max);
    let bpe = bpe();
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.len() <= max {
        return vec![text.to_string()];
    }
    let stride = max - overlap;
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < tokens.len() {
        let end = (start + max).min(tokens.len());
        let slice = tokens[start..end].to_vec();
        if let Ok(decoded) = bpe.decode(slice) {
            out.push(decoded);
        }
        if end == tokens.len() {
            break;
        }
        start += stride;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn make_chunk(
    session: &SessionId,
    project: &ProjectId,
    turn_index: u32,
    sub_index: u32,
    role: ChunkRole,
    kind: ChunkKind,
    text: String,
    start_uuid: String,
    end_uuid: String,
    started_at: DateTime<Utc>,
    tool_name: Option<String>,
    tool_input_preview: Option<String>,
) -> Chunk {
    let token_count = count_tokens(&text);
    Chunk {
        id: ChunkId::new(session, turn_index, sub_index),
        session_id: session.clone(),
        project_id: project.clone(),
        turn_index,
        role,
        kind,
        text,
        token_count,
        start_uuid,
        end_uuid,
        started_at,
        tool_name,
        tool_input_preview,
        embedding_ref: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ck_transcript::ContentBlock;

    fn rec(ty: &str, uuid: &str, blocks: Vec<ContentBlock>) -> RecordView {
        RecordView {
            ty: ty.to_string(),
            uuid: uuid.to_string(),
            parent_uuid: None,
            timestamp: Some(Utc::now()),
            role: None,
            model: None,
            content_blocks: blocks,
            attachment_kind: None,
            attachment_text: None,
        }
    }

    fn attachment(parent: &str, text: &str) -> RecordView {
        RecordView {
            ty: "attachment".to_string(),
            uuid: format!("att-{parent}"),
            parent_uuid: Some(parent.to_string()),
            timestamp: Some(Utc::now()),
            role: None,
            model: None,
            content_blocks: vec![],
            attachment_kind: Some("tool-result".to_string()),
            attachment_text: Some(text.to_string()),
        }
    }

    fn text_block(s: &str) -> ContentBlock {
        ContentBlock::Text {
            text: s.to_string(),
        }
    }

    fn tool_use(id: &str, name: &str, input: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input_preview: input.to_string(),
        }
    }

    fn ids() -> (SessionId, ProjectId) {
        (SessionId("sess-1".into()), ProjectId("-proj".into()))
    }

    #[test]
    fn one_chunk_per_user_record() {
        let (s, p) = ids();
        let records = vec![
            rec("user", "u1", vec![text_block("hello world")]),
            rec("user", "u2", vec![text_block("how are you")]),
        ];
        let chunks = chunk_session(&s, &p, &records);
        assert_eq!(chunks.len(), 2);
        assert!(matches!(chunks[0].kind, ChunkKind::UserPrompt));
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn slash_command_recognized() {
        let (s, p) = ids();
        let records = vec![rec(
            "user",
            "u1",
            vec![text_block("<command-name>init</command-name>\nrest")],
        )];
        let chunks = chunk_session(&s, &p, &records);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0].kind, ChunkKind::CommandMessage));
    }

    #[test]
    fn assistant_text_split_at_token_window() {
        let (s, p) = ids();
        // 1500 BPE tokens ≈ 6000 ASCII chars of repetitive content.
        let big = "the quick brown fox jumps over the lazy dog ".repeat(400);
        let records = vec![rec("assistant", "a1", vec![text_block(&big)])];
        let chunks = chunk_session(&s, &p, &records);
        assert!(
            chunks.len() >= 3,
            "expected ≥3 chunks for {} tokens, got {}",
            count_tokens(&big),
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.token_count <= ASSISTANT_CHUNK_TOKENS as u32 + 5,
                "chunk over budget: {} tokens",
                c.token_count
            );
        }
    }

    #[test]
    fn tool_call_merged_with_tool_result() {
        let (s, p) = ids();
        let records = vec![
            rec(
                "assistant",
                "a1",
                vec![
                    text_block("running it"),
                    tool_use("toolu_1", "Bash", r#"{"command":"ls"}"#),
                ],
            ),
            attachment("a1", "file1\nfile2\n"),
        ];
        let chunks = chunk_session(&s, &p, &records);
        // assistant text + tool call = 2 chunks; attachment isn't its own chunk
        assert_eq!(chunks.len(), 2);
        assert!(matches!(chunks[1].kind, ChunkKind::ToolCall));
        assert_eq!(chunks[1].tool_name.as_deref(), Some("Bash"));
        assert!(chunks[1].text.contains("file1"));
    }

    #[test]
    fn thinking_blocks_are_skipped() {
        let (s, p) = ids();
        let records = vec![rec(
            "assistant",
            "a1",
            vec![
                ContentBlock::Thinking,
                text_block("after thinking, I do this"),
            ],
        )];
        let chunks = chunk_session(&s, &p, &records);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.starts_with("after thinking"));
    }

    #[test]
    fn noisy_record_types_are_dropped() {
        let (s, p) = ids();
        let records = vec![
            rec("file-history-snapshot", "x1", vec![]),
            rec("queue-operation", "x2", vec![]),
            rec("permission-mode", "x3", vec![]),
            rec("user", "u1", vec![text_block("hello")]),
        ];
        let chunks = chunk_session(&s, &p, &records);
        assert_eq!(chunks.len(), 1);
    }
}
