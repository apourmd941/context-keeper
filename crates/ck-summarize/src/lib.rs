//! LLM session summarizer with content-addressed cache.
//!
//! M4 ships a single concrete impl, [`AnthropicSummarizer`], talking to
//! `api.anthropic.com/v1/messages` with prompt caching enabled on the system
//! prompt. The output schema mirrors [`ck_core::SessionSummary`].
//!
//! Caching is two-tier:
//! - In-memory: not implemented (each summarize call is one HTTP request).
//! - On-disk: keyed on `sha256(model || joined_chunks)` at
//!   `cache/llm-summaries/<hash>.json`. Re-running summarize on an unchanged
//!   session is free (no API call).

use chrono::Utc;
use ck_core::{sha256_hex, Chunk, SessionSummary};
use ck_store::Layout;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, time::Duration};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum SummarizeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("missing API key (set ANTHROPIC_API_KEY)")]
    MissingKey,
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, SummarizeError>;

pub const DEFAULT_MODEL: &str = "claude-haiku-4-5";

/// One-shot summarizer. M4 ships a single Anthropic impl; future impls
/// (Voyage, local llama.cpp) plug in here.
pub trait Summarizer: Send + Sync {
    fn model_name(&self) -> &str;
    fn summarize(
        &self,
        chunks: &[Chunk],
    ) -> impl std::future::Future<Output = Result<SessionSummary>> + Send;
    /// Low-level text completion. Used by ck-graph for topic naming and any
    /// future "ask the LLM a one-shot question" needs.
    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
}

pub struct AnthropicSummarizer {
    api_key: String,
    model: String,
    http: reqwest::Client,
    max_tokens: u32,
}

impl AnthropicSummarizer {
    /// Construct from environment. Reads `ANTHROPIC_API_KEY`; errors if
    /// missing.
    pub fn from_env() -> Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| SummarizeError::MissingKey)?;
        Self::with_key(key)
    }

    pub fn with_key(api_key: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            api_key,
            model: DEFAULT_MODEL.to_string(),
            http,
            max_tokens: 1024,
        })
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

impl Summarizer for AnthropicSummarizer {
    fn model_name(&self) -> &str {
        &self.model
    }

    async fn summarize(&self, chunks: &[Chunk]) -> Result<SessionSummary> {
        let user_text = render_transcript(chunks);
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": [{
                "type": "text",
                "text": SYSTEM_PROMPT,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [
                {"role": "user", "content": user_text},
                // Prefill the assistant's reply with `{` so the model continues
                // a JSON object — saves us from parsing markdown wrappers.
                {"role": "assistant", "content": "{"}
            ]
        });

        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SummarizeError::Api {
                status: status.as_u16(),
                body: text,
            });
        }

        let raw: AnthropicResponse = resp.json().await?;
        let assistant_text = raw
            .content
            .iter()
            .filter_map(|b| match b {
                AnthropicBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        // Re-attach the prefill `{` we sent.
        let json_str = format!("{{{}", assistant_text.trim_start());

        let parsed: SummaryPayload = match serde_json::from_str(&json_str) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, raw = %json_str, "failed to parse summary JSON; storing raw");
                SummaryPayload {
                    text: json_str,
                    bullets: vec![],
                    decisions: vec![],
                    artifacts: vec![],
                }
            }
        };

        info!(
            model = %self.model,
            input = raw.usage.input_tokens,
            output = raw.usage.output_tokens,
            cached_read = raw.usage.cache_read_input_tokens.unwrap_or(0),
            cached_write = raw.usage.cache_creation_input_tokens.unwrap_or(0),
            "summarize complete"
        );

        Ok(SessionSummary {
            text: parsed.text,
            bullets: parsed.bullets,
            decisions: parsed.decisions,
            artifacts: parsed.artifacts,
            generated_by: format!("anthropic:{}", self.model),
            generated_at: Utc::now(),
            input_hash: input_hash(&self.model, chunks),
        })
    }

    async fn complete(&self, system: &str, user: &str, max_tokens: u32) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": [{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": [
                {"role": "user", "content": user}
            ]
        });
        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SummarizeError::Api {
                status: status.as_u16(),
                body: text,
            });
        }
        let raw: AnthropicResponse = resp.json().await?;
        let text = raw
            .content
            .iter()
            .filter_map(|b| match b {
                AnthropicBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(text)
    }
}

/// Summarizer that routes through the **orchestrator's** loopback `/v1/generate`
/// (R1-016 / ARCHITECTURE.md §3) instead of calling `api.anthropic.com` directly.
/// The orchestrator holds the cloud key (via its Keychain bridge), enforces this
/// app's egress ceiling + residency guard, and routes by policy (local / server /
/// cloud) — so context-keeper never holds `ANTHROPIC_API_KEY` and can't bypass the
/// guard. This is the **default** summarizer; [`AnthropicSummarizer`] (direct) is
/// kept for dev/offline use and deliberately bypasses the guard.
pub struct OrchestratorSummarizer {
    base_url: String,
    app: String,
    model_label: String,
    http: reqwest::Client,
}

impl OrchestratorSummarizer {
    /// Resolve the orchestrator URL from `$SELRAN_ORCHESTRATOR_URL` (else the
    /// loopback default). No API key needed — the orchestrator owns it.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("SELRAN_ORCHESTRATOR_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:15454".to_string())
            .trim_end_matches('/')
            .to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            base_url,
            app: "context-keeper".to_string(),
            model_label: "orchestrator".to_string(),
            http,
        })
    }

    /// Label only — the orchestrator's policy picks the actual model. Kept for
    /// API parity with [`AnthropicSummarizer::with_model`].
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_label = model.into();
        self
    }

    /// The R1-020 loopback badge (`~/.selran/loopback.badge`) so the call passes
    /// the orchestrator's auth when enforcement is on. None if not yet minted.
    fn badge() -> Option<String> {
        let home = std::env::var_os("HOME")?;
        let p = std::path::Path::new(&home)
            .join(".selran")
            .join("loopback.badge");
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// One `/v1/generate` round-trip. Folds the system instruction into the user
    /// turn so it's provider-agnostic (Anthropic rejects a "system" role inside
    /// the messages array; local/OpenAI accept either). Returns (text, model).
    async fn generate(&self, system: &str, user: &str) -> Result<(String, String)> {
        let content = if system.is_empty() {
            user.to_string()
        } else {
            format!("{system}\n\n{user}")
        };
        let body = serde_json::json!({
            "app": self.app,
            "messages": [{ "role": "user", "content": content }],
        });
        let mut rb = self
            .http
            .post(format!("{}/v1/generate", self.base_url))
            .header("content-type", "application/json");
        if let Some(tok) = Self::badge() {
            rb = rb.header("x-selran-token", tok);
        }
        let resp = rb.json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SummarizeError::Api {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        #[derive(Deserialize)]
        struct GenResp {
            text: String,
            #[serde(default)]
            model: String,
        }
        let raw: GenResp = resp.json().await?;
        Ok((raw.text, raw.model))
    }
}

impl Summarizer for OrchestratorSummarizer {
    fn model_name(&self) -> &str {
        &self.model_label
    }

    async fn summarize(&self, chunks: &[Chunk]) -> Result<SessionSummary> {
        let user_text = render_transcript(chunks);
        let (text, model) = self.generate(SYSTEM_PROMPT, &user_text).await?;
        // No assistant-prefill via the orchestrator, so extract the JSON object
        // leniently from the reply (the system prompt asks for JSON-only).
        let parsed = parse_summary_payload(&text);
        info!(model = %model, "summarize complete (via orchestrator)");
        Ok(SessionSummary {
            text: parsed.text,
            bullets: parsed.bullets,
            decisions: parsed.decisions,
            artifacts: parsed.artifacts,
            generated_by: format!("orchestrator:{model}"),
            generated_at: Utc::now(),
            input_hash: input_hash(&self.model_label, chunks),
        })
    }

    async fn complete(&self, system: &str, user: &str, _max_tokens: u32) -> Result<String> {
        let (text, _model) = self.generate(system, user).await?;
        Ok(text)
    }
}

/// Extract the summary JSON from an LLM reply that may wrap it in prose or a
/// markdown fence, then deserialize; falls back to storing the raw text.
fn parse_summary_payload(text: &str) -> SummaryPayload {
    let candidate = match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => text,
    };
    match serde_json::from_str::<SummaryPayload>(candidate) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "failed to parse orchestrator summary JSON; storing raw");
            SummaryPayload {
                text: text.to_string(),
                bullets: vec![],
                decisions: vec![],
                artifacts: vec![],
            }
        }
    }
}

const SYSTEM_PROMPT: &str = r#"You summarize Claude Code conversation transcripts.

Output ONLY a single valid JSON object with this exact schema (no markdown, no commentary):
{
  "text":      "1-2 paragraph natural-language summary of what happened in the session, in past tense",
  "bullets":   ["3-7 short, specific key points"],
  "decisions": ["explicit decisions or pivotal choices made (e.g., 'pivoted from X to Y because Z')"],
  "artifacts": ["file paths, URLs, package names, command names that were created or referenced"]
}

Rules:
- Be specific. Prefer concrete nouns and verbs over generic phrasing.
- A "decision" must include the reason when it was given in the conversation.
- Skip filler: do not mention that the assistant ran tools, or that the user replied, or that hooks fired.
- Do not include any text before or after the JSON object.
- The first character of your reply must be a property string ("text"); the prefilled '{' is already present."#;

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct SummaryPayload {
    #[serde(default)]
    text: String,
    #[serde(default)]
    bullets: Vec<String>,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
}

// ---------- prompt rendering ----------

fn render_transcript(chunks: &[Chunk]) -> String {
    let mut out = String::with_capacity(chunks.iter().map(|c| c.text.len()).sum::<usize>() + 64);
    out.push_str("Transcript:\n\n");
    for c in chunks {
        let role = match c.role {
            ck_core::ChunkRole::User => "user",
            ck_core::ChunkRole::Assistant => "assistant",
            ck_core::ChunkRole::ToolUse => "tool_use",
            ck_core::ChunkRole::ToolResult => "tool_result",
            ck_core::ChunkRole::System => "system",
        };
        let label = match (c.role, c.tool_name.as_deref()) {
            (ck_core::ChunkRole::ToolUse, Some(name)) => format!("[{role}: {name}]"),
            _ => format!("[{role}]"),
        };
        out.push_str(&label);
        out.push('\n');
        out.push_str(&c.text);
        out.push_str("\n\n");
    }
    out
}

pub fn input_hash(model: &str, chunks: &[Chunk]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update(b"\n");
    for c in chunks {
        h.update(c.id.0.as_bytes());
        h.update(b"|");
        h.update(c.text.as_bytes());
        h.update(b"\n");
    }
    sha256_hex(&h.finalize())
}

// ---------- on-disk cache ----------

fn cache_path(layout: &Layout, hash: &str) -> PathBuf {
    layout
        .root
        .join("cache/llm-summaries")
        .join(format!("{hash}.json"))
}

pub fn read_cached(layout: &Layout, hash: &str) -> Option<SessionSummary> {
    let path = cache_path(layout, hash);
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_cached(layout: &Layout, summary: &SessionSummary) -> Result<()> {
    let path = cache_path(layout, &summary.input_hash);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(summary)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Convenience: check cache first, summarize if missing, persist on success.
pub async fn summarize_with_cache<S: Summarizer>(
    summarizer: &S,
    layout: &Layout,
    chunks: &[Chunk],
) -> Result<(SessionSummary, bool)> {
    let hash = input_hash(summarizer.model_name(), chunks);
    if let Some(cached) = read_cached(layout, &hash) {
        debug!(hash = %hash, "summary cache hit");
        return Ok((cached, true));
    }
    let summary = summarizer.summarize(chunks).await?;
    write_cached(layout, &summary)?;
    Ok((summary, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ck_core::{ChunkId, ChunkKind, ChunkRole, EmbeddingRef, ProjectId, SessionId};
    use tempfile::tempdir;

    #[test]
    fn parse_summary_payload_extracts_json() {
        // bare object
        let p = parse_summary_payload(r#"{"text":"hi","bullets":["a"]}"#);
        assert_eq!(p.text, "hi");
        assert_eq!(p.bullets, vec!["a".to_string()]);
        // wrapped in prose / markdown fence (no assistant-prefill via orchestrator)
        let p2 = parse_summary_payload("Here you go:\n```json\n{\"text\":\"x\"}\n```\ndone");
        assert_eq!(p2.text, "x");
        // unparseable → raw fallback, never panics
        let p3 = parse_summary_payload("not json at all");
        assert_eq!(p3.text, "not json at all");
        assert!(p3.bullets.is_empty());
    }

    fn dummy_chunk(id: &str, text: &str, role: ChunkRole) -> Chunk {
        Chunk {
            id: ChunkId(id.into()),
            session_id: SessionId("s1".into()),
            project_id: ProjectId("-p".into()),
            turn_index: 0,
            role,
            kind: ChunkKind::AssistantText,
            text: text.into(),
            token_count: text.split_whitespace().count() as u32,
            start_uuid: "u".into(),
            end_uuid: "u".into(),
            started_at: Utc::now(),
            tool_name: None,
            tool_input_preview: None,
            embedding_ref: Some(EmbeddingRef {
                model: "bge".into(),
                sha256: "0".into(),
            }),
        }
    }

    #[test]
    fn input_hash_is_stable_and_order_sensitive() {
        let a = dummy_chunk("c1", "hello", ChunkRole::User);
        let b = dummy_chunk("c2", "world", ChunkRole::Assistant);
        let h1 = input_hash("m", &[a.clone(), b.clone()]);
        let h2 = input_hash("m", &[a.clone(), b.clone()]);
        let h3 = input_hash("m", &[b, a]);
        assert_eq!(h1, h2);
        assert_ne!(h1, h3, "order matters for transcripts");
    }

    #[test]
    fn cache_round_trip() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let s = SessionSummary {
            text: "did stuff".into(),
            bullets: vec!["a".into()],
            decisions: vec!["chose Rust".into()],
            artifacts: vec!["main.rs".into()],
            generated_by: "anthropic:test".into(),
            generated_at: Utc::now(),
            input_hash: "deadbeef".into(),
        };
        write_cached(&layout, &s).unwrap();
        let back = read_cached(&layout, "deadbeef").unwrap();
        assert_eq!(back.text, "did stuff");
        assert_eq!(back.bullets, vec!["a".to_string()]);
    }

    #[test]
    fn render_transcript_labels_and_orders_correctly() {
        let chunks = vec![
            dummy_chunk("u1", "hi", ChunkRole::User),
            dummy_chunk("a1", "hello", ChunkRole::Assistant),
        ];
        let rendered = render_transcript(&chunks);
        assert!(rendered.contains("[user]"));
        assert!(rendered.contains("[assistant]"));
        let i_user = rendered.find("[user]").unwrap();
        let i_asst = rendered.find("[assistant]").unwrap();
        assert!(i_user < i_asst);
    }
}
