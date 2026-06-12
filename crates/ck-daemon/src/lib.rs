//! Daemon entrypoint. Constructs [`DaemonState`], runs the initial scan,
//! arms the watcher, and serves the HTTP/WS API.

use anyhow::{Context, Result};
use ck_embed::BGE_SMALL_EN_V15_DIM;
use ck_pipeline::{build_local_embedder, initial_scan, spawn_watcher, DaemonState, Event};
use ck_store::{Layout, MetaIndex};
use ck_vector::VectorStore;
use std::{net::SocketAddr, path::PathBuf};
use tracing::{info, warn};

pub struct DaemonConfig {
    pub layout: Layout,
    pub projects_root: PathBuf,
    pub bind: SocketAddr,
}

pub async fn run(config: DaemonConfig) -> Result<()> {
    info!(
        bind = %config.bind,
        root = ?config.layout.root,
        projects_root = ?config.projects_root,
        "daemon starting"
    );

    if config.projects_root.is_dir() {
        if let Err(e) = config
            .layout
            .ensure_claude_projects_symlink(&config.projects_root)
        {
            warn!(error = %e, "failed to ensure sources/claude-projects symlink");
        }
    } else {
        warn!(
            "projects root {} does not exist; daemon will start anyway and the watcher will fire \
             only if it appears later",
            config.projects_root.display()
        );
    }

    let embedder = build_local_embedder(&config.layout).context("init embedder")?;
    let vector = VectorStore::connect(&config.layout, BGE_SMALL_EN_V15_DIM)
        .context("connect vector store")?;
    let meta = MetaIndex::open(&config.layout).context("open meta index")?;
    let state = DaemonState::new(config.layout, config.projects_root, embedder, vector, meta);

    // Bind and serve FIRST. The boot scan of a large historical corpus can
    // take minutes (it re-embeds whatever changed since last run); holding
    // the port closed that whole time made the app look broken — browsers got
    // ERR_CONNECTION_REFUSED instead of a "warming up" answer. The API now
    // serves immediately: /v1/health reports `indexing` with progress, and
    // queries run against whatever is indexed so far.
    let scan_state = state.clone();
    tokio::spawn(async move {
        let bg = scan_state.clone();
        let result = tokio::task::spawn_blocking(move || initial_scan(&bg)).await;
        match result {
            Ok(Ok(scanned)) => info!(sessions = scanned, "initial scan complete"),
            Ok(Err(e)) => {
                warn!(error = %e, "initial scan failed; serving whatever is already indexed")
            }
            Err(e) => warn!(error = %e, "initial scan task panicked"),
        }
        scan_state
            .indexing
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = scan_state.events.send(Event::DaemonReady);
        // Watcher arms after the boot scan so the two never race on the same
        // files. It runs on a dedicated OS thread (notify is not async).
        let _watcher_handle = spawn_watcher(scan_state.clone());
    });

    // HTTP/WS server.
    let app = ck_api::router(state);
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| {
            format!(
                "bind {} — is another ck daemon already running? \
                 `./stop.sh` (or `pkill -f 'release/ck daemon'`) clears it",
                config.bind
            )
        })?;
    let promote_enabled = ck_pipeline::promote::auto_promote_enabled();
    info!(
        addr = %config.bind,
        auto_promote = promote_enabled,
        "axum listening (auto-promote {}; recall hits are tracked unconditionally)",
        if promote_enabled { "ENABLED — hot chunks will be summarized into project CLAUDE.md" } else { "off; export CK_AUTO_PROMOTE=1 to enable" }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    info!("daemon stopped cleanly");
    Ok(())
}

/// Resolve when the process is asked to stop (Ctrl-C or SIGTERM). Without
/// this, `kill` left half-dead daemons holding the port — the README used to
/// recommend `pkill` as a workaround.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => info!("SIGINT received; shutting down"),
        _ = terminate => info!("SIGTERM received; shutting down"),
    }
}
