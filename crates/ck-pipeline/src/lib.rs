//! Indexing pipeline + file-watcher.
//!
//! Owns the shared [`DaemonState`] used by both the watcher loop and the
//! HTTP/WebSocket API. The pipeline is fully synchronous internally — only
//! the watcher's debounce thread and the broadcast channel touch tokio.

pub mod promote;

use chrono::Utc;
use ck_chunk::chunk_session;
use ck_core::{hex_encode, AgentMeta, EmbeddingRef, ModelUsage, ProjectId, Session, SessionId};
use ck_embed::{embed_with_cache, Embedder, LocalEmbedder};
use ck_store::{write_chunk, write_session, Config, Layout, MetaIndex};
use ck_transcript::{discover_projects, parse_session_file, parse_session_records, ParsedSession};
use ck_vector::VectorStore;
use notify::{EventKind, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transcript: {0}")]
    Transcript(#[from] ck_transcript::TranscriptError),
    #[error("store: {0}")]
    Store(#[from] ck_store::StoreError),
    #[error("embed: {0}")]
    Embed(#[from] ck_embed::EmbedError),
    #[error("vector: {0}")]
    Vector(#[from] ck_vector::VectorError),
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

pub type Result<T> = std::result::Result<T, PipelineError>;

/// One JSON event emitted from the indexer to subscribers (HTTP WS, MCP).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    DaemonReady,
    /// Boot-scan heartbeat (every 25 files) so UIs can show live progress.
    ScanProgress {
        done: u32,
        total: u32,
    },
    SessionIndexed {
        session_id: String,
        project_id: String,
        message_count: u32,
        chunks: u32,
        new_embeds: u32,
        cache_hits: u32,
        is_sidechain: bool,
    },
    SessionSkipped {
        session_id: String,
        reason: String,
    },
}

/// Shared state owned by the daemon. Cheap to clone (every field is `Arc`).
#[derive(Clone)]
pub struct DaemonState {
    pub layout: Arc<Layout>,
    pub projects_root: Arc<PathBuf>,
    pub embedder: Arc<dyn Embedder>,
    pub vector: Arc<RwLock<VectorStore>>,
    pub meta: Arc<Mutex<MetaIndex>>,
    pub events: broadcast::Sender<Event>,
    /// True while the boot scan is still walking the historical corpus. The
    /// API serves immediately (health reports `indexing`); queries run against
    /// whatever is indexed so far.
    pub indexing: Arc<std::sync::atomic::AtomicBool>,
    /// Files processed by the boot scan so far (for progress reporting).
    pub scan_progress: Arc<std::sync::atomic::AtomicU32>,
    /// User configuration (config.toml). Reloaded in place when the
    /// settings API writes a new file.
    pub config: Arc<RwLock<Config>>,
}

impl DaemonState {
    pub fn new(
        layout: Layout,
        projects_root: PathBuf,
        embedder: Arc<dyn Embedder>,
        vector: VectorStore,
        meta: MetaIndex,
    ) -> Self {
        let (tx, _) = broadcast::channel(1024);
        let config = Config::load(&layout).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "config.toml unreadable; using defaults");
            Config::default()
        });
        Self {
            layout: Arc::new(layout),
            projects_root: Arc::new(projects_root),
            embedder,
            vector: Arc::new(RwLock::new(vector)),
            meta: Arc::new(Mutex::new(meta)),
            events: tx,
            indexing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            scan_progress: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// Subscribe to the broadcast channel. Each subscriber gets independent
    /// receive semantics; unconsumed events are dropped per-subscriber when
    /// the channel fills up.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

/// Conservative concrete embedder constructor used by the daemon entrypoint.
pub fn build_local_embedder(layout: &Layout) -> Result<Arc<dyn Embedder>> {
    let e = LocalEmbedder::new(layout)?;
    Ok(Arc::new(e))
}

/// Result of indexing one session file.
#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub chunks: u32,
    pub new_embeds: u32,
    pub cache_hits: u32,
    pub message_count: u32,
    pub is_sidechain: bool,
}

/// Re-index a single session file end-to-end. Idempotent: re-running does no
/// new embedding work because every chunk hits the content-addressed cache.
///
/// `is_sidechain` is supplied by the caller because the watcher knows whether
/// the path lives under a `subagents/` directory; the parser also detects it
/// but the caller's view is authoritative for incremental indexing.
pub fn index_file(
    state: &DaemonState,
    project_id: &ProjectId,
    file: &Path,
) -> Result<IndexOutcome> {
    let parsed = parse_session_file(project_id, file)?;
    let mut session = parsed_to_session(parsed);

    {
        let meta = state.meta.lock().expect("meta mutex");
        meta.upsert_session(&session)?;
    }

    let records = parse_session_records(file)?;
    let mut chunks = chunk_session(&session.id, &session.project_id, &records);
    let chunk_count = chunks.len() as u32;

    let (new_embeds, cache_hits) = if !chunks.is_empty() {
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let outcome = embed_with_cache(state.embedder.as_ref(), &state.layout, &texts)?;
        for (chunk, hash) in chunks.iter_mut().zip(outcome.hashes.iter()) {
            chunk.embedding_ref = Some(EmbeddingRef {
                model: state.embedder.model_name().to_string(),
                sha256: hash.clone(),
            });
        }
        {
            let mut store = state.vector.write().expect("vector lock");
            store.upsert_chunks(&chunks, &outcome.embeddings)?;
        }
        {
            let mut meta = state.meta.lock().expect("meta mutex");
            meta.upsert_chunks(&chunks)?;
        }
        for c in &chunks {
            write_chunk(&state.layout, c)?;
        }
        (outcome.new_embeds, outcome.cache_hits)
    } else {
        (0, 0)
    };

    session.chunk_ids = chunks.iter().map(|c| c.id.clone()).collect();
    write_session(&state.layout, &session)?;

    let outcome = IndexOutcome {
        session_id: session.id.clone(),
        project_id: session.project_id.clone(),
        chunks: chunk_count,
        new_embeds,
        cache_hits,
        message_count: session.message_count,
        is_sidechain: session.is_sidechain,
    };

    let _ = state.events.send(Event::SessionIndexed {
        session_id: outcome.session_id.0.clone(),
        project_id: outcome.project_id.0.clone(),
        message_count: outcome.message_count,
        chunks: outcome.chunks,
        new_embeds: outcome.new_embeds,
        cache_hits: outcome.cache_hits,
        is_sidechain: outcome.is_sidechain,
    });

    Ok(outcome)
}

/// Walk the projects root and index every session + subagent file. Used at
/// daemon startup so the API has data immediately.
pub fn initial_scan(state: &DaemonState) -> Result<u32> {
    use std::sync::atomic::Ordering;
    let projects = discover_projects(state.projects_root.as_path())?;
    let total: usize = projects
        .iter()
        .map(|p| p.session_files.len() + p.subagent_files.len())
        .sum();
    info!(files = total, "initial scan starting");
    let mut indexed = 0u32;
    for project in projects {
        for f in project
            .session_files
            .iter()
            .chain(project.subagent_files.iter())
        {
            match index_file(state, &project.id, f) {
                Ok(_) => {
                    indexed += 1;
                }
                Err(e) => warn!(file = ?f, error = %e, "initial scan: index failed"),
            }
            let done = state.scan_progress.fetch_add(1, Ordering::Relaxed) + 1;
            // Periodic heartbeat so a large historical corpus never looks
            // like a hang (the gap between "starting" and "complete" used to
            // be minutes of silence).
            if done % 25 == 0 {
                info!(done, total, "initial scan progress");
                let _ = state.events.send(Event::ScanProgress {
                    done,
                    total: total as u32,
                });
            }
        }
    }
    Ok(indexed)
}

/// Spawn the file watcher on a dedicated OS thread. Events are debounced for
/// 250ms; on each batch, every changed `.jsonl` is re-indexed (the embedding
/// cache makes unchanged chunks free).
///
/// Returns a `JoinHandle` so the caller can detect catastrophic failures.
pub fn spawn_watcher(state: DaemonState) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        if let Err(e) = run_watcher(state) {
            error!(error = %e, "watcher exited with error");
        }
    })
}

fn run_watcher(state: DaemonState) -> Result<()> {
    let projects_root = state.projects_root.as_path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(Duration::from_millis(250), None, move |res| {
        let _ = tx.send(res);
    })?;
    debouncer
        .watcher()
        .watch(&projects_root, RecursiveMode::Recursive)?;
    info!(?projects_root, "watcher armed");

    while let Ok(result) = rx.recv() {
        let events = match result {
            Ok(events) => events,
            Err(errors) => {
                for e in errors {
                    warn!(error = %e, "watcher reported error");
                }
                continue;
            }
        };
        let mut to_index: HashSet<PathBuf> = HashSet::new();
        for ev in events {
            // Only react to mutation events for .jsonl files.
            if !matches!(
                ev.event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                continue;
            }
            for path in &ev.event.paths {
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                if !path.is_file() {
                    // Removed file: nothing to do for M3 (we don't garbage-
                    // collect dropped chunks yet — that's an M3.x or M4 task).
                    continue;
                }
                to_index.insert(path.clone());
            }
        }
        if to_index.is_empty() {
            continue;
        }
        info!(n = to_index.len(), "watcher batch");
        for path in to_index {
            match resolve_project(&projects_root, &path) {
                Some(project_id) => match index_file(&state, &project_id, &path) {
                    Ok(o) => info!(
                        session = %o.session_id.0,
                        chunks = o.chunks,
                        new = o.new_embeds,
                        hits = o.cache_hits,
                        is_sidechain = o.is_sidechain,
                        "indexed"
                    ),
                    Err(e) => warn!(file = ?path, error = %e, "watcher index failed"),
                },
                None => warn!(file = ?path, "could not resolve project_id"),
            }
        }
    }
    info!("watcher channel closed; exiting");
    Ok(())
}

/// Given a `.jsonl` path under projects_root, recover its project_id (= the
/// first directory component).
fn resolve_project(projects_root: &Path, file: &Path) -> Option<ProjectId> {
    let rel = file.strip_prefix(projects_root).ok()?;
    let first = rel.components().next()?;
    let name = first.as_os_str().to_str()?;
    Some(ProjectId(name.to_string()))
}

fn parsed_to_session(p: ParsedSession) -> Session {
    let now = Utc::now();
    let model_usage: Vec<ModelUsage> = p
        .model_usage
        .into_iter()
        .map(|(model, u)| ModelUsage {
            model,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
        })
        .collect();
    let mut h = Sha256::new();
    h.update(p.source_file_sha256.as_bytes());
    h.update(p.message_count.to_le_bytes());
    let content_hash = hex_encode(h.finalize().as_slice());
    Session {
        id: p.session_id,
        project_id: p.project_id,
        is_sidechain: p.is_sidechain,
        parent_session_id: p.parent_session_id,
        agent_meta: p.agent_meta.map(|m| AgentMeta {
            agent_type: m.agent_type,
            description: m.description.unwrap_or_default(),
        }),
        source_file: p.source_file.to_string_lossy().into_owned(),
        source_file_mtime_ms: p.source_file_mtime_ms,
        source_file_sha256: p.source_file_sha256,
        content_hash,
        first_prompt: p.first_prompt,
        ai_title: p.ai_title,
        started_at: p.started_at.unwrap_or(now),
        ended_at: p.ended_at.unwrap_or(now),
        message_count: p.message_count,
        model_usage,
        git_branch: p.git_branch,
        cwd: p.cwd,
        summary: None,
        chunk_ids: vec![],
        topic_ids: vec![],
    }
}
