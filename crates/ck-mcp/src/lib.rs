//! Hand-rolled MCP stdio shim.
//!
//! MCP is a small JSON-RPC 2.0 protocol. We implement just what Claude Code
//! needs to use our tools: `initialize`, `tools/list`, `tools/call`. Each
//! tool call is forwarded to the daemon's HTTP API at `127.0.0.1:7421`.
//!
//! Error UX: if the daemon isn't running, every tool call returns a result
//! whose `isError` is true and `content` explains that the user should run
//! `ck daemon`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, warn};

pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7421";
pub const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone)]
pub struct McpConfig {
    pub daemon_url: String,
    pub server_name: String,
    pub server_version: String,
    /// C4: the daemon's local auth token, sent as `Authorization: Bearer` on
    /// every forwarded request. Read best-effort from the data-dir token file
    /// (same `Layout::local_token_path` the daemon writes). Harmless when the
    /// daemon isn't enforcing (the header is simply ignored); required once the
    /// operator sets `require_token`. `None` when the file can't be read — the
    /// shim still works against a non-enforcing daemon.
    pub auth_token: Option<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            daemon_url: DEFAULT_DAEMON_URL.to_string(),
            server_name: "context-keeper".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            auth_token: resolve_local_token(),
        }
    }
}

/// Best-effort read of the daemon's local auth token from the default data
/// dir. Never panics; `None` on any error (no home dir, file absent, etc.) —
/// the shim then sends no Authorization header, which is correct against a
/// non-enforcing daemon and yields a clean 401 against an enforcing one.
fn resolve_local_token() -> Option<String> {
    let root = ck_store::Layout::default_root().ok()?;
    let layout = ck_store::Layout::new_at(root);
    ck_store::local_token(&layout).ok()
}

/// Attach `Authorization: Bearer <token>` to a request when a token is known.
/// A no-op when `auth_token` is `None`.
fn with_auth(rb: reqwest::RequestBuilder, config: &McpConfig) -> reqwest::RequestBuilder {
    match &config.auth_token {
        Some(tok) => rb.bearer_auth(tok),
        None => rb,
    }
}

/// Run the MCP loop. Reads JSON-RPC newline-delimited from stdin, writes
/// responses to stdout. Returns when stdin closes.
pub async fn run(config: McpConfig) -> anyhow::Result<()> {
    let http = Arc::new(reqwest::Client::builder().build()?);
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = tokio::io::stdout();
    let stdout = Arc::new(tokio::sync::Mutex::new(stdout));

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, line = %line, "invalid JSON-RPC line; skipping");
                continue;
            }
        };
        let id = req.id.clone();
        let resp = handle(&config, http.clone(), req).await;
        let envelope = match resp {
            Ok(result) => Response::success(id, result),
            Err(err) => Response::error(id, err.code, err.message, err.data),
        };
        let payload = serde_json::to_string(&envelope)? + "\n";
        let mut out = stdout.lock().await;
        out.write_all(payload.as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

// ---------- protocol types ----------

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

impl Response {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }
    fn error(id: Option<Value>, code: i32, message: String, data: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message,
                data,
            }),
        }
    }
}

#[derive(Debug)]
struct McpError {
    code: i32,
    message: String,
    data: Option<Value>,
}

impl McpError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }
    fn unknown_tool(name: &str) -> Self {
        Self {
            code: -32602,
            message: format!("unknown tool: {name}"),
            data: None,
        }
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
            data: None,
        }
    }
}

// ---------- dispatch ----------

async fn handle(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    req: Request,
) -> Result<Value, McpError> {
    debug!(method = %req.method, "mcp request");
    match req.method.as_str() {
        "initialize" => Ok(initialize_result(config)),
        "tools/list" => Ok(tools_list()),
        "tools/call" => tools_call(config, http, req.params).await,
        // Standard MCP methods we acknowledge but don't yet support.
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        m => Err(McpError::method_not_found(m)),
    }
}

fn initialize_result(config: &McpConfig) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": config.server_name,
            "version": config.server_version,
        }
    })
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "recall",
                "description": "Search across all indexed Claude Code chunks. Returns the top-K \
                                most relevant chunks with provenance (session id, title, score). \
                                Use this to pull only the past conversation context relevant to \
                                the current task instead of re-reading whole sessions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query":   {"type": "string", "description": "Natural-language query."},
                        "limit":   {"type": "integer", "default": 10, "minimum": 1, "maximum": 50},
                        "project": {"type": "string", "description": "Optional project id (e.g. -Users-me-Development)."}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "list_sessions",
                "description": "List recent indexed sessions, most recent first. Optionally \
                                filter by project. Returns id, title, message_count, chunk_count.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string"},
                        "limit":   {"type": "integer", "default": 20, "minimum": 1, "maximum": 200}
                    }
                }
            },
            {
                "name": "list_projects",
                "description": "List indexed projects with session counts and last-seen timestamps.",
                "inputSchema": {"type": "object", "properties": {}}
            },
            {
                "name": "list_unsummarized_sessions",
                "description": "Sessions that have no AI summary yet (most recent first). Use this \
                                to find sessions worth summarizing, then get_session_transcript + \
                                save_session_summary. YOU are the summarizer — no API key involved.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string"},
                        "limit":   {"type": "integer", "default": 20, "minimum": 1, "maximum": 100}
                    }
                }
            },
            {
                "name": "get_session_transcript",
                "description": "Full transcript text of one session (role-prefixed turns), for \
                                reading or summarizing. Long sessions are truncated to max_chars.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "max_chars":  {"type": "integer", "default": 60000, "minimum": 1000, "maximum": 400000}
                    },
                    "required": ["session_id"]
                }
            },
            {
                "name": "save_session_summary",
                "description": "Persist a summary you wrote for a session. Shows up in the UI and \
                                future recalls immediately. Keep text ≤4000 chars; bullets/decisions/\
                                artifacts ≤16 items each.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "text":       {"type": "string", "description": "2-4 sentence narrative summary."},
                        "bullets":    {"type": "array", "items": {"type": "string"}},
                        "decisions":  {"type": "array", "items": {"type": "string"}},
                        "artifacts":  {"type": "array", "items": {"type": "string"}, "description": "Files/outputs touched."}
                    },
                    "required": ["session_id", "text"]
                }
            },
            {
                "name": "list_topics",
                "description": "All topic clusters with their current labels and sizes. Topics with \
                                raw centroid-text labels are good candidates for name_topic.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": {"type": "string"}
                    }
                }
            },
            {
                "name": "name_topic",
                "description": "Give a topic cluster a concise human name (3-6 words, specific not \
                                generic) and optional one-line description. Survives reindexing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic_id":    {"type": "string"},
                        "label":       {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["topic_id", "label"]
                }
            },
            {
                "name": "remember",
                "description": "Save a durable fact to long-term project memory. Use this to \
                                persist a decision, constraint, gotcha, convention, or preference \
                                that should be recalled in FUTURE sessions — not just this one. \
                                Memories are searched semantically (local embeddings, no API key) \
                                and auto-injected into recall. Keep each one to a single specific \
                                sentence. \
                                SCOPE controls WHEN a memory is injected: 'auto' (default) = by \
                                semantic relevance; 'always' = a standing RULE injected on every \
                                recall (use for a coding standard the user wants enforced); \
                                'glob' = injected only when a file matching `globs` is in play; \
                                'manual' = stored but never auto-injected. Set global:true (or \
                                project_id:\"__global__\") to apply the memory across ALL projects. \
                                Returns the created memory (with its id, needed for \
                                update_memory / forget).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content":    {"type": "string", "description": "One durable fact, a single specific sentence."},
                        "project_id": {"type": "string", "description": "Project id (e.g. -Users-me-Development). If omitted, the most-recently-active project is used. Use \"__global__\" (or global:true) to apply everywhere."},
                        "source":     {"type": "string", "enum": ["agent", "user"], "default": "agent", "description": "Who originated this fact: 'agent' (you) or 'user' (an explicit user instruction)."},
                        "pinned":     {"type": "boolean", "default": false, "description": "Pin to always rank ahead of unpinned memories and bypass the recall score floor."},
                        "scope":      {"type": "string", "enum": ["auto", "always", "glob", "manual"], "default": "auto", "description": "When to inject: auto=by relevance; always=a standing rule injected every recall; glob=only when a matching file path is present (requires globs); manual=never auto-injected."},
                        "globs":      {"type": "array", "items": {"type": "string"}, "description": "Glob patterns (e.g. [\"**/*.rs\"]). REQUIRED and non-empty when scope=glob; ignored otherwise."},
                        "global":     {"type": "boolean", "default": false, "description": "Store under the reserved __global__ project so it applies across every project. Equivalent to project_id:\"__global__\"."}
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "update_memory",
                "description": "Revise an existing memory by id (from remember/list_memories). \
                                Change its content (re-embedded automatically), pinned state, \
                                scope, and/or globs. Returns the updated memory. 404s if the id \
                                is unknown.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id":      {"type": "string", "description": "Memory id."},
                        "content": {"type": "string", "description": "New content (optional)."},
                        "pinned":  {"type": "boolean", "description": "New pinned state (optional)."},
                        "scope":   {"type": "string", "enum": ["auto", "always", "glob", "manual"], "description": "New injection scope (optional)."},
                        "globs":   {"type": "array", "items": {"type": "string"}, "description": "New glob patterns (optional; required non-empty if the resulting scope is glob)."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "forget",
                "description": "Permanently delete a memory by id (from remember/list_memories). \
                                Use when a fact is wrong, stale, or superseded. 404s if unknown.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory id to delete."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "list_memories",
                "description": "List durable memories for a project, pinned first then most \
                                recently updated. Use to review what's stored before adding or \
                                editing — and to surface 'manual'-scoped memories, which never \
                                auto-inject. Returns id, content, source, pinned, scope, globs, \
                                timestamps. Pass project_id:\"__global__\" to list cross-project \
                                rules/notes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project_id": {"type": "string", "description": "Project id. If omitted, the most-recently-active project is used. Use \"__global__\" for cross-project entries."},
                        "scope":      {"type": "string", "enum": ["auto", "always", "glob", "manual"], "description": "Filter to one scope (optional)."},
                        "limit":      {"type": "integer", "default": 50, "minimum": 1, "maximum": 500}
                    }
                }
            },
            {
                "name": "help",
                "description": "Show usage info for all context-keeper MCP tools, with example \
                                arguments. Call this first when you're unsure which tool to use.",
                "inputSchema": {"type": "object", "properties": {}}
            }
        ]
    })
}

const HELP_TEXT: &str = r#"context-keeper MCP tools
========================

recall — semantic search across indexed Claude Code sessions
  args:
    query    (string, required)  natural-language query
    limit    (int, 1..=50, default 10)
    project  (string, optional)  e.g. "-Users-me-Development"
  example:
    {"query": "token budgets in MCP", "limit": 5}

list_sessions — recent indexed sessions, newest first
  args:
    project  (string, optional)
    limit    (int, 1..=200, default 20)
  example:
    {"project": "-private-tmp", "limit": 10}

list_projects — every indexed project with session counts and last-seen times
  args: none
  example:
    {}

Summarize & name (YOU are the model — no API key needed)
--------------------------------------------------------
list_unsummarized_sessions — sessions with no AI summary yet, newest first
  args: project (optional), limit (int, default 20)

get_session_transcript — role-prefixed transcript text for one session
  args: session_id (required), max_chars (default 60000)

save_session_summary — persist a summary you wrote; appears in the UI instantly
  args: session_id, text (2-4 sentences), bullets[], decisions[], artifacts[]

list_topics — topic clusters with current labels (raw labels = rename candidates)
  args: project (optional)

name_topic — give a topic a concise 3-6 word name (+ optional description)
  args: topic_id, label, description (optional)

Durable memory (writable, queryable — local embeddings, no API key)
-------------------------------------------------------------------
remember — save a durable fact to long-term project memory (auto-injected
  into future recall). Keep it to one specific sentence.
  args: content (required), project_id (optional → most-recent project),
        source ("agent"|"user", default "agent"), pinned (bool, default false),
        scope ("auto"|"always"|"glob"|"manual", default "auto"),
        globs (string[], required when scope="glob"),
        global (bool, default false → store under __global__, applies everywhere)
  scope decides WHEN a memory injects:
    auto   — by semantic relevance (the default).
    always — a standing RULE: injected on EVERY recall (a coding standard).
    glob   — injected only when a file matching `globs` is in play
             (e.g. globs:["**/*.rs"] fires when src/foo.rs is mentioned).
    manual — stored but never auto-injected; surfaced only via list_memories.
  examples:
    {"content": "Recall packs results to a 4000-token budget by default."}
    {"content": "Always run cargo fmt before committing.", "scope": "always"}
    {"content": "Rust files: 4-space indent.", "scope": "glob", "globs": ["**/*.rs"]}
    {"content": "Prefer the local embedder.", "scope": "always", "global": true}

list_memories — list a project's memories, pinned first then most recent.
  Also the way to surface manual-scoped memories (they never auto-inject).
  args: project_id (optional; "__global__" for cross-project), scope (optional
        filter), limit (int, default 50)

update_memory — revise a memory's content (re-embedded), pinned, scope, globs
  args: id (required), content (optional), pinned (optional),
        scope (optional), globs (optional)

forget — permanently delete a memory by id
  args: id (required)

A good flow when the user asks "summarize my recent sessions":
  1. list_unsummarized_sessions {"limit": 5}
  2. for each: get_session_transcript → write the summary → save_session_summary
  3. list_topics → name_topic for any raw-looking labels

help — this message
  args: none

Notes
-----
- All tools require the ck daemon at 127.0.0.1:7421. If you see
  "daemon is not reachable", run `ck daemon` in another terminal.
- recall packs results to a token budget (defaults: token_budget=4000,
  mmr_lambda=0.6) — pass `limit` to cap chunks before packing.
"#;

fn call_help() -> Value {
    json!({
        "content": [{"type": "text", "text": HELP_TEXT}],
        "isError": false
    })
}

async fn tools_call(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    params: Value,
) -> Result<Value, McpError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::internal("tools/call missing name"))?
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name.as_str() {
        "recall" => call_recall(config, http, args).await,
        "list_sessions" => call_list_sessions(config, http, args).await,
        "list_projects" => call_list_projects(config, http).await,
        "list_unsummarized_sessions" => call_list_unsummarized(config, http, args).await,
        "get_session_transcript" => call_get_transcript(config, http, args).await,
        "save_session_summary" => call_save_summary(config, http, args).await,
        "list_topics" => call_list_topics(config, http, args).await,
        "name_topic" => call_name_topic(config, http, args).await,
        "remember" => call_remember(config, http, args).await,
        "update_memory" => call_update_memory(config, http, args).await,
        "forget" => call_forget(config, http, args).await,
        "list_memories" => call_list_memories(config, http, args).await,
        "help" => Ok(call_help()),
        other => Err(McpError::unknown_tool(other)),
    }
}

// ---------- tool impls ----------

async fn call_recall(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let url = format!("{}/v1/recall", config.daemon_url);
    // Tag the request `source: "mcp"` so the daemon's hot-chunk count
    // distinguishes intentional MCP recalls from ambient hook recalls.
    // Caller-supplied `source` (rare) wins; default sets it to "mcp".
    let mut tagged = args.clone();
    if let Some(obj) = tagged.as_object_mut() {
        obj.entry("source".to_string())
            .or_insert(Value::String("mcp".into()));
    }
    let resp = match with_auth(http.post(&url), config)
        .json(&tagged)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        // Also surface as structured content for clients that want it.
        "structuredContent": payload
    }))
}

async fn call_list_sessions(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let mut url = format!("{}/v1/sessions?", config.daemon_url);
    if let Some(p) = args.get("project").and_then(|v| v.as_str()) {
        url.push_str(&format!("project={}&", urlencode(p)));
    }
    if let Some(l) = args.get("limit").and_then(|v| v.as_u64()) {
        url.push_str(&format!("limit={l}&"));
    }
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        "structuredContent": {"sessions": payload}
    }))
}

async fn call_list_projects(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
) -> Result<Value, McpError> {
    let url = format!("{}/v1/projects", config.daemon_url);
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        "structuredContent": {"projects": payload}
    }))
}

async fn call_list_unsummarized(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let mut url = format!("{}/v1/sessions/unsummarized?", config.daemon_url);
    if let Some(p) = args.get("project").and_then(|v| v.as_str()) {
        url.push_str(&format!("project={}&", urlencode(p)));
    }
    if let Some(l) = args.get("limit").and_then(|v| v.as_u64()) {
        url.push_str(&format!("limit={l}&"));
    }
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        "structuredContent": {"sessions": payload}
    }))
}

async fn call_get_transcript(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) else {
        return Ok(tool_error("get_session_transcript requires session_id"));
    };
    let max_chars = args
        .get("max_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(60_000)
        .clamp(1_000, 400_000) as usize;
    let url = format!(
        "{}/v1/sessions/{}/transcript",
        config.daemon_url,
        urlencode(sid)
    );
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let entries: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let mut text = String::new();
    let mut truncated = false;
    if let Some(arr) = entries.as_array() {
        for e in arr {
            let role = e.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            let tool = e
                .get("tool_name")
                .and_then(|v| v.as_str())
                .map(|t| format!(" [{t}]"))
                .unwrap_or_default();
            let body = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let line = format!("{role}{tool}: {body}\n\n");
            if text.len() + line.len() > max_chars {
                truncated = true;
                break;
            }
            text.push_str(&line);
        }
    }
    if truncated {
        text.push_str("[…transcript truncated at max_chars…]");
    }
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "isError": false
    }))
}

async fn call_save_summary(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(sid) = args.get("session_id").and_then(|v| v.as_str()) else {
        return Ok(tool_error("save_session_summary requires session_id"));
    };
    if args.get("text").and_then(|v| v.as_str()).is_none() {
        return Ok(tool_error("save_session_summary requires text"));
    }
    let mut body = args.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("session_id");
        obj.insert(
            "generated_by".into(),
            Value::String("claude-via-mcp".into()),
        );
    }
    let url = format!(
        "{}/v1/sessions/{}/summary",
        config.daemon_url,
        urlencode(sid)
    );
    let resp = match with_auth(http.put(&url), config).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    Ok(json!({
        "content": [{"type": "text", "text": format!("Summary saved for session {sid}.")}],
        "isError": false
    }))
}

async fn call_list_topics(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let mut url = format!("{}/v1/graph?include_sessions=false&", config.daemon_url);
    if let Some(p) = args.get("project").and_then(|v| v.as_str()) {
        url.push_str(&format!("project={}&", urlencode(p)));
    }
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    // Project the graph nodes down to just the topics (id without the t: prefix).
    let topics: Vec<Value> = payload
        .get("nodes")
        .and_then(|n| n.as_array())
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| n.get("kind").and_then(|k| k.as_str()) == Some("topic"))
                .map(|n| {
                    json!({
                        "topic_id": n.get("id").and_then(|v| v.as_str())
                            .map(|s| s.trim_start_matches("t:")).unwrap_or(""),
                        "label": n.get("label").cloned().unwrap_or(Value::Null),
                        "description": n.get("description").cloned().unwrap_or(Value::Null),
                        "size": n.get("size").cloned().unwrap_or(Value::Null),
                        "project_ids": n.get("project_ids").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let pretty = serde_json::to_string_pretty(&topics).unwrap_or_default();
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        "structuredContent": {"topics": topics}
    }))
}

async fn call_name_topic(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(tid) = args.get("topic_id").and_then(|v| v.as_str()) else {
        return Ok(tool_error("name_topic requires topic_id"));
    };
    let Some(label) = args.get("label").and_then(|v| v.as_str()) else {
        return Ok(tool_error("name_topic requires label"));
    };
    let mut body = json!({ "label": label });
    if let Some(d) = args.get("description").and_then(|v| v.as_str()) {
        body["description"] = Value::String(d.to_string());
    }
    let url = format!("{}/v1/topics/{}/name", config.daemon_url, urlencode(tid));
    let resp = match with_auth(http.put(&url), config).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    Ok(json!({
        "content": [{"type": "text", "text": format!("Topic {tid} renamed to \"{label}\".")}],
        "isError": false
    }))
}

// ---------- C1: writable memory tools ----------

/// Resolve the most-recently-active project id via `/v1/projects` (sorted by
/// last_seen). Returns None when the daemon is unreachable or has no projects.
/// Used when a memory tool is called without an explicit `project_id`.
async fn resolve_default_project(config: &McpConfig, http: &reqwest::Client) -> Option<String> {
    let url = format!("{}/v1/projects", config.daemon_url);
    let resp = with_auth(http.get(&url), config).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let projects: Value = resp.json().await.ok()?;
    let arr = projects.as_array()?;
    // /v1/projects returns {id, sessions, last_seen}. Pick the max last_seen.
    arr.iter()
        .filter_map(|p| {
            let id = p.get("id").and_then(|v| v.as_str())?;
            let last = p
                .get("last_seen")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some((last, id.to_string()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, id)| id)
}

async fn call_remember(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
        return Ok(tool_error("remember requires content"));
    };
    let is_global = args
        .get("global")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || args.get("project_id").and_then(|v| v.as_str()) == Some("__global__");
    // Resolve the project: global flag/sentinel wins; else explicit arg; else
    // the most-recently-active project.
    let project_id =
        if is_global {
            "__global__".to_string()
        } else {
            match args.get("project_id").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => match resolve_default_project(config, &http).await {
                    Some(p) => p,
                    None => return Ok(tool_error(
                        "remember: no project_id given and no active project could be resolved. \
                         Pass project_id explicitly (e.g. from list_projects), or set global:true.",
                    )),
                },
            }
        };
    let mut body = json!({ "content": content, "project_id": project_id });
    if let Some(s) = args.get("source").and_then(|v| v.as_str()) {
        body["source"] = Value::String(s.to_string());
    }
    if let Some(p) = args.get("pinned").and_then(|v| v.as_bool()) {
        body["pinned"] = Value::Bool(p);
    }
    if let Some(s) = args.get("scope").and_then(|v| v.as_str()) {
        body["scope"] = Value::String(s.to_string());
    }
    if let Some(g) = args.get("globs") {
        if g.is_array() {
            body["globs"] = g.clone();
        }
    }
    let url = format!("{}/v1/memories", config.daemon_url);
    let resp = match with_auth(http.post(&url), config).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    Ok(json!({
        "content": [{"type": "text", "text": format!("Remembered (id {id}).")}],
        "isError": false,
        "structuredContent": payload
    }))
}

async fn call_update_memory(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return Ok(tool_error("update_memory requires id"));
    };
    let mut body = json!({});
    if let Some(c) = args.get("content").and_then(|v| v.as_str()) {
        body["content"] = Value::String(c.to_string());
    }
    if let Some(p) = args.get("pinned").and_then(|v| v.as_bool()) {
        body["pinned"] = Value::Bool(p);
    }
    if let Some(s) = args.get("scope").and_then(|v| v.as_str()) {
        body["scope"] = Value::String(s.to_string());
    }
    if let Some(g) = args.get("globs") {
        if g.is_array() {
            body["globs"] = g.clone();
        }
    }
    let url = format!("{}/v1/memories/{}", config.daemon_url, urlencode(id));
    let resp = match with_auth(http.put(&url), config).json(&body).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    Ok(json!({
        "content": [{"type": "text", "text": format!("Updated memory {id}.")}],
        "isError": false,
        "structuredContent": payload
    }))
}

async fn call_forget(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    let Some(id) = args.get("id").and_then(|v| v.as_str()) else {
        return Ok(tool_error("forget requires id"));
    };
    let url = format!("{}/v1/memories/{}", config.daemon_url, urlencode(id));
    let resp = match with_auth(http.delete(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    Ok(json!({
        "content": [{"type": "text", "text": format!("Forgot memory {id}.")}],
        "isError": false
    }))
}

async fn call_list_memories(
    config: &McpConfig,
    http: Arc<reqwest::Client>,
    args: Value,
) -> Result<Value, McpError> {
    // Resolve project: explicit arg wins, else most-recently-active.
    let project_id =
        match args.get("project_id").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => match resolve_default_project(config, &http).await {
                Some(p) => p,
                None => return Ok(tool_error(
                    "list_memories: no project_id given and no active project could be resolved. \
                     Pass project_id explicitly (e.g. from list_projects).",
                )),
            },
        };
    let mut url = format!(
        "{}/v1/memories?project={}",
        config.daemon_url,
        urlencode(&project_id)
    );
    if let Some(l) = args.get("limit").and_then(|v| v.as_u64()) {
        url.push_str(&format!("&limit={l}"));
    }
    if let Some(s) = args.get("scope").and_then(|v| v.as_str()) {
        url.push_str(&format!("&scope={}", urlencode(s)));
    }
    let resp = match with_auth(http.get(&url), config).send().await {
        Ok(r) => r,
        Err(e) => return Ok(daemon_unreachable(&e.to_string())),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Ok(tool_error(format!("daemon returned {status}: {body}")));
    }
    let payload: Value = resp
        .json()
        .await
        .map_err(|e| McpError::internal(e.to_string()))?;
    let pretty = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    Ok(json!({
        "content": [{"type": "text", "text": pretty}],
        "isError": false,
        "structuredContent": {"memories": payload}
    }))
}

fn daemon_unreachable(reason: &str) -> Value {
    error!(reason, "daemon unreachable");
    tool_error(format!(
        "context-keeper daemon is not reachable at 127.0.0.1:7421. \
         Run `ck daemon` in another terminal first. (network error: {reason})"
    ))
}

fn tool_error(msg: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": msg.into()}],
        "isError": true
    })
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_advertises_all_tools() {
        let v = tools_list();
        let arr = v["tools"].as_array().unwrap();
        let names: Vec<&str> = arr.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "recall",
                "list_sessions",
                "list_projects",
                "list_unsummarized_sessions",
                "get_session_transcript",
                "save_session_summary",
                "list_topics",
                "name_topic",
                "remember",
                "update_memory",
                "forget",
                "list_memories",
                "help"
            ]
        );
    }

    #[test]
    fn help_tool_returns_non_error_with_text_content() {
        let v = call_help();
        assert_eq!(v["isError"], false);
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("recall"));
        assert!(text.contains("list_sessions"));
        assert!(text.contains("list_projects"));
    }

    #[test]
    fn initialize_advertises_protocol_and_tools_capability() {
        let v = initialize_result(&McpConfig::default());
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["capabilities"]["tools"].is_object());
        assert_eq!(v["serverInfo"]["name"], "context-keeper");
    }

    #[test]
    fn urlencode_handles_dashes_and_unicode() {
        assert_eq!(urlencode("-Users-me-Development"), "-Users-me-Development");
        assert_eq!(urlencode("hello world"), "hello%20world");
    }

    /// The shim's contract when the daemon is down: tool calls return a
    /// RESULT with `isError: true` and a human explanation — never a raw
    /// transport error that would surface to the model as a protocol fault.
    #[tokio::test]
    async fn tool_call_with_daemon_down_returns_is_error_result() {
        let config = McpConfig {
            // Port 9 (discard) is never serving HTTP on loopback.
            daemon_url: "http://127.0.0.1:9".to_string(),
            server_name: "context-keeper".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            // Explicit None so the test never reads/creates the real
            // ~/.context-keeper/state/local-token on the test machine.
            auth_token: None,
        };
        let http = Arc::new(reqwest::Client::new());
        for (name, args) in [
            ("recall", serde_json::json!({"query": "anything"})),
            ("list_sessions", serde_json::json!({})),
            ("list_projects", serde_json::json!({})),
        ] {
            let v = tools_call(
                &config,
                http.clone(),
                serde_json::json!({"name": name, "arguments": args}),
            )
            .await
            .expect("daemon-down must yield an isError result, not Err");
            assert_eq!(v["isError"], true, "tool {name} should flag isError");
            let text = v["content"][0]["text"].as_str().unwrap_or_default();
            assert!(
                text.contains("daemon"),
                "tool {name} should explain the daemon is unreachable, got: {text}"
            );
        }
    }
}
