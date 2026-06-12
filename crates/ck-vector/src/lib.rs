//! Flat-file linear-scan vector store.
//!
//! For v0.1 corpora (hundreds-to-low-thousands of chunks) a brute-force
//! cosine scan is faster, simpler, and far smaller than dragging in LanceDB
//! or any ANN crate. The index lives at
//! `~/.context-keeper/index/vectors.bin` and is bincode-serialized.
//!
//! When the corpus grows past ~50k chunks (or M3+ wants concurrent writers)
//! swap this for an ANN backend behind the same `VectorStore` API.

use ck_core::{Chunk, ChunkId, ProjectId, SessionId};
use ck_store::Layout;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, io, path::PathBuf};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("dim mismatch: expected {expected}, got {got}")]
    DimMismatch { expected: usize, got: usize },
    #[error("upsert: chunks ({chunks}) and embeddings ({embeddings}) length mismatch")]
    LenMismatch { chunks: usize, embeddings: usize },
}

pub type Result<T> = std::result::Result<T, VectorError>;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub chunk_id: ChunkId,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub score: f32,
}

/// Read-only view of one stored record. Used by ck-graph for clustering.
#[derive(Debug, Clone)]
pub struct StoredVector {
    pub chunk_id: String,
    pub session_id: String,
    pub project_id: String,
    pub started_at: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    chunk_id: String,
    session_id: String,
    project_id: String,
    started_at: String,
    embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OnDisk {
    dim: usize,
    records: Vec<Record>,
}

/// Flat-file vector store. All records loaded into memory on `connect`.
pub struct VectorStore {
    path: PathBuf,
    dim: usize,
    by_id: HashMap<String, usize>,
    records: Vec<Record>,
}

impl VectorStore {
    /// Open or create the store. If a file exists with a different `dim`,
    /// returns [`VectorError::DimMismatch`].
    pub fn connect(layout: &Layout, dim: usize) -> Result<Self> {
        let path = layout.vectors_bin_path();
        let on_disk = if path.is_file() {
            let bytes = fs::read(&path)?;
            let v: OnDisk = bincode::deserialize(&bytes)?;
            if v.dim != dim {
                return Err(VectorError::DimMismatch {
                    expected: v.dim,
                    got: dim,
                });
            }
            v
        } else {
            OnDisk {
                dim,
                records: Vec::new(),
            }
        };
        let mut by_id = HashMap::with_capacity(on_disk.records.len());
        for (i, r) in on_disk.records.iter().enumerate() {
            by_id.insert(r.chunk_id.clone(), i);
        }
        Ok(Self {
            path,
            dim,
            by_id,
            records: on_disk.records,
        })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Snapshot of every record. Cheap at v0.1 corpus sizes (cloning a few
    /// hundred 384-float vectors is sub-millisecond).
    pub fn all(&self) -> Vec<StoredVector> {
        self.records
            .iter()
            .map(|r| StoredVector {
                chunk_id: r.chunk_id.clone(),
                session_id: r.session_id.clone(),
                project_id: r.project_id.clone(),
                started_at: r.started_at.clone(),
                embedding: r.embedding.clone(),
            })
            .collect()
    }

    /// Insert or replace records keyed by `chunk_id`. Persists on success.
    pub fn upsert_chunks(&mut self, chunks: &[Chunk], embeddings: &[Vec<f32>]) -> Result<()> {
        if chunks.len() != embeddings.len() {
            return Err(VectorError::LenMismatch {
                chunks: chunks.len(),
                embeddings: embeddings.len(),
            });
        }
        for (chunk, emb) in chunks.iter().zip(embeddings.iter()) {
            if emb.len() != self.dim {
                return Err(VectorError::DimMismatch {
                    expected: self.dim,
                    got: emb.len(),
                });
            }
            let normalized = l2_normalize(emb);
            let rec = Record {
                chunk_id: chunk.id.0.clone(),
                session_id: chunk.session_id.0.clone(),
                project_id: chunk.project_id.0.clone(),
                started_at: chunk.started_at.to_rfc3339(),
                embedding: normalized,
            };
            match self.by_id.get(&rec.chunk_id) {
                Some(&i) => self.records[i] = rec,
                None => {
                    let i = self.records.len();
                    self.by_id.insert(rec.chunk_id.clone(), i);
                    self.records.push(rec);
                }
            }
        }
        self.persist()?;
        Ok(())
    }

    /// MMR re-ranked search. Over-fetches `overfetch` candidates by raw
    /// cosine, then iteratively picks the next best by
    /// `lambda * relevance - (1-lambda) * max_sim_to_selected`.
    /// `lambda=1.0` is pure relevance (equivalent to `search`); `lambda=0.0`
    /// is pure diversity.
    pub fn search_mmr(
        &self,
        query: &[f32],
        limit: usize,
        project_filter: Option<&str>,
        lambda: f32,
        overfetch: usize,
    ) -> Result<Vec<SearchHit>> {
        if query.len() != self.dim {
            return Err(VectorError::DimMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        let q = l2_normalize(query);
        // Stage 1: top-(overfetch) by cosine relevance.
        let mut candidates: Vec<(usize, f32)> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| project_filter.is_none_or(|p| r.project_id == p))
            .map(|(i, r)| (i, dot(&q, &r.embedding)))
            .collect();
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(overfetch.max(limit));

        // Stage 2: greedy MMR.
        let mut selected: Vec<(usize, f32)> = Vec::with_capacity(limit);
        while selected.len() < limit && !candidates.is_empty() {
            let mut best_pos = 0usize;
            let mut best_mmr = f32::MIN;
            for (k, &(idx, rel)) in candidates.iter().enumerate() {
                let max_to_sel = selected
                    .iter()
                    .map(|&(j, _)| dot(&self.records[idx].embedding, &self.records[j].embedding))
                    .fold(f32::MIN, f32::max);
                let penalty = if selected.is_empty() { 0.0 } else { max_to_sel };
                let mmr = lambda * rel - (1.0 - lambda) * penalty;
                if mmr > best_mmr {
                    best_mmr = mmr;
                    best_pos = k;
                }
            }
            selected.push(candidates.remove(best_pos));
        }

        Ok(selected
            .into_iter()
            .map(|(i, score)| {
                let r = &self.records[i];
                SearchHit {
                    chunk_id: ChunkId(r.chunk_id.clone()),
                    session_id: SessionId(r.session_id.clone()),
                    project_id: ProjectId(r.project_id.clone()),
                    score,
                }
            })
            .collect())
    }

    /// Cosine search. `query` does not need to be pre-normalized — we
    /// normalize both sides and use the dot product.
    pub fn search(
        &self,
        query: &[f32],
        limit: usize,
        project_filter: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        if query.len() != self.dim {
            return Err(VectorError::DimMismatch {
                expected: self.dim,
                got: query.len(),
            });
        }
        let q = l2_normalize(query);
        let mut scored: Vec<(f32, usize)> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| project_filter.is_none_or(|p| r.project_id == p))
            .map(|(i, r)| (dot(&q, &r.embedding), i))
            .collect();
        // Top-K via partial sort.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored
            .into_iter()
            .map(|(score, i)| {
                let r = &self.records[i];
                SearchHit {
                    chunk_id: ChunkId(r.chunk_id.clone()),
                    session_id: SessionId(r.session_id.clone()),
                    project_id: ProjectId(r.project_id.clone()),
                    score,
                }
            })
            .collect())
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = bincode::serialize(&OnDisk {
            dim: self.dim,
            records: self.records.clone(),
        })?;
        let tmp = self.path.with_extension("bin.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &self.path)?;
        debug!(path = ?self.path, n = self.records.len(), "vectors persisted");
        Ok(())
    }
}

fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / mag).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ck_core::{ChunkKind, ChunkRole};
    use tempfile::tempdir;

    fn dummy_chunk(id: &str, session: &str, project: &str) -> Chunk {
        Chunk {
            id: ChunkId(id.into()),
            session_id: SessionId(session.into()),
            project_id: ProjectId(project.into()),
            turn_index: 0,
            role: ChunkRole::Assistant,
            kind: ChunkKind::AssistantText,
            text: "hello".into(),
            token_count: 1,
            start_uuid: "u".into(),
            end_uuid: "u".into(),
            started_at: Utc::now(),
            tool_name: None,
            tool_input_preview: None,
            embedding_ref: None,
        }
    }

    #[test]
    fn upsert_then_search_returns_top_match() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let mut store = VectorStore::connect(&layout, 4).unwrap();

        let chunks = vec![
            dummy_chunk("c1", "s1", "-p1"),
            dummy_chunk("c2", "s1", "-p1"),
            dummy_chunk("c3", "s2", "-p2"),
        ];
        let embeds = vec![
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0, 0.0],
            vec![0.0, 0.0, 1.0, 0.0],
        ];
        store.upsert_chunks(&chunks, &embeds).unwrap();

        let hits = store.search(&[0.9, 0.1, 0.0, 0.0], 2, None).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].chunk_id.0, "c1");
        assert!(hits[0].score > hits[1].score);

        // project filter
        let only_p2 = store.search(&[0.0, 0.0, 1.0, 0.0], 5, Some("-p2")).unwrap();
        assert_eq!(only_p2.len(), 1);
        assert_eq!(only_p2[0].chunk_id.0, "c3");
    }

    #[test]
    fn upsert_replaces_existing_record() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let mut store = VectorStore::connect(&layout, 2).unwrap();

        store
            .upsert_chunks(&[dummy_chunk("c1", "s1", "-p1")], &[vec![1.0, 0.0]])
            .unwrap();
        store
            .upsert_chunks(&[dummy_chunk("c1", "s1", "-p1")], &[vec![0.0, 1.0]])
            .unwrap();
        assert_eq!(store.len(), 1);

        let hits = store.search(&[0.0, 1.0], 1, None).unwrap();
        assert!((hits[0].score - 1.0).abs() < 1e-5);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        {
            let mut store = VectorStore::connect(&layout, 3).unwrap();
            store
                .upsert_chunks(&[dummy_chunk("c1", "s1", "-p1")], &[vec![1.0, 0.0, 0.0]])
                .unwrap();
        }
        let store = VectorStore::connect(&layout, 3).unwrap();
        assert_eq!(store.len(), 1);
        let hits = store.search(&[1.0, 0.0, 0.0], 1, None).unwrap();
        assert_eq!(hits[0].chunk_id.0, "c1");
    }

    #[test]
    fn mmr_diversifies_results() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let mut store = VectorStore::connect(&layout, 4).unwrap();
        // Three near-duplicates clustered around (1,0,0,0), one outlier.
        let chunks = vec![
            dummy_chunk("a1", "s1", "-p"),
            dummy_chunk("a2", "s1", "-p"),
            dummy_chunk("a3", "s1", "-p"),
            dummy_chunk("b1", "s2", "-p"),
        ];
        let embeds = vec![
            vec![1.0, 0.05, 0.0, 0.0],
            vec![1.0, 0.04, 0.0, 0.0],
            vec![1.0, 0.06, 0.0, 0.0],
            vec![0.5, 0.5, 0.5, 0.5],
        ];
        store.upsert_chunks(&chunks, &embeds).unwrap();

        // Pure relevance picks the three near-duplicates first.
        let by_rel = store.search(&[1.0, 0.0, 0.0, 0.0], 2, None).unwrap();
        assert!(by_rel.iter().all(|h| h.chunk_id.0.starts_with('a')));

        // MMR with strong diversity penalty surfaces the outlier in slot 2.
        let mmr = store
            .search_mmr(&[1.0, 0.0, 0.0, 0.0], 2, None, 0.2, 10)
            .unwrap();
        assert_eq!(mmr.len(), 2);
        assert!(mmr.iter().any(|h| h.chunk_id.0 == "b1"));
    }

    #[test]
    fn dim_mismatch_errors() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let mut store = VectorStore::connect(&layout, 4).unwrap();
        let err = store
            .upsert_chunks(
                &[dummy_chunk("c1", "s1", "-p1")],
                &[vec![1.0, 0.0]], // wrong dim
            )
            .unwrap_err();
        assert!(matches!(err, VectorError::DimMismatch { .. }));
    }
}
