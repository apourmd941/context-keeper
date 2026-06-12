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
            "#,
        )?;
        // bump schema-version sentinel to 3
        if let Some(parent) = layout.schema_version_path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(layout.schema_version_path(), b"3\n")?;
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
        // were added; bump this assertion alongside any future bumps.
        assert_eq!(
            std::fs::read_to_string(layout.schema_version_path())
                .unwrap()
                .trim(),
            "3"
        );
        let chunks = vec![dummy_chunk("s1:0:0", "s1"), dummy_chunk("s1:1:0", "s1")];
        idx.upsert_chunks(&chunks).unwrap();
        idx.upsert_chunks(&chunks).unwrap(); // idempotent
        assert_eq!(idx.count_chunks().unwrap(), 2);
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
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            score_threshold: 0.60,
            limit: 5,
            token_budget: 1500,
            min_words: 4,
            scope: "project".into(),
        }
    }
}

/// On-disk user configuration. Lives at `<root>/config.toml`; absent file
/// means all-defaults. Unknown keys are preserved-by-ignore (serde default),
/// so older binaries tolerate newer config files.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    /// Promote hot chunks into project CLAUDE.md. Env CK_AUTO_PROMOTE=1
    /// still wins when set (operator override).
    pub auto_promote: bool,
    pub hook: HookConfig,
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

        // Save modified → load back identical.
        let mut c2 = c.clone();
        c2.auto_promote = true;
        c2.hook.limit = 9;
        c2.hook.scope = "global".into();
        c2.save(&layout).unwrap();
        let c3 = Config::load(&layout).unwrap();
        assert_eq!(c2, c3);

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
}
