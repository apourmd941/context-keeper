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
use ck_store::{read_chunk, read_session, Memory};
use ck_vector::SearchHit;
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::warn;

pub fn router(state: DaemonState) -> Router {
    // C4: read-or-create the local auth token once at startup (always generated,
    // cheap, regardless of the enforcement flag). A failure here is non-fatal —
    // we fall back to an empty token, which the middleware treats as "no token
    // configured" and (only when enforcement is on) refuses every request, so a
    // broken token file fails CLOSED rather than silently open.
    let token = ck_store::local_token(&state.layout).unwrap_or_else(|e| {
        warn!(error = %e, "could not read/create local auth token");
        String::new()
    });
    let auth = AuthState {
        token,
        state: state.clone(),
    };

    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/projects", get(list_projects))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/:id", get(get_session))
        .route("/v1/sessions/:id/transcript", get(get_transcript))
        .route("/v1/recall", post(recall))
        .route("/v1/recall/metrics", get(recall_metrics))
        .route(
            "/v1/memories",
            post(create_memory).get(list_memories_handler),
        )
        .route("/v1/memories/search", post(search_memories_handler))
        .route(
            "/v1/memories/:id",
            axum::routing::put(update_memory_handler).delete(delete_memory_handler),
        )
        .route("/v1/graph", get(get_graph))
        .route("/v1/settings", get(get_settings).put(put_settings))
        .route("/v1/sessions/unsummarized", get(list_unsummarized))
        .route(
            "/v1/sessions/:id/summary",
            axum::routing::put(put_session_summary),
        )
        .route("/v1/topics/:id/name", axum::routing::put(put_topic_name))
        .route("/v1/ws", get(ws_upgrade))
        // The only request bodies are a small recall query and a settings
        // object; cap at 256 KiB so a malicious body can't balloon memory
        // (axum's 2 MB default is looser than this surface needs).
        .layer(DefaultBodyLimit::max(256 * 1024))
        // C4: opt-in local-caller auth. Default OFF → pass-through (no behavior
        // change). When ON, gates /v1/* (EXCEPT GET /v1/health) on a valid token
        // via `Authorization: Bearer` or the `ck_token` cookie. Layered INSIDE
        // the rebinding guard so a rejected cross-origin write is refused before
        // we even check the token. Static SPA assets are NOT gated (handled by
        // the fallback_service below, outside this `api` sub-router).
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            local_auth,
        ))
        // DNS-rebinding guard on state-changing methods. The daemon has no
        // auth (it's loopback-only by design), but a malicious web page the
        // user visits could still POST/PUT to 127.0.0.1:<port>. Browsers
        // attach an `Origin` header to such cross-origin writes; we reject any
        // Origin that isn't loopback. Same-origin UI calls (served by this
        // daemon) and non-browser callers (the hook's curl, the MCP shim) send
        // no foreign Origin and pass. GETs are read-only and unaffected.
        .layer(axum::middleware::from_fn(rebinding_guard))
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
        Some(dist) if dist.join("index.html").is_file() => {
            // C4: when enforcement is on, set the `ck_token` cookie on the SPA
            // HTML response so the same-origin browser auto-authenticates its
            // /v1 fetches. HttpOnly + SameSite=Strict; no Secure flag (loopback
            // http). The static-asset service itself is NOT token-gated — the
            // page must load to obtain the cookie, and assets aren't sensitive.
            let spa =
                ServeDir::new(&dist).not_found_service(ServeFile::new(dist.join("index.html")));
            api.fallback_service(spa)
                .layer(axum::middleware::from_fn_with_state(auth, spa_cookie))
        }
        _ => api,
    }
}

/// C4: state shared with the auth middleware — the stable local token plus the
/// daemon state (read for the live `require_token` enforcement flag, so a
/// settings change takes effect without a restart). Cheap to clone (token is a
/// 64-char String; state is all-`Arc`).
#[derive(Clone)]
struct AuthState {
    token: String,
    state: DaemonState,
}

impl AuthState {
    /// Live enforcement decision: env `CK_REQUIRE_TOKEN=1` OR the config flag.
    fn enforcing(&self) -> bool {
        let cfg_on = self
            .state
            .config
            .read()
            .map(|c| c.require_token)
            .unwrap_or(false);
        std::env::var("CK_REQUIRE_TOKEN")
            .map(|v| v == "1")
            .unwrap_or(false)
            || cfg_on
    }
}

/// C4: opt-in local-caller auth middleware. When enforcement is OFF this is a
/// pure pass-through (the default — identical to pre-C4 behavior). When ON it
/// requires a valid local token on every `/v1/*` request EXCEPT `GET
/// /v1/health` (liveness/bootstrap stays open). The token may arrive as
/// `Authorization: Bearer <tok>` or as a `ck_token` cookie; the compare is
/// constant-time. Static SPA assets are gated by a separate (non-token) layer.
async fn local_auth(
    State(auth): State<AuthState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !auth.enforcing() {
        return next.run(req).await;
    }
    // Liveness/bootstrap exemption: GET /v1/health is always open so a probe
    // (and the UI's own health check before it has a cookie) can reach it.
    let path = req.uri().path();
    // GET or HEAD — health probes (load balancers, monitors) commonly use HEAD.
    if (req.method() == axum::http::Method::GET || req.method() == axum::http::Method::HEAD)
        && path == "/v1/health"
    {
        return next.run(req).await;
    }
    let presented = extract_bearer(req.headers()).or_else(|| extract_cookie_token(req.headers()));
    let ok = match presented {
        Some(tok) => {
            !auth.token.is_empty()
                && ck_store::constant_time_eq(tok.as_bytes(), auth.token.as_bytes())
        }
        None => false,
    };
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing or invalid local token"})),
        )
            .into_response();
    }
    next.run(req).await
}

/// Extract a bearer token from `Authorization: Bearer <tok>` (case-insensitive
/// scheme). Returns None when absent or malformed.
fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let tok = rest.trim();
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_string())
    }
}

/// Extract the `ck_token` value from the `Cookie` header. Parses the standard
/// `a=1; b=2` form; returns the first `ck_token` cookie's value.
fn extract_cookie_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix("ck_token=") {
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// C4: set the `ck_token` cookie on the SPA HTML response when enforcement is
/// on, so the same-origin browser auto-authenticates its /v1 fetches. Only
/// touches HTML responses (the navigation that loads the app) — not every
/// static asset — and only when enforcing and the token is non-empty. A no-op
/// pass-through otherwise.
async fn spa_cookie(
    State(auth): State<AuthState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    if auth.enforcing() && !auth.token.is_empty() {
        let is_html = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.starts_with("text/html"))
            .unwrap_or(false);
        if is_html {
            // HttpOnly (JS can't read it) + SameSite=Strict (never sent on a
            // cross-site request) — together with the rebinding guard this
            // closes CSRF. No Secure flag: loopback is plain http.
            let cookie = format!("ck_token={}; HttpOnly; SameSite=Strict; Path=/", auth.token);
            if let Ok(val) = axum::http::HeaderValue::from_str(&cookie) {
                resp.headers_mut()
                    .insert(axum::http::header::SET_COOKIE, val);
            }
        }
    }
    resp
}

/// Reject state-changing requests that carry a non-loopback `Origin` — the
/// DNS-rebinding vector for a keyless localhost daemon. Read-only methods
/// (GET/HEAD) always pass; writes pass only when Origin is absent or points
/// at localhost/127.0.0.1/[::1] (any port).
async fn rebinding_guard(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = req.method();
    let is_write = !matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if is_write {
        if let Some(origin) = req.headers().get(axum::http::header::ORIGIN) {
            let ok = origin
                .to_str()
                .ok()
                .map(origin_is_loopback)
                .unwrap_or(false);
            if !ok {
                warn!(?origin, "rejected cross-origin write (DNS-rebinding guard)");
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "cross-origin write refused"})),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// True when an `Origin` value's host is a loopback address. Parses
/// `scheme://host[:port]`; anything unparseable is treated as not-loopback.
fn origin_is_loopback(origin: &str) -> bool {
    let after_scheme = origin.split("://").nth(1).unwrap_or(origin);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Strip a trailing :port, accounting for [::1]:port.
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
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
    /// C2: explicit candidate file paths for glob-scoped memory injection
    /// (e.g. the hook sends `cwd`). The daemon also harvests path-like tokens
    /// from `query`; the union is matched against each glob memory's patterns.
    #[serde(default)]
    paths: Vec<String>,
}
fn default_mmr_lambda() -> f32 {
    0.6
}

/// C2: pull path-like tokens out of free-text `query` so glob-scoped memories
/// can fire even when the caller didn't pass explicit `paths`. Heuristic: split
/// on whitespace and a few punctuation delimiters, keep tokens that either
/// contain a `/` (look like a path) OR carry a file extension (`name.ext` with
/// a short alphanumeric ext). Strips trailing sentence punctuation. Bounded to
/// 32 tokens so a pathological query can't blow up the candidate set.
fn extract_path_tokens(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in query.split([
        ' ', '\t', '\n', '\r', ',', ';', '(', ')', '"', '\'', '`', '<', '>',
    ]) {
        let tok =
            raw.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?' | ')' | '(' | '[' | ']'));
        if tok.is_empty() || tok.len() > 256 {
            continue;
        }
        let looks_like_path = tok.contains('/');
        let has_extension = std::path::Path::new(tok)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                (1..=8).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())
            });
        if (looks_like_path || has_extension) && !out.iter().any(|x| x == tok) {
            out.push(tok.to_string());
            if out.len() >= 32 {
                break;
            }
        }
    }
    out
}
fn default_recall_source() -> String {
    "mcp".into()
}

// ---------- C4: bounded in-memory recall metrics ring ----------

/// One observed recall, kept in a bounded ring for `GET /v1/recall/metrics`.
/// Process-local and lossy by design (the last [`RECALL_RING_CAP`] only) — a
/// cheap observability aid, not durable analytics. No new endpoint state on
/// `DaemonState`; lives in a module-level `OnceLock` so it stays self-contained.
#[derive(Clone, Serialize)]
struct RecallMetric {
    ts: String,
    source: String,
    query_len: usize,
    n_chunks: u32,
    n_memories: u32,
    injected_tokens: u32,
    elapsed_ms: u128,
}

const RECALL_RING_CAP: usize = 50;

fn recall_ring() -> &'static std::sync::Mutex<std::collections::VecDeque<RecallMetric>> {
    static RING: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<RecallMetric>>> =
        std::sync::OnceLock::new();
    RING.get_or_init(|| {
        std::sync::Mutex::new(std::collections::VecDeque::with_capacity(RECALL_RING_CAP))
    })
}

fn push_recall_metric(m: RecallMetric) {
    if let Ok(mut ring) = recall_ring().lock() {
        if ring.len() == RECALL_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(m);
    }
}

/// GET /v1/recall/metrics — the last [`RECALL_RING_CAP`] recalls, newest last.
/// Read-only; gated by `local_auth` like the rest of `/v1` when enforcement is
/// on. Returns an empty array before any recall has run.
async fn recall_metrics() -> Json<Vec<RecallMetric>> {
    let snapshot = recall_ring()
        .lock()
        .map(|r| r.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    Json(snapshot)
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

/// C4: build a short, human-readable provenance string for a recalled chunk
/// from the SAME signals used to rank it. Format: `"semantic <cos>"`, plus
/// `" · keywords: a, b"` for any content keywords the chunk hit (title or
/// text), plus `" · boilerplate down-weighted"` when the penalty fired. Kept
/// terse — this rides alongside every result.
fn chunk_why(semantic: f32, keywords: &[String], title: &str, text: &str, penalty: f32) -> String {
    let mut why = format!("semantic {semantic:.2}");
    // Which content keywords actually appear (title or text). Bounded to a few
    // so the line stays short.
    if !keywords.is_empty() {
        let title_l = title.to_lowercase();
        let text_l = text.to_lowercase();
        let hits: Vec<&str> = keywords
            .iter()
            .filter(|k| title_l.contains(k.as_str()) || text_l.contains(k.as_str()))
            .take(4)
            .map(|s| s.as_str())
            .collect();
        if !hits.is_empty() {
            why.push_str(" · keywords: ");
            why.push_str(&hits.join(", "));
        }
    }
    if penalty > 0.0 {
        why.push_str(" · boilerplate down-weighted");
    }
    why
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
    /// C4 provenance: a short, human-readable reason this chunk was recalled —
    /// e.g. `"semantic 0.71"`, `"semantic 0.66 · keywords: duckdb, schema"`,
    /// or with a note when the boilerplate penalty fired. Built from signals
    /// already computed during hybrid scoring; additive (serde).
    why: String,
}

/// A distilled memory injected into a recall response. Kept in a separate
/// `memories` array (not mixed into `items`) so the field is purely additive
/// and existing chunk-only consumers are unaffected. `kind` is always
/// "memory" for easy client-side tagging.
///
/// C2: `scope` tells consumers whether this is a standing rule (`always`), a
/// path-scoped note (`glob`), or a semantic recall (`auto`) — so a UI can
/// render rules distinctly from recalled facts. `globs` echoes the patterns
/// for glob-scoped entries (None otherwise).
#[derive(Serialize)]
struct RecallMemory {
    kind: &'static str,
    id: String,
    project: String,
    content: String,
    source: String,
    pinned: bool,
    scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    globs: Option<Vec<String>>,
    score: f32,
    /// C4 provenance: why this memory was injected — by tier. e.g.
    /// `"standing rule (always)"`, `"matches glob \"**/*.rs\""`, `"pinned"`,
    /// or `"semantic match 0.83"`. Additive (serde).
    why: String,
}

#[derive(Serialize)]
struct RecallResponse {
    items: Vec<RecallItem>,
    /// Distilled memories (C1) matching the same query+project, ranked by
    /// cosine similarity. Empty for global recall or when no memory matches.
    /// Additive — chunk-only consumers can ignore it.
    #[serde(default)]
    memories: Vec<RecallMemory>,
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
    // The tuple now carries a provenance string built from the same signals
    // used to rank (semantic cosine, which content keywords hit, whether the
    // boilerplate penalty applied) — so the `why` is honest by construction.
    let mut ranked: Vec<(SearchHit, Chunk, Option<String>, f32, String)> =
        Vec::with_capacity(raw.len());
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
        let why = chunk_why(
            hit.score,
            &keywords,
            title.as_deref().unwrap_or(""),
            &chunk.text,
            penalty,
        );
        ranked.push((hit, chunk, title, hybrid, why));
    }
    ranked.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));

    // Drop junk below the floor (caller's min_score, else the junk floor) and
    // cap per-session so one chat can't flood the results with its greeting +
    // setup chunks — "show me the chats about X" wants distinct chats.
    let floor = min_score.unwrap_or(0.0).max(HYBRID_JUNK_FLOOR);
    let mut per_session: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let hits: Vec<(SearchHit, Chunk, Option<String>, f32, String)> = ranked
        .into_iter()
        .filter(|(_, _, _, hybrid, _)| *hybrid >= floor)
        .filter(|(hit, _, _, _, _)| {
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
    for (hit, chunk, title, hybrid, why) in hits {
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
            why,
        });
    }

    // C2: scope-aware memory injection. Replaces C1's single semantic pass.
    // Candidates come from BOTH the request's project AND the reserved
    // `__global__` project (rules/notes authored once, applied everywhere);
    // project-scoped entries take precedence over global ones. Assembled in
    // strict precedence order, de-duplicated by id, packed under the SAME
    // token budget as the chunks — no extra LLM/API call (the no-key moat):
    //   1. `always` rules   — project first, then global, up to the cap.
    //   2. `glob`  memories — those whose globs match a candidate path.
    //   3. `auto`  memories — semantic match (C1's path), score-floored,
    //                         pinned-bypass.
    // `manual`-scoped memories never inject here (surfaced only via list/get).
    let mut memories: Vec<RecallMemory> = Vec::new();
    if let Some(p) = project.as_deref() {
        // The two project scopes to draw from, in precedence order. Skip the
        // duplicate when the request is already for the global project.
        let scopes: Vec<&str> = if p == ck_store::GLOBAL_PROJECT {
            vec![ck_store::GLOBAL_PROJECT]
        } else {
            vec![p, ck_store::GLOBAL_PROJECT]
        };

        // Candidate path set for glob matching: explicit caller `paths` ∪
        // path-like tokens harvested from the query text. Bounded in count and
        // length so a huge `paths` array (thousands of short strings still fit
        // under the 256 KiB body limit) can't force expensive glob matching
        // across every glob-scoped memory. `take` after `filter` stops early.
        const MAX_RECALL_PATHS: usize = 128;
        let mut candidate_paths: Vec<String> = req
            .paths
            .iter()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .take(MAX_RECALL_PATHS)
            .cloned()
            .collect();
        candidate_paths.extend(extract_path_tokens(&req.query));

        let max_always = hook_cfg
            .as_ref()
            .map(|h| h.max_always_injected as usize)
            .unwrap_or(3);
        let mem_floor = min_score.unwrap_or(0.0);

        // Estimate token cost of a memory (short single-sentence facts).
        let est = |content: &str| (content.split_whitespace().count() as u32 * 4 / 3).max(1);
        let mut mem_tokens = total_tokens;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Helper closure body is inlined (closures can't borrow `memories`
        // mutably while we also read it). `push` returns false when the budget
        // is exhausted so the caller can stop early.
        macro_rules! try_push {
            ($m:expr, $score:expr, $why:expr) => {{
                let m: ck_store::Memory = $m;
                if seen.contains(&m.id) {
                    false
                } else {
                    let cost = est(&m.content);
                    if mem_tokens + cost > budget && !memories.is_empty() {
                        false
                    } else {
                        mem_tokens += cost;
                        seen.insert(m.id.clone());
                        memories.push(RecallMemory {
                            kind: "memory",
                            id: m.id,
                            project: m.project_id,
                            content: m.content,
                            source: m.source,
                            pinned: m.pinned,
                            scope: m.scope,
                            globs: m.globs,
                            score: $score,
                            why: $why,
                        });
                        true
                    }
                }
            }};
        }

        // --- 1. `always` rules (project first, then global), capped. ---
        let always: Vec<ck_store::Memory> = {
            let meta = state
                .meta
                .lock()
                .map_err(|_| ApiError::internal("meta lock"))?;
            let mut v = Vec::new();
            for sc in &scopes {
                v.extend(meta.always_memories(sc, max_always).unwrap_or_default());
            }
            v
        };
        let mut always_injected = 0usize;
        for m in always {
            if always_injected >= max_always {
                break;
            }
            // Rules ignore the score floor entirely (score 1.0 = top precedence).
            let why = if m.pinned {
                "standing rule (always) · pinned".to_string()
            } else {
                "standing rule (always)".to_string()
            };
            if try_push!(m, 1.0, why) {
                always_injected += 1;
            }
        }

        // --- 2. `glob` memories whose patterns match a candidate path. ---
        // Skip the whole tier when there are no candidate paths (the common
        // case — most prompts mention no file). That avoids fetching glob rows
        // and recompiling their globsets on every prompt.
        let glob_mems: Vec<ck_store::Memory> = if candidate_paths.is_empty() {
            Vec::new()
        } else {
            let meta = state
                .meta
                .lock()
                .map_err(|_| ApiError::internal("meta lock"))?;
            let mut v = Vec::new();
            for sc in &scopes {
                v.extend(meta.glob_memories(sc, 50).unwrap_or_default());
            }
            v
        };
        for m in glob_mems {
            let patterns = m.globs.clone().unwrap_or_default();
            if ck_store::globs_match_any_path(&patterns, &candidate_paths) {
                // Provenance: name the specific pattern that fired (the first
                // one that matches any candidate path), so the reason is
                // concrete. Falls back to a generic phrasing if none isolates.
                let matched = patterns
                    .iter()
                    .find(|p| {
                        ck_store::globs_match_any_path(std::slice::from_ref(p), &candidate_paths)
                    })
                    .cloned();
                let why = match matched {
                    Some(p) => format!("matches glob {p:?}"),
                    None => "matches a glob pattern".to_string(),
                };
                // score 0.9: below a rule, above any semantic match.
                let _ = try_push!(m, 0.9, why);
            }
        }

        // --- 3. `auto` semantic match (C1 behavior), floored + pinned-bypass.
        let ranked: Vec<(ck_store::Memory, f32)> = {
            const MAX_INJECTED_MEMORIES: usize = 5;
            let meta = state
                .meta
                .lock()
                .map_err(|_| ApiError::internal("meta lock"))?;
            let mut v = Vec::new();
            for sc in &scopes {
                v.extend(
                    meta.search_memories(sc, q, MAX_INJECTED_MEMORIES)
                        .unwrap_or_default(),
                );
            }
            v
        };

        // C5 (part B): mild recency adjustment for staleness — the latest fact
        // on a topic should win a near-tie. We sort the auto tier by an
        // *effective* score = cosine + a small recency bonus derived from
        // `updated_at`, with the bonus capped at `RECENCY_BONUS_MAX` (0.03)
        // — far smaller than the gap between distinct facts, so relevance still
        // dominates and recency only breaks ties between near-equal cosines.
        // The newest memory in the set gets the full bonus, the oldest ~zero,
        // scaled linearly across the observed `updated_at` span (no bonus when
        // all timestamps are equal). Display/floor logic stays on the raw
        // cosine, so the score-floor and the "why" string remain honest.
        // (Project rows still precede global rows: search drew them separately,
        // and the stable sort preserves that order within an effective tie.)
        const RECENCY_BONUS_MAX: f32 = 0.03;
        // `min_ts`/`max_ts` over the (possibly empty) set. `saturating_sub`
        // keeps the empty/degenerate case (where the seeds invert to
        // MAX/MIN) from overflowing; the span then clamps to 0 → no bonus.
        let (min_ts, max_ts) = ranked
            .iter()
            .fold((i64::MAX, i64::MIN), |(lo, hi), (m, _)| {
                (lo.min(m.updated_at), hi.max(m.updated_at))
            });
        let span = max_ts.saturating_sub(min_ts).max(0) as f32;
        let recency_bonus = |updated_at: i64| -> f32 {
            if span <= 0.0 {
                0.0
            } else {
                (updated_at.saturating_sub(min_ts) as f32 / span) * RECENCY_BONUS_MAX
            }
        };
        let mut ranked = ranked;
        ranked.sort_by(|a, b| {
            let ea = a.1 + recency_bonus(a.0.updated_at);
            let eb = b.1 + recency_bonus(b.0.updated_at);
            eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
        });

        for (m, score) in ranked {
            // Pinned memories bypass the score floor — the user explicitly
            // kept them.
            if !m.pinned && score < mem_floor {
                continue;
            }
            let why = if m.pinned {
                format!("semantic match {score:.2} · pinned")
            } else {
                format!("semantic match {score:.2}")
            };
            if !try_push!(m, score, why) {
                break;
            }
        }
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

    // C4 observability: one structured line per recall (source, query length,
    // counts, injected tokens, elapsed). Cheap, no new endpoint required; the
    // bounded ring below backs GET /v1/recall/metrics.
    let elapsed_ms = start.elapsed().as_millis();
    let n_chunks = items.len() as u32;
    let n_memories = memories.len() as u32;
    tracing::info!(
        source = %req.source,
        query_len = req.query.len(),
        n_chunks,
        n_memories,
        injected_tokens = total_tokens,
        elapsed_ms,
        "recall"
    );
    push_recall_metric(RecallMetric {
        ts: now.clone(),
        source: req.source.clone(),
        query_len: req.query.len(),
        n_chunks,
        n_memories,
        injected_tokens: total_tokens,
        elapsed_ms,
    });

    Ok(Json(RecallResponse {
        items,
        memories,
        total_chunks: total,
        total_tokens,
        truncated,
        token_budget: budget,
        mmr_lambda: req.mmr_lambda,
        elapsed_ms,
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

// ---------- /v1/memories (C1: writable, queryable memory store) ----------
//
// All five routes use ONLY the existing LOCAL embedder — no LLM, no API key
// (the no-API-key moat). The agent (via MCP) and the opt-in distiller both
// write here; recall semantically searches and injects the results.

/// Max memory content length. Mirrors recall's query bound (8 KiB) — a memory
/// is a short distilled fact, not a document; this caps embedding work and DB
/// row size for an attacker-controlled body.
const MAX_MEMORY_CONTENT_BYTES: usize = 8_192;

/// Validate a `source` value; defaults to "agent" when absent. Only the three
/// documented kinds are accepted so the column stays a clean enum.
fn normalize_source(s: Option<&str>) -> std::result::Result<String, ApiError> {
    match s.unwrap_or("agent") {
        v @ ("agent" | "user" | "distilled") => Ok(v.to_string()),
        other => Err(ApiError::bad_request(format!(
            "source must be one of agent|user|distilled, got {other:?}"
        ))),
    }
}

/// Max glob patterns per memory, and max length of a single pattern. Bounds an
/// attacker-controlled body (these compile into a globset at recall time).
const MAX_GLOBS: usize = 32;
const MAX_GLOB_LEN: usize = 256;

/// Validate a `scope` value (default "auto") and the accompanying `globs`.
/// Enforces the C2 contract: `scope` ∈ auto|always|glob|manual, and a
/// `glob`-scoped memory MUST carry at least one non-empty, well-formed glob
/// (an empty/absent glob list on scope=glob is a 400). Returns the normalized
/// scope and the cleaned glob list (only `Some` for scope=glob).
fn normalize_scope_and_globs(
    scope: Option<&str>,
    globs: Option<&[String]>,
) -> std::result::Result<(String, Option<Vec<String>>), ApiError> {
    let scope = scope.unwrap_or("auto");
    if !ck_store::is_valid_scope(scope) {
        return Err(ApiError::bad_request(format!(
            "scope must be one of auto|always|glob|manual, got {scope:?}"
        )));
    }
    // Clean any provided globs (trim, drop blanks, bound count + length).
    let cleaned: Vec<String> = globs
        .map(|g| {
            g.iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    if cleaned.len() > MAX_GLOBS {
        return Err(ApiError::bad_request(format!("at most {MAX_GLOBS} globs")));
    }
    if cleaned.iter().any(|g| g.len() > MAX_GLOB_LEN) {
        return Err(ApiError::bad_request(format!(
            "each glob must be ≤ {MAX_GLOB_LEN} bytes"
        )));
    }
    // Every provided pattern must compile (a typo'd glob is a bad request, not
    // a silently-ignored memory).
    for g in &cleaned {
        if globset::Glob::new(g).is_err() {
            return Err(ApiError::bad_request(format!(
                "invalid glob pattern: {g:?}"
            )));
        }
    }
    if scope == "glob" {
        if cleaned.is_empty() {
            return Err(ApiError::bad_request(
                "scope=glob requires a non-empty globs array",
            ));
        }
        Ok((scope.to_string(), Some(cleaned)))
    } else {
        // For non-glob scopes we ignore any provided globs (they have no
        // effect) and store NULL — keeps the column meaningful.
        Ok((scope.to_string(), None))
    }
}

/// Embed one piece of text with the LOCAL embedder (content-addressed cache).
fn embed_one(state: &DaemonState, text: &str) -> std::result::Result<Vec<f32>, ApiError> {
    let outcome = embed_with_cache(
        state.embedder.as_ref(),
        &state.layout,
        std::slice::from_ref(&text.to_string()),
    )
    .map_err(|e| ApiError::internal(format!("embed: {e}")))?;
    outcome
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::internal("embed produced no vector"))
}

#[derive(Deserialize)]
struct CreateMemoryRequest {
    #[serde(default)]
    project_id: Option<String>,
    content: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    pinned: bool,
    /// C2: injection scope — auto|always|glob|manual (default auto).
    #[serde(default)]
    scope: Option<String>,
    /// C2: glob patterns; required (non-empty) when scope=glob, ignored else.
    #[serde(default)]
    globs: Option<Vec<String>>,
    /// C2 convenience: when true, store under the reserved `__global__`
    /// project (rule/note applied across every project). Equivalent to
    /// passing `project_id:"__global__"`; `global:true` wins if both are set.
    #[serde(default)]
    global: bool,
}

async fn create_memory(
    State(state): State<DaemonState>,
    Json(req): Json<CreateMemoryRequest>,
) -> std::result::Result<Json<Memory>, ApiError> {
    // Resolve the target project: `global:true` → the reserved sentinel;
    // otherwise the explicit project_id. `__global__` passes ensure_safe_id.
    let project_id = if req.global {
        ck_store::GLOBAL_PROJECT.to_string()
    } else {
        req.project_id
            .clone()
            .ok_or_else(|| ApiError::bad_request("project_id is required (or set global:true)"))?
    };
    ck_store::ensure_safe_id(&project_id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let content = req.content.trim();
    if content.is_empty() {
        return Err(ApiError::bad_request("content must not be empty"));
    }
    if content.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(ApiError::bad_request(format!(
            "content too long (max {MAX_MEMORY_CONTENT_BYTES} bytes)"
        )));
    }
    let source = normalize_source(req.source.as_deref())?;
    let (scope, globs) = normalize_scope_and_globs(req.scope.as_deref(), req.globs.as_deref())?;
    // Redact secrets before embedding/storage — the same invariant transcript
    // chunks hold (ck-chunk::make_chunk): no secret reaches a derived artifact
    // (embedding, store, recall result, or UI). Agent/user-written memories must
    // not become the hole in that promise.
    let (clean, n) = ck_chunk::redact::redact_secrets(content);
    if n > 0 {
        tracing::debug!(redactions = n, "redacted secrets from memory content");
    }
    let content = clean.as_ref();
    let embedding = embed_one(&state, content)?;

    // C5 (part C, opt-in): full LLM reconciliation — ADD / UPDATE <id> / NOOP
    // for true contradictions ("build is X" → "build is Y"). Gated behind the
    // `memory_reconcile` flag (or env `CK_MEMORY_RECONCILE=1`); default OFF, so
    // the default path is exactly the part-A local dedupe below. On ANY error or
    // unavailable LLM it falls through to part A — never blocks the write, never
    // requires a key (the no-API-key moat).
    let reconcile_on = state
        .config
        .read()
        .map(|c| ck_store::memory_reconcile_enabled(&c))
        .unwrap_or(false);
    if reconcile_on {
        match reconcile_with_llm(&state, &project_id, &scope, content, &embedding).await {
            ReconcileDecision::Noop(existing) => return Ok(Json(existing)),
            ReconcileDecision::Update(updated) => return Ok(Json(updated)),
            ReconcileDecision::Add => { /* fall through to a fresh insert below */ }
            ReconcileDecision::FallBackToLocal => {
                if let Some(m) = local_supersede(&state, &project_id, &scope, content, &embedding)?
                {
                    return Ok(Json(m));
                }
            }
        }
    } else {
        // C5 (part A, default ON, no LLM): supersede-on-write. If the nearest
        // same-(project,scope) memory is within the dedupe threshold, UPDATE it
        // in place (keep id/pinned/scope/globs, replace content+embedding, bump
        // updated_at) instead of inserting a near-duplicate.
        if let Some(m) = local_supersede(&state, &project_id, &scope, content, &embedding)? {
            return Ok(Json(m));
        }
    }

    let now = chrono::Utc::now().timestamp();
    let memory = Memory {
        id: ck_store::MetaIndex::new_memory_id(),
        project_id,
        content: content.to_string(),
        source,
        pinned: req.pinned,
        scope,
        globs,
        created_at: now,
        updated_at: now,
    };
    {
        let meta = state
            .meta
            .lock()
            .map_err(|_| ApiError::internal("meta lock"))?;
        meta.insert_memory(&memory, &embedding)
            .map_err(|e| ApiError::internal(format!("insert_memory: {e}")))?;
    }
    Ok(Json(memory))
}

/// C5 part A: local (no-LLM) supersede-on-write. If the nearest existing memory
/// in the SAME `(project_id, scope)` is within the configured dedupe threshold,
/// UPDATE that row in place — replacing its content + embedding and bumping
/// `updated_at`, while keeping its id/pinned/scope/globs — and return the
/// updated row. Returns `Ok(None)` when nothing is close enough (caller inserts
/// a fresh memory). Scope-aware and project-scoped by construction (the store
/// query filters on both), so an `always` rule never dedupes against an `auto`
/// memory and one project never merges into another or into `__global__`.
fn local_supersede(
    state: &DaemonState,
    project_id: &str,
    scope: &str,
    content: &str,
    embedding: &[f32],
) -> std::result::Result<Option<Memory>, ApiError> {
    let threshold = state
        .config
        .read()
        .map(|c| ck_store::clamp_dedupe_threshold(c.dedupe_threshold))
        .unwrap_or_else(|_| ck_store::clamp_dedupe_threshold(0.95));
    let meta = state
        .meta
        .lock()
        .map_err(|_| ApiError::internal("meta lock"))?;
    let nearest = meta
        .nearest_memory_in_scope(project_id, scope, embedding)
        .map_err(|e| ApiError::internal(format!("nearest_memory_in_scope: {e}")))?;
    if let Some((existing, score)) = nearest {
        if score >= threshold {
            // Supersede in place: replace content + embedding, bump updated_at;
            // keep id/pinned/scope/globs (None args leave those columns alone).
            meta.update_memory(
                &existing.id,
                Some(content),
                None,
                Some(embedding),
                None,
                None,
            )
            .map_err(|e| ApiError::internal(format!("update_memory: {e}")))?;
            let updated = meta
                .get_memory(&existing.id)
                .map_err(|e| ApiError::internal(format!("get_memory: {e}")))?
                .ok_or_else(|| ApiError::internal("memory vanished after supersede"))?;
            tracing::debug!(
                id = %updated.id,
                score,
                threshold,
                "memory superseded in place (local dedupe)"
            );
            return Ok(Some(updated));
        }
    }
    Ok(None)
}

/// C5 part C: structured outcome of an LLM reconciliation attempt.
enum ReconcileDecision {
    /// Insert the new fact as a brand-new memory (the LLM said it's novel).
    Add,
    /// The new fact superseded/merged an existing one, already updated in
    /// place — carry the updated row back to the caller.
    Update(Memory),
    /// The fact is already known; nothing changed — return the existing row.
    Noop(Memory),
    /// The LLM was unavailable, errored, or returned something unparseable —
    /// the caller should fall back to the part-A local dedupe.
    FallBackToLocal,
}

/// Cap how many candidate memories we hand the LLM to reconcile against. Small
/// (≤5) so the prompt stays tight and the (opt-in) call stays cheap.
const RECONCILE_TOP_K: usize = 5;

/// System prompt for the opt-in reconcile call. Asks for ONE of three
/// machine-parseable verdicts on its own first line; everything else is
/// ignored. Mirrors mem0/Zep ADD/UPDATE/NOOP semantics.
const RECONCILE_SYSTEM_PROMPT: &str = "\
You maintain a small store of durable project facts. Given a NEW fact and a \
numbered list of EXISTING facts on related topics, decide how the store should \
change. Reply with EXACTLY ONE line, nothing else, in one of these forms:\n\
  ADD            — the new fact is genuinely novel; keep it as a new entry.\n\
  UPDATE <n>     — the new fact supersedes or refines existing fact #<n> \
(same topic, newer/corrected value, e.g. a reversed decision); replace it.\n\
  NOOP <n>       — the new fact is already captured by existing fact #<n>; \
change nothing.\n\
Prefer UPDATE over ADD when the new fact contradicts or restates an existing \
one on the same subject. Use the existing fact's number exactly as listed.";

/// C5 part C: attempt LLM-based reconciliation of `content` against the top-K
/// most-similar same-`(project,scope)` memories. Returns a structured
/// [`ReconcileDecision`]; ANY failure (no LLM, network error, unparseable
/// reply, out-of-range index) yields [`ReconcileDecision::FallBackToLocal`] so
/// the caller degrades to the local part-A dedupe — never blocking the write
/// and never requiring a key. Routed through `OrchestratorSummarizer` exactly
/// like the distiller (no cloud key held here).
async fn reconcile_with_llm(
    state: &DaemonState,
    project_id: &str,
    scope: &str,
    content: &str,
    embedding: &[f32],
) -> ReconcileDecision {
    // Gather the top-K candidates by cosine within the same project+scope
    // (in-process, no LLM). A store error degrades to the local fallback.
    let Some(candidates) = top_k_by_cosine(state, project_id, scope, embedding, RECONCILE_TOP_K)
    else {
        return ReconcileDecision::FallBackToLocal;
    };
    if candidates.is_empty() {
        // Nothing to reconcile against — it's unambiguously new.
        return ReconcileDecision::Add;
    }

    // Build the prompt: numbered existing facts + the new one.
    let mut user = String::from("NEW fact:\n");
    user.push_str(content);
    user.push_str("\n\nEXISTING facts:\n");
    for (i, (m, _)) in candidates.iter().enumerate() {
        user.push_str(&format!("{}. {}\n", i + 1, m.content));
    }

    // Route through the orchestrator exactly like the distiller. from_env never
    // needs a key; the call itself fails gracefully when no LLM is reachable.
    let summarizer = match ck_summarize::OrchestratorSummarizer::from_env() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "reconcile: orchestrator unavailable, local fallback");
            return ReconcileDecision::FallBackToLocal;
        }
    };
    use ck_summarize::Summarizer as _;
    let raw = match summarizer
        .complete(RECONCILE_SYSTEM_PROMPT, &user, 32)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "reconcile: LLM call failed, local fallback");
            return ReconcileDecision::FallBackToLocal;
        }
    };

    apply_reconcile_verdict(state, &raw, &candidates, content, embedding)
}

/// Top-K same-`(project,scope)` memories by cosine to `embedding`, descending.
/// In-process O(n) over the scope's rows (small sets); no LLM. Returns `None`
/// only on a store error so the caller can keep its earlier candidate list.
fn top_k_by_cosine(
    state: &DaemonState,
    project_id: &str,
    scope: &str,
    embedding: &[f32],
    k: usize,
) -> Option<Vec<(Memory, f32)>> {
    // For the `auto` scope the store's existing semantic search already returns
    // scored top-K. For non-auto scopes (rules/globs/manual — few rows, rarely
    // near-duplicate) the single nearest is sufficient for the UPDATE/NOOP call.
    let meta = state.meta.lock().ok()?;
    if scope == "auto" {
        return meta.search_memories(project_id, embedding, k).ok();
    }
    match meta
        .nearest_memory_in_scope(project_id, scope, embedding)
        .ok()?
    {
        Some(best) => Some(vec![best]),
        None => Some(Vec::new()),
    }
}

/// Parse the LLM's one-line verdict and apply it. Defensive: an unparseable
/// reply or an out-of-range index degrades to [`ReconcileDecision::FallBackToLocal`].
fn apply_reconcile_verdict(
    state: &DaemonState,
    raw: &str,
    candidates: &[(Memory, f32)],
    content: &str,
    embedding: &[f32],
) -> ReconcileDecision {
    // First non-empty line, uppercased verb.
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut parts = line.split_whitespace();
    let verb = parts.next().unwrap_or("").to_ascii_uppercase();
    let idx_1based: Option<usize> = parts.next().and_then(|s| {
        s.trim_matches(|c: char| !c.is_ascii_digit())
            .parse::<usize>()
            .ok()
    });

    match verb.as_str() {
        "ADD" => ReconcileDecision::Add,
        "NOOP" => match idx_1based.and_then(|n| candidates.get(n - 1)) {
            Some((m, _)) => ReconcileDecision::Noop(m.clone()),
            None => ReconcileDecision::FallBackToLocal,
        },
        "UPDATE" => {
            let Some((existing, _)) = idx_1based.and_then(|n| candidates.get(n - 1)) else {
                return ReconcileDecision::FallBackToLocal;
            };
            // Never auto-supersede a pinned (user-asserted) memory — for the
            // auto scope the candidate set (search_memories) includes pinned
            // rows, so guard here. Treat it as already-known (NOOP), adding
            // nothing rather than overwriting the user's pinned fact.
            if existing.pinned {
                return ReconcileDecision::Noop(existing.clone());
            }
            let meta = match state.meta.lock() {
                Ok(m) => m,
                Err(_) => return ReconcileDecision::FallBackToLocal,
            };
            // Supersede in place — keep id/pinned/scope/globs, replace
            // content + embedding, bump updated_at.
            if meta
                .update_memory(
                    &existing.id,
                    Some(content),
                    None,
                    Some(embedding),
                    None,
                    None,
                )
                .unwrap_or(false)
            {
                match meta.get_memory(&existing.id) {
                    Ok(Some(m)) => ReconcileDecision::Update(m),
                    _ => ReconcileDecision::FallBackToLocal,
                }
            } else {
                ReconcileDecision::FallBackToLocal
            }
        }
        _ => ReconcileDecision::FallBackToLocal,
    }
}

#[derive(Deserialize)]
struct MemoriesQuery {
    project: String,
    #[serde(default = "default_memories_limit")]
    limit: u32,
    /// C2: optional scope filter (auto|always|glob|manual). Absent → all.
    #[serde(default)]
    scope: Option<String>,
}
fn default_memories_limit() -> u32 {
    50
}

async fn list_memories_handler(
    State(state): State<DaemonState>,
    Query(q): Query<MemoriesQuery>,
) -> std::result::Result<Json<Vec<Memory>>, ApiError> {
    ck_store::ensure_safe_id(&q.project).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if let Some(s) = q.scope.as_deref() {
        if !ck_store::is_valid_scope(s) {
            return Err(ApiError::bad_request(
                "scope must be one of auto|always|glob|manual",
            ));
        }
    }
    let limit = q.limit.clamp(1, 500) as usize;
    let meta = state
        .meta
        .lock()
        .map_err(|_| ApiError::internal("meta lock"))?;
    let out = meta
        .list_memories(&q.project, q.scope.as_deref(), limit)
        .map_err(|e| ApiError::internal(format!("list_memories: {e}")))?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct UpdateMemoryRequest {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
    /// C2: change the injection scope (auto|always|glob|manual).
    #[serde(default)]
    scope: Option<String>,
    /// C2: replace the glob patterns. Only meaningful when the (new or
    /// existing) scope is `glob`; cleared for non-glob scopes.
    #[serde(default)]
    globs: Option<Vec<String>>,
}

async fn update_memory_handler(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMemoryRequest>,
) -> std::result::Result<Json<Memory>, ApiError> {
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    // Re-embed only when content changes. Validate it first.
    let trimmed = match &req.content {
        Some(c) => {
            let t = c.trim();
            if t.is_empty() {
                return Err(ApiError::bad_request("content must not be empty"));
            }
            if t.len() > MAX_MEMORY_CONTENT_BYTES {
                return Err(ApiError::bad_request(format!(
                    "content too long (max {MAX_MEMORY_CONTENT_BYTES} bytes)"
                )));
            }
            // Redact secrets before re-embedding/storage (see create_memory).
            let (clean, n) = ck_chunk::redact::redact_secrets(t);
            if n > 0 {
                tracing::debug!(redactions = n, "redacted secrets from memory content");
            }
            Some(clean.into_owned())
        }
        None => None,
    };

    // Resolve the scope change. When `scope` is set, validate it against the
    // accompanying globs (scope=glob with empty globs → 400). When only
    // `globs` is set (no scope), fetch the current scope so we can enforce the
    // glob contract against it and so a stale scope/glob pairing can't arise.
    let (scope_change, globs_change): (Option<String>, Option<Option<Vec<String>>>) =
        match (&req.scope, &req.globs) {
            (None, None) => (None, None),
            (Some(s), g) => {
                let (sc, gl) = normalize_scope_and_globs(Some(s), g.as_deref())?;
                // For a glob scope, set the cleaned globs; otherwise clear them.
                (Some(sc), Some(gl))
            }
            (None, Some(g)) => {
                // Only globs changed — validate against the existing scope.
                let existing = {
                    let meta = state
                        .meta
                        .lock()
                        .map_err(|_| ApiError::internal("meta lock"))?;
                    meta.get_memory(&id)
                        .map_err(|e| ApiError::internal(format!("get_memory: {e}")))?
                        .ok_or_else(|| ApiError::not_found(format!("memory: {id}")))?
                };
                let (_sc, gl) =
                    normalize_scope_and_globs(Some(&existing.scope), Some(g.as_slice()))?;
                (None, Some(gl))
            }
        };

    if trimmed.is_none() && req.pinned.is_none() && scope_change.is_none() && globs_change.is_none()
    {
        return Err(ApiError::bad_request(
            "nothing to update (provide content, pinned, scope, and/or globs)",
        ));
    }
    let new_embedding = match &trimmed {
        Some(t) => Some(embed_one(&state, t)?),
        None => None,
    };

    let meta = state
        .meta
        .lock()
        .map_err(|_| ApiError::internal("meta lock"))?;
    // Map the resolved glob change into the store's nested-Option contract:
    // outer None = leave alone, Some(None) = clear, Some(Some(v)) = set.
    let globs_arg: Option<Option<&[String]>> = globs_change.as_ref().map(|g| g.as_deref());
    let updated = meta
        .update_memory(
            &id,
            trimmed.as_deref(),
            req.pinned,
            new_embedding.as_deref(),
            scope_change.as_deref(),
            globs_arg,
        )
        .map_err(|e| ApiError::internal(format!("update_memory: {e}")))?;
    if !updated {
        return Err(ApiError::not_found(format!("memory: {id}")));
    }
    let memory = meta
        .get_memory(&id)
        .map_err(|e| ApiError::internal(format!("get_memory: {e}")))?
        .ok_or_else(|| ApiError::not_found(format!("memory: {id}")))?;
    Ok(Json(memory))
}

async fn delete_memory_handler(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let meta = state
        .meta
        .lock()
        .map_err(|_| ApiError::internal("meta lock"))?;
    let deleted = meta
        .delete_memory(&id)
        .map_err(|e| ApiError::internal(format!("delete_memory: {e}")))?;
    if !deleted {
        return Err(ApiError::not_found(format!("memory: {id}")));
    }
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize)]
struct SearchMemoriesRequest {
    project_id: String,
    query: String,
    #[serde(default = "default_memories_search_limit")]
    limit: u32,
}
fn default_memories_search_limit() -> u32 {
    10
}

#[derive(Serialize)]
struct ScoredMemory {
    memory: Memory,
    score: f32,
}

async fn search_memories_handler(
    State(state): State<DaemonState>,
    Json(req): Json<SearchMemoriesRequest>,
) -> std::result::Result<Json<Vec<ScoredMemory>>, ApiError> {
    ck_store::ensure_safe_id(&req.project_id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if req.query.trim().is_empty() {
        return Err(ApiError::bad_request("query must not be empty"));
    }
    if req.query.len() > MAX_MEMORY_CONTENT_BYTES {
        return Err(ApiError::bad_request("query too long"));
    }
    let limit = req.limit.clamp(1, 100) as usize;
    let q = embed_one(&state, &req.query)?;
    let meta = state
        .meta
        .lock()
        .map_err(|_| ApiError::internal("meta lock"))?;
    let ranked = meta
        .search_memories(&req.project_id, &q, limit)
        .map_err(|e| ApiError::internal(format!("search_memories: {e}")))?;
    Ok(Json(
        ranked
            .into_iter()
            .map(|(memory, score)| ScoredMemory { memory, score })
            .collect(),
    ))
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
    if h.max_always_injected > 20 {
        return Err(ApiError::bad_request(
            "hook.max_always_injected must be ≤ 20",
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

// ---------- contributed summaries & topic names (the no-API-key path) ----------
//
// Claude itself — connected through the MCP plugin — generates summaries and
// topic names in conversation and saves them here. Same schema, same caches,
// same idempotency (`input_hash`) as the API-key path; only `generated_by`
// differs. The daemon never needs a key for any of this.

/// Generator label used for client-contributed summaries. Part of the
/// input_hash, so a re-contribution over unchanged chunks is a cache no-op.
const MCP_SUMMARY_MODEL: &str = "claude-mcp";

#[derive(Serialize)]
struct UnsummarizedSession {
    id: String,
    project_id: String,
    title: Option<String>,
    message_count: u32,
    chunk_count: u32,
    ended_at: String,
}

#[derive(Deserialize)]
struct UnsummarizedQuery {
    project: Option<String>,
    #[serde(default = "default_unsummarized_limit")]
    limit: u32,
}
fn default_unsummarized_limit() -> u32 {
    20
}

async fn list_unsummarized(
    State(state): State<DaemonState>,
    Query(q): Query<UnsummarizedQuery>,
) -> Json<Vec<UnsummarizedSession>> {
    let mut sessions: Vec<Session> = Vec::new();
    if let Ok(read) = std::fs::read_dir(state.layout.sessions_dir()) {
        for entry in read.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                if let Ok(s) = serde_json::from_slice::<Session>(&bytes) {
                    if s.summary.is_some() || s.is_sidechain {
                        continue;
                    }
                    if let Some(p) = &q.project {
                        if s.project_id.0 != *p {
                            continue;
                        }
                    }
                    // Trivial sessions aren't worth a summary slot.
                    if s.message_count < 4 {
                        continue;
                    }
                    sessions.push(s);
                }
            }
        }
    }
    sessions.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
    sessions.truncate(q.limit.clamp(1, 100) as usize);
    Json(
        sessions
            .into_iter()
            .map(|s| UnsummarizedSession {
                id: s.id.0,
                project_id: s.project_id.0,
                title: s.ai_title.or(s.first_prompt),
                message_count: s.message_count,
                chunk_count: s.chunk_ids.len() as u32,
                ended_at: s.ended_at.to_rfc3339(),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct ContributedSummary {
    text: String,
    #[serde(default)]
    bullets: Vec<String>,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    generated_by: Option<String>,
}

async fn put_session_summary(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
    Json(body): Json<ContributedSummary>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let text = body.text.trim();
    if text.is_empty() || text.chars().count() > 4000 {
        return Err(ApiError::bad_request("summary text must be 1–4000 chars"));
    }
    for (name, list) in [
        ("bullets", &body.bullets),
        ("decisions", &body.decisions),
        ("artifacts", &body.artifacts),
    ] {
        if list.len() > 16 || list.iter().any(|s| s.chars().count() > 500) {
            return Err(ApiError::bad_request(format!(
                "{name}: at most 16 items of ≤500 chars each"
            )));
        }
    }
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let session_id = SessionId(id);
    let mut session = read_session(&state.layout, &session_id)
        .map_err(|e| ApiError::not_found(format!("session: {e}")))?;

    // Hash over the session's current chunks so staleness is detectable
    // exactly like the API-key path.
    let mut chunks = Vec::with_capacity(session.chunk_ids.len());
    for cid in &session.chunk_ids {
        if let Ok(c) = read_chunk(&state.layout, &session_id, cid) {
            chunks.push(c);
        }
    }
    let summary = ck_core::SessionSummary {
        text: text.to_string(),
        bullets: body.bullets,
        decisions: body.decisions,
        artifacts: body.artifacts,
        generated_by: body
            .generated_by
            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 80)
            .unwrap_or_else(|| "claude-via-mcp".to_string()),
        generated_at: chrono::Utc::now(),
        input_hash: ck_summarize::input_hash(MCP_SUMMARY_MODEL, &chunks),
    };
    ck_summarize::write_cached(&state.layout, &summary)
        .map_err(|e| ApiError::internal(format!("cache summary: {e}")))?;
    session.summary = Some(summary);
    ck_store::write_session(&state.layout, &session)
        .map_err(|e| ApiError::internal(format!("write session: {e}")))?;
    Ok(Json(
        serde_json::json!({ "ok": true, "session": session.id.0 }),
    ))
}

#[derive(Deserialize)]
struct TopicNameBody {
    label: String,
    #[serde(default)]
    description: Option<String>,
}

async fn put_topic_name(
    State(state): State<DaemonState>,
    Path(id): Path<String>,
    Json(body): Json<TopicNameBody>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    // Defense in depth: the id flows into a cache file path. set_topic_name
    // only writes for an id that matches an existing (hash-shaped) topic, but
    // don't let that cross-crate invariant be the only guard.
    ck_store::ensure_safe_id(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let label = body.label.trim();
    let count = label.chars().count();
    if !(3..=80).contains(&count) {
        return Err(ApiError::bad_request("label must be 3–80 chars"));
    }
    if let Some(d) = &body.description {
        if d.chars().count() > 300 {
            return Err(ApiError::bad_request("description must be ≤300 chars"));
        }
    }
    let found = ck_graph::set_topic_name(&state.layout, &id, label, body.description.as_deref())
        .map_err(|e| ApiError::internal(format!("set_topic_name: {e}")))?;
    if !found {
        return Err(ApiError::not_found(format!("topic: {id}")));
    }
    Ok(Json(
        serde_json::json!({ "ok": true, "topic": id, "label": label }),
    ))
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
    fn origin_loopback_classification() {
        for ok in [
            "http://localhost:5173",
            "http://127.0.0.1:12100",
            "http://[::1]:7421",
            "https://localhost",
            "http://127.0.0.1",
        ] {
            assert!(super::origin_is_loopback(ok), "should allow {ok}");
        }
        for bad in [
            "http://evil.com",
            "https://attacker.example:443",
            "http://127.0.0.1.evil.com",
            "http://10.0.0.5:12100",
            "null",
        ] {
            assert!(!super::origin_is_loopback(bad), "should reject {bad}");
        }
    }

    /// Agent/user-written memory content must be secret-redacted before it is
    /// embedded or stored — the same invariant transcript chunks hold. A
    /// `remember` call carrying a credential must not persist it in the clear.
    #[tokio::test]
    async fn create_memory_redacts_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        let secret = "sk-ant-api03-abcdEFGH1234567890abcdEFGH1234567890";
        let Json(created) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: format!("the api key is {secret}"),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create");
        assert!(
            !created.content.contains(secret),
            "secret must not be stored verbatim: {}",
            created.content
        );
        assert!(
            created.content.contains("[REDACTED:"),
            "content should carry the redaction marker: {}",
            created.content
        );

        // …and it stays redacted on the way back out.
        let Json(listed) = list_memories_handler(
            State(state.clone()),
            Query(MemoriesQuery {
                project,
                limit: 50,
                scope: None,
            }),
        )
        .await
        .expect("list");
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].content.contains(secret));
    }

    /// Full memory CRUD roundtrip through the axum handlers (not raw store):
    /// create → list → search → recall-injection → update → delete → 404.
    /// Uses the stub embedder so no model download is required.
    #[tokio::test]
    async fn memory_crud_and_recall_injection_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        // create
        let Json(created) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "  always use the local embedder  ".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create");
        assert_eq!(created.content, "always use the local embedder"); // trimmed
        assert_eq!(created.source, "agent"); // default
        assert_eq!(created.scope, "auto"); // default scope
        let id = created.id.clone();

        // list
        let Json(listed) = list_memories_handler(
            State(state.clone()),
            Query(MemoriesQuery {
                project: project.clone(),
                limit: 50,
                scope: None,
            }),
        )
        .await
        .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);

        // search
        let Json(found) = search_memories_handler(
            State(state.clone()),
            Json(SearchMemoriesRequest {
                project_id: project.clone(),
                query: "embedder".into(),
                limit: 10,
            }),
        )
        .await
        .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].memory.id, id);

        // recall injects the memory (project-scoped)
        let Json(rc) = recall(
            State(state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "query": "embedder",
                    "project": project,
                    "source": "mcp"
                }))
                .unwrap(),
            ),
        )
        .await
        .expect("recall");
        assert_eq!(
            rc.memories.len(),
            1,
            "memory should be injected into recall"
        );
        assert_eq!(rc.memories[0].id, id);
        assert_eq!(rc.memories[0].kind, "memory");

        // update (pin + new content)
        let Json(updated) = update_memory_handler(
            State(state.clone()),
            Path(id.clone()),
            Json(UpdateMemoryRequest {
                content: Some("pinned revised fact".into()),
                pinned: Some(true),
                scope: None,
                globs: None,
            }),
        )
        .await
        .expect("update");
        assert_eq!(updated.content, "pinned revised fact");
        assert!(updated.pinned);

        // delete
        let Json(del) = delete_memory_handler(State(state.clone()), Path(id.clone()))
            .await
            .expect("delete");
        assert_eq!(del["ok"], true);

        // re-delete → 404
        let err = delete_memory_handler(State(state.clone()), Path(id.clone()))
            .await
            .expect_err("second delete should 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);

        // get-after-delete via update → 404
        let err = update_memory_handler(
            State(state.clone()),
            Path(id),
            Json(UpdateMemoryRequest {
                content: Some("x".into()),
                pinned: None,
                scope: None,
                globs: None,
            }),
        )
        .await
        .expect_err("update of deleted should 404");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    /// A path-unsafe memory id must be rejected with 400 at the boundary,
    /// never reaching the store (defense in depth, matching get_session).
    #[tokio::test]
    async fn memory_id_traversal_rejected_with_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let bad = "../../etc/passwd".to_string();

        let err = delete_memory_handler(State(state.clone()), Path(bad.clone()))
            .await
            .expect_err("bad id must error");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let err = update_memory_handler(
            State(state.clone()),
            Path(bad),
            Json(UpdateMemoryRequest {
                content: Some("x".into()),
                pinned: None,
                scope: None,
                globs: None,
            }),
        )
        .await
        .expect_err("bad id must error");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        // bad project_id on create
        let err = create_memory(
            State(state),
            Json(CreateMemoryRequest {
                project_id: Some("a/b".into()),
                content: "x".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect_err("bad project_id must error");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    /// Empty content is a 400, not a stored blank memory.
    #[tokio::test]
    async fn empty_memory_content_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let err = create_memory(
            State(state),
            Json(CreateMemoryRequest {
                project_id: Some("-proj".into()),
                content: "   ".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect_err("empty content must 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
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

    // ---------- C2: scope-aware injection ----------

    /// Helper: run a recall and return the response.
    async fn do_recall(state: &DaemonState, body: serde_json::Value) -> RecallResponse {
        let Json(rc) = recall(
            State(state.clone()),
            Json(serde_json::from_value(body).unwrap()),
        )
        .await
        .expect("recall");
        rc
    }

    /// C2: an `always` rule injects on EVERY recall, even one with no semantic
    /// match. The stub embedder returns all-zeros, so cosine is 0.0 for every
    /// memory — an `auto` memory would NOT clear a floor, but the rule must.
    #[tokio::test]
    async fn always_rule_injected_without_semantic_match() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        // An always-scoped rule.
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Always run cargo fmt before committing.".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("always".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create rule");

        // A plain auto memory that should NOT inject (score 0.0 < floor 0.6).
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "An ordinary auto fact.".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create auto");

        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "something totally unrelated to the rule",
                "project": project,
                "min_score": 0.6,
                "source": "mcp"
            }),
        )
        .await;

        let always: Vec<&RecallMemory> =
            rc.memories.iter().filter(|m| m.scope == "always").collect();
        assert_eq!(always.len(), 1, "the rule must inject");
        assert!(always[0].content.contains("cargo fmt"));
        // The auto memory did NOT clear the score floor.
        assert!(
            rc.memories.iter().all(|m| m.scope != "auto"),
            "auto memory must not inject below the floor: {:?}",
            rc.memories
                .iter()
                .map(|m| (&m.scope, &m.content))
                .collect::<Vec<_>>()
        );
    }

    /// C2: a `glob` memory injects ONLY when a candidate path matches; an
    /// unrelated path leaves it out. Candidate paths come from the request's
    /// `paths` field and from path-like tokens in the query.
    #[tokio::test]
    async fn glob_memory_injects_only_on_matching_path() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Rust files use 4-space indent and rustfmt.".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("glob".into()),
                globs: Some(vec!["**/*.rs".into()]),
                global: false,
            }),
        )
        .await
        .expect("create glob");

        // Matching path via explicit `paths`.
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "how should I format this",
                "project": project,
                "paths": ["src/foo.rs"],
                "source": "mcp"
            }),
        )
        .await;
        assert_eq!(
            rc.memories.iter().filter(|m| m.scope == "glob").count(),
            1,
            "glob memory should inject for src/foo.rs"
        );

        // Matching path harvested from the query text.
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "please look at src/bar.rs for the issue",
                "project": project,
                "source": "mcp"
            }),
        )
        .await;
        assert_eq!(
            rc.memories.iter().filter(|m| m.scope == "glob").count(),
            1,
            "glob memory should inject for a query-mentioned .rs path"
        );

        // Non-matching path → NOT injected.
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "edit the README.md file",
                "project": project,
                "paths": ["docs/README.md"],
                "source": "mcp"
            }),
        )
        .await;
        assert_eq!(
            rc.memories.iter().filter(|m| m.scope == "glob").count(),
            0,
            "glob memory must NOT inject for README.md"
        );
    }

    /// C2: a `manual` memory is never auto-injected by recall, but IS returned
    /// by list_memories.
    #[tokio::test]
    async fn manual_memory_excluded_from_recall_but_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        let Json(created) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "A manual note the model can pull explicitly.".into(),
                source: None,
                pinned: true, // even pinned, manual must not auto-inject
                scope: Some("manual".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create manual");

        // recall: not injected.
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "manual note model pull explicitly",
                "project": project,
                "paths": ["x.rs"],
                "source": "mcp"
            }),
        )
        .await;
        assert!(
            rc.memories.is_empty(),
            "manual memory must not be auto-injected: {:?}",
            rc.memories.iter().map(|m| &m.scope).collect::<Vec<_>>()
        );

        // list: present, with scope=manual.
        let Json(listed) = list_memories_handler(
            State(state.clone()),
            Query(MemoriesQuery {
                project: project.clone(),
                limit: 50,
                scope: None,
            }),
        )
        .await
        .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].scope, "manual");

        // scope filter narrows to manual.
        let Json(manual_only) = list_memories_handler(
            State(state.clone()),
            Query(MemoriesQuery {
                project,
                limit: 50,
                scope: Some("manual".into()),
            }),
        )
        .await
        .expect("list manual");
        assert_eq!(manual_only.len(), 1);
    }

    /// C2: project-scoped `always` rules inject BEFORE `__global__` ones
    /// (project > global precedence).
    #[tokio::test]
    async fn project_rules_precede_global_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        // A global rule (via global:true) …
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: None,
                content: "GLOBAL rule".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("always".into()),
                globs: None,
                global: true,
            }),
        )
        .await
        .expect("create global rule");
        // … and a project rule.
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "PROJECT rule".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("always".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create project rule");

        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "anything",
                "project": project,
                "source": "mcp"
            }),
        )
        .await;
        let rules: Vec<&str> = rc
            .memories
            .iter()
            .filter(|m| m.scope == "always")
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            rules,
            vec!["PROJECT rule", "GLOBAL rule"],
            "project rule must precede the global rule"
        );
        // The global rule was authored under the reserved project id.
        assert!(rc
            .memories
            .iter()
            .any(|m| m.project == ck_store::GLOBAL_PROJECT && m.content == "GLOBAL rule"));
    }

    /// C2 hardening: the explicit `paths` array is bounded (MAX_RECALL_PATHS),
    /// so a recall packing thousands of paths can't force glob matching across
    /// the whole set. Paths beyond the cap are dropped: a lone matching `.rs`
    /// path placed AFTER 128 non-matching paths must NOT trigger the glob.
    #[tokio::test]
    async fn recall_paths_are_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Rust files use rustfmt.".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("glob".into()),
                globs: Some(vec!["**/*.rs".into()]),
                global: false,
            }),
        )
        .await
        .expect("create glob");

        // 128 non-matching paths, then the only matching one — dropped by the cap.
        let mut paths: Vec<String> = (0..128).map(|i| format!("d/n{i}.txt")).collect();
        paths.push("src/match.rs".into());
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "format",   // no path tokens harvested from this
                "project": project,
                "paths": paths,
                "source": "mcp"
            }),
        )
        .await;
        assert_eq!(
            rc.memories.iter().filter(|m| m.scope == "glob").count(),
            0,
            "a matching path beyond the cap must be ignored"
        );

        // The same match within the cap still injects (cap doesn't break globbing).
        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "format",
                "project": project,
                "paths": ["src/match.rs"],
                "source": "mcp"
            }),
        )
        .await;
        assert_eq!(
            rc.memories.iter().filter(|m| m.scope == "glob").count(),
            1,
            "a matching path within the cap injects"
        );
    }

    /// C2: scope=glob with an empty/absent globs array is a 400.
    #[tokio::test]
    async fn glob_scope_without_globs_is_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);

        // Absent globs.
        let err = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some("-proj".into()),
                content: "x".into(),
                source: None,
                pinned: false,
                scope: Some("glob".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect_err("glob with no globs must 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        // Empty (all-blank) globs.
        let err = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some("-proj".into()),
                content: "x".into(),
                source: None,
                pinned: false,
                scope: Some("glob".into()),
                globs: Some(vec!["  ".into()]),
                global: false,
            }),
        )
        .await
        .expect_err("glob with blank globs must 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        // Invalid scope value → 400.
        let err = create_memory(
            State(state),
            Json(CreateMemoryRequest {
                project_id: Some("-proj".into()),
                content: "x".into(),
                source: None,
                pinned: false,
                scope: Some("bogus".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect_err("invalid scope must 400");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn path_tokens_extracted_from_query() {
        let toks = extract_path_tokens("please look at src/bar.rs and config.toml, not foo");
        assert!(toks.contains(&"src/bar.rs".to_string()));
        assert!(toks.contains(&"config.toml".to_string()));
        assert!(!toks.iter().any(|t| t == "foo")); // no slash, no ext
        assert!(!toks.iter().any(|t| t == "please"));
        // Trailing sentence punctuation is stripped.
        let toks = extract_path_tokens("edit src/main.rs.");
        assert!(toks.contains(&"src/main.rs".to_string()));
    }

    // ---------- C4: recall provenance (`why`) ----------

    /// C4: an `always` rule's injected memory carries a provenance `why`
    /// naming its tier; a `glob` memory names the matched pattern.
    #[tokio::test]
    async fn provenance_why_for_always_rule_and_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Always run cargo fmt before committing.".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("always".into()),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create rule");
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Rust files use rustfmt.".into(),
                source: Some("user".into()),
                pinned: false,
                scope: Some("glob".into()),
                globs: Some(vec!["**/*.rs".into()]),
                global: false,
            }),
        )
        .await
        .expect("create glob");

        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "format this",
                "project": project,
                "paths": ["src/foo.rs"],
                "source": "mcp"
            }),
        )
        .await;

        let rule = rc
            .memories
            .iter()
            .find(|m| m.scope == "always")
            .expect("always rule injected");
        assert!(
            rule.why.contains("standing rule (always)"),
            "always why should name the tier: {:?}",
            rule.why
        );
        let glob = rc
            .memories
            .iter()
            .find(|m| m.scope == "glob")
            .expect("glob memory injected");
        assert!(
            glob.why.contains("matches glob") && glob.why.contains("**/*.rs"),
            "glob why should name the matched pattern: {:?}",
            glob.why
        );
    }

    /// C4: an `auto` memory injected by semantic match carries a `why` that
    /// reports the semantic score; every returned CHUNK also carries a non-empty
    /// `why` beginning with "semantic".
    #[tokio::test]
    async fn provenance_why_for_semantic_memory_and_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let project = "-proj".to_string();

        // Index a chunk so the recall returns at least one item.
        let session = SessionId("s1".into());
        let chunk = Chunk {
            id: ck_core::ChunkId("s1:0:0".into()),
            session_id: session.clone(),
            project_id: ck_core::ProjectId(project.clone()),
            turn_index: 0,
            role: ck_core::ChunkRole::Assistant,
            kind: ck_core::ChunkKind::AssistantText,
            text: "We chose duckdb for the analytics schema and column store.".into(),
            token_count: 12,
            start_uuid: "u1".into(),
            end_uuid: "u1".into(),
            started_at: chrono::Utc::now(),
            tool_name: None,
            tool_input_preview: None,
            embedding_ref: None,
        };
        {
            let s = Session {
                id: session.clone(),
                project_id: ck_core::ProjectId(project.clone()),
                is_sidechain: false,
                parent_session_id: None,
                agent_meta: None,
                source_file: "/tmp/x.jsonl".into(),
                source_file_mtime_ms: 0,
                source_file_sha256: "d".into(),
                content_hash: "d".into(),
                first_prompt: Some("hi".into()),
                ai_title: Some("Analytics store".into()),
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
                message_count: 2,
                model_usage: vec![],
                git_branch: None,
                cwd: None,
                summary: None,
                chunk_ids: vec![chunk.id.clone()],
                topic_ids: vec![],
            };
            ck_store::write_session(&state.layout, &s).unwrap();
            ck_store::write_chunk(&state.layout, &chunk).unwrap();
            let mut meta = state.meta.lock().unwrap();
            meta.upsert_session(&s).unwrap();
            meta.upsert_chunks(std::slice::from_ref(&chunk)).unwrap();
        }
        {
            let mut store = state.vector.write().unwrap();
            store
                .upsert_chunks(std::slice::from_ref(&chunk), &[vec![0.0; 4]])
                .unwrap();
        }

        // A plain auto memory (will inject at floor 0.0 with the stub embedder).
        let _ = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.clone()),
                content: "Analytics uses a column store.".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create auto");

        let rc = do_recall(
            &state,
            serde_json::json!({
                "query": "duckdb schema",
                "project": project,
                "source": "mcp"
            }),
        )
        .await;

        // Chunk provenance.
        assert!(!rc.items.is_empty(), "expected at least one chunk");
        for it in &rc.items {
            assert!(
                it.why.starts_with("semantic"),
                "chunk why must start with 'semantic': {:?}",
                it.why
            );
        }
        // The "duckdb" / "schema" keywords should be reflected on the matching chunk.
        assert!(
            rc.items.iter().any(|it| it.why.contains("keywords:")
                && (it.why.contains("duckdb") || it.why.contains("schema"))),
            "a chunk why should list keyword hits: {:?}",
            rc.items.iter().map(|i| &i.why).collect::<Vec<_>>()
        );

        // Auto memory provenance.
        let auto = rc
            .memories
            .iter()
            .find(|m| m.scope == "auto")
            .expect("auto memory injected");
        assert!(
            auto.why.starts_with("semantic match"),
            "auto why should report the semantic match: {:?}",
            auto.why
        );
    }

    /// C4: the recall metrics ring records a recall and is readable.
    #[tokio::test]
    async fn recall_metrics_ring_records() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let _ = do_recall(
            &state,
            serde_json::json!({"query": "anything at all", "source": "mcp"}),
        )
        .await;
        let Json(metrics) = recall_metrics().await;
        assert!(
            metrics.iter().any(|m| m.source == "mcp"),
            "ring should hold the mcp recall"
        );
    }

    // ---------- C4: opt-in local-caller auth ----------

    use http_body_util::BodyExt;
    use tower::ServiceExt; // oneshot

    /// Serializes tests that mutate the process-global `CK_REQUIRE_TOKEN` env
    /// var so they don't race under the default parallel test runner. An
    /// async-aware mutex so the guard can be held across the `.await`s that
    /// drive the router while the env var is set.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Build the assembled router (with the auth + cookie middleware) over a
    /// fresh temp state. The token file is generated as a side effect; we read
    /// it back for the Bearer/cookie cases.
    fn router_with_token(tmp: &tempfile::TempDir) -> (Router, String) {
        let state = test_state(tmp);
        let token = ck_store::local_token(&state.layout).unwrap();
        (router(state), token)
    }

    async fn status_of(router: Router, req: axum::http::Request<axum::body::Body>) -> StatusCode {
        router.oneshot(req).await.unwrap().status()
    }

    /// C4: with enforcement OFF (the default), a /v1 request WITHOUT a token
    /// succeeds — the default behavior is unchanged.
    #[tokio::test]
    async fn auth_off_by_default_no_token_succeeds() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("CK_REQUIRE_TOKEN");
        let tmp = tempfile::tempdir().unwrap();
        let (router, _tok) = router_with_token(&tmp);
        let req = axum::http::Request::builder()
            .uri("/v1/projects")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::OK);
    }

    /// C4: with enforcement ON (via CK_REQUIRE_TOKEN=1): a /v1 request without a
    /// token is 401; with `Authorization: Bearer <tok>` it's 200; with a
    /// `ck_token` cookie it's 200; and GET /v1/health stays open without a token.
    #[tokio::test]
    async fn auth_on_requires_token_except_health() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("CK_REQUIRE_TOKEN", "1");
        let tmp = tempfile::tempdir().unwrap();
        let (router, tok) = router_with_token(&tmp);

        // No token → 401.
        let req = axum::http::Request::builder()
            .uri("/v1/projects")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            status_of(router.clone(), req).await,
            StatusCode::UNAUTHORIZED
        );

        // Bearer → 200.
        let req = axum::http::Request::builder()
            .uri("/v1/projects")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router.clone(), req).await, StatusCode::OK);

        // Cookie → 200.
        let req = axum::http::Request::builder()
            .uri("/v1/projects")
            .header(axum::http::header::COOKIE, format!("ck_token={tok}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router.clone(), req).await, StatusCode::OK);

        // Wrong token → 401.
        let req = axum::http::Request::builder()
            .uri("/v1/projects")
            .header(axum::http::header::AUTHORIZATION, "Bearer deadbeef")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            status_of(router.clone(), req).await,
            StatusCode::UNAUTHORIZED
        );

        // HEAD /v1/health also stays open (health probes commonly use HEAD).
        let req = axum::http::Request::builder()
            .method(axum::http::Method::HEAD)
            .uri("/v1/health")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router.clone(), req).await, StatusCode::OK);

        // The WebSocket upgrade route is gated too: no token → 401 (the auth
        // middleware rejects the GET upgrade request before ws_upgrade runs).
        let req = axum::http::Request::builder()
            .uri("/v1/ws")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            status_of(router.clone(), req).await,
            StatusCode::UNAUTHORIZED
        );

        // GET /v1/health stays open WITHOUT a token.
        let req = axum::http::Request::builder()
            .uri("/v1/health")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(status_of(router, req).await, StatusCode::OK);

        std::env::remove_var("CK_REQUIRE_TOKEN");
    }

    /// C4: the auth path uses the constant-time comparator (an exact-match
    /// token passes; a token differing only in the last byte fails). This is a
    /// direct check of the comparator the middleware calls.
    #[tokio::test]
    async fn auth_uses_constant_time_compare() {
        let tok = "a".repeat(64);
        assert!(ck_store::constant_time_eq(tok.as_bytes(), tok.as_bytes()));
        let mut wrong = tok.clone().into_bytes();
        wrong[63] = b'b';
        assert!(!ck_store::constant_time_eq(tok.as_bytes(), &wrong));
    }

    /// C4: when enforcing, the SPA HTML navigation gets a `Set-Cookie:
    /// ck_token=…; HttpOnly; SameSite=Strict`. We exercise the `spa_cookie`
    /// response layer by serving an index.html via the fallback service.
    #[tokio::test]
    async fn spa_html_sets_token_cookie_when_enforcing() {
        let _guard = ENV_LOCK.lock().await;
        std::env::set_var("CK_REQUIRE_TOKEN", "1");
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);
        let token = ck_store::local_token(&state.layout).unwrap();

        // Stand up a dist dir with an index.html and point CK_WEB_DIST at it.
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("index.html"), "<!doctype html><title>ck</title>").unwrap();
        std::env::set_var("CK_WEB_DIST", &dist);
        let router = router(state);
        std::env::remove_var("CK_WEB_DIST");

        // A navigation to "/" falls through to the SPA index.html.
        let req = axum::http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            set_cookie.contains(&format!("ck_token={token}")),
            "SPA HTML must set the ck_token cookie: {set_cookie:?}"
        );
        assert!(set_cookie.contains("HttpOnly"), "cookie must be HttpOnly");
        assert!(
            set_cookie.contains("SameSite=Strict"),
            "cookie must be SameSite=Strict"
        );
        // Drain the body so the response is fully consumed (tidy).
        let _ = resp.into_body().collect().await;
        std::env::remove_var("CK_REQUIRE_TOKEN");
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

    // ---------- C5: conflict resolution / staleness ----------

    /// Content-keyed embedder for the C5 dedupe tests. The default StubEmbedder
    /// returns all-zeros (cosine 0 everywhere), which can't drive similarity.
    /// This maps a text to a 4-d unit vector chosen by a keyword so we get
    /// *controllable* cosine: every "build" sentence embeds to +x (so they're
    /// near-identical to each other) and every "deploy" sentence to +y (so it's
    /// orthogonal — clearly distinct). Anything else lands on a third axis.
    struct TopicEmbedder;
    impl Embedder for TopicEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn model_name(&self) -> &str {
            "topic-stub"
        }
        fn embed_batch(&self, texts: &[String]) -> ck_embed::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let lc = t.to_ascii_lowercase();
                    if lc.contains("build") {
                        vec![1.0, 0.0, 0.0, 0.0]
                    } else if lc.contains("deploy") {
                        vec![0.0, 1.0, 0.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0, 0.0]
                    }
                })
                .collect())
        }
    }

    fn topic_state(tmp: &tempfile::TempDir) -> DaemonState {
        let layout = ck_store::Layout::new_at(tmp.path().join("root"));
        layout.ensure().expect("layout ensure");
        let vector = ck_vector::VectorStore::connect(&layout, 4).expect("vector connect");
        let meta = ck_store::MetaIndex::open(&layout).expect("meta open");
        DaemonState::new(
            layout,
            tmp.path().join("projects"),
            Arc::new(TopicEmbedder),
            vector,
            meta,
        )
    }

    async fn remember(
        state: &DaemonState,
        project: &str,
        content: &str,
        scope: Option<&str>,
    ) -> Memory {
        let Json(m) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.to_string()),
                content: content.to_string(),
                source: None,
                pinned: false,
                scope: scope.map(str::to_string),
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create_memory");
        m
    }

    async fn list_all(state: &DaemonState, project: &str) -> Vec<Memory> {
        let Json(v) = list_memories_handler(
            State(state.clone()),
            Query(MemoriesQuery {
                project: project.to_string(),
                limit: 500,
                scope: None,
            }),
        )
        .await
        .expect("list");
        v
    }

    /// HEADLINE (part A): two near-identical remembers collapse to ONE row whose
    /// content is the LATEST (supersede-on-write), with `updated_at` bumped and
    /// the original id preserved. A third, clearly-distinct fact inserts as a
    /// SECOND row. Default config (no LLM, no key): reconcile is off.
    #[tokio::test]
    async fn supersede_on_write_collapses_near_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);
        let project = "-proj";

        let first = remember(&state, project, "the build command is cargo build", None).await;
        // Same topic, rephrased / updated value → supersedes in place.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = remember(
            &state,
            project,
            "the build command is now cargo build --release",
            None,
        )
        .await;

        // ONE row, same id, latest content, updated_at bumped.
        assert_eq!(second.id, first.id, "supersede keeps the original id");
        let rows = list_all(&state, project).await;
        assert_eq!(
            rows.len(),
            1,
            "near-duplicate must not accumulate: {rows:?}"
        );
        assert_eq!(
            rows[0].content,
            "the build command is now cargo build --release"
        );
        assert!(
            rows[0].updated_at > first.updated_at,
            "updated_at must be bumped on supersede (was {}, now {})",
            first.updated_at,
            rows[0].updated_at
        );

        // A clearly-distinct fact (orthogonal embedding) inserts as a new row.
        let third = remember(&state, project, "the deploy target is fly.io", None).await;
        assert_ne!(third.id, first.id);
        let rows = list_all(&state, project).await;
        assert_eq!(rows.len(), 2, "distinct facts stay separate: {rows:?}");
    }

    /// Part A: a PINNED memory (a user-asserted durable fact) is NEVER an
    /// auto-supersede target — a near-identical new memory inserts fresh
    /// instead of silently overwriting it.
    #[tokio::test]
    async fn pinned_memory_is_not_superseded_on_write() {
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);
        let project = "-proj";

        let Json(pinned) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: Some(project.to_string()),
                content: "the build command is cargo build".into(),
                source: Some("user".into()),
                pinned: true,
                scope: None,
                globs: None,
                global: false,
            }),
        )
        .await
        .expect("create pinned");

        // A near-identical rephrasing that WOULD supersede an unpinned memory.
        let near = remember(
            &state,
            project,
            "the build command is now cargo build --release",
            None,
        )
        .await;

        assert_ne!(near.id, pinned.id, "must not overwrite the pinned memory");
        let rows = list_all(&state, project).await;
        assert_eq!(rows.len(), 2, "pinned original + new row: {rows:?}");
        let original = rows.iter().find(|m| m.id == pinned.id).unwrap();
        assert_eq!(original.content, "the build command is cargo build");
        assert!(original.pinned);
    }

    /// Part A: supersede is SCOPE-aware — an `always` rule must not dedupe
    /// against an `auto` memory even when their embeddings are identical.
    #[tokio::test]
    async fn supersede_is_scope_aware() {
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);
        let project = "-proj";

        // Same embedding (both "build"), different scopes → two rows, not one.
        let _auto = remember(&state, project, "build uses cargo", Some("auto")).await;
        let _rule = remember(
            &state,
            project,
            "always run cargo build first",
            Some("always"),
        )
        .await;

        let rows = list_all(&state, project).await;
        assert_eq!(rows.len(), 2, "different scopes must not merge: {rows:?}");
        let scopes: std::collections::HashSet<&str> =
            rows.iter().map(|m| m.scope.as_str()).collect();
        assert!(scopes.contains("auto") && scopes.contains("always"));

        // A second always-rule on the same topic DOES supersede the first rule.
        let rows_before = rows.len();
        let _rule2 = remember(
            &state,
            project,
            "always run cargo build --release first",
            Some("always"),
        )
        .await;
        let rows = list_all(&state, project).await;
        assert_eq!(
            rows.len(),
            rows_before,
            "same-scope near-dup supersedes (no new row): {rows:?}"
        );
    }

    /// Part A: supersede is PROJECT-scoped — an identical fact in another
    /// project (and in `__global__`) is never merged into this project's row.
    #[tokio::test]
    async fn supersede_is_project_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);

        let a = remember(&state, "-proj-a", "build uses cargo", None).await;
        let b = remember(&state, "-proj-b", "build uses cargo", None).await;
        // global:true stores under __global__ — also a distinct bucket.
        let Json(g) = create_memory(
            State(state.clone()),
            Json(CreateMemoryRequest {
                project_id: None,
                content: "build uses cargo".into(),
                source: None,
                pinned: false,
                scope: None,
                globs: None,
                global: true,
            }),
        )
        .await
        .expect("create global");

        assert_ne!(a.id, b.id);
        assert_ne!(a.id, g.id);
        assert_eq!(list_all(&state, "-proj-a").await.len(), 1);
        assert_eq!(list_all(&state, "-proj-b").await.len(), 1);
        assert_eq!(list_all(&state, ck_store::GLOBAL_PROJECT).await.len(), 1);
    }

    /// Part C is OFF by default: a fresh create on a clean DB with NO key and NO
    /// orchestrator reachable succeeds via the part-A local path (reconcile flag
    /// defaults false, so the LLM path is never taken). Proves the no-key moat.
    #[tokio::test]
    async fn reconcile_off_by_default_write_succeeds_without_key() {
        // Ensure neither the env override nor a key is set for this test.
        std::env::remove_var("CK_MEMORY_RECONCILE");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);
        // Sanity: the default config has reconcile off.
        assert!(!state.config.read().unwrap().memory_reconcile);
        assert!(!ck_store::memory_reconcile_enabled(
            &state.config.read().unwrap()
        ));

        // A write succeeds (would hang/err if it tried to reach an LLM).
        let m = remember(&state, "-proj", "the build command is cargo build", None).await;
        assert_eq!(m.content, "the build command is cargo build");
        assert_eq!(list_all(&state, "-proj").await.len(), 1);
    }

    /// Part B: a NEWER memory breaks a near-tie in the auto recall tier — the
    /// latest fact on a topic is injected first (staleness). Both memories share
    /// the SAME embedding (cosine tie); only `updated_at` differs.
    #[tokio::test]
    async fn recency_breaks_near_tie_in_auto_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let state = topic_state(&tmp);
        let project = "-proj";

        // Insert two auto memories with identical embeddings but different
        // updated_at directly via the store (bypassing supersede, which would
        // otherwise collapse them — here we deliberately want both rows to test
        // recall ordering). Same emb → cosine tie; newer must rank first.
        let emb = vec![1.0f32, 0.0, 0.0, 0.0];
        {
            let meta = state.meta.lock().unwrap();
            let older = Memory {
                id: "older".into(),
                project_id: project.into(),
                content: "build fact OLD".into(),
                source: "agent".into(),
                pinned: false,
                scope: "auto".into(),
                globs: None,
                created_at: 1_000,
                updated_at: 1_000,
            };
            let newer = Memory {
                id: "newer".into(),
                project_id: project.into(),
                content: "build fact NEW".into(),
                source: "agent".into(),
                pinned: false,
                scope: "auto".into(),
                globs: None,
                created_at: 2_000,
                updated_at: 2_000,
            };
            meta.insert_memory(&older, &emb).unwrap();
            meta.insert_memory(&newer, &emb).unwrap();
        }

        let Json(rc) = recall(
            State(state.clone()),
            Json(
                serde_json::from_value(serde_json::json!({
                    "query": "what is the build fact",
                    "project": project,
                    "source": "mcp"
                }))
                .unwrap(),
            ),
        )
        .await
        .expect("recall");

        let mem_ids: Vec<&str> = rc.memories.iter().map(|m| m.id.as_str()).collect();
        assert!(
            mem_ids.contains(&"newer") && mem_ids.contains(&"older"),
            "both memories injected: {mem_ids:?}"
        );
        let pos_new = mem_ids.iter().position(|&id| id == "newer").unwrap();
        let pos_old = mem_ids.iter().position(|&id| id == "older").unwrap();
        assert!(
            pos_new < pos_old,
            "newer memory must rank ahead of the older near-tie: {mem_ids:?}"
        );
    }
}
