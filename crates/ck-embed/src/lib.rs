//! Synchronous embedder + content-addressed disk cache.
//!
//! [`Embedder`] is the trait everything else codes against. M2 ships a single
//! impl, [`LocalEmbedder`], which wraps `fastembed::TextEmbedding` over
//! BGE-small-en-v1.5 (384 dims). Voyage cloud impl is M4.
//!
//! [`embed_with_cache`] is the call sites should use: hash every input, look
//! in `~/.context-keeper/cache/embeddings/<model>/<sha256>.bin`, embed only
//! the misses, and write each new vector back to disk so the next reindex is
//! free.

use ck_core::sha256_hex;
use ck_store::Layout;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("fastembed: {0}")]
    Fastembed(String),
    #[error("dim mismatch: expected {expected}, got {got}")]
    DimMismatch { expected: usize, got: usize },
    #[error("response length mismatch: requested {requested}, got {got}")]
    LenMismatch { requested: usize, got: usize },
}

pub type Result<T> = std::result::Result<T, EmbedError>;

pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn model_name(&self) -> &str;
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Local ONNX embedder over BGE-small-en-v1.5 via `fastembed`.
///
/// The model files live under `~/.context-keeper/cache/models/`. First
/// construction downloads ~130MB and prints a `tracing::info!` line so the
/// user knows what's happening.
pub struct LocalEmbedder {
    inner: TextEmbedding,
    dim: usize,
    name: String,
}

pub const BGE_SMALL_EN_V15: &str = "bge-small-en-v1.5";
pub const BGE_SMALL_EN_V15_DIM: usize = 384;

impl LocalEmbedder {
    pub fn new(layout: &Layout) -> Result<Self> {
        let cache_dir = layout.root.join("cache/models");
        fs::create_dir_all(&cache_dir)?;
        info!(
            cache_dir = ?cache_dir,
            model = BGE_SMALL_EN_V15,
            "initializing local embedder (downloads model on first run, ~130MB)"
        );
        let options = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_show_download_progress(true)
            .with_cache_dir(cache_dir);
        let inner =
            TextEmbedding::try_new(options).map_err(|e| EmbedError::Fastembed(e.to_string()))?;
        Ok(Self {
            inner,
            dim: BGE_SMALL_EN_V15_DIM,
            name: BGE_SMALL_EN_V15.to_string(),
        })
    }
}

impl Embedder for LocalEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn model_name(&self) -> &str {
        &self.name
    }
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = texts.to_vec();
        let out = self
            .inner
            .embed(owned, Some(32))
            .map_err(|e| EmbedError::Fastembed(e.to_string()))?;
        if out.len() != texts.len() {
            return Err(EmbedError::LenMismatch {
                requested: texts.len(),
                got: out.len(),
            });
        }
        if let Some(first) = out.first() {
            if first.len() != self.dim {
                return Err(EmbedError::DimMismatch {
                    expected: self.dim,
                    got: first.len(),
                });
            }
        }
        Ok(out)
    }
}

/// Result of [`embed_with_cache`].
pub struct EmbedOutcome {
    pub embeddings: Vec<Vec<f32>>,
    /// SHA-256 of each input text, parallel to `embeddings`.
    pub hashes: Vec<String>,
    pub cache_hits: u32,
    pub new_embeds: u32,
}

/// Embed `texts` using `embedder`, hitting the on-disk cache when possible
/// and writing new vectors back to it.
pub fn embed_with_cache(
    embedder: &dyn Embedder,
    layout: &Layout,
    texts: &[String],
) -> Result<EmbedOutcome> {
    let cache_dir = layout.embeddings_cache_dir(embedder.model_name());
    fs::create_dir_all(&cache_dir)?;

    let dim = embedder.dim();
    let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    let mut hashes: Vec<String> = Vec::with_capacity(texts.len());
    let mut hits = 0u32;
    let mut misses_idx: Vec<usize> = Vec::new();

    for (i, text) in texts.iter().enumerate() {
        let h = sha256_hex(text.as_bytes());
        let path = cache_dir.join(format!("{h}.bin"));
        hashes.push(h);
        if let Some(v) = read_cached(&path, dim) {
            out[i] = Some(v);
            hits += 1;
        } else {
            misses_idx.push(i);
        }
    }

    let new_embeds = misses_idx.len() as u32;
    if !misses_idx.is_empty() {
        let batch: Vec<String> = misses_idx.iter().map(|&i| texts[i].clone()).collect();
        let vecs = embedder.embed_batch(&batch)?;
        if vecs.len() != misses_idx.len() {
            return Err(EmbedError::LenMismatch {
                requested: misses_idx.len(),
                got: vecs.len(),
            });
        }
        for (j, &i) in misses_idx.iter().enumerate() {
            let v = vecs[j].clone();
            let path = cache_dir.join(format!("{}.bin", hashes[i]));
            write_cached(&path, &v)?;
            out[i] = Some(v);
        }
    }

    debug!(
        n = texts.len(),
        hits,
        new = new_embeds,
        "embed_with_cache complete"
    );

    Ok(EmbedOutcome {
        embeddings: out.into_iter().map(|v| v.expect("filled")).collect(),
        hashes,
        cache_hits: hits,
        new_embeds,
    })
}

fn read_cached(path: &Path, dim: usize) -> Option<Vec<f32>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() != dim * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(dim);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    Some(out)
}

fn write_cached(path: &Path, v: &[f32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for f in v {
        bytes.extend_from_slice(&f.to_le_bytes());
    }
    let tmp: PathBuf = path.with_extension("bin.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Fake embedder that returns canned vectors and counts calls. Lets us
    /// test cache behavior without downloading the BGE model.
    struct StubEmbedder {
        vecs: Vec<Vec<f32>>,
        called_with: Mutex<Vec<String>>,
    }

    impl Embedder for StubEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn model_name(&self) -> &str {
            "stub"
        }
        fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.called_with.lock().unwrap().extend_from_slice(texts);
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| self.vecs[i % self.vecs.len()].clone())
                .collect())
        }
    }

    #[test]
    fn cached_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("v.bin");
        let v = vec![1.0_f32, -2.5, 3.25, 0.0];
        write_cached(&path, &v).unwrap();
        let back = read_cached(&path, 4).unwrap();
        assert_eq!(back, v);
        // wrong dim → None
        assert!(read_cached(&path, 8).is_none());
    }

    #[test]
    fn embed_with_cache_skips_known_inputs() {
        let dir = tempdir().unwrap();
        let layout = Layout::new_at(dir.path().to_path_buf());
        layout.ensure().unwrap();
        let stub = StubEmbedder {
            vecs: vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]],
            called_with: Mutex::new(Vec::new()),
        };

        let texts = vec!["alpha".to_string(), "beta".to_string()];
        let r1 = embed_with_cache(&stub, &layout, &texts).unwrap();
        assert_eq!(r1.cache_hits, 0);
        assert_eq!(r1.new_embeds, 2);
        assert_eq!(r1.embeddings.len(), 2);

        // Second call: both should hit cache, no new embed calls.
        let calls_before = stub.called_with.lock().unwrap().len();
        let r2 = embed_with_cache(&stub, &layout, &texts).unwrap();
        let calls_after = stub.called_with.lock().unwrap().len();
        assert_eq!(r2.cache_hits, 2);
        assert_eq!(r2.new_embeds, 0);
        assert_eq!(
            calls_after, calls_before,
            "no extra embedder calls expected"
        );
        assert_eq!(r2.embeddings, r1.embeddings);

        // Mixed: one known, one new.
        let texts3 = vec!["alpha".to_string(), "gamma".to_string()];
        let r3 = embed_with_cache(&stub, &layout, &texts3).unwrap();
        assert_eq!(r3.cache_hits, 1);
        assert_eq!(r3.new_embeds, 1);
    }
}
