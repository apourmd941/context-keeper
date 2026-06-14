//! Topic clustering + cross-session edge scoring.
//!
//! Inputs: every embedding in the [`VectorStore`] (already L2-normalized) +
//! the chunk JSONs on disk (for label/description text and file-path
//! evidence).
//!
//! Per project (and a `_global` pass), we run DBSCAN on the embeddings.
//! Each cluster becomes a [`Topic`] with:
//! - `id`   = sha1 over sorted member chunk-ids (stable across re-runs while
//!   membership is stable, so labels persist across reindex)
//! - `label`/`description` = first sentence + first 200 chars of the chunk
//!   closest to the centroid. (LLM naming is M5.x.)
//! - `centroid` = mean of member embeddings (kept normalized).
//!
//! Edges:
//! - `topic-similarity`: cosine(centroid_a, centroid_b) > 0.78, capped to the
//!   top-5 neighbors per topic.
//! - `shared-file`: file-path-shaped substrings extracted from member chunks;
//!   edge if ≥3 chunks per topic mention the same path.
//! - (`session-continuation` deferred to M5.x — needs session-level
//!   timestamp comparisons.)

use chrono::Utc;
use ck_core::{ChunkId, EdgeId, ProjectId, SessionId, Topic, TopicId, TopicLink, TopicLinkKind};
use ck_store::{read_chunk, Layout};
use ck_summarize::Summarizer;
use ck_vector::{StoredVector, VectorStore};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::OnceLock,
};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum GraphError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] ck_store::StoreError),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("ndarray shape: {0}")]
    Shape(String),
}

pub type Result<T> = std::result::Result<T, GraphError>;

/// Tunables. The defaults are deliberately conservative; small projects auto-
/// downshift `eps`.
#[derive(Debug, Clone, Copy)]
pub struct ClusterParams {
    pub eps: f32,
    pub min_points: usize,
    pub small_project_eps: f32,
    pub small_project_threshold: usize,
    pub similarity_edge_threshold: f32,
    pub similarity_edge_top_k: usize,
    pub shared_file_min_chunks: usize,
}

impl Default for ClusterParams {
    fn default() -> Self {
        Self {
            eps: 0.18,
            min_points: 4,
            small_project_eps: 0.12,
            small_project_threshold: 5,
            similarity_edge_threshold: 0.78,
            similarity_edge_top_k: 5,
            shared_file_min_chunks: 3,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterReport {
    pub topics: u32,
    pub edges: u32,
    pub ungrouped_chunks: u32,
    pub clustered_chunks: u32,
    pub per_project: BTreeMap<String, ProjectReport>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectReport {
    pub topics: u32,
    pub clustered_chunks: u32,
    pub ungrouped_chunks: u32,
}

/// Cluster + score edges + persist topics and edges. Idempotent: re-running
/// with unchanged embeddings produces the same TopicIds and the same edge
/// graph (modulo any mtime-only fields).
pub fn cluster_and_persist(
    layout: &Layout,
    vector: &VectorStore,
    params: ClusterParams,
) -> Result<ClusterReport> {
    let stored = vector.all();
    if stored.is_empty() {
        warn!("vector store is empty — nothing to cluster");
        return Ok(ClusterReport {
            generated_at: Utc::now(),
            ..Default::default()
        });
    }

    let dim = stored[0].embedding.len();

    // Group chunks by project. Skip a "global" pass for M5a: per-project
    // gives cleaner topics and matches the UI's per-project landing view.
    let mut by_project: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, v) in stored.iter().enumerate() {
        by_project.entry(v.project_id.clone()).or_default().push(i);
    }

    let mut all_topics: Vec<Topic> = Vec::new();
    let mut report = ClusterReport {
        generated_at: Utc::now(),
        ..Default::default()
    };

    for (project_id, idxs) in &by_project {
        let n = idxs.len();
        debug!(project = %project_id, chunks = n, "clustering");

        // Pick eps based on project size. Tiny projects benefit from a tighter
        // radius so DBSCAN doesn't merge unrelated chunks into one blob.
        let eps = if n < params.small_project_threshold * 5 {
            params.small_project_eps
        } else {
            params.eps
        } as f64;

        let topics_this_project = match cluster_one_project(
            layout,
            &stored,
            idxs,
            dim,
            eps,
            params.min_points,
            project_id,
        ) {
            Ok(t) => t,
            Err(e) => {
                warn!(project = %project_id, error = %e, "clustering failed");
                continue;
            }
        };

        let clustered: u32 = topics_this_project.iter().map(|t| t.size).sum();
        let proj_report = ProjectReport {
            topics: topics_this_project.len() as u32,
            clustered_chunks: clustered,
            ungrouped_chunks: (n as u32).saturating_sub(clustered),
        };
        report
            .per_project
            .insert(project_id.clone(), proj_report.clone());
        report.topics += proj_report.topics;
        report.clustered_chunks += proj_report.clustered_chunks;
        report.ungrouped_chunks += proj_report.ungrouped_chunks;
        all_topics.extend(topics_this_project);
    }

    // Persist topics. Wipe stale topic JSONs that aren't in the new set, so
    // re-clustering after corpus changes doesn't leave orphans.
    let active_ids: BTreeSet<String> = all_topics.iter().map(|t| t.id.0.clone()).collect();
    purge_stale_jsons(&layout.root.join("derived/topics"), &active_ids)?;
    for t in &all_topics {
        write_topic_json(layout, t)?;
    }

    // Edges.
    let mut edges = score_topic_similarity_edges(
        &all_topics,
        params.similarity_edge_threshold,
        params.similarity_edge_top_k,
    );
    let file_edges = score_shared_file_edges(layout, &all_topics, params.shared_file_min_chunks)?;
    edges.extend(file_edges);

    let active_edge_ids: BTreeSet<String> = edges.iter().map(|e| e.id.0.clone()).collect();
    purge_stale_jsons(&layout.root.join("derived/edges"), &active_edge_ids)?;
    for e in &edges {
        write_edge_json(layout, e)?;
    }
    report.edges = edges.len() as u32;

    info!(
        topics = report.topics,
        edges = report.edges,
        clustered = report.clustered_chunks,
        ungrouped = report.ungrouped_chunks,
        "cluster persist complete"
    );
    Ok(report)
}

fn cluster_one_project(
    layout: &Layout,
    stored: &[StoredVector],
    idxs: &[usize],
    dim: usize,
    eps: f64,
    min_points: usize,
    project_id: &str,
) -> Result<Vec<Topic>> {
    let n = idxs.len();
    if n < min_points {
        return Ok(Vec::new());
    }
    let _ = dim; // dim is implicit in the stored embedding length
                 // Hand-rolled DBSCAN over cosine distance. Embeddings are L2-normalized
                 // at upsert time, so cosine_distance(a, b) = 1 - dot(a, b).
                 // Eps is interpreted as cosine distance (matches the threshold semantics
                 // documented in ClusterParams).
    let labels = dbscan_cosine(stored, idxs, eps as f32, min_points);

    // Group rows by cluster index (None = noise).
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (row, label) in labels.into_iter().enumerate() {
        if let Some(c) = label {
            groups.entry(c).or_default().push(row);
        }
    }

    let mut topics = Vec::with_capacity(groups.len());
    for (_cluster_id, rows) in groups {
        let member_idxs: Vec<usize> = rows.iter().map(|&r| idxs[r]).collect();
        let member_chunk_ids: Vec<ChunkId> = member_idxs
            .iter()
            .map(|&i| ChunkId(stored[i].chunk_id.clone()))
            .collect();
        let id = TopicId::from_members(&member_chunk_ids);

        // Centroid = mean of the (already-normalized) member embeddings,
        // re-normalized.
        let mut centroid = vec![0.0_f32; dim];
        for &i in &member_idxs {
            for (j, x) in stored[i].embedding.iter().enumerate() {
                centroid[j] += x;
            }
        }
        let m = member_idxs.len() as f32;
        for x in centroid.iter_mut() {
            *x /= m;
        }
        l2_normalize_in_place(&mut centroid);

        // Find the most-central chunk by cosine similarity to centroid.
        let mut best_i = member_idxs[0];
        let mut best_score = f32::MIN;
        for &i in &member_idxs {
            let s = dot(&centroid, &stored[i].embedding);
            if s > best_score {
                best_score = s;
                best_i = i;
            }
        }

        // Pull the chunk text for the label.
        let central_chunk_id = ChunkId(stored[best_i].chunk_id.clone());
        let central_session_id = SessionId(stored[best_i].session_id.clone());
        let (label, description) =
            chunk_label(layout, &central_session_id, &central_chunk_id, project_id);

        let session_ids: BTreeSet<String> = member_idxs
            .iter()
            .map(|&i| stored[i].session_id.clone())
            .collect();
        let project_ids: BTreeSet<String> = member_idxs
            .iter()
            .map(|&i| stored[i].project_id.clone())
            .collect();

        topics.push(Topic {
            id,
            label,
            description,
            member_chunk_ids,
            session_ids: session_ids.into_iter().map(SessionId).collect(),
            project_ids: project_ids.into_iter().map(ProjectId).collect(),
            centroid,
            size: member_idxs.len() as u32,
            created_at: Utc::now(),
            last_updated_at: Utc::now(),
        });
    }
    Ok(topics)
}

fn chunk_label(
    layout: &Layout,
    session: &SessionId,
    chunk: &ChunkId,
    fallback_project: &str,
) -> (String, String) {
    let chunk = match read_chunk(layout, session, chunk) {
        Ok(c) => c,
        Err(_) => {
            return (
                format!("topic ({fallback_project})"),
                "(label unavailable: chunk missing on disk)".into(),
            );
        }
    };
    let trimmed: String = chunk
        .text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let label = first_sentence(&trimmed, 80);
    let description = clip(&trimmed, 240);
    (label, description)
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        out.push(c);
        if out.chars().count() >= max_chars {
            break;
        }
        if matches!(c, '.' | '?' | '!' | '\n') && out.chars().count() > 8 {
            break;
        }
    }
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        "(no preview)".into()
    } else {
        trimmed
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    }
}

// ---------- edges ----------

fn score_topic_similarity_edges(topics: &[Topic], threshold: f32, top_k: usize) -> Vec<TopicLink> {
    if topics.len() < 2 {
        return Vec::new();
    }
    // For each topic, score every other topic; keep top-K above threshold.
    let mut edges_by_topic: HashMap<usize, Vec<(usize, f32)>> = HashMap::new();
    for (i, a) in topics.iter().enumerate() {
        let mut scored: Vec<(usize, f32)> = topics
            .iter()
            .enumerate()
            .filter_map(|(j, b)| {
                if i == j {
                    return None;
                }
                let s = dot(&a.centroid, &b.centroid);
                if s >= threshold {
                    Some((j, s))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        edges_by_topic.insert(i, scored);
    }

    // Materialize edges (canonicalize so each pair appears once).
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut edges = Vec::new();
    for (i, neighbors) in &edges_by_topic {
        for (j, score) in neighbors {
            let a = &topics[*i].id.0;
            let b = &topics[*j].id.0;
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            if !seen.insert((lo.clone(), hi.clone())) {
                continue;
            }
            edges.push(TopicLink {
                id: EdgeId::new("topic-similarity", lo, hi),
                kind: TopicLinkKind::TopicSimilarity,
                from_id: lo.clone(),
                to_id: hi.clone(),
                weight: ((*score - 0.78) / 0.22).clamp(0.0, 1.0),
                evidence: vec![format!("cosine={score:.3}")],
                created_at: Utc::now(),
            });
        }
    }
    edges
}

fn score_shared_file_edges(
    layout: &Layout,
    topics: &[Topic],
    min_chunks: usize,
) -> Result<Vec<TopicLink>> {
    let regex = path_regex();
    // For each topic, build set of paths that ≥ min_chunks members mention.
    let mut topic_paths: Vec<HashMap<String, u32>> = Vec::with_capacity(topics.len());
    for t in topics {
        let mut counts: HashMap<String, u32> = HashMap::new();
        for cid in &t.member_chunk_ids {
            // Discover session_id from member_chunk_ids is awkward — use the
            // first session in topic.session_ids: the chunk JSON path on disk
            // is keyed by its own session, and we have ChunkId ↔ session
            // mapping in the chunk JSON itself (chunk.session_id). Easier:
            // walk topic.session_ids and try each.
            let mut chunk_text: Option<String> = None;
            for sid in &t.session_ids {
                if let Ok(c) = read_chunk(layout, sid, cid) {
                    chunk_text = Some(c.text);
                    break;
                }
            }
            if let Some(text) = chunk_text {
                for cap in regex.find_iter(&text) {
                    let path = cap
                        .as_str()
                        .trim_end_matches(['.', ',', ';', ':', ')', '"']);
                    *counts.entry(path.to_string()).or_insert(0) += 1;
                }
            }
        }
        topic_paths.push(counts);
    }

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut edges = Vec::new();
    for i in 0..topics.len() {
        for j in (i + 1)..topics.len() {
            let shared: Vec<String> = topic_paths[i]
                .iter()
                .filter(|(p, n)| {
                    **n >= min_chunks as u32
                        && topic_paths[j].get(*p).copied().unwrap_or(0) >= min_chunks as u32
                })
                .map(|(p, _)| p.clone())
                .collect();
            if shared.is_empty() {
                continue;
            }
            let a = &topics[i].id.0;
            let b = &topics[j].id.0;
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            if !seen.insert((lo.clone(), hi.clone())) {
                continue;
            }
            let weight = ((shared.len() as f32) / 5.0).min(1.0);
            edges.push(TopicLink {
                id: EdgeId::new("shared-file", lo, hi),
                kind: TopicLinkKind::SharedFile,
                from_id: lo.clone(),
                to_id: hi.clone(),
                weight,
                evidence: shared.into_iter().take(5).collect(),
                created_at: Utc::now(),
            });
        }
    }
    Ok(edges)
}

fn path_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Rough but practical: file paths starting with `/`, `~/`, or `./`,
        // and URLs.
        Regex::new(
            r#"(?x)
            (?:
                (?:/|~/|\./)[A-Za-z0-9_./-]+\.[A-Za-z0-9]{1,8}
                |
                https?://[^\s<>"'`]+
            )
        "#,
        )
        .expect("path regex")
    })
}

// ---------- persistence helpers ----------

fn write_topic_json(layout: &Layout, t: &Topic) -> Result<()> {
    let path = layout
        .root
        .join("derived/topics")
        .join(format!("{}.json", t.id.0));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(t)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn write_edge_json(layout: &Layout, e: &TopicLink) -> Result<()> {
    let path = layout
        .root
        .join("derived/edges")
        .join(format!("{}.json", e.id.0));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(e)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn purge_stale_jsons(dir: &PathBuf, active: &BTreeSet<String>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !active.contains(&stem) {
            let _ = fs::remove_file(&path);
            debug!(?path, "purged stale json");
        }
    }
    Ok(())
}

// ---------- LLM topic naming (M6) ----------

/// For every topic on disk, ask `summarizer` for a short human label.
/// Cached on `topic_id` (which is content-derived from the cluster's member
/// chunk-ids, so an unchanged topic re-uses the same name across reindexes).
///
/// Returns the count of topics that got *new* names (as opposed to a cache
/// hit or a no-op).
pub async fn rename_topics_with_summarizer<S: Summarizer + Sync>(
    layout: &Layout,
    summarizer: &S,
) -> Result<u32> {
    let topics = list_topics(layout)?;
    if topics.is_empty() {
        return Ok(0);
    }
    let cache_root = layout.root.join("cache/topic-names");
    fs::create_dir_all(&cache_root)?;

    let mut renamed = 0u32;
    for mut topic in topics {
        let cache_path = cache_root.join(format!("{}.txt", topic.id.0));
        let new_label = if let Ok(cached) = fs::read_to_string(&cache_path) {
            cached.trim().to_string()
        } else {
            let user = build_topic_naming_prompt(layout, &topic);
            let raw = match summarizer.complete(TOPIC_NAMING_SYSTEM, &user, 32).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(topic = %topic.id.0, error = %e, "name failed");
                    continue;
                }
            };
            let cleaned = clean_topic_name(&raw);
            if cleaned.is_empty() {
                warn!(topic = %topic.id.0, raw = %raw, "empty topic name; skipping");
                continue;
            }
            let _ = fs::write(&cache_path, &cleaned);
            renamed += 1;
            info!(topic = %topic.id.0, name = %cleaned, "named");
            cleaned
        };
        if topic.label == new_label {
            continue;
        }
        topic.label = new_label;
        topic.last_updated_at = Utc::now();
        write_topic_json(layout, &topic)?;
    }
    Ok(renamed)
}

const TOPIC_NAMING_SYSTEM: &str = "You name conversation topics for a code-assistant memory system.\n\n\
Given excerpts from a related set of conversation chunks, output ONLY the topic name in 3-6 words.\n\
\n\
Rules:\n\
- No quotes, no preamble, no trailing punctuation.\n\
- Be specific. Prefer concrete nouns over generic phrasing.\n\
- Reflect what the chunks are *about*, not who wrote them.\n\
- Good: \"Hand-rolled DBSCAN clustering\" / \"Anthropic API client setup\".\n\
- Bad: \"Tool calls\" / \"Discussion\" / \"Various\".";

fn build_topic_naming_prompt(layout: &Layout, topic: &Topic) -> String {
    // Use the most-central chunk first (which is the existing label seed),
    // plus up to 3 other member chunks for context. Keep total under ~3000
    // chars so the call is cheap.
    let mut buf = String::with_capacity(2048);
    buf.push_str("Centroid chunk:\n");
    buf.push_str(topic.description.as_str());
    buf.push_str("\n\n");

    let mut samples = 0;
    for cid in topic.member_chunk_ids.iter().skip(1) {
        if samples >= 3 {
            break;
        }
        for sid in &topic.session_ids {
            if let Ok(c) = read_chunk(layout, sid, cid) {
                buf.push_str("Sample chunk:\n");
                buf.push_str(&clip(&c.text, 600));
                buf.push_str("\n\n");
                samples += 1;
                break;
            }
        }
    }
    buf.push_str(&format!(
        "There are {} chunks in this topic across {} session(s).",
        topic.size,
        topic.session_ids.len()
    ));
    buf
}

fn clean_topic_name(raw: &str) -> String {
    raw.trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c.is_whitespace())
        .to_string()
}

// ---------- public read helpers ----------

pub fn list_topics(layout: &Layout) -> Result<Vec<Topic>> {
    let dir = layout.root.join("derived/topics");
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(t) = serde_json::from_slice::<Topic>(&bytes) {
            out.push(t);
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(out)
}

pub fn list_edges(layout: &Layout) -> Result<Vec<TopicLink>> {
    let dir = layout.root.join("derived/edges");
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = match fs::read(entry.path()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if let Ok(e) = serde_json::from_slice::<TopicLink>(&bytes) {
            out.push(e);
        }
    }
    Ok(out)
}

// ---------- DBSCAN (cosine distance, hand-rolled) ----------

/// Returns a `Vec<Option<usize>>` parallel to `idxs`: `Some(cluster_id)` for
/// core/border points, `None` for noise.
fn dbscan_cosine(
    stored: &[StoredVector],
    idxs: &[usize],
    eps: f32,
    min_points: usize,
) -> Vec<Option<usize>> {
    let n = idxs.len();
    let mut visited = vec![false; n];
    let mut labels: Vec<Option<usize>> = vec![None; n];
    let mut next_cluster = 0usize;

    // Precompute neighbors lazily, but cache once requested. For 400 points
    // in 384-dim this is microseconds either way.
    let neighbors = |p: usize| -> Vec<usize> {
        let mut out = Vec::new();
        let a = &stored[idxs[p]].embedding;
        for (q, &q_idx) in idxs.iter().enumerate() {
            if q == p {
                continue;
            }
            let b = &stored[q_idx].embedding;
            // distance = 1 - dot, with both normalized to unit length.
            let dist = 1.0 - dot(a, b);
            if dist <= eps {
                out.push(q);
            }
        }
        out
    };

    for p in 0..n {
        if visited[p] {
            continue;
        }
        visited[p] = true;
        let mut nb = neighbors(p);
        if nb.len() + 1 < min_points {
            // p is noise (could be reassigned later by another cluster).
            continue;
        }
        // Form a new cluster with p.
        let cluster = next_cluster;
        next_cluster += 1;
        labels[p] = Some(cluster);

        // Expand using a BFS-like queue.
        let mut i = 0;
        while i < nb.len() {
            let q = nb[i];
            i += 1;
            if !visited[q] {
                visited[q] = true;
                let q_nb = neighbors(q);
                if q_nb.len() + 1 >= min_points {
                    // Add q's neighbors to the queue (dedup).
                    for r in q_nb {
                        if !nb.contains(&r) {
                            nb.push(r);
                        }
                    }
                }
            }
            if labels[q].is_none() {
                labels[q] = Some(cluster);
            }
        }
    }
    labels
}

// ---------- math helpers ----------

fn l2_normalize_in_place(v: &mut [f32]) {
    let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag == 0.0 {
        return;
    }
    for x in v.iter_mut() {
        *x /= mag;
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Set a topic's name (and optionally description) from an external
/// contributor — the MCP path where Claude itself names topics through the
/// plugin, no API key involved. Persists to the same `cache/topic-names/`
/// file the LLM path uses, so the name survives reclustering (topic ids are
/// content-derived). Returns false when no such topic exists.
pub fn set_topic_name(
    layout: &Layout,
    topic_id: &str,
    label: &str,
    description: Option<&str>,
) -> Result<bool> {
    let topics = list_topics(layout)?;
    let Some(mut topic) = topics.into_iter().find(|t| t.id.0 == topic_id) else {
        return Ok(false);
    };
    let cache_root = layout.root.join("cache/topic-names");
    fs::create_dir_all(&cache_root)?;
    let _ = fs::write(cache_root.join(format!("{topic_id}.txt")), label);
    topic.label = label.to_string();
    if let Some(d) = description {
        topic.description = d.to_string();
    }
    topic.last_updated_at = Utc::now();
    write_topic_json(layout, &topic)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentence_caps_at_max_chars() {
        let s = "this is a sentence that goes on and on and on and on and on and on and on and on and on";
        let f = first_sentence(s, 30);
        assert!(f.chars().count() <= 30);
    }

    #[test]
    fn first_sentence_breaks_on_period() {
        let s = "Short sentence. Then another one that should not appear.";
        let f = first_sentence(s, 200);
        assert_eq!(f, "Short sentence.");
    }

    #[test]
    fn path_regex_finds_paths_and_urls() {
        let r = path_regex();
        let text =
            "see /Users/me/Development/foo.rs and https://example.com/page.html and /etc/hosts.txt";
        let hits: Vec<&str> = r.find_iter(text).map(|m| m.as_str()).collect();
        assert!(hits.iter().any(|h| h.contains("foo.rs")));
        assert!(hits.iter().any(|h| h.starts_with("https://example.com")));
    }

    #[test]
    fn similarity_edges_respect_threshold_and_top_k() {
        let topics = vec![
            mk_topic("t1", &[1.0, 0.0]),
            mk_topic("t2", &[0.95, 0.05]),
            mk_topic("t3", &[0.0, 1.0]),
            mk_topic("t4", &[0.9, 0.1]),
        ];
        let edges = score_topic_similarity_edges(&topics, 0.78, 5);
        // t1 close to t2 and t4 (cos > 0.78); t3 close to none.
        assert!(!edges.is_empty());
        for e in &edges {
            assert!(matches!(e.kind, TopicLinkKind::TopicSimilarity));
            assert!(e.weight >= 0.0 && e.weight <= 1.0);
        }
    }

    fn mk_topic(id: &str, centroid_in: &[f32]) -> Topic {
        let mut c = centroid_in.to_vec();
        l2_normalize_in_place(&mut c);
        Topic {
            id: TopicId(id.into()),
            label: id.into(),
            description: id.into(),
            member_chunk_ids: vec![],
            session_ids: vec![],
            project_ids: vec![],
            centroid: c,
            size: 1,
            created_at: Utc::now(),
            last_updated_at: Utc::now(),
        }
    }
}
