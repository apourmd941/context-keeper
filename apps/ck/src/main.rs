use anyhow::{Context, Result};
use chrono::Utc;
use ck_chunk::chunk_session;
use ck_core::{hex_encode, AgentMeta, EmbeddingRef, ModelUsage, Session};
use ck_embed::{embed_with_cache, Embedder, LocalEmbedder, BGE_SMALL_EN_V15_DIM};
use ck_store::{read_chunk_text, write_chunk, write_session, Layout, MetaIndex};
use ck_transcript::{discover_projects, parse_session_file, parse_session_records, ParsedSession};
use ck_vector::VectorStore;
use clap::{Parser, Subcommand};
use directories::BaseDirs;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, time::Instant};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "ck", version, about = "context-keeper")]
struct Cli {
    /// Override the context-keeper root (default: ~/.context-keeper)
    #[arg(long, env = "CK_ROOT", global = true)]
    root: Option<PathBuf>,

    /// Override the Claude Code projects root (default: ~/.claude/projects)
    #[arg(long, env = "CK_CLAUDE_PROJECTS", global = true)]
    claude_projects: Option<PathBuf>,

    #[arg(long, default_value = "info", global = true)]
    log_level: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Walk transcripts and report what's there. Persists raw Session records
    /// unless `--dry-run` is passed. Pass `--with-embeddings` to also chunk,
    /// embed, and index for `ck search`.
    Doctor {
        #[arg(long)]
        dry_run: bool,
        /// Chunk + embed + upsert into the vector index. First run downloads
        /// the BGE-small-en-v1.5 model (~130MB) to ~/.context-keeper/cache/models/.
        #[arg(long)]
        with_embeddings: bool,
    },
    /// Run the long-lived indexer + HTTP/WebSocket API. Watches
    /// ~/.claude/projects/ for changes and re-indexes affected sessions.
    Daemon {
        /// Address the HTTP/WS server binds to. Always loopback-only.
        #[arg(long, default_value = "127.0.0.1:7421")]
        bind: SocketAddr,
    },
    /// Run the MCP stdio shim. Forwards `recall`, `list_sessions`, and
    /// `list_projects` tool calls to a running `ck daemon`'s HTTP API.
    /// Wire into Claude Code with: `claude mcp add context-keeper -- /path/to/ck mcp`.
    Mcp,
    /// Generate per-session LLM summaries via the Anthropic API. Skips
    /// sessions whose chunks haven't changed since the last summary
    /// (cache-keyed on input_hash). Requires ANTHROPIC_API_KEY.
    Summarize {
        /// Restrict to a single session id.
        #[arg(long)]
        session: Option<String>,
        /// Force re-summarize even when the cached input_hash matches.
        #[arg(long)]
        force: bool,
        /// Override model name (default: claude-haiku-4-5).
        #[arg(long)]
        model: Option<String>,
    },
    /// Re-cluster all chunks into topics and rescore cross-topic edges.
    /// Reads from the vector store + chunk JSONs; writes
    /// derived/topics/*.json and derived/edges/*.json. Cheap (~ms for v0.1
    /// corpora); can run anytime after `ck doctor --with-embeddings`.
    Cluster,
    /// Reindex everything from scratch (planned for M2.x).
    Reindex {
        #[arg(long)]
        rebuild_index: bool,
    },
    /// Search across all indexed chunks. Requires `ck doctor --with-embeddings`
    /// to have populated the vector store.
    Search {
        query: String,
        /// Restrict to a specific project id (e.g. `-Users-me-Development`).
        #[arg(long)]
        project: Option<String>,
        /// Max number of hits to return.
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Print daemon status (planned for M3).
    Status,
    /// Install or remove start-on-login for the daemon (macOS launchd).
    Autostart {
        /// install | remove | status
        action: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    match &cli.command {
        Command::Doctor {
            dry_run,
            with_embeddings,
        } => doctor(&cli, *dry_run, *with_embeddings),
        Command::Daemon { bind } => daemon(&cli, *bind),
        Command::Mcp => mcp(&cli),
        Command::Summarize {
            session,
            force,
            model,
        } => summarize(&cli, session.as_deref(), *force, model.as_deref()),
        Command::Cluster => cluster(&cli),
        Command::Reindex { .. } => not_yet("reindex", "M2.x"),
        Command::Search {
            query,
            project,
            limit,
        } => search(&cli, query, project.as_deref(), *limit),
        Command::Status => not_yet("status", "M3"),
        Command::Autostart { action } => autostart(action),
    }
}

fn cluster(cli: &Cli) -> Result<()> {
    let layout = open_layout(cli)?;
    let store = ck_vector::VectorStore::connect(&layout, ck_embed::BGE_SMALL_EN_V15_DIM)
        .context("open vector store (run `ck doctor --with-embeddings` first?)")?;
    if store.is_empty() {
        eprintln!("vector store is empty — run `ck doctor --with-embeddings` first.");
        std::process::exit(1);
    }
    let report = ck_graph::cluster_and_persist(&layout, &store, ck_graph::ClusterParams::default())
        .context("cluster_and_persist")?;
    println!();
    println!("Topics:    {}", report.topics);
    println!("Edges:     {}", report.edges);
    println!("Clustered: {}", report.clustered_chunks);
    println!("Ungrouped: {}", report.ungrouped_chunks);
    println!();
    println!("Per-project:");
    for (project, p) in &report.per_project {
        println!(
            "  {project:<48}  topics={:>3}  clustered={:>4}  ungrouped={:>4}",
            p.topics, p.clustered_chunks, p.ungrouped_chunks
        );
    }

    // Auto-name topics with the LLM when the orchestrator is provisioned (it
    // holds the cloud key + enforces this app's egress ceiling — R1-016), or when
    // a key is set directly (dev). Cached per-topic on disk, so re-running
    // clustering is free unless cluster membership actually changed.
    let orchestrator_ready = std::env::var_os("HOME")
        .map(|h| {
            std::path::Path::new(&h)
                .join(".selran/loopback.badge")
                .exists()
        })
        .unwrap_or(false);
    if orchestrator_ready || std::env::var("ANTHROPIC_API_KEY").is_ok() {
        println!();
        println!("Naming topics with the LLM via the orchestrator (cached per topic_id)…");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        let renamed: anyhow::Result<u32> = rt.block_on(async {
            let summarizer = ck_summarize::OrchestratorSummarizer::from_env()?;
            let n = ck_graph::rename_topics_with_summarizer(&layout, &summarizer).await?;
            Ok(n)
        });
        match renamed {
            Ok(n) => println!("Topics named (new): {n}"),
            Err(e) => warn!(error = %e, "LLM naming failed; existing labels retained"),
        }
    } else {
        println!();
        println!(
            "(set ANTHROPIC_API_KEY to auto-rename topics with the LLM; otherwise auto-labels are used)"
        );
    }
    Ok(())
}

fn mcp(_cli: &Cli) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(ck_mcp::run(ck_mcp::McpConfig::default()))?;
    Ok(())
}

fn summarize(
    cli: &Cli,
    session_filter: Option<&str>,
    force: bool,
    model_override: Option<&str>,
) -> Result<()> {
    let layout = open_layout(cli)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(summarize_async(
        &layout,
        session_filter,
        force,
        model_override,
    ))
}

async fn summarize_async(
    layout: &Layout,
    session_filter: Option<&str>,
    force: bool,
    model_override: Option<&str>,
) -> Result<()> {
    use ck_summarize::{summarize_with_cache, OrchestratorSummarizer, Summarizer};

    // Route summarization through the orchestrator (R1-016): it holds the cloud
    // key, enforces context-keeper's egress ceiling, and routes by policy.
    let mut summarizer = OrchestratorSummarizer::from_env()
        .context("OrchestratorSummarizer (is the Selran orchestrator running?)")?;
    if let Some(m) = model_override {
        summarizer = summarizer.with_model(m);
    }
    info!(model = summarizer.model_name(), "summarizer ready");

    let dir = layout.sessions_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .context("read sessions dir")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        warn!(
            "no sessions found under {} — run `ck doctor --with-embeddings` first",
            dir.display()
        );
        return Ok(());
    }

    let mut summarized = 0u32;
    let mut cache_hits = 0u32;
    let mut skipped = 0u32;

    for path in paths {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                warn!(?path, error = %e, "skip: read failed");
                continue;
            }
        };
        let mut session: ck_core::Session = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                warn!(?path, error = %e, "skip: parse failed");
                continue;
            }
        };
        if let Some(filter) = session_filter {
            if session.id.0 != filter {
                continue;
            }
        }

        let mut chunks: Vec<ck_core::Chunk> = Vec::with_capacity(session.chunk_ids.len());
        for cid in &session.chunk_ids {
            match ck_store::read_chunk(layout, &session.id, cid) {
                Ok(c) => chunks.push(c),
                Err(e) => warn!(chunk = %cid.0, error = %e, "missing chunk"),
            }
        }
        if chunks.is_empty() {
            skipped += 1;
            continue;
        }

        let new_hash = ck_summarize::input_hash(summarizer.model_name(), &chunks);
        if !force {
            if let Some(existing) = &session.summary {
                if existing.input_hash == new_hash {
                    cache_hits += 1;
                    println!(
                        "{:<48}  {:>4} chunks  cached  {}",
                        truncate(&session.id.0, 48),
                        chunks.len(),
                        truncate(&existing.text, 70)
                    );
                    continue;
                }
            }
        }

        match summarize_with_cache(&summarizer, layout, &chunks).await {
            Ok((summary, was_cached)) => {
                session.summary = Some(summary.clone());
                ck_store::write_session(layout, &session).context("persist session summary")?;
                summarized += 1;
                if was_cached {
                    cache_hits += 1;
                }
                println!(
                    "{:<48}  {:>4} chunks  {}  {}",
                    truncate(&session.id.0, 48),
                    chunks.len(),
                    if was_cached { "cache" } else { " new " },
                    truncate(&summary.text, 70)
                );
            }
            Err(e) => {
                warn!(session = %session.id.0, error = %e, "summarize failed");
            }
        }
    }

    println!();
    println!("Summarized: {summarized}");
    println!("Cache hits: {cache_hits}");
    if skipped > 0 {
        println!("Skipped (no chunks): {skipped}");
    }
    Ok(())
}

fn daemon(cli: &Cli, bind: SocketAddr) -> Result<()> {
    let layout = open_layout(cli)?;
    let projects_root = resolve_claude_projects(cli)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(ck_daemon::run(ck_daemon::DaemonConfig {
        layout,
        projects_root,
        bind,
    }))
}

fn init_tracing(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn not_yet(name: &str, milestone: &str) -> Result<()> {
    eprintln!("`ck {name}` is not implemented yet (planned for {milestone}).");
    std::process::exit(2);
}

fn doctor(cli: &Cli, dry_run: bool, with_embeddings: bool) -> Result<()> {
    let layout = open_layout(cli)?;
    let projects_root = resolve_claude_projects(cli)?;
    info!(?projects_root, root = ?layout.root, dry_run, with_embeddings, "starting doctor");

    if !dry_run && projects_root.is_dir() {
        layout.ensure_claude_projects_symlink(&projects_root)?;
    }

    let projects = discover_projects(&projects_root)?;
    if projects.is_empty() {
        warn!(
            "no projects found under {} — is Claude Code installed and used here?",
            projects_root.display()
        );
        return Ok(());
    }

    let mut meta = if dry_run {
        None
    } else {
        Some(MetaIndex::open(&layout)?)
    };

    // Embedder + vector store are only constructed when requested. The
    // embedder ctor downloads ~130MB on first run.
    let embedder = if with_embeddings && !dry_run {
        Some(LocalEmbedder::new(&layout).context("init local embedder")?)
    } else {
        None
    };
    let mut vector_store = if with_embeddings && !dry_run {
        Some(VectorStore::connect(&layout, BGE_SMALL_EN_V15_DIM).context("open vector store")?)
    } else {
        None
    };

    let mut total_sessions: u32 = 0;
    let mut total_subagents: u32 = 0;
    let mut total_messages: u32 = 0;
    let mut total_chunks: u32 = 0;
    let mut total_new_embeds: u32 = 0;
    let mut total_cache_hits: u32 = 0;
    let mut combined_unknown: BTreeMap<String, u32> = Default::default();
    let mut combined_types: BTreeMap<String, u32> = Default::default();

    println!();
    println!(
        "{:<48}  {:<38}  {:>5}  {:>5}  {:>5}  TITLE",
        "PROJECT", "SESSION", "MSGS", "USER", "ASST"
    );
    println!("{}", "-".repeat(120));

    for project in projects {
        let mut all =
            Vec::with_capacity(project.session_files.len() + project.subagent_files.len());
        for f in &project.session_files {
            all.push((false, f.clone()));
        }
        for f in &project.subagent_files {
            all.push((true, f.clone()));
        }
        for (is_sub, file) in all {
            let parsed = match parse_session_file(&project.id, &file) {
                Ok(p) => p,
                Err(e) => {
                    warn!(file = ?file, error = %e, "failed to parse session");
                    continue;
                }
            };
            if is_sub {
                total_subagents += 1;
            } else {
                total_sessions += 1;
            }
            total_messages += parsed.message_count;
            for (k, v) in &parsed.stats.unknown_types {
                *combined_unknown.entry(k.clone()).or_insert(0) += v;
            }
            for (k, v) in &parsed.stats.types {
                *combined_types.entry(k.clone()).or_insert(0) += v;
            }
            let title = parsed.ai_title.clone().unwrap_or_default();
            let label_id = if is_sub {
                format!("↳ {}", parsed.session_id.0)
            } else {
                parsed.session_id.0.clone()
            };
            println!(
                "{:<48}  {:<38}  {:>5}  {:>5}  {:>5}  {}",
                truncate(&project.id.0, 48),
                truncate(&label_id, 38),
                parsed.message_count,
                parsed.user_count,
                parsed.assistant_count,
                truncate(&title, 40),
            );

            if dry_run {
                continue;
            }

            let mut session = parsed_to_session(parsed);
            if let Some(meta) = meta.as_mut() {
                meta.upsert_session(&session)?;
            }

            if with_embeddings {
                match index_session_chunks(
                    &layout,
                    &file,
                    &mut session,
                    embedder
                        .as_ref()
                        .expect("embedder present when with_embeddings"),
                    vector_store
                        .as_mut()
                        .expect("vector store present when with_embeddings"),
                    meta.as_mut(),
                ) {
                    Ok((n_chunks, new, hits)) => {
                        total_chunks += n_chunks;
                        total_new_embeds += new;
                        total_cache_hits += hits;
                        if n_chunks > 0 {
                            println!(
                                "    chunks: {n_chunks}   embedded: {new} new, {hits} cache hits"
                            );
                        }
                    }
                    Err(e) => {
                        warn!(session = %session.id.0, error = %e, "embedding failed");
                    }
                }
            }

            write_session(&layout, &session)?;
        }
    }

    println!();
    println!("Sessions:           {total_sessions}");
    println!("Subagent sessions:  {total_subagents}");
    println!("Messages:           {total_messages}");
    if with_embeddings {
        println!("Chunks indexed:     {total_chunks}");
        println!("Embeddings (new):   {total_new_embeds}");
        println!("Embeddings (hit):   {total_cache_hits}");
    }
    if !combined_types.is_empty() {
        println!("Record-type histogram:");
        for (k, v) in &combined_types {
            println!("  {k:<32} {v}");
        }
    }
    if combined_unknown.is_empty() {
        println!("Unknown record types: none");
    } else {
        println!("Unknown record types (potential schema drift):");
        for (k, v) in combined_unknown {
            println!("  - {k}: {v}");
        }
    }
    Ok(())
}

/// Returns (chunk_count, new_embeds, cache_hits).
fn index_session_chunks(
    layout: &Layout,
    file: &std::path::Path,
    session: &mut Session,
    embedder: &LocalEmbedder,
    vector_store: &mut VectorStore,
    meta: Option<&mut MetaIndex>,
) -> Result<(u32, u32, u32)> {
    let records = parse_session_records(file)?;
    let mut chunks = chunk_session(&session.id, &session.project_id, &records);
    if chunks.is_empty() {
        return Ok((0, 0, 0));
    }
    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let outcome = embed_with_cache(embedder, layout, &texts)?;
    // Stamp embedding refs onto chunks.
    for (chunk, hash) in chunks.iter_mut().zip(outcome.hashes.iter()) {
        chunk.embedding_ref = Some(EmbeddingRef {
            model: embedder.model_name().to_string(),
            sha256: hash.clone(),
        });
    }
    vector_store.upsert_chunks(&chunks, &outcome.embeddings)?;
    if let Some(meta) = meta {
        meta.upsert_chunks(&chunks)?;
    }
    for chunk in &chunks {
        write_chunk(layout, chunk)?;
    }
    session.chunk_ids = chunks.iter().map(|c| c.id.clone()).collect();
    Ok((chunks.len() as u32, outcome.new_embeds, outcome.cache_hits))
}

fn search(cli: &Cli, query: &str, project: Option<&str>, limit: u32) -> Result<()> {
    let layout = open_layout(cli)?;
    let t_total = Instant::now();

    let embedder = LocalEmbedder::new(&layout).context("init embedder")?;
    let store = VectorStore::connect(&layout, embedder.dim())
        .context("open vector store (run `ck doctor --with-embeddings` first?)")?;
    if store.is_empty() {
        eprintln!("vector store is empty — run `ck doctor --with-embeddings` first.");
        std::process::exit(1);
    }

    let t_embed = Instant::now();
    let outcome = embed_with_cache(&embedder, &layout, &[query.to_string()])?;
    let embed_ms = t_embed.elapsed().as_millis();
    let q_vec = &outcome.embeddings[0];

    let t_search = Instant::now();
    let hits = store.search(q_vec, limit as usize, project)?;
    let search_ms = t_search.elapsed().as_millis();

    println!();
    println!("query: {query}");
    if let Some(p) = project {
        println!("project filter: {p}");
    }
    println!("hits: {} of {}", hits.len(), store.len());
    println!("{}", "-".repeat(80));
    for (i, hit) in hits.iter().enumerate() {
        let snippet = read_chunk_text(&layout, &hit.session_id, &hit.chunk_id)
            .map(|t| snippet(&t, 220))
            .unwrap_or_else(|_| "<chunk text unavailable>".to_string());
        let title = match ck_store::read_session(&layout, &hit.session_id) {
            Ok(s) => s
                .ai_title
                .unwrap_or_else(|| s.first_prompt.unwrap_or_default()),
            Err(_) => String::new(),
        };
        println!(
            "[{:>2}] score={:.3}  session={}  project={}",
            i + 1,
            hit.score,
            shorten(&hit.session_id.0, 12),
            hit.project_id.0
        );
        if !title.is_empty() {
            println!("     title: {}", truncate(&title, 100));
        }
        println!("     {}", snippet);
        println!();
    }

    let total_ms = t_total.elapsed().as_millis();
    println!("elapsed: {total_ms}ms total (embed {embed_ms}ms + search {search_ms}ms)");
    Ok(())
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

fn open_layout(cli: &Cli) -> Result<Layout> {
    let root = match &cli.root {
        Some(r) => r.clone(),
        None => Layout::default_root()?,
    };
    let layout = Layout::new_at(root);
    layout.ensure()?;
    Ok(layout)
}

fn resolve_claude_projects(cli: &Cli) -> Result<PathBuf> {
    if let Some(p) = &cli.claude_projects {
        return Ok(p.clone());
    }
    let base = BaseDirs::new().ok_or_else(|| anyhow::anyhow!("home directory not found"))?;
    Ok(base.home_dir().join(".claude/projects"))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

fn shorten(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn snippet(text: &str, max_chars: usize) -> String {
    let collapsed: String = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    truncate(&collapsed, max_chars)
}

// ---------------------------------------------------------------------------
// Autostart (macOS launchd)
// ---------------------------------------------------------------------------

const LAUNCHD_LABEL: &str = "com.selran.context-keeper";

fn autostart(action: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!(
            "autostart is macOS-only for now; on Linux create a systemd user \
             unit running ./start.sh (see README)"
        );
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    let plist = std::path::PathBuf::from(&home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    match action {
        "install" => {
            // start.sh lives at the repo root; the running binary is
            // <repo>/target/release/ck.
            let exe = std::env::current_exe().context("current_exe")?;
            let repo = exe
                .ancestors()
                .nth(3)
                .context("cannot locate repo root from binary path")?
                .to_path_buf();
            let start = repo.join("start.sh");
            anyhow::ensure!(
                start.is_file(),
                "start.sh not found at {} — run autostart from a repo build",
                start.display()
            );
            let log = std::path::PathBuf::from(&home).join(".context-keeper/launchd.log");
            let body = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array>
    <string>/bin/bash</string>
    <string>{start}</string>
  </array>
  <key>WorkingDirectory</key><string>{repo}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#,
                start = start.display(),
                repo = repo.display(),
                log = log.display(),
            );
            std::fs::create_dir_all(plist.parent().unwrap())?;
            std::fs::write(&plist, body)?;
            let uid = unsafe { libc_getuid() };
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
                .output();
            let out = std::process::Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
                .output()
                .context("launchctl bootstrap")?;
            anyhow::ensure!(
                out.status.success(),
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            println!("✓ autostart installed — context-keeper starts at login");
            println!("  plist: {}", plist.display());
            Ok(())
        }
        "remove" => {
            let uid = unsafe { libc_getuid() };
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
                .output();
            if plist.is_file() {
                std::fs::remove_file(&plist)?;
            }
            println!("✓ autostart removed");
            Ok(())
        }
        "status" => {
            let installed = plist.is_file();
            let uid = unsafe { libc_getuid() };
            let loaded = std::process::Command::new("launchctl")
                .args(["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            println!(
                "plist: {} · launchd: {}",
                if installed { "installed" } else { "absent" },
                if loaded { "loaded" } else { "not loaded" }
            );
            Ok(())
        }
        other => anyhow::bail!("unknown autostart action '{other}' (install|remove|status)"),
    }
}

unsafe fn libc_getuid() -> u32 {
    // Avoid a libc dependency for one syscall: `id -u` is universal on macOS.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(501)
}
