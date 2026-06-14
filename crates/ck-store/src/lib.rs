//! On-disk layout + SQLite metadata index for context-keeper.

use ck_core::{Chunk, ChunkId, Session, SessionId};
use directories::BaseDirs;
use rusqlite::{params, Connection};
use std::{
    fs, io,
    os::unix,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("home directory not found")]
    NoHome,
    #[error("config: {0}")]
    Config(String),
    #[error("invalid id (rejected for path safety): {0:?}")]
    InvalidId(String),
}

/// Reject id strings that could escape the storage root when interpolated into
/// a path. Real session/chunk ids produced by the parser are uuid-ish or
/// `sid:turn:idx` and never contain path separators. Rejecting separators +
/// NUL is *sufficient* for containment: without a separator a string cannot
/// reference a parent directory or an absolute path, so `format!("{id}.json")`
/// and `dir.join(id)` always stay one component below the intended directory.
pub fn ensure_safe_id(s: &str) -> Result<()> {
    let unsafe_id =
        s.is_empty() || s.len() > 256 || s.bytes().any(|b| b == b'/' || b == b'\\' || b == 0);
    if unsafe_id {
        return Err(StoreError::InvalidId(s.chars().take(80).collect()));
    }
    Ok(())
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Owns every path under `~/.context-keeper/`.
#[derive(Debug, Clone)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn default_root() -> Result<PathBuf> {
        let base = BaseDirs::new().ok_or(StoreError::NoHome)?;
        Ok(base.home_dir().join(".context-keeper"))
    }

    pub fn new_at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Open the default-rooted layout, creating directories as needed.
    pub fn open() -> Result<Self> {
        let root = Self::default_root()?;
        let layout = Self::new_at(root);
        layout.ensure()?;
        Ok(layout)
    }

    /// Create every standard subdirectory if it is missing.
    pub fn ensure(&self) -> Result<()> {
        for sub in [
            "sources",
            "derived/sessions",
            "derived/chunks",
            "derived/topics",
            "derived/edges",
            "derived/projects",
            "index",
            "cache/embeddings",
            "cache/llm-summaries",
            "cache/models",
            "runtime",
            "state",
        ] {
            fs::create_dir_all(self.root.join(sub))?;
        }
        let sv = self.root.join("state/schema-version");
        if !sv.exists() {
            fs::write(&sv, b"1\n")?;
        }
        Ok(())
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("derived/sessions")
    }
    pub fn chunks_dir(&self) -> PathBuf {
        self.root.join("derived/chunks")
    }
    pub fn meta_db(&self) -> PathBuf {
        self.root.join("index/meta.sqlite")
    }
    pub fn sources_dir(&self) -> PathBuf {
        self.root.join("sources")
    }
    pub fn cursor_path(&self) -> PathBuf {
        self.root.join("state/cursor.json")
    }
    pub fn vectors_bin_path(&self) -> PathBuf {
        self.root.join("index/vectors.bin")
    }
    pub fn embeddings_cache_dir(&self, model: &str) -> PathBuf {
        self.root
            .join("cache/embeddings")
            .join(safe_model_dir(model))
    }
    pub fn schema_version_path(&self) -> PathBuf {
        self.root.join("state/schema-version")
    }

    /// C4: the opt-in local-caller auth token (0600). Always generated on
    /// first daemon start (cheap); only *enforced* when `Config::require_token`
    /// (or `CK_REQUIRE_TOKEN=1`) is set. Lives under `state/` next to the other
    /// machine-local bootstrap files.
    pub fn local_token_path(&self) -> PathBuf {
        self.root.join("state/local-token")
    }

    pub fn session_json_path(&self, id: &SessionId) -> PathBuf {
        self.sessions_dir().join(format!("{}.json", id.0))
    }

    pub fn chunk_json_path(&self, session: &SessionId, chunk: &ChunkId) -> PathBuf {
        self.chunks_dir()
            .join(&session.0)
            .join(format!("{}.json", chunk.0))
    }

    /// Symlink `sources/claude-projects` → `target` if it doesn't already exist.
    /// If a symlink with a different target is present we leave it alone and
    /// log a warning — we never overwrite user state silently.
    pub fn ensure_claude_projects_symlink(&self, target: &Path) -> Result<()> {
        let link = self.sources_dir().join("claude-projects");
        if link.is_symlink() || link.exists() {
            if let Ok(existing) = fs::read_link(&link) {
                if existing == target {
                    return Ok(());
                }
            }
            warn!(
                ?link,
                "sources/claude-projects already exists; not replacing"
            );
            return Ok(());
        }
        unix::fs::symlink(target, &link)?;
        info!(?link, ?target, "created sources/claude-projects symlink");
        Ok(())
    }
}

pub fn write_session(layout: &Layout, session: &Session) -> Result<()> {
    let path = layout.session_json_path(&session.id);
    let json = serde_json::to_vec_pretty(session)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(tmp, path)?;
    Ok(())
}

pub fn read_session(layout: &Layout, id: &SessionId) -> Result<Session> {
    ensure_safe_id(&id.0)?;
    let path = layout.session_json_path(id);
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn write_chunk(layout: &Layout, chunk: &Chunk) -> Result<()> {
    let path = layout.chunk_json_path(&chunk.session_id, &chunk.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(chunk)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json)?;
    fs::rename(tmp, path)?;
    Ok(())
}

pub fn read_chunk(layout: &Layout, session: &SessionId, chunk: &ChunkId) -> Result<Chunk> {
    ensure_safe_id(&session.0)?;
    ensure_safe_id(&chunk.0)?;
    let path = layout.chunk_json_path(session, chunk);
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Delete chunk JSON files for `session` that are NOT in `keep_ids`. Called on
/// re-index so a session that was already indexed leaves no orphan chunk file
/// on disk — important because a redacted re-chunk can shift chunk boundaries,
/// and an un-pruned orphan would keep the pre-redaction (secret-bearing) text
/// on disk even though nothing references it. Returns the removed chunk ids.
pub fn prune_session_chunk_files(
    layout: &Layout,
    session: &SessionId,
    keep_ids: &std::collections::HashSet<String>,
) -> Result<Vec<String>> {
    ensure_safe_id(&session.0)?;
    let dir = layout.chunks_dir().join(&session.0);
    let mut removed = Vec::new();
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Ok(removed), // no dir yet → nothing to prune
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !keep_ids.contains(stem) && fs::remove_file(&path).is_ok() {
            removed.push(stem.to_string());
        }
    }
    Ok(removed)
}

/// Just the `text` field — cheaper than parsing the whole chunk when all we
/// need for `ck search` is the snippet.
pub fn read_chunk_text(layout: &Layout, session: &SessionId, chunk: &ChunkId) -> Result<String> {
    let c = read_chunk(layout, session, chunk)?;
    Ok(c.text)
}

fn safe_model_dir(model: &str) -> String {
    model.replace(['/', '\\', ':'], "_")
}

// ---------- SQLite metadata index ----------

#[derive(Debug, Clone)]
pub struct HotChunk {
    pub chunk_id: String,
    pub session_id: String,
    pub distinct_sessions: u32,
}

#[derive(Debug, Clone)]
pub struct PromotionState {
    pub project_id: String,
    pub target_path: String,
    pub content_hash: String,
    pub promoted_at: String,
}

/// Reserved `project_id` meaning "applies to every project". Used by C2 so a
/// rule (`scope:"always"`) or a glob memory can be authored once and recalled
/// across all projects. It contains no path separators, so `ensure_safe_id`
/// accepts it like any real (sanitized) project id.
pub const GLOBAL_PROJECT: &str = "__global__";

/// The four injection scopes a memory can carry (C2). The string form is what
/// lives in the `scope` column and crosses the API/MCP boundary.
/// - `auto`   — eligible for semantic-match injection (C1's original behavior).
/// - `always` — a user-authored RULE: injected on every recall, highest
///   precedence, score-floor-free, but strictly capped.
/// - `glob`   — injected only when a candidate file path matches one of `globs`.
/// - `manual` — never auto-injected; surfaced only via list/get.
pub const MEMORY_SCOPES: &[&str] = &["auto", "always", "glob", "manual"];

/// True when `s` is one of the four valid [`MEMORY_SCOPES`].
pub fn is_valid_scope(s: &str) -> bool {
    MEMORY_SCOPES.contains(&s)
}

/// A durable distilled fact in the writable memory store (C1). The embedding
/// is kept out of this struct (held as a `Vec<f32>` only at the in/out
/// boundary) so callers that just want metadata don't pay for the 1.5 KiB
/// vector. `source` is one of `agent` | `user` | `distilled`.
///
/// C2 added `scope` + `globs`: `scope` decides WHEN the memory is injected
/// (see [`MEMORY_SCOPES`]); `globs` is the glob set for `scope == "glob"`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub id: String,
    pub project_id: String,
    pub content: String,
    pub source: String,
    pub pinned: bool,
    /// When this memory is eligible for injection. One of [`MEMORY_SCOPES`];
    /// existing pre-C2 rows default to `"auto"`.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// For `scope == "glob"`: the glob patterns (e.g. `["**/*.rs"]`) a candidate
    /// path must match for this memory to inject. `None`/empty for other scopes.
    #[serde(default)]
    pub globs: Option<Vec<String>>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
}

fn default_scope() -> String {
    "auto".into()
}

/// Serialize a 384-dim (or any length) f32 vector to a little-endian byte
/// BLOB — identical layout to the ck-embed disk cache, so the two are
/// interchangeable on disk.
fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    bytes
}

/// Inverse of [`embedding_to_blob`]. Returns `None` if the blob length is not
/// a multiple of 4 (corrupt / wrong format).
fn blob_to_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Some(out)
}

/// Cosine similarity between two equal-length vectors. Returns 0.0 when the
/// lengths differ or either vector has zero magnitude (so a degenerate or
/// mismatched embedding never out-ranks a real match).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

pub struct MetaIndex {
    conn: Connection,
}

impl MetaIndex {
    pub fn open(layout: &Layout) -> Result<Self> {
        let conn = Connection::open(layout.meta_db())?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                is_sidechain INTEGER NOT NULL,
                parent_session_id TEXT,
                source_file TEXT NOT NULL,
                source_file_mtime_ms INTEGER NOT NULL,
                source_file_sha256 TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                first_prompt TEXT,
                ai_title TEXT,
                started_at TEXT,
                ended_at TEXT,
                message_count INTEGER NOT NULL,
                cwd TEXT,
                git_branch TEXT,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_tokens INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);

            -- M2: chunks table. Additive migration; safe to re-run.
            CREATE TABLE IF NOT EXISTS chunks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                turn_index INTEGER NOT NULL,
                role TEXT NOT NULL,
                kind TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                start_uuid TEXT NOT NULL,
                end_uuid TEXT NOT NULL,
                started_at TEXT NOT NULL,
                tool_name TEXT,
                embedding_model TEXT,
                embedding_sha256 TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_chunks_session ON chunks(session_id);
            CREATE INDEX IF NOT EXISTS idx_chunks_project ON chunks(project_id);

            -- M6.x: recall hit tracking. One row per returned chunk per
            -- recall call. `source` distinguishes "mcp" / "cli" / "http"
            -- (count toward "hot") from "hook" (tracked but excluded
            -- from promotion thresholds — the auto-recall hook fires on
            -- every prompt and would over-weight ambient curiosity hits).
            CREATE TABLE IF NOT EXISTS recall_hits (
                chunk_id   TEXT NOT NULL,
                session_id TEXT,
                project_id TEXT NOT NULL,
                source     TEXT NOT NULL,
                hit_at     TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_recall_hits_chunk ON recall_hits(chunk_id);
            CREATE INDEX IF NOT EXISTS idx_recall_hits_at    ON recall_hits(hit_at);

            -- M6.x: promotion-state per project. content_hash is sha256
            -- of the rendered managed-block body. Same hash = no rewrite.
            CREATE TABLE IF NOT EXISTS promotions (
                project_id   TEXT PRIMARY KEY,
                target_path  TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                promoted_at  TEXT NOT NULL
            );

            -- C1: writable, queryable distilled-memory store. Durable facts the
            -- agent (or the opt-in distiller) can add/update/delete; searched
            -- semantically via the LOCAL embedder and injected into recall.
            -- `embedding` is 384 little-endian f32 (same byte layout as the
            -- ck-embed cache); NULL is allowed but search skips NULL rows.
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                content TEXT NOT NULL,
                source TEXT NOT NULL,            -- 'agent' | 'user' | 'distilled'
                pinned INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,     -- unix seconds
                updated_at INTEGER NOT NULL,
                embedding BLOB,                  -- 384 LE f32; NULL allowed
                -- C2: injection scope + glob patterns. Present in the CREATE for
                -- fresh DBs; back-filled on existing DBs by the ALTER below.
                scope TEXT NOT NULL DEFAULT 'auto', -- auto|always|glob|manual
                globs TEXT                          -- JSON array of glob strings; NULL otherwise
            );
            CREATE INDEX IF NOT EXISTS idx_memories_project ON memories(project_id);
            "#,
        )?;
        // C2 migration: a DB created before C2 has the `memories` table without
        // the `scope`/`globs` columns (CREATE IF NOT EXISTS won't add them). Add
        // them now; an "duplicate column name" error means a fresh DB already
        // has them (the CREATE above ran with the new shape), which is benign.
        for stmt in [
            "ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'auto'",
            "ALTER TABLE memories ADD COLUMN globs TEXT",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }
        // bump schema-version sentinel to 5 (C2 added scope/globs to `memories`).
        // Write-only sentinel: nothing reindexes on it (verified in C1).
        if let Some(parent) = layout.schema_version_path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(layout.schema_version_path(), b"5\n")?;
        Ok(Self { conn })
    }

    pub fn upsert_chunks(&mut self, chunks: &[Chunk]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO chunks (
                    id, session_id, project_id, turn_index, role, kind,
                    token_count, start_uuid, end_uuid, started_at, tool_name,
                    embedding_model, embedding_sha256
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(id) DO UPDATE SET
                    session_id = excluded.session_id,
                    project_id = excluded.project_id,
                    turn_index = excluded.turn_index,
                    role = excluded.role,
                    kind = excluded.kind,
                    token_count = excluded.token_count,
                    start_uuid = excluded.start_uuid,
                    end_uuid = excluded.end_uuid,
                    started_at = excluded.started_at,
                    tool_name = excluded.tool_name,
                    embedding_model = excluded.embedding_model,
                    embedding_sha256 = excluded.embedding_sha256
                "#,
            )?;
            for c in chunks {
                let role = serde_json::to_string(&c.role)
                    .map(|s| s.trim_matches('"').to_string())
                    .unwrap_or_else(|_| "unknown".into());
                let kind = serde_json::to_string(&c.kind)
                    .map(|s| s.trim_matches('"').to_string())
                    .unwrap_or_else(|_| "unknown".into());
                let (emb_model, emb_sha) = c
                    .embedding_ref
                    .as_ref()
                    .map(|e| (Some(e.model.clone()), Some(e.sha256.clone())))
                    .unwrap_or((None, None));
                stmt.execute(params![
                    c.id.0,
                    c.session_id.0,
                    c.project_id.0,
                    c.turn_index,
                    role,
                    kind,
                    c.token_count,
                    c.start_uuid,
                    c.end_uuid,
                    c.started_at.to_rfc3339(),
                    c.tool_name,
                    emb_model,
                    emb_sha,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete chunk rows for `session_id` whose id is not in `keep_ids`.
    /// Keeps the metadata index consistent with the on-disk chunks after a
    /// re-index that produced a different chunk set.
    pub fn prune_session_chunks(
        &mut self,
        session_id: &str,
        keep_ids: &std::collections::HashSet<String>,
    ) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut removed = 0usize;
        {
            let mut sel = tx.prepare("SELECT id FROM chunks WHERE session_id = ?1")?;
            let ids: Vec<String> = sel
                .query_map([session_id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .filter(|id| !keep_ids.contains(id))
                .collect();
            let mut del = tx.prepare("DELETE FROM chunks WHERE id = ?1")?;
            for id in &ids {
                del.execute([id])?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    pub fn count_chunks(&self) -> Result<u32> {
        let n: u32 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok(n)
    }

    pub fn count_chunks_for_project(&self, project_id: &str) -> Result<u32> {
        let n: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Sum of `token_count` across all indexed chunks. Used to report
    /// recall-time corpus size in tokens (the denominator for compression
    /// stats). COALESCE handles the empty-table case (SUM returns NULL).
    pub fn total_chunk_tokens(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM chunks",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn total_chunk_tokens_for_project(&self, project_id: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM chunks WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Append one row per returned chunk for a recall call. `source`
    /// is one of "mcp" | "cli" | "http" | "hook"; the hot-chunk query
    /// excludes "hook" since the auto-recall hook fires on every prompt.
    pub fn record_recall_hits(
        &mut self,
        items: &[(String, String)],
        session_id: Option<&str>,
        source: &str,
        at: &str,
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO recall_hits (chunk_id, session_id, project_id, source, hit_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (chunk_id, project_id) in items {
                stmt.execute(params![chunk_id, session_id, project_id, source, at])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Distinct-session hit count per chunk_id, restricted to the given
    /// project, excluding hook-sourced hits, since the cutoff timestamp.
    /// Returns rows where the count is >= `min_distinct`.
    pub fn hot_chunks_in_project(
        &self,
        project_id: &str,
        since: &str,
        min_distinct: u32,
    ) -> Result<Vec<HotChunk>> {
        // JOIN chunks to surface the owning session_id (needed to locate
        // the chunk JSON on disk via Layout::chunk_path).
        let mut stmt = self.conn.prepare(
            "SELECT rh.chunk_id, c.session_id, COUNT(DISTINCT rh.session_id) AS n
             FROM recall_hits rh
             JOIN chunks c ON c.id = rh.chunk_id
             WHERE rh.project_id = ?1
               AND rh.source <> 'hook'
               AND rh.hit_at >= ?2
             GROUP BY rh.chunk_id, c.session_id
             HAVING n >= ?3
             ORDER BY n DESC, rh.chunk_id ASC",
        )?;
        let rows = stmt.query_map(params![project_id, since, min_distinct], |r| {
            Ok(HotChunk {
                chunk_id: r.get::<_, String>(0)?,
                session_id: r.get::<_, String>(1)?,
                distinct_sessions: r.get::<_, u32>(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Returns any cwd recorded for a session in this project, preferring
    /// the most recent one. Used to locate the project's CLAUDE.md.
    pub fn project_cwd(&self, project_id: &str) -> Result<Option<String>> {
        let cwd: Option<String> = self
            .conn
            .query_row(
                "SELECT cwd FROM sessions
                 WHERE project_id = ?1 AND cwd IS NOT NULL AND cwd <> ''
                 ORDER BY started_at DESC LIMIT 1",
                params![project_id],
                |r| r.get(0),
            )
            .ok();
        Ok(cwd)
    }

    pub fn get_promotion_state(&self, project_id: &str) -> Result<Option<PromotionState>> {
        let res = self.conn.query_row(
            "SELECT target_path, content_hash, promoted_at FROM promotions WHERE project_id = ?1",
            params![project_id],
            |r| {
                Ok(PromotionState {
                    project_id: project_id.to_string(),
                    target_path: r.get(0)?,
                    content_hash: r.get(1)?,
                    promoted_at: r.get(2)?,
                })
            },
        );
        match res {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn upsert_promotion_state(&self, s: &PromotionState) -> Result<()> {
        self.conn.execute(
            "INSERT INTO promotions (project_id, target_path, content_hash, promoted_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_id) DO UPDATE SET
                target_path  = excluded.target_path,
                content_hash = excluded.content_hash,
                promoted_at  = excluded.promoted_at",
            params![s.project_id, s.target_path, s.content_hash, s.promoted_at],
        )?;
        Ok(())
    }

    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let total_input: u64 = s.model_usage.iter().map(|u| u.input_tokens).sum();
        let total_output: u64 = s.model_usage.iter().map(|u| u.output_tokens).sum();
        let total_cache_read: u64 = s.model_usage.iter().map(|u| u.cache_read_tokens).sum();
        let total_cache_create: u64 = s.model_usage.iter().map(|u| u.cache_creation_tokens).sum();

        self.conn.execute(
            r#"
            INSERT INTO sessions (
                id, project_id, is_sidechain, parent_session_id,
                source_file, source_file_mtime_ms, source_file_sha256, content_hash,
                first_prompt, ai_title, started_at, ended_at, message_count,
                cwd, git_branch,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
            ON CONFLICT(id) DO UPDATE SET
                project_id = excluded.project_id,
                is_sidechain = excluded.is_sidechain,
                parent_session_id = excluded.parent_session_id,
                source_file = excluded.source_file,
                source_file_mtime_ms = excluded.source_file_mtime_ms,
                source_file_sha256 = excluded.source_file_sha256,
                content_hash = excluded.content_hash,
                first_prompt = excluded.first_prompt,
                ai_title = excluded.ai_title,
                started_at = excluded.started_at,
                ended_at = excluded.ended_at,
                message_count = excluded.message_count,
                cwd = excluded.cwd,
                git_branch = excluded.git_branch,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_creation_tokens = excluded.cache_creation_tokens
            "#,
            params![
                s.id.0,
                s.project_id.0,
                s.is_sidechain as i64,
                s.parent_session_id.as_ref().map(|p| p.0.clone()),
                s.source_file,
                s.source_file_mtime_ms,
                s.source_file_sha256,
                s.content_hash,
                s.first_prompt,
                s.ai_title,
                s.started_at.to_rfc3339(),
                s.ended_at.to_rfc3339(),
                s.message_count,
                s.cwd,
                s.git_branch,
                total_input as i64,
                total_output as i64,
                total_cache_read as i64,
                total_cache_create as i64,
            ],
        )?;
        Ok(())
    }

    pub fn count_sessions(&self) -> Result<u32> {
        let n: u32 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
        Ok(n)
    }

    // ---------- C1: writable memory store ----------

    /// Generate a fresh memory id (uuid v4). Path-safe by construction
    /// (hex + hyphens only) so `ensure_safe_id` always accepts it.
    pub fn new_memory_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Insert a new memory with its local embedding. `m.id` is the primary
    /// key; callers normally mint it via [`Self::new_memory_id`]. `m.scope`
    /// and `m.globs` are persisted as-is (the API/MCP layer validates them).
    pub fn insert_memory(&self, m: &Memory, embedding: &[f32]) -> Result<()> {
        let blob = embedding_to_blob(embedding);
        let globs_json = globs_to_json(m.globs.as_deref())?;
        self.conn.execute(
            "INSERT INTO memories
                (id, project_id, content, source, pinned, created_at, updated_at, embedding, scope, globs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.id,
                m.project_id,
                m.content,
                m.source,
                m.pinned as i64,
                m.created_at,
                m.updated_at,
                blob,
                m.scope,
                globs_json,
            ],
        )?;
        Ok(())
    }

    pub fn get_memory(&self, id: &str) -> Result<Option<Memory>> {
        let res = self.conn.query_row(
            "SELECT id, project_id, content, source, pinned, created_at, updated_at, scope, globs
             FROM memories WHERE id = ?1",
            params![id],
            row_to_memory,
        );
        match res {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Update content and/or pinned state and/or the embedding and/or the
    /// scope/globs. Always bumps `updated_at`. Returns `false` when no row
    /// matched the id (so callers can map that to a 404). Passing all-`None`
    /// still bumps `updated_at` (a no-op "touch"), but in practice content is
    /// re-embedded when present.
    ///
    /// `globs` uses a nested `Option`: outer `None` = leave the column alone,
    /// `Some(None)` = clear it to NULL, `Some(Some(v))` = set it to `v`.
    #[allow(clippy::too_many_arguments)]
    pub fn update_memory(
        &self,
        id: &str,
        content: Option<&str>,
        pinned: Option<bool>,
        new_embedding: Option<&[f32]>,
        scope: Option<&str>,
        globs: Option<Option<&[String]>>,
    ) -> Result<bool> {
        // Build the SET clause dynamically so we only touch the columns the
        // caller asked to change. updated_at is always set.
        let now = chrono::Utc::now().timestamp();
        let mut sets: Vec<&str> = vec!["updated_at = ?"];
        let mut vals: Vec<rusqlite::types::Value> = vec![now.into()];
        if let Some(c) = content {
            sets.push("content = ?");
            vals.push(c.to_string().into());
        }
        if let Some(p) = pinned {
            sets.push("pinned = ?");
            vals.push((p as i64).into());
        }
        if let Some(e) = new_embedding {
            sets.push("embedding = ?");
            vals.push(embedding_to_blob(e).into());
        }
        if let Some(s) = scope {
            sets.push("scope = ?");
            vals.push(s.to_string().into());
        }
        if let Some(g) = globs {
            sets.push("globs = ?");
            vals.push(match globs_to_json(g)? {
                Some(json) => json.into(),
                None => rusqlite::types::Value::Null,
            });
        }
        let sql = format!("UPDATE memories SET {} WHERE id = ?", sets.join(", "));
        vals.push(id.to_string().into());
        let n = self
            .conn
            .execute(&sql, rusqlite::params_from_iter(vals.iter()))?;
        Ok(n > 0)
    }

    /// Delete a memory by id. Returns `false` when no row matched.
    pub fn delete_memory(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// List a project's memories, pinned first then most-recently-updated.
    /// When `scope` is `Some`, only rows with that scope are returned.
    pub fn list_memories(
        &self,
        project_id: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let mut out = Vec::new();
        match scope {
            Some(sc) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, project_id, content, source, pinned, created_at, updated_at, scope, globs
                     FROM memories WHERE project_id = ?1 AND scope = ?2
                     ORDER BY pinned DESC, updated_at DESC
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![project_id, sc, limit as i64], row_to_memory)?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, project_id, content, source, pinned, created_at, updated_at, scope, globs
                     FROM memories WHERE project_id = ?1
                     ORDER BY pinned DESC, updated_at DESC
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![project_id, limit as i64], row_to_memory)?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// All `always`-scoped memories (rules) for a project, pinned first then
    /// most-recently-updated. Used by the recall handler to inject standing
    /// standards regardless of semantic score. `limit` caps the result.
    pub fn always_memories(&self, project_id: &str, limit: usize) -> Result<Vec<Memory>> {
        self.list_memories(project_id, Some("always"), limit)
    }

    /// All `glob`-scoped memories for a project (a small set in practice); the
    /// recall handler filters these in-process against candidate paths using
    /// the parsed glob set, so we return them whole rather than push glob
    /// matching into SQL.
    pub fn glob_memories(&self, project_id: &str, limit: usize) -> Result<Vec<Memory>> {
        self.list_memories(project_id, Some("glob"), limit)
    }

    /// Semantic search over a project's `auto`-scoped memories: load every
    /// `auto` row that has an embedding, cosine-rank against `query`, return
    /// the top `limit` with scores. Project memory counts are small, so an O(n)
    /// in-process scan is fine — we deliberately do NOT touch the big flat
    /// vector store here.
    ///
    /// C2: only `scope = 'auto'` rows participate. `always` rules and `glob`
    /// memories inject through dedicated paths in the recall handler, and
    /// `manual` memories never auto-inject. This keeps the semantic pass
    /// identical to C1's behavior for the (default) auto scope.
    pub fn search_memories(
        &self,
        project_id: &str,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, content, source, pinned, created_at, updated_at, scope, globs, embedding
             FROM memories WHERE project_id = ?1 AND scope = 'auto' AND embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![project_id], |r| {
            let m = row_to_memory(r)?;
            let blob: Vec<u8> = r.get(9)?;
            Ok((m, blob))
        })?;
        let mut scored: Vec<(Memory, f32)> = Vec::new();
        for r in rows {
            let (m, blob) = r?;
            if let Some(emb) = blob_to_embedding(&blob) {
                let score = cosine_similarity(query, &emb);
                scored.push((m, score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// C5: nearest existing memory to `embedding` within the SAME
    /// `(project_id, scope)`, by cosine similarity. Returns the best
    /// `(Memory, score)` or `None` when that project+scope has no rows with an
    /// embedding. This is the local (no-LLM) dedupe/supersede primitive: a
    /// near-identical or rephrased fact on the same topic lands close to its
    /// predecessor, so the caller can UPDATE in place instead of accumulating.
    ///
    /// Scope-aware and project-scoped by construction — an `always` rule only
    /// compares against `always`, and `__global__` rows never mix with a real
    /// project's rows. O(n) over the (small) per-project+scope set, like
    /// [`Self::search_memories`]; deliberately does NOT touch the flat vector
    /// store.
    pub fn nearest_memory_in_scope(
        &self,
        project_id: &str,
        scope: &str,
        embedding: &[f32],
    ) -> Result<Option<(Memory, f32)>> {
        // `pinned = 0`: a pinned memory is a user-asserted durable fact and is
        // NEVER an auto-supersede target — a near-duplicate inserts fresh
        // instead of silently overwriting it. (Recall still injects pinned
        // memories; that path uses search_memories, not this one.)
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, content, source, pinned, created_at, updated_at, scope, globs, embedding
             FROM memories WHERE project_id = ?1 AND scope = ?2 AND embedding IS NOT NULL AND pinned = 0",
        )?;
        let rows = stmt.query_map(params![project_id, scope], |r| {
            let m = row_to_memory(r)?;
            let blob: Vec<u8> = r.get(9)?;
            Ok((m, blob))
        })?;
        let mut best: Option<(Memory, f32)> = None;
        for r in rows {
            let (m, blob) = r?;
            if let Some(emb) = blob_to_embedding(&blob) {
                let score = cosine_similarity(embedding, &emb);
                if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                    best = Some((m, score));
                }
            }
        }
        Ok(best)
    }

    /// True when a project already has a memory with exactly this content.
    /// Used by the distiller to avoid inserting duplicate distilled bullets
    /// on re-promotion.
    pub fn memory_exists_with_content(&self, project_id: &str, content: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE project_id = ?1 AND content = ?2",
            params![project_id, content],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn count_memories(&self, project_id: &str) -> Result<u32> {
        let n: u32 = self.conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE project_id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }
}

/// Map the first nine memory columns of a row into a [`Memory`]:
/// `id, project_id, content, source, pinned, created_at, updated_at, scope, globs`.
/// Callers that also select the embedding pull it from a later index separately.
fn row_to_memory(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let globs_json: Option<String> = r.get(8)?;
    Ok(Memory {
        id: r.get(0)?,
        project_id: r.get(1)?,
        content: r.get(2)?,
        source: r.get(3)?,
        pinned: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        scope: r.get(7)?,
        globs: globs_json.and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok()),
    })
}

/// Serialize an optional glob list to the JSON the `globs` column stores.
/// `None`/empty → `None` (stored as SQL NULL). A real list → `Some(json)`.
fn globs_to_json(globs: Option<&[String]>) -> Result<Option<String>> {
    match globs {
        Some(g) if !g.is_empty() => Ok(Some(serde_json::to_string(g)?)),
        _ => Ok(None),
    }
}

/// Build a [`globset::GlobSet`] from glob patterns. Invalid patterns are
/// skipped (a bad user glob should narrow matching, never error the recall).
pub fn build_globset(patterns: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = globset::Glob::new(p) {
            builder.add(g);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| globset::GlobSet::empty())
}

/// True when any of `patterns` matches any of `paths` — matched both on the
/// path as-is and on its basename, so `**/*.rs` and `*.rs` both catch
/// `src/foo.rs`. Used by the recall handler to decide whether a `glob`-scoped
/// memory injects for the current request's candidate path set.
pub fn globs_match_any_path(patterns: &[String], paths: &[String]) -> bool {
    if patterns.is_empty() || paths.is_empty() {
        return false;
    }
    let set = build_globset(patterns);
    if set.is_empty() {
        return false;
    }
    for p in paths {
        if set.is_match(p) {
            return true;
        }
        // Also try the basename so a bare `*.rs` glob matches `src/foo.rs`.
        if let Some(base) = std::path::Path::new(p).file_name().and_then(|s| s.to_str()) {
            if base != p.as_str() && set.is_match(base) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// C4: opt-in local-caller auth token
// ---------------------------------------------------------------------------

/// Read the local auth token, creating it on first call. The token is a
/// cryptographically-random 32-byte value, hex-encoded (64 chars), written to
/// `<root>/state/local-token` with 0600 permissions via the same atomic
/// temp-file + rename pattern used for sessions/chunks.
///
/// It is ALWAYS generated (cheap), regardless of whether enforcement is on —
/// so the bundled callers (MCP shim, hook) can read a stable token and the
/// daemon can enforce it the moment the operator flips `require_token`.
///
/// The token is local-only: it never leaves the machine, requires no API key,
/// and is not a cloud credential. The 0600 file blocks *other-user* processes;
/// it does not (and cannot) block a same-user process that can read the file —
/// an inherent limit of any local-token scheme (documented in PRIVACY.md).
pub fn local_token(layout: &Layout) -> io::Result<String> {
    let path = layout.local_token_path();
    // Fast path: reuse an existing, well-formed token.
    if let Ok(existing) = fs::read_to_string(&path) {
        let tok = existing.trim();
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }
    // Generate 32 cryptographically-random bytes. uuid v4 draws from the OS
    // CSPRNG (getrandom); two v4 uuids give 32 random bytes with no new
    // dependency. Hex-encode to a URL/header-safe ASCII token.
    let a = *uuid::Uuid::new_v4().as_bytes();
    let b = *uuid::Uuid::new_v4().as_bytes();
    let mut token = String::with_capacity(64);
    for byte in a.iter().chain(b.iter()) {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Atomic write + 0600, mirroring write_session/write_chunk.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, token.as_bytes())?;
    set_owner_only_permissions(&tmp)?;
    fs::rename(&tmp, &path)?;
    // Re-assert mode on the final path in case rename inherited a umask-wider
    // mode on some filesystems.
    set_owner_only_permissions(&path)?;
    Ok(token)
}

/// chmod 0600 (owner read/write only). Unix-only; the daemon targets macOS/Linux.
fn set_owner_only_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Constant-time byte-slice equality. For equal-length inputs it compares
/// every byte regardless of where the first mismatch is, so a network/timing
/// observer can't binary-search a token by measuring how long the comparison
/// takes. A length mismatch returns false immediately — the token length is
/// fixed (64 chars) and not itself secret, so the early-out leaks nothing
/// useful.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ck_core::{ChunkKind, ChunkRole, EmbeddingRef, ProjectId, SessionId};
    use tempfile::tempdir;

    fn dummy_chunk(id: &str, session: &str) -> Chunk {
        Chunk {
            id: ChunkId(id.into()),
            session_id: SessionId(session.into()),
            project_id: ProjectId("-proj".into()),
            turn_index: 0,
            role: ChunkRole::Assistant,
            kind: ChunkKind::AssistantText,
            text: "hello world".into(),
            token_count: 2,
            start_uuid: "u1".into(),
            end_uuid: "u1".into(),
            started_at: Utc::now(),
            tool_name: None,
            tool_input_preview: None,
            embedding_ref: Some(EmbeddingRef {
                model: "bge-small".into(),
                sha256: "abc".into(),
            }),
        }
    }

    fn dummy_session() -> Session {
        Session {
            id: SessionId("abc".into()),
            project_id: ProjectId("-proj".into()),
            is_sidechain: false,
            parent_session_id: None,
            agent_meta: None,
            source_file: "/tmp/x.jsonl".into(),
            source_file_mtime_ms: 0,
            source_file_sha256: "deadbeef".into(),
            content_hash: "deadbeef".into(),
            first_prompt: Some("hi".into()),
            ai_title: Some("Greeting".into()),
            started_at: Utc::now(),
            ended_at: Utc::now(),
            message_count: 2,
            model_usage: vec![],
            git_branch: None,
            cwd: None,
            summary: None,
            chunk_ids: vec![],
            topic_ids: vec![],
        }
    }

    #[test]
    fn layout_creates_dirs_and_schema_file() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        assert!(layout.sessions_dir().is_dir());
        assert!(layout.cursor_path().parent().unwrap().is_dir());
        assert!(layout.root.join("state/schema-version").is_file());
    }

    #[test]
    fn write_then_read_session_round_trips() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let s = dummy_session();
        write_session(&layout, &s).unwrap();
        let back = read_session(&layout, &s.id).unwrap();
        assert_eq!(back.id.0, s.id.0);
        assert_eq!(back.ai_title, s.ai_title);
    }

    #[test]
    fn meta_index_upserts() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();
        let s = dummy_session();
        idx.upsert_session(&s).unwrap();
        idx.upsert_session(&s).unwrap(); // upsert idempotent
        assert_eq!(idx.count_sessions().unwrap(), 1);
    }

    #[test]
    fn chunk_json_round_trip() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let c = dummy_chunk("s1:0:0", "s1");
        write_chunk(&layout, &c).unwrap();
        let back = read_chunk(&layout, &c.session_id, &c.id).unwrap();
        assert_eq!(back.text, c.text);
        assert_eq!(
            read_chunk_text(&layout, &c.session_id, &c.id).unwrap(),
            c.text
        );
    }

    #[test]
    fn chunks_table_upserts_and_bumps_schema() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        // schema-version starts at 1 from ensure()
        assert_eq!(
            std::fs::read_to_string(layout.schema_version_path())
                .unwrap()
                .trim(),
            "1"
        );

        let mut idx = MetaIndex::open(&layout).unwrap();
        // open() should have bumped to the current schema version. Bumped
        // from 2 to 3 in M6.x when the recall_hits + promotions tables
        // were added; bumped to 4 in C1 when the `memories` table was added;
        // bumped to 5 in C2 when scope/globs columns were added to `memories`.
        // Bump this assertion alongside any future bumps.
        assert_eq!(
            std::fs::read_to_string(layout.schema_version_path())
                .unwrap()
                .trim(),
            "5"
        );
        let chunks = vec![dummy_chunk("s1:0:0", "s1"), dummy_chunk("s1:1:0", "s1")];
        idx.upsert_chunks(&chunks).unwrap();
        idx.upsert_chunks(&chunks).unwrap(); // idempotent
        assert_eq!(idx.count_chunks().unwrap(), 2);
    }

    fn mem(id: &str, project: &str, content: &str, pinned: bool) -> Memory {
        let now = Utc::now().timestamp();
        Memory {
            id: id.into(),
            project_id: project.into(),
            content: content.into(),
            source: "agent".into(),
            pinned,
            scope: "auto".into(),
            globs: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn mem_scoped(
        id: &str,
        project: &str,
        content: &str,
        scope: &str,
        globs: Option<Vec<String>>,
    ) -> Memory {
        let now = Utc::now().timestamp();
        Memory {
            id: id.into(),
            project_id: project.into(),
            content: content.into(),
            source: "user".into(),
            pinned: false,
            scope: scope.into(),
            globs,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn memory_crud_round_trip() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        let id = MetaIndex::new_memory_id();
        let m = mem(
            &id,
            "-proj",
            "use the local embedder, never an API key",
            false,
        );
        idx.insert_memory(&m, &[1.0, 0.0, 0.0, 0.0]).unwrap();

        // get
        let got = idx.get_memory(&id).unwrap().expect("inserted memory");
        assert_eq!(got.content, m.content);
        assert!(!got.pinned);
        assert_eq!(got.source, "agent");

        // list
        let list = idx.list_memories("-proj", None, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].scope, "auto"); // default scope on a plain insert
        assert_eq!(idx.count_memories("-proj").unwrap(), 1);

        // update content + pin (bumps updated_at)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let updated = idx
            .update_memory(
                &id,
                Some("revised fact"),
                Some(true),
                Some(&[0.0, 1.0, 0.0, 0.0]),
                None,
                None,
            )
            .unwrap();
        assert!(updated);
        let got2 = idx.get_memory(&id).unwrap().unwrap();
        assert_eq!(got2.content, "revised fact");
        assert!(got2.pinned);
        assert!(
            got2.updated_at >= got.updated_at,
            "updated_at should be bumped (was {}, now {})",
            got.updated_at,
            got2.updated_at
        );

        // update of a missing id → false (maps to 404 at the API)
        assert!(!idx
            .update_memory("does-not-exist", Some("x"), None, None, None, None)
            .unwrap());

        // delete
        assert!(idx.delete_memory(&id).unwrap());
        assert!(idx.get_memory(&id).unwrap().is_none());
        assert!(!idx.delete_memory(&id).unwrap()); // second delete → false
    }

    #[test]
    fn list_orders_pinned_first_then_recent() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        // Three memories with controlled timestamps: a (old), b (new),
        // c (oldest but pinned). Expected order: c (pinned), b, a.
        let base = Utc::now().timestamp();
        let mk = |id: &str, ts: i64, pinned: bool| Memory {
            id: id.into(),
            project_id: "-p".into(),
            content: id.into(),
            source: "user".into(),
            pinned,
            scope: "auto".into(),
            globs: None,
            created_at: ts,
            updated_at: ts,
        };
        idx.insert_memory(&mk("a", base + 10, false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(&mk("b", base + 20, false), &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(&mk("c", base + 1, true), &[0.0, 0.0, 1.0, 0.0])
            .unwrap();

        let ids: Vec<String> = idx
            .list_memories("-p", None, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
    }

    #[test]
    fn search_ranks_by_cosine_descending() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        // Stub orthonormal vectors so cosine ordering is exact and obvious.
        idx.insert_memory(&mem("m_x", "-p", "x axis", false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(&mem("m_y", "-p", "y axis", false), &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(&mem("m_xy", "-p", "diagonal", false), &[1.0, 1.0, 0.0, 0.0])
            .unwrap();
        // Different project — must be excluded from results.
        idx.insert_memory(
            &mem("other", "-q", "elsewhere", false),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();

        // Query along +x: m_x (cos 1.0) > m_xy (cos ~0.707) > m_y (cos 0.0).
        let ranked = idx
            .search_memories("-p", &[1.0, 0.0, 0.0, 0.0], 10)
            .unwrap();
        let ids: Vec<&str> = ranked.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m_x", "m_xy", "m_y"]);
        assert!((ranked[0].1 - 1.0).abs() < 1e-5);
        assert!(ranked[0].1 >= ranked[1].1 && ranked[1].1 >= ranked[2].1);
        // Cross-project isolation.
        assert!(!ids.contains(&"other"));

        // limit caps the result set.
        let top1 = idx.search_memories("-p", &[1.0, 0.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(top1.len(), 1);
        assert_eq!(top1[0].0.id, "m_x");
    }

    #[test]
    fn dedupe_by_content_for_distiller() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();
        assert!(!idx.memory_exists_with_content("-p", "fact one").unwrap());
        idx.insert_memory(&mem("d1", "-p", "fact one", false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        assert!(idx.memory_exists_with_content("-p", "fact one").unwrap());
        // Same content in a different project is NOT a duplicate.
        assert!(!idx
            .memory_exists_with_content("-other", "fact one")
            .unwrap());
    }

    /// C5: nearest_memory_in_scope returns the best cosine match within the
    /// same (project, scope), is scope-aware (won't reach across scopes), and
    /// project-scoped (won't reach across projects). None when the scope is
    /// empty / has no embedded rows.
    #[test]
    fn nearest_memory_is_scope_and_project_aware() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        // Empty scope → None.
        assert!(idx
            .nearest_memory_in_scope("-p", "auto", &[1.0, 0.0, 0.0, 0.0])
            .unwrap()
            .is_none());

        // Two auto rows on -p: one along +x, one along +y.
        idx.insert_memory(&mem("ax", "-p", "x fact", false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(&mem("ay", "-p", "y fact", false), &[0.0, 1.0, 0.0, 0.0])
            .unwrap();
        // An always-scoped row that's a perfect +x match — must NOT be returned
        // for the auto scope (scope isolation).
        idx.insert_memory(
            &mem_scoped("rule", "-p", "x rule", "always", None),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();
        // A different project with a perfect +x match — must NOT be returned.
        idx.insert_memory(
            &mem("other", "-q", "x elsewhere", false),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();

        // Query +x in (-p, auto): best is ax (cos 1.0), not the always rule
        // and not the -q row.
        let (best, score) = idx
            .nearest_memory_in_scope("-p", "auto", &[1.0, 0.0, 0.0, 0.0])
            .unwrap()
            .expect("a match");
        assert_eq!(best.id, "ax");
        assert!((score - 1.0).abs() < 1e-5);

        // The same +x query against the `always` scope finds the rule (scope
        // isolation works in both directions).
        let (rule, _) = idx
            .nearest_memory_in_scope("-p", "always", &[1.0, 0.0, 0.0, 0.0])
            .unwrap()
            .expect("a rule");
        assert_eq!(rule.id, "rule");

        // A scope with no rows → None.
        assert!(idx
            .nearest_memory_in_scope("-p", "manual", &[1.0, 0.0, 0.0, 0.0])
            .unwrap()
            .is_none());
    }

    /// C2: a memory inserted by a pre-C2 path (i.e. a row that pre-dates the
    /// `scope` column) reads back with the migrated default `scope = 'auto'`,
    /// and the `search_memories` semantic pass still finds it. We simulate the
    /// pre-C2 row by writing directly with the legacy column set, then opening
    /// a fresh MetaIndex over the same DB to drive the ALTER-TABLE migration.
    #[test]
    fn scope_column_defaults_to_auto_on_legacy_rows() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();

        // Build a DB with the *legacy* (pre-C2) memories table — no scope/globs.
        {
            let conn = rusqlite::Connection::open(layout.meta_db()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE memories (
                    id TEXT PRIMARY KEY,
                    project_id TEXT NOT NULL,
                    content TEXT NOT NULL,
                    source TEXT NOT NULL,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    embedding BLOB
                );
                "#,
            )
            .unwrap();
            let blob = embedding_to_blob(&[1.0, 0.0, 0.0, 0.0]);
            conn.execute(
                "INSERT INTO memories (id, project_id, content, source, pinned, created_at, updated_at, embedding)
                 VALUES ('legacy', '-p', 'old fact', 'user', 0, 1, 1, ?1)",
                params![blob],
            )
            .unwrap();
        }

        // Opening MetaIndex must add scope/globs and back-fill scope='auto'.
        let idx = MetaIndex::open(&layout).unwrap();
        let got = idx.get_memory("legacy").unwrap().expect("legacy row");
        assert_eq!(got.scope, "auto", "migrated legacy rows default to auto");
        assert!(got.globs.is_none());

        // And the migrated row still participates in the auto semantic pass.
        let ranked = idx
            .search_memories("-p", &[1.0, 0.0, 0.0, 0.0], 10)
            .unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0.id, "legacy");
    }

    /// C2: scope/globs round-trip through insert → get → list; the scope
    /// filter narrows list; `search_memories` returns ONLY auto rows (always /
    /// glob / manual are excluded from the semantic pass).
    #[test]
    fn scope_and_globs_roundtrip_and_filter_search() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        idx.insert_memory(&mem("a1", "-p", "auto fact", false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();
        idx.insert_memory(
            &mem_scoped("r1", "-p", "always rule", "always", None),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();
        idx.insert_memory(
            &mem_scoped(
                "g1",
                "-p",
                "rust glob",
                "glob",
                Some(vec!["**/*.rs".into()]),
            ),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();
        idx.insert_memory(
            &mem_scoped("m1", "-p", "manual note", "manual", None),
            &[1.0, 0.0, 0.0, 0.0],
        )
        .unwrap();

        // globs round-trip
        let g = idx.get_memory("g1").unwrap().unwrap();
        assert_eq!(g.scope, "glob");
        assert_eq!(g.globs.as_deref(), Some(&["**/*.rs".to_string()][..]));

        // list with no filter returns all four; scope filter narrows.
        assert_eq!(idx.list_memories("-p", None, 50).unwrap().len(), 4);
        assert_eq!(idx.always_memories("-p", 50).unwrap().len(), 1);
        assert_eq!(idx.glob_memories("-p", 50).unwrap().len(), 1);
        assert_eq!(
            idx.list_memories("-p", Some("manual"), 50).unwrap().len(),
            1
        );

        // The semantic pass sees ONLY the auto row even though every row has
        // the same (matching) embedding.
        let ranked = idx
            .search_memories("-p", &[1.0, 0.0, 0.0, 0.0], 50)
            .unwrap();
        let ids: Vec<&str> = ranked.iter().map(|(m, _)| m.id.as_str()).collect();
        assert_eq!(ids, vec!["a1"]);
    }

    /// C2: update_memory can change scope and set/clear globs.
    #[test]
    fn update_memory_changes_scope_and_globs() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let idx = MetaIndex::open(&layout).unwrap();

        idx.insert_memory(&mem("x", "-p", "fact", false), &[1.0, 0.0, 0.0, 0.0])
            .unwrap();

        // Promote to a glob memory with patterns.
        let globs = vec!["src/**/*.rs".to_string()];
        assert!(idx
            .update_memory("x", None, None, None, Some("glob"), Some(Some(&globs)))
            .unwrap());
        let g = idx.get_memory("x").unwrap().unwrap();
        assert_eq!(g.scope, "glob");
        assert_eq!(g.globs.as_deref(), Some(&["src/**/*.rs".to_string()][..]));

        // Switch to always and clear globs (Some(None) → NULL).
        assert!(idx
            .update_memory("x", None, None, None, Some("always"), Some(None))
            .unwrap());
        let a = idx.get_memory("x").unwrap().unwrap();
        assert_eq!(a.scope, "always");
        assert!(a.globs.is_none());
    }

    #[test]
    fn globs_match_paths_with_basename_fallback() {
        let pats = vec!["**/*.rs".to_string()];
        assert!(globs_match_any_path(&pats, &["src/foo.rs".to_string()]));
        assert!(globs_match_any_path(&pats, &["foo.rs".to_string()]));
        assert!(!globs_match_any_path(&pats, &["README.md".to_string()]));

        // Bare `*.rs` matches a nested path via the basename fallback.
        let bare = vec!["*.rs".to_string()];
        assert!(globs_match_any_path(
            &bare,
            &["src/deep/foo.rs".to_string()]
        ));

        // Empty inputs never match; an invalid glob is skipped, not an error.
        assert!(!globs_match_any_path(&[], &["x.rs".to_string()]));
        assert!(!globs_match_any_path(&pats, &[]));
        assert!(!globs_match_any_path(
            &["[".to_string()],
            &["x".to_string()]
        ));
    }

    #[test]
    fn cosine_similarity_edge_cases() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        // Length mismatch and zero magnitude both yield 0.0 (never NaN).
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}

// ---------------------------------------------------------------------------
// User configuration (~/.context-keeper/config.toml)
// ---------------------------------------------------------------------------

/// Defaults applied to hook-sourced `/v1/recall` calls when the request
/// doesn't specify a value. The auto-recall hook intentionally sends a
/// minimal body so these settings (editable in the UI) govern its behavior
/// without touching shell env vars.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HookConfig {
    /// Drop hits scoring below this (0.0–1.0).
    pub score_threshold: f32,
    /// Max chunks injected per prompt.
    pub limit: u32,
    /// Token budget across injected chunks.
    pub token_budget: u32,
    /// Skip recall entirely for prompts shorter than this many words.
    pub min_words: u32,
    /// "project" = recall only from the current project; "global" = all.
    pub scope: String,
    /// C2: max `always`-scoped rules injected per recall (highest precedence,
    /// score-floor-free). Capped low so standing standards never crowd out the
    /// semantic recall. Backward-compatible serde default keeps old config
    /// files loading.
    #[serde(default = "default_max_always_injected")]
    pub max_always_injected: u32,
}

fn default_max_always_injected() -> u32 {
    3
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            score_threshold: 0.60,
            limit: 5,
            token_budget: 1500,
            min_words: 4,
            scope: "project".into(),
            max_always_injected: default_max_always_injected(),
        }
    }
}

/// On-disk user configuration. Lives at `<root>/config.toml`; absent file
/// means all-defaults. Unknown keys are preserved-by-ignore (serde default),
/// so older binaries tolerate newer config files.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Promote hot chunks into project CLAUDE.md. Env CK_AUTO_PROMOTE=1
    /// still wins when set (operator override).
    pub auto_promote: bool,
    /// C4: opt-in local-caller auth. Default false → NO behavior change: the
    /// daemon stays keyless on loopback exactly as before. When true (or env
    /// `CK_REQUIRE_TOKEN=1`, the operator override), `/v1/*` requests (except
    /// `GET /v1/health`) must present the local token. `#[serde(default)]` keeps
    /// existing config.toml files loading unchanged.
    #[serde(default)]
    pub require_token: bool,
    /// C5: cosine threshold for local supersede-on-write dedupe. When a new
    /// memory's embedding is within this distance of the nearest existing
    /// memory in the SAME (project, scope), the existing row is UPDATED in
    /// place instead of a near-duplicate being inserted. Default ~0.95 (very
    /// close — only collapses near-identical / rephrased facts on the same
    /// topic). Clamped to a sane range at use; `#[serde(default)]` keeps
    /// pre-C5 config.toml files loading unchanged. Always-on (no LLM).
    #[serde(default = "default_dedupe_threshold")]
    pub dedupe_threshold: f32,
    /// C5: opt-in LLM reconciliation (full mem0-style ADD/UPDATE/NOOP for true
    /// contradictions). Default false → behavior is exactly the part-A local
    /// dedupe, NO LLM and NO key (the no-API-key moat). Env `CK_MEMORY_RECONCILE=1`
    /// is the operator override, mirroring `auto_promote`/`require_token`. Even
    /// when on, it falls back to local dedupe whenever the LLM is unavailable or
    /// errors — it never blocks a write and never requires a key.
    #[serde(default)]
    pub memory_reconcile: bool,
    pub hook: HookConfig,
}

/// C5 default dedupe threshold: 0.95 cosine — close enough that only
/// near-identical or rephrased facts on the same topic supersede in place,
/// while genuinely distinct facts still insert as separate rows.
fn default_dedupe_threshold() -> f32 {
    0.95
}

impl Default for Config {
    fn default() -> Self {
        // Hand-written so the all-defaults `Config` matches the serde defaults
        // exactly (the derive would zero `dedupe_threshold`, diverging from
        // `default_dedupe_threshold`).
        Self {
            auto_promote: false,
            require_token: false,
            dedupe_threshold: default_dedupe_threshold(),
            memory_reconcile: false,
            hook: HookConfig::default(),
        }
    }
}

/// Clamp a configured dedupe threshold into a usable cosine range. A value at
/// or below this floor would collapse unrelated facts; above the ceiling no
/// rephrasing would ever match. Keeps an out-of-range config from silently
/// breaking supersede-on-write in either direction.
pub fn clamp_dedupe_threshold(t: f32) -> f32 {
    t.clamp(0.50, 0.999)
}

/// True when opt-in LLM reconciliation is on: env `CK_MEMORY_RECONCILE=1`
/// (operator override, mirroring `require_token_enabled`) OR the config flag.
pub fn memory_reconcile_enabled(config: &Config) -> bool {
    std::env::var("CK_MEMORY_RECONCILE")
        .map(|v| v == "1")
        .unwrap_or(false)
        || config.memory_reconcile
}

/// True when local-token enforcement is on: env `CK_REQUIRE_TOKEN=1` (operator
/// override, mirroring the `CK_AUTO_PROMOTE` pattern) OR the config flag.
pub fn require_token_enabled(config: &Config) -> bool {
    std::env::var("CK_REQUIRE_TOKEN")
        .map(|v| v == "1")
        .unwrap_or(false)
        || config.require_token
}

impl Config {
    pub fn path(layout: &Layout) -> PathBuf {
        layout.root.join("config.toml")
    }

    /// Read `<root>/config.toml`. Missing file → defaults. A malformed file
    /// is an error (silent fallback would mask typos and "my settings do
    /// nothing" is a worse failure than a parse message).
    pub fn load(layout: &Layout) -> Result<Self> {
        let p = Self::path(layout);
        if !p.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&p)?;
        toml::from_str(&text).map_err(|e| StoreError::Config(format!("{}: {e}", p.display())))
    }

    /// Write the full config as pretty TOML (atomic: temp file + rename).
    pub fn save(&self, layout: &Layout) -> Result<()> {
        let p = Self::path(layout);
        let text = toml::to_string_pretty(self)
            .map_err(|e| StoreError::Config(format!("serialize: {e}")))?;
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

#[cfg(test)]
mod id_safety_tests {
    use super::*;
    use ck_core::{ChunkId, SessionId};

    #[test]
    fn ensure_safe_id_rejects_traversal_vectors() {
        for bad in [
            "../../../etc/passwd",
            "..\\..\\windows",
            "/etc/hostname",
            "a/b",
            "x\0y",
            "",
            &"z".repeat(257),
        ] {
            assert!(
                ensure_safe_id(bad).is_err(),
                "should reject {bad:?} as path-unsafe"
            );
        }
        for ok in ["abc-123", "7f7ca66c-3bd0", "sid:12:0", "a..b"] {
            assert!(ensure_safe_id(ok).is_ok(), "should accept {ok:?}");
        }
    }

    #[test]
    fn read_session_refuses_to_escape_root() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new_at(tmp.path().join("ck"));
        layout.ensure().unwrap();
        // Plant a file one level above the sessions dir that a traversal id
        // would otherwise reach.
        let secret = layout.root.join("secret.json");
        std::fs::write(&secret, br#"{"stolen":true}"#).unwrap();

        let err = read_session(&layout, &SessionId("../secret".into()));
        assert!(matches!(err, Err(StoreError::InvalidId(_))));

        let err = read_chunk(
            &layout,
            &SessionId("../..".into()),
            &ChunkId("secret".into()),
        );
        assert!(matches!(err, Err(StoreError::InvalidId(_))));
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new_at(tmp.path().join("ck"));
        layout.ensure().unwrap();

        // Missing file → defaults.
        let c = Config::load(&layout).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.hook.limit, 5);
        assert!((c.hook.score_threshold - 0.60).abs() < f32::EPSILON);

        // C4: require_token defaults false (the no-key moat is intact).
        assert!(!c.require_token);

        // C5: dedupe is ON-by-default via the threshold default (~0.95), and
        // reconcile is OFF-by-default (the no-LLM/no-key moat).
        assert!((c.dedupe_threshold - default_dedupe_threshold()).abs() < f32::EPSILON);
        assert!(!c.memory_reconcile);

        // Save modified → load back identical.
        let mut c2 = c.clone();
        c2.auto_promote = true;
        c2.require_token = true;
        c2.hook.limit = 9;
        c2.hook.scope = "global".into();
        c2.save(&layout).unwrap();
        let c3 = Config::load(&layout).unwrap();
        assert_eq!(c2, c3);
        assert!(c3.require_token);

        // A pre-C4 config.toml (no require_token key) still loads, defaulting
        // the flag to false — the default behavior is unchanged.
        std::fs::write(
            Config::path(&layout),
            "auto_promote = false\n[hook]\nlimit = 5\n",
        )
        .unwrap();
        let pre_c4 = Config::load(&layout).unwrap();
        assert!(!pre_c4.require_token, "missing key defaults to false");
        // C5: a pre-C5 file (no dedupe_threshold / memory_reconcile keys) still
        // loads with the dedupe default applied and reconcile off — default
        // behavior (always-on local dedupe, no LLM) is preserved.
        assert!(
            (pre_c4.dedupe_threshold - default_dedupe_threshold()).abs() < f32::EPSILON,
            "missing dedupe_threshold key defaults to ~0.95"
        );
        assert!(
            !pre_c4.memory_reconcile,
            "missing reconcile key defaults off"
        );

        // Unknown keys tolerated; partial files fill from defaults.
        std::fs::write(
            Config::path(&layout),
            "auto_promote = true\nfuture_key = 1\n[hook]\nlimit = 3\n",
        )
        .unwrap();
        let c4 = Config::load(&layout).unwrap();
        assert!(c4.auto_promote);
        assert_eq!(c4.hook.limit, 3);
        assert_eq!(c4.hook.min_words, 4); // default fills in

        // Malformed file → loud error, not silent defaults.
        std::fs::write(Config::path(&layout), "auto_promote = {{{{").unwrap();
        assert!(Config::load(&layout).is_err());
    }

    /// C5: the dedupe threshold clamps into a usable cosine range so an
    /// out-of-range config can't silently disable supersede-on-write.
    #[test]
    fn dedupe_threshold_clamps() {
        assert!((clamp_dedupe_threshold(0.95) - 0.95).abs() < f32::EPSILON);
        assert_eq!(clamp_dedupe_threshold(-5.0), 0.50); // floor
        assert_eq!(clamp_dedupe_threshold(2.0), 0.999); // ceiling
        assert!((clamp_dedupe_threshold(0.50) - 0.50).abs() < f32::EPSILON);
    }

    /// C5: reconcile is OFF by default and ON via env or config (operator
    /// override mirrors CK_AUTO_PROMOTE / CK_REQUIRE_TOKEN). Serialized via the
    /// env var; remove it on entry/exit to avoid leaking into other tests.
    #[test]
    fn memory_reconcile_enabled_reads_config_and_env() {
        std::env::remove_var("CK_MEMORY_RECONCILE");
        let mut cfg = Config::default();
        assert!(!memory_reconcile_enabled(&cfg), "default: off (the moat)");
        cfg.memory_reconcile = true;
        assert!(memory_reconcile_enabled(&cfg), "config flag: on");
        cfg.memory_reconcile = false;
        std::env::set_var("CK_MEMORY_RECONCILE", "1");
        assert!(memory_reconcile_enabled(&cfg), "env override: on");
        std::env::remove_var("CK_MEMORY_RECONCILE");
    }
}

#[cfg(test)]
mod local_token_tests {
    use super::*;

    /// C4: the token file is generated on first read, is 64 hex chars, lands at
    /// state/local-token, and is created with 0600 permissions. A second read
    /// returns the SAME token (stable across daemon restarts).
    #[test]
    fn token_generated_with_0600_and_is_stable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::new_at(tmp.path().join("ck"));
        layout.ensure().unwrap();

        let path = layout.local_token_path();
        assert!(!path.exists(), "token must not exist before first read");

        let tok = local_token(&layout).unwrap();
        assert_eq!(tok.len(), 64, "32 bytes hex-encoded → 64 chars");
        assert!(
            tok.bytes().all(|b| b.is_ascii_hexdigit()),
            "token is hex: {tok}"
        );
        assert!(path.is_file(), "token file written");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be 0600, got {mode:o}");

        // Stable: a second call reads the same value, doesn't regenerate.
        let tok2 = local_token(&layout).unwrap();
        assert_eq!(tok, tok2, "token must be stable across reads");
    }

    /// C4: constant-time compare returns true only for byte-identical inputs.
    #[test]
    fn constant_time_eq_matches_only_equal_bytes() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
        // A realistic 64-char token round-trips.
        let t = "a".repeat(64);
        assert!(constant_time_eq(t.as_bytes(), t.as_bytes()));
        let mut bad = t.clone().into_bytes();
        bad[63] = b'b';
        assert!(!constant_time_eq(t.as_bytes(), &bad));
    }

    /// C4: enforcement is OFF by default and ON via env or config. (The env
    /// override mirrors CK_AUTO_PROMOTE.) Serialized to avoid a global-env race
    /// with other tests touching CK_REQUIRE_TOKEN.
    #[test]
    fn require_token_enabled_reads_config_and_env() {
        // Make sure no stray env from another test leaks in.
        std::env::remove_var("CK_REQUIRE_TOKEN");
        let mut cfg = Config::default();
        assert!(!require_token_enabled(&cfg), "default: off");
        cfg.require_token = true;
        assert!(require_token_enabled(&cfg), "config flag: on");
        cfg.require_token = false;
        std::env::set_var("CK_REQUIRE_TOKEN", "1");
        assert!(require_token_enabled(&cfg), "env override: on");
        std::env::remove_var("CK_REQUIRE_TOKEN");
    }
}
