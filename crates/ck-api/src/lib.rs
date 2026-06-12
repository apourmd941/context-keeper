//! Axum HTTP/WS server bound to 127.0.0.1:7421.
//!
//! Endpoints (all under `/v1`):
//! - `GET  /health`
//! - `GET  /projects`
//! - `GET  /sessions?project=&limit=&cursor=`
//! - `GET  /sessions/:id`
//! - `GET  /sessions/:id/transcript`
//! - `POST /recall`
//! - `GET  /ws` — WebSocket upgrade; broadcasts pipeline [`Event`]s as JSON.

use axum::{
    extract::{ws::WebSocketUpgrade, DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ck_core::{Chunk, Session, SessionId};
use ck_embed::embed_with_cache;
use ck_pipeline::DaemonState;
use ck_store::{read_chunk, read_session};
use ck_vector::SearchHit;
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::warn;

pub fn router(state: DaemonState) -> Router {
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/:id", get(get_session))
        .route("/v1/sessions/:id/transcript", get(get_transcript))
        .route("/v1/recall", post(recall))
        .route("/v1/graph", get(get_graph))
        .route("/v1/settings", get(get_settings).put(put_settings))
        .route("/v1/ws", get(ws_upgrade))
        // The only request bodies are a small recall query and a settings
        // object; cap at 256 KiB so a malicious body can't balloon memory
        // (axum's 2 MB default is looser than this surface needs).
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // One process serves BOTH the /v1 API/WS AND the built web UI
    // (apps/web/dist), so the Launchpad opens one window and the UI's /v1 calls
    // are same-origin — no separate Vite dev server, no proxy, no second port.
    // The dist path is injected via CK_WEB_DIST (set by start.sh); without it
    // the daemon stays API-only (the prior behaviour). Unmatched paths fall
    // back to index.html for client-side SPA routing.
    match std::env::var("CK_WEB_DIST")
        .ok()
        .map(std::path::PathBuf::from)
    {
        Some(dist) if dist.join("index.html").is_file() => api.fallback_service(
            ServeDir::new(&dist).not_found_service(ServeFile::new(dist.join("index.html"))),
        ),
        _ => api,
    }
}

// ---------- responses ----------

#[derive(Serialize)]
struct Health {
    status: &'static str,
    sessions: u32,
    chunks: u32,
    /// True while the boot scan is still walking the historical corpus.
    indexing: bool,
    /// Files the boot scan has processed so far (monotonic within one boot).
    scan_progress: u32,
}

async fn health(State(state): State<DaemonState>) -> Json<Health> {
    use std::sync::atomic::Ordering;
    let chunks = state.vector.read().map(|v| v.len() as u32).unwrap_or(0);
    let sessions = state
        .meta
        .lock()
        .ok()
        .and_then(|m| m.count_sessions().ok())
        .unwrap_or(0);
    let indexing = state.indexing.load(Ordering::Relaxed);
    Json(Health {
        status: if indexing { "indexing" } else { "ok" },
        sessions,
        chunks,
        indexing,
        scan_progress: state.scan_progress.load(Ordering::Relaxed),
    })
}

#[derive(Serialize)]
struct ProjectSummary {
    id: String,
    sessions: u32,
    last_seen: Option<String>,
}

async fn list_projects(State(state): State<DaemonState>) -> Json<Vec<ProjectSummary>> {
    // Walk derived/sessions/*.json once; group by project.
    let mut counts: BTreeMap<String, (u32, Option<String>)> = BTreeMap::new();
    let dir = state.layout.sessions_dir();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                if let Ok(s) = serde_json::from_slice::<Session>(&bytes) {
                    let e = counts.entry(s.project_id.0.clone()).or_insert((0, None));
                    e.0 += 1;
                    let ts = s.ended_at.to_rfc3339();
                    if e.1.as_deref().is_none_or(|existing| existing < ts.as_str()) {
                        e.1 = Some(ts);
                    }
                }
            }
        }
    }
    Json(
        counts
            .into_iter()
            .map(|(id, (sessions, last_seen))| ProjectSummary {
                id,
                sessions,
                last_seen,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct SessionsQuery {
    project: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}
fn default_limit() -> u32 {
    50
}

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    project_id: String,
    is_sidechain: bool,
    started_at: String,
    ended_at: String,
    message_count: u32,
    ai_title: Option<String>,
    first_prompt: Option<String>,
    chunk_count: u32,
}

async fn list_sessions(
    State(state): State<DaemonState>,
    Query(q): Query<SessionsQuery>,
) -> Json<Vec<SessionSummary>> {
    let dir = state.layout.sessions_dir();
    let mut sessions: Vec<Session> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                if let Ok(s) = serde_json::from_slice::<Session>(&bytes) {
                    if let Some(p) = &q.project {
                        if s.project_id.0 != *p {
                            continue;
                        }
                    }
                    sessions.push(s);
                }
            }
        }
    }
    sessions.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
    sessions.truncate(q.limit.min(1000) as usize);
    Json(
        sessions
            .into_iter()
            .map(|s| SessionSummary {
                id: s.id.0,
                project_id: s.project_id.0,
                is_sidechain: s.is_sidechain,
                started_at: s.started_at.to_rfc3339(),
                ended_at: s.ended_at.to_rfc3339(),
                message_count: s.message_count,
                ai_title: s.ai_title,
                first_prompt: s.first_prompt,
                chunk_count: s.chunk_ids.len() as u32,
            })
            .collect(),
    )
}

async fn get_session(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Session>, ApiError> {
    // Reject unsafe ids at the boundary with a 400 (a traversal attempt is a
    // bad request, not a missing resource). ck-store re-checks as defense in
    // depth.
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let s = read_session(&state.layout, &SessionId(id))
        .map_err(|e| ApiError::not_found(format!("session: {e}")))?;
    Ok(Json(s))
}

#[derive(Serialize)]
struct TranscriptEntry {
    chunk_id: String,
    turn_index: u32,
    role: String,
    kind: String,
    text: String,
    token_count: u32,
    started_at: String,
    tool_name: Option<String>,
}

async fn get_transcript(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
) -> std::result::Result<Json<Vec<TranscriptEntry>>, ApiError> {
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let session_id = SessionId(id);
    let s = read_session(&state.layout, &session_id)
        .map_err(|e| ApiError::not_found(format!("session: {e}")))?;
    let mut out = Vec::with_capacity(s.chunk_ids.len());
    for cid in &s.chunk_ids {
        match read_chunk(&state.layout, &session_id, cid) {
            Ok(c) => out.push(TranscriptEntry {
                chunk_id: c.id.0,
                turn_index: c.turn_index,
                role: serde_json::to_string(&c.role)
                    .map(|s| s.trim_matches('"').to_string())
                    .unwrap_or_default(),
                kind: serde_json::to_string(&c.kind)
                    .map(|s| s.trim_matches('"').to_string())
                    .unwrap_or_default(),
                text: c.text,
                token_count: c.token_count,
                started_at: c.started_at.to_rfc3339(),
                tool_name: c.tool_name,
            }),
            Err(e) => warn!(chunk = %cid.0, error = %e, "missing chunk"),
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct RecallRequest {
    query: String,
    /// Hard cap on number of chunks to return *before* token-budget packing.
    /// The packer may return fewer if the budget runs out earlier. Absent →
    /// 10, or the configured hook limit for hook-sourced calls.
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    project: Option<String>,
    /// Total token budget across returned chunks. Absent → 4000 (small
    /// enough to fit comfortably alongside other context in a typical
    /// Claude Code session), or the configured hook budget for hook calls.
    #[serde(default)]
    token_budget: Option<u32>,
    /// Drop hits scoring below this. Absent → no floor, except hook calls
    /// which default to the configured hook score_threshold.
    #[serde(default)]
    min_score: Option<f32>,
    /// MMR lambda: 1.0 = pure relevance, 0.0 = pure diversity. Default 0.6
    /// trades off a little relevance for a lot less redundancy in the result
    /// set (most useful when the corpus has many near-duplicate chunks, which
    /// happens often with tool-call-heavy sessions).
    #[serde(default = "default_mmr_lambda")]
    mmr_lambda: f32,
    /// Source of this recall: "mcp" (default), "cli", "http", or "hook".
    /// Hook-sourced calls are tracked in `recall_hits` but excluded from
    /// the hot-chunk count that drives auto-promotion (the hook fires on
    /// every prompt and would over-weight ambient curiosity hits).
    #[serde(default = "default_recall_source")]
    source: String,
    /// Optional caller session id; recorded in `recall_hits` so the
    /// hot-chunk query can count distinct sessions per chunk.
    #[serde(default)]
    caller_session_id: Option<String>,
}
fn default_mmr_lambda() -> f32 {
    0.6
}
fn default_recall_source() -> String {
    "mcp".into()
}

// ---------- hybrid search scoring ----------

/// Hybrid result below this score is dropped as junk (pure boilerplate with
/// no keyword match). Keeps the floor low so semantic-only matches survive.
const HYBRID_JUNK_FLOOR: f32 = 0.18;

/// Filler / stop words removed when extracting content keywords from a query,
/// so "show me the chats about writing skill" reduces to ["writing", "skill"].
const QUERY_STOPWORDS: &[&str] = &[
    "show",
    "me",
    "the",
    "a",
    "an",
    "chats",
    "chat",
    "about",
    "find",
    "get",
    "what",
    "which",
    "was",
    "were",
    "are",
    "is",
    "of",
    "to",
    "in",
    "on",
    "for",
    "with",
    "and",
    "or",
    "that",
    "this",
    "how",
    "did",
    "do",
    "does",
    "we",
    "i",
    "you",
    "your",
    "all",
    "any",
    "related",
    "regarding",
    "please",
    "can",
    "could",
    "would",
    "tell",
    "give",
    "see",
    "look",
    "my",
    "our",
    "from",
    "when",
    "where",
    "who",
    "there",
    "their",
    "it",
    "its",
    "be",
    "been",
    "has",
    "have",
    "had",
    "will",
    "just",
    "some",
    "more",
    "show",
    "list",
    "search",
    "session",
    "sessions",
    "conversation",
    "conversations",
    "thing",
    "things",
    "stuff",
    "anything",
];

/// Lowercased content keywords from a query (filler removed, length ≥ 2).
fn extract_keywords(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in q.split(|c: char| !c.is_alphanumeric()) {
        if w.len() < 2 {
            continue;
        }
        let lw = w.to_lowercase();
        if QUERY_STOPWORDS.contains(&lw.as_str()) || out.contains(&lw) {
            continue;
        }
        out.push(lw);
    }
    out
}

/// Keyword overlap of a chunk's title + text against the query keywords.
/// 1.0 = every keyword present; a hit in the session title counts full, a
/// text-only hit counts 0.7 (a keyword in the title means the whole chat is
/// about it). 0.0 when there are no keywords.
fn lexical_score(keywords: &[String], title: &str, text: &str) -> f32 {
    if keywords.is_empty() {
        return 0.0;
    }
    let title_l = title.to_lowercase();
    let text_l = text.to_lowercase();
    let mut sum = 0.0f32;
    for k in keywords {
        if title_l.contains(k.as_str()) {
            sum += 1.0;
        } else if text_l.contains(k.as_str()) {
            sum += 0.7;
        }
    }
    (sum / keywords.len() as f32).min(1.0)
}

/// Penalty (0.0–0.7) for greeting / setup boilerplate that sits near the
/// embedding centroid and matches almost any query. We judge the chunk's own
/// text, not its session title, so a substantive chunk in a chat that merely
/// opened with "hi" is not penalised.
fn boilerplate_penalty(text: &str) -> f32 {
    let t = text.trim();
    let lower = t.to_lowercase();
    let first_word = lower
        .split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .unwrap_or("");
    let greeting = matches!(
        first_word,
        "hi" | "hello" | "hey" | "thanks" | "thx" | "ok" | "okay" | "yes" | "no" | "sure"
    );
    let setup = lower.contains("initial greeting")
        || lower.contains("session setup")
        || lower.contains("greeting and initial");
    let very_short = t.chars().count() < 40;
    let mut p = 0.0f32;
    if setup {
        p += 0.4;
    }
    if greeting && very_short {
        p += 0.5;
    } else if greeting {
        p += 0.2;
    } else if very_short {
        p += 0.25;
    }
    p.min(0.7)
}

#[derive(Serialize)]
struct RecallItem {
    chunk_id: String,
    session_id: String,
    project: String,
    score: f32,
    text: String,
    token_count: u32,
    session_title: Option<String>,
    started_at: String,
}

#[derive(Serialize)]
struct RecallResponse {
    items: Vec<RecallItem>,
    total_chunks: u32,
    /// Sum of `token_count` across `items`.
    total_tokens: u32,
    /// True when the post-MMR ranking had additional hits but we stopped
    /// adding them because of `token_budget`.
    truncated: bool,
    /// MMR over-fetch + budget packing settings actually used (echoed for
    /// debuggability).
    token_budget: u32,
    mmr_lambda: f32,
    elapsed_ms: u128,
    /// Compression / corpus stats — concrete numbers, no counterfactuals.
    /// The consumer is responsible for narrating "savings" since the
    /// honest baseline depends on what the user would have done instead.
    stats: RecallStats,
}

#[derive(Serialize)]
struct RecallStats {
    /// Chunks searched. Equals `total_chunks` for global recall, or
    /// the per-project chunk count when a project filter was applied.
    corpus_chunks: u32,
    /// Sum of `token_count` across `corpus_chunks`. The denominator for
    /// the compression ratio.
    corpus_tokens: u64,
    /// Number of chunks returned (== `items.len()`).
    returned_chunks: u32,
    /// Sum of `token_count` across returned chunks (== `total_tokens`).
    returned_tokens: u32,
    /// `returned_tokens / corpus_tokens`. Lower is more compression.
    /// 0.0 when the corpus is empty.
    compression_ratio: f32,
    /// MMR over-fetch size (candidate pool the re-ranker considered).
    mmr_overfetch: u32,
    /// Whether the corpus stat is scoped to a single project.
    project_scoped: bool,
}

async fn recall(
    State(state): State<DaemonState>,
    Json(req): Json<RecallRequest>,
) -> std::result::Result<Json<RecallResponse>, ApiError> {
    let start = std::time::Instant::now();
    // Bound the attacker-controlled inputs before any allocation: a giant
    // query would balloon embedding work, and an unbounded limit/budget would
    // drive unbounded fetch + result buffers. (Loopback-only, but a rogue
    // local page or plugin can still reach this.)
    if req.query.len() > 8_192 {
        return Err(ApiError::bad_request("query too long (max 8192 bytes)"));
    }
    // Hook-sourced calls inherit their tunables from config.toml (editable
    // in the UI) so the auto-recall hook needs no env-var tuning. Explicit
    // request values always win.
    let is_hook = req.source == "hook";
    let hook_cfg = if is_hook {
        state.config.read().ok().map(|c| c.hook.clone())
    } else {
        None
    };
    let mut project = req.project.clone();
    if let Some(h) = &hook_cfg {
        if h.scope == "global" {
            project = None;
        }
    }
    let min_score = req
        .min_score
        .or_else(|| hook_cfg.as_ref().map(|h| h.score_threshold));
    // Extract content keywords for the lexical pass, and embed the CLEANED
    // query so filler ("show me the chats about …") doesn't dilute the vector
    // — only the real subject drives semantic similarity.
    let keywords = extract_keywords(&req.query);
    let embed_text = if keywords.is_empty() {
        req.query.clone()
    } else {
        keywords.join(" ")
    };
    let outcome = embed_with_cache(
        state.embedder.as_ref(),
        &state.layout,
        std::slice::from_ref(&embed_text),
    )
    .map_err(|e| ApiError::internal(format!("embed: {e}")))?;
    let q = &outcome.embeddings[0];
    let limit = req
        .limit
        .unwrap_or_else(|| hook_cfg.as_ref().map(|h| h.limit).unwrap_or(10))
        .clamp(1, 100) as usize;

    // Stage 1: a WIDE cosine candidate pool. BGE-small crams short queries
    // into a narrow 0.7–0.85 band, so cosine alone barely discriminates; we
    // pull a large pool and re-rank it with lexical + anti-boilerplate signals.
    let pool = (limit * 16).max(240);
    let (raw, total): (Vec<SearchHit>, u32) = {
        let store = state
            .vector
            .read()
            .map_err(|_| ApiError::internal("vector lock"))?;
        let total = store.len() as u32;
        let raw = store
            .search(q, pool, project.as_deref())
            .map_err(|e| ApiError::internal(format!("search: {e}")))?;
        (raw, total)
    };

    // Stage 2: hybrid re-rank. Combine semantic cosine with keyword overlap
    // (title weighted) and push down greeting/setup boilerplate that matches
    // everything. With no content keywords we fall back to pure semantic so
    // behaviour is unchanged for those queries.
    let (w_sem, w_lex) = if keywords.is_empty() {
        (1.0f32, 0.0f32)
    } else {
        (0.4f32, 0.6f32)
    };
    let mut ranked: Vec<(SearchHit, Chunk, Option<String>, f32)> = Vec::with_capacity(raw.len());
    for hit in raw {
        let chunk = match read_chunk(&state.layout, &hit.session_id, &hit.chunk_id) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let title = read_session(&state.layout, &hit.session_id)
            .ok()
            .and_then(|s| s.ai_title.or(s.first_prompt));
        let lex = lexical_score(&keywords, title.as_deref().unwrap_or(""), &chunk.text);
        let penalty = boilerplate_penalty(&chunk.text);
        let hybrid = (w_sem * hit.score + w_lex * lex) * (1.0 - penalty);
        ranked.push((hit, chunk, title, hybrid));
    }
    ranked.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    // Drop junk below the floor (caller's min_score, else the junk floor) and
    // cap per-session so one chat can't flood the results with its greeting +
    // setup chunks — "show me the chats about X" wants distinct chats.
    let floor = min_score.unwrap_or(0.0).max(HYBRID_JUNK_FLOOR);
    let mut per_session: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let hits: Vec<(SearchHit, Chunk, Option<String>, f32)> = ranked
        .into_iter()
        .filter(|(_, _, _, hybrid)| *hybrid >= floor)
        .filter(|(hit, _, _, _)| {
            let n = per_session.entry(hit.session_id.0.clone()).or_insert(0);
            *n += 1;
            *n <= 2
        })
        .collect();

    // Corpus-size stats. Per-project counts when a project filter is set,
    // otherwise global. SQLite SUM is sub-millisecond at v0.1 corpora.
    let (corpus_chunks, corpus_tokens, project_scoped) = {
        let meta = state
            .meta
            .lock()
            .map_err(|_| ApiError::internal("meta lock"))?;
        match project.as_deref() {
            Some(p) => (
                meta.count_chunks_for_project(p).unwrap_or(total),
                meta.total_chunk_tokens_for_project(p).unwrap_or(0),
                true,
            ),
            None => (
                meta.count_chunks().unwrap_or(total),
                meta.total_chunk_tokens().unwrap_or(0),
                false,
            ),
        }
    };

    // Greedy token-budget packing over the hybrid-ranked, pre-read hits.
    // Stop at `limit` results or when the next chunk would exceed the budget.
    let mut items = Vec::with_capacity(limit.min(hits.len()));
    let mut total_tokens: u32 = 0;
    let budget = req
        .token_budget
        .unwrap_or_else(|| hook_cfg.as_ref().map(|h| h.token_budget).unwrap_or(4000))
        .clamp(1, 200_000);
    let mut truncated = false;
    for (hit, chunk, title, hybrid) in hits {
        if items.len() >= limit {
            truncated = true;
            break;
        }
        if total_tokens + chunk.token_count > budget && !items.is_empty() {
            truncated = true;
            break;
        }
        total_tokens += chunk.token_count;
        items.push(RecallItem {
            chunk_id: hit.chunk_id.0,
            session_id: hit.session_id.0,
            project: hit.project_id.0,
            score: hybrid,
            text: chunk.text,
            token_count: chunk.token_count,
            session_title: title,
            started_at: chunk.started_at.to_rfc3339(),
        });
    }

    let returned_chunks = items.len() as u32;
    let compression_ratio = if corpus_tokens == 0 {
        0.0
    } else {
        total_tokens as f32 / corpus_tokens as f32
    };

    // Record one row per returned chunk in `recall_hits`. Always tracked,
    // but only non-"hook" rows count toward auto-promotion thresholds.
    // Failure here is non-fatal — recall result is more important than the
    // analytics side-effect.
    let hit_rows: Vec<(String, String)> = items
        .iter()
        .map(|i| (i.chunk_id.clone(), i.project.clone()))
        .collect();
    let now = chrono::Utc::now().to_rfc3339();
    if let Ok(mut meta) = state.meta.lock() {
        if let Err(e) = meta.record_recall_hits(
            &hit_rows,
            req.caller_session_id.as_deref(),
            &req.source,
            &now,
        ) {
            warn!(error = %e, "record_recall_hits failed");
        }
    }

    // Auto-promote check. Only runs for project-scoped, non-hook recalls
    // and only when CK_AUTO_PROMOTE=1. Spawned so the recall response
    // returns immediately; the LLM call (if any) happens in the background.
    let promote_on = ck_pipeline::promote::auto_promote_enabled()
        || state.config.read().map(|c| c.auto_promote).unwrap_or(false);
    if !is_hook && promote_on {
        if let Some(project) = project.clone() {
            let state_clone = state.clone();
            tokio::spawn(async move {
                let _ = ck_pipeline::promote::check_promotion(state_clone, project).await;
            });
        }
    }

    Ok(Json(RecallResponse {
        items,
        total_chunks: total,
        total_tokens,
        truncated,
        token_budget: budget,
        mmr_lambda: req.mmr_lambda,
        elapsed_ms: start.elapsed().as_millis(),
        stats: RecallStats {
            corpus_chunks,
            corpus_tokens,
            returned_chunks,
            returned_tokens: total_tokens,
            compression_ratio,
            mmr_overfetch: pool as u32,
            project_scoped,
        },
    }))
}

// ---------- /v1/graph ----------

#[derive(Deserialize)]
struct GraphQuery {
    /// Restrict to one project; default returns all projects.
    project: Option<String>,
    /// Whether to include session nodes (default true).
    #[serde(default = "default_true")]
    include_sessions: bool,
}
fn default_true() -> bool {
    true
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GraphNode {
    Project {
        id: String,
        label: String,
        sessions: u32,
        /// Original filesystem path the project's transcripts were
        /// captured under, sampled from one of its sessions' `cwd`
        /// (sanitized project ids are lossy — un-hyphenating doesn't
        /// round-trip when the original path contains hyphens).
        cwd: Option<String>,
    },
    Topic {
        id: String,
        label: String,
        description: String,
        size: u32,
        session_ids: Vec<String>,
        project_ids: Vec<String>,
    },
    Session {
        id: String,
        label: String,
        ai_title: Option<String>,
        is_sidechain: bool,
        project_id: String,
        chunk_count: u32,
        message_count: u32,
        ended_at: String,
    },
}

#[derive(Serialize)]
struct GraphEdge {
    from: String,
    to: String,
    kind: String,
    weight: f32,
    evidence: Vec<String>,
}

#[derive(Serialize)]
struct GraphResponse {
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    elapsed_ms: u128,
}

async fn get_graph(
    State(state): State<DaemonState>,
    Query(q): Query<GraphQuery>,
) -> std::result::Result<Json<GraphResponse>, ApiError> {
    if q.project.as_ref().is_some_and(|p| p.len() > 256) {
        return Err(ApiError::bad_request("project id too long"));
    }
    let start = std::time::Instant::now();

    let topics = ck_graph::list_topics(&state.layout)
        .map_err(|e| ApiError::internal(format!("list_topics: {e}")))?;
    let edges = ck_graph::list_edges(&state.layout)
        .map_err(|e| ApiError::internal(format!("list_edges: {e}")))?;

    // Sessions live in derived/sessions/*.json — same scan as /v1/sessions.
    let mut sessions: Vec<Session> = Vec::new();
    if let Ok(read) = std::fs::read_dir(state.layout.sessions_dir()) {
        for entry in read.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                if let Ok(s) = serde_json::from_slice::<Session>(&bytes) {
                    if let Some(p) = &q.project {
                        if s.project_id.0 != *p {
                            continue;
                        }
                    }
                    sessions.push(s);
                }
            }
        }
    }

    // Filter topics by project if requested. A topic may span projects (when
    // the global pass is enabled) — for M5a it's per-project so this is 1:1.
    let topics: Vec<_> = topics
        .into_iter()
        .filter(|t| {
            if let Some(p) = &q.project {
                t.project_ids.iter().any(|x| &x.0 == p)
            } else {
                true
            }
        })
        .collect();

    // Project nodes: derive from sessions in scope. Also surface the
    // first non-empty cwd we encounter per project so the UI can show
    // the real filesystem path (sanitized ids are lossy).
    let mut project_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut project_cwd: BTreeMap<String, String> = BTreeMap::new();
    for s in &sessions {
        *project_counts.entry(s.project_id.0.clone()).or_insert(0) += 1;
        if let Some(cwd) = s.cwd.as_deref().filter(|c| !c.is_empty()) {
            project_cwd
                .entry(s.project_id.0.clone())
                .or_insert_with(|| cwd.to_string());
        }
    }

    let mut nodes: Vec<GraphNode> = Vec::new();
    for (id, sessions) in &project_counts {
        nodes.push(GraphNode::Project {
            id: format!("p:{id}"),
            label: id.clone(),
            sessions: *sessions,
            cwd: project_cwd.get(id).cloned(),
        });
    }
    for t in &topics {
        nodes.push(GraphNode::Topic {
            id: format!("t:{}", t.id.0),
            label: t.label.clone(),
            description: t.description.clone(),
            size: t.size,
            session_ids: t.session_ids.iter().map(|s| s.0.clone()).collect(),
            project_ids: t.project_ids.iter().map(|p| p.0.clone()).collect(),
        });
    }
    if q.include_sessions {
        for s in &sessions {
            let label = s
                .ai_title
                .clone()
                .or_else(|| s.first_prompt.clone())
                .unwrap_or_else(|| s.id.0.clone());
            nodes.push(GraphNode::Session {
                id: format!("s:{}", s.id.0),
                label: clip(&label, 60),
                ai_title: s.ai_title.clone(),
                is_sidechain: s.is_sidechain,
                project_id: s.project_id.0.clone(),
                chunk_count: s.chunk_ids.len() as u32,
                message_count: s.message_count,
                ended_at: s.ended_at.to_rfc3339(),
            });
        }
    }

    // Topic-topic edges (similarity, shared-file).
    let active_topic_ids: std::collections::HashSet<String> =
        topics.iter().map(|t| t.id.0.clone()).collect();
    let mut out_edges: Vec<GraphEdge> = Vec::new();
    for e in edges {
        if !active_topic_ids.contains(&e.from_id) || !active_topic_ids.contains(&e.to_id) {
            continue;
        }
        let kind = match e.kind {
            ck_core::TopicLinkKind::TopicSimilarity => "topic-similarity",
            ck_core::TopicLinkKind::SharedFile => "shared-file",
            ck_core::TopicLinkKind::SessionContinuation => "session-continuation",
        };
        out_edges.push(GraphEdge {
            from: format!("t:{}", e.from_id),
            to: format!("t:{}", e.to_id),
            kind: kind.into(),
            weight: e.weight,
            evidence: e.evidence,
        });
    }

    // Implicit containment edges: topic → its sessions (when sessions are
    // included), and project → topic.
    if q.include_sessions {
        for t in &topics {
            for sid in &t.session_ids {
                out_edges.push(GraphEdge {
                    from: format!("t:{}", t.id.0),
                    to: format!("s:{}", sid.0),
                    kind: "contains-session".into(),
                    weight: 1.0,
                    evidence: vec![],
                });
            }
        }
        // Session-continuation edges: "this session picked up where that
        // one left off" — computed from timestamps at request time.
        let windows: Vec<SessionWindow> = sessions
            .iter()
            .map(|s| {
                (
                    s.id.0.clone(),
                    s.project_id.0.clone(),
                    s.is_sidechain,
                    s.started_at,
                    s.ended_at,
                )
            })
            .collect();
        for (from, to, weight, why) in continuation_pairs(&windows) {
            out_edges.push(GraphEdge {
                from: format!("s:{from}"),
                to: format!("s:{to}"),
                kind: "session-continuation".into(),
                weight,
                evidence: vec![why],
            });
        }
    }
    for t in &topics {
        for pid in &t.project_ids {
            out_edges.push(GraphEdge {
                from: format!("p:{}", pid.0),
                to: format!("t:{}", t.id.0),
                kind: "contains-topic".into(),
                weight: 1.0,
                evidence: vec![],
            });
        }
    }

    Ok(Json(GraphResponse {
        nodes,
        edges: out_edges,
        elapsed_ms: start.elapsed().as_millis(),
    }))
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

// ---------- /v1/settings ----------

async fn get_settings(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<ck_store::Config>, ApiError> {
    let cfg = state
        .config
        .read()
        .map_err(|_| ApiError::internal("config lock"))?
        .clone();
    Ok(Json(cfg))
}

async fn put_settings(
    State(state): State<DaemonState>,
    Json(new_cfg): Json<ck_store::Config>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    // Validate before anything touches disk. Bounds are generous — they
    // exist to catch unit mistakes (e.g. 60 meaning 60%), not to police.
    let h = &new_cfg.hook;
    if !(0.0..=1.0).contains(&h.score_threshold) {
        return Err(ApiError::bad_request(
            "hook.score_threshold must be 0.0–1.0",
        ));
    }
    if !(1..=50).contains(&h.limit) {
        return Err(ApiError::bad_request("hook.limit must be 1–50"));
    }
    if !(100..=20_000).contains(&h.token_budget) {
        return Err(ApiError::bad_request("hook.token_budget must be 100–20000"));
    }
    if h.min_words > 100 {
        return Err(ApiError::bad_request("hook.min_words must be ≤ 100"));
    }
    if h.scope != "project" && h.scope != "global" {
        return Err(ApiError::bad_request(
            "hook.scope must be 'project' or 'global'",
        ));
    }
    new_cfg
        .save(&state.layout)
        .map_err(|e| ApiError::internal(format!("save config: {e}")))?;
    *state
        .config
        .write()
        .map_err(|_| ApiError::internal("config lock"))? = new_cfg;
    Ok(Json(serde_json::json!({
        "ok": true,
        "path": ck_store::Config::path(&state.layout).display().to_string(),
    })))
}

// ---------- session-continuation edges ----------

/// Window within which "B started after A ended" counts as a continuation.
const CONTINUATION_WINDOW_MINUTES: i64 = 30;

/// Lightweight session view for continuation detection:
/// (session_id, project_id, is_sidechain, started_at, ended_at).
type SessionWindow = (
    String,
    String,
    bool,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

/// Pure pairing logic: within one project (sidechains excluded), connect
/// consecutive sessions where the next starts 0–30 minutes after the
/// previous ends. Weight decays linearly with the gap (1.0 → 0.25).
fn continuation_pairs(sessions: &[SessionWindow]) -> Vec<(String, String, f32, String)> {
    let mut by_project: BTreeMap<&str, Vec<&SessionWindow>> = BTreeMap::new();
    for s in sessions {
        if !s.2 {
            by_project.entry(s.1.as_str()).or_default().push(s);
        }
    }
    let mut out = Vec::new();
    for (_, mut list) in by_project {
        list.sort_by_key(|s| s.3);
        for w in list.windows(2) {
            let (a, b) = (w[0], w[1]);
            let gap_min = (b.3 - a.4).num_minutes();
            if (0..=CONTINUATION_WINDOW_MINUTES).contains(&gap_min) {
                let frac = gap_min as f32 / CONTINUATION_WINDOW_MINUTES as f32;
                let weight = 1.0 - frac * 0.75;
                out.push((
                    a.0.clone(),
                    b.0.clone(),
                    weight,
                    format!("resumed {gap_min}m after the previous session ended"),
                ));
            }
        }
    }
    out
}

// ---------- WebSocket ----------

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<DaemonState>) -> Response {
    ws.on_upgrade(|socket| ws_loop(socket, state))
}

async fn ws_loop(socket: axum::extract::ws::WebSocket, state: DaemonState) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.subscribe();

    // Send a hello on connect
    let hello = serde_json::json!({"type":"hello","sessions":state.meta.lock().ok().and_then(|m| m.count_sessions().ok()).unwrap_or(0)});
    if sink
        .send(axum::extract::ws::Message::Text(hello.to_string()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            evt = events.recv() => {
                match evt {
                    Ok(e) => {
                        let payload = serde_json::to_string(&e).unwrap_or_else(|_| "{}".into());
                        if sink.send(axum::extract::ws::Message::Text(payload)).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        // Lagged or closed — close politely.
                        let _ = sink.send(axum::extract::ws::Message::Close(None)).await;
                        return;
                    }
                }
            }
            client = stream.next() => {
                match client {
                    Some(Ok(axum::extract::ws::Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => {}  // ignore client text/binary/ping; we never expect input
                }
            }
        }
    }
}

// ---------- error type ----------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    msg: String,
}
impl ApiError {
    fn not_found(msg: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            msg,
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            msg: msg.into(),
        }
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            msg: msg.into(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({"error": self.msg}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ck_embed::Embedder;
    use std::sync::Arc;

    struct StubEmbedder;
    impl Embedder for StubEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn model_name(&self) -> &str {
            "stub"
        }
        fn embed_batch(&self, texts: &[String]) -> ck_embed::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0; 4]).collect())
        }
    }

    fn test_state(tmp: &tempfile::TempDir) -> DaemonState {
        let layout = ck_store::Layout::new_at(tmp.path().join("root"));
        layout.ensure().expect("layout ensure");
        let vector = ck_vector::VectorStore::connect(&layout, 4).expect("vector connect");
        let meta = ck_store::MetaIndex::open(&layout).expect("meta open");
        DaemonState::new(
            layout,
            tmp.path().join("projects"),
            Arc::new(StubEmbedder),
            vector,
            meta,
        )
    }

    /// Health must answer truthfully from the instant the server binds:
    /// `indexing` while the boot scan runs, `ok` once it completes. This is
    /// the contract that replaced the old boot blackout (port closed for the
    /// entire scan).
    #[tokio::test]
    async fn health_reports_indexing_then_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);

        let Json(h) = health(State(state.clone())).await;
        assert_eq!(h.status, "indexing");
        assert!(h.indexing);
        assert_eq!(h.scan_progress, 0);

        state
            .scan_progress
            .store(42, std::sync::atomic::Ordering::Relaxed);
        state
            .indexing
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let Json(h) = health(State(state)).await;
        assert_eq!(h.status, "ok");
        assert!(!h.indexing);
        assert_eq!(h.scan_progress, 42);
    }

    #[test]
    fn keywords_strip_filler() {
        assert_eq!(
            extract_keywords("show me the chats about writing skill"),
            vec!["writing", "skill"]
        );
        assert_eq!(
            extract_keywords("medical writer"),
            vec!["medical", "writer"]
        );
        // all-filler query yields nothing → caller falls back to pure semantic
        assert!(extract_keywords("show me the chats").is_empty());
    }

    #[test]
    fn lexical_prefers_title_then_text() {
        let kw = vec!["writing".to_string(), "skill".to_string()];
        // both keywords in title → full
        assert!((lexical_score(&kw, "Selran writing skill platform", "x") - 1.0).abs() < 1e-6);
        // both in text only → 0.7
        assert!((lexical_score(&kw, "Untitled", "about writing skill here") - 0.7).abs() < 1e-6);
        // none → 0
        assert_eq!(lexical_score(&kw, "hello there", "just a greeting"), 0.0);
    }

    #[test]
    fn boilerplate_penalises_greetings_not_substance() {
        assert!(boilerplate_penalty("hi") >= 0.5);
        assert!(boilerplate_penalty("Initial greeting and session setup") >= 0.4);
        // a real, substantive chunk is not penalised
        assert_eq!(
            boilerplate_penalty(
                "We decided to use a hybrid retrieval approach combining BM25 with \
                 dense vectors, and to cap the per-session results at two."
            ),
            0.0
        );
    }

    #[test]
    fn continuation_pairs_links_resumed_sessions_only() {
        use chrono::{TimeZone, Utc};
        let t0 = Utc.with_ymd_and_hms(2026, 6, 12, 9, 0, 0).unwrap();
        let m = |min: i64| t0 + chrono::Duration::minutes(min);
        let w = |id: &str, proj: &str, side: bool, s: i64, e: i64| {
            (id.to_string(), proj.to_string(), side, m(s), m(e))
        };
        let sessions = vec![
            w("a", "p1", false, 0, 60),    // ends 10:00
            w("b", "p1", false, 70, 120),  // starts 10:10 → continuation (10m gap)
            w("c", "p1", false, 240, 300), // starts 13:00 → 2h gap, no edge
            w("d", "p2", false, 65, 90),   // other project, no cross edge
            w("e", "p1", true, 61, 80),    // sidechain, excluded
        ];
        let pairs = continuation_pairs(&sessions);
        assert_eq!(
            pairs.len(),
            1,
            "exactly one continuation expected: {pairs:?}"
        );
        let (from, to, weight, why) = &pairs[0];
        assert_eq!((from.as_str(), to.as_str()), ("a", "b"));
        assert!((0.25..=1.0).contains(weight));
        assert!(why.contains("10m"));
    }
}
