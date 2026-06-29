//! usearch HNSW vector index wrapper.
//!
//! Each vector is keyed by `vid` (a u64 stored in content_vectors.vid in rusqlite).
//! The reverse mapping vid→document is in rusqlite; this module only handles
//! the vector similarity search itself.

use anyhow::{anyhow, Result};
use std::path::Path;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use rqmd_llm::EMBED_DIM;

pub struct VectorIndex {
    inner: Index,
    next_vid: u64,
}

impl VectorIndex {
    fn make_opts() -> IndexOptions {
        IndexOptions {
            dimensions: EMBED_DIM,
            metric: MetricKind::Cos,
            quantization: ScalarKind::F32,
            ..Default::default()
        }
    }

    /// Create a new in-memory HNSW index (dim=768, cosine distance).
    pub fn new() -> Result<Self> {
        let opts = Self::make_opts();
        let inner = Index::new(&opts).map_err(|e| anyhow!("usearch new: {e}"))?;
        inner
            .reserve(4096)
            .map_err(|e| anyhow!("usearch reserve: {e}"))?;
        Ok(Self { inner, next_vid: 0 })
    }

    /// Load from a previously saved file.
    pub fn load(path: &Path) -> Result<Self> {
        let opts = Self::make_opts();
        let inner = Index::new(&opts).map_err(|e| anyhow!("usearch new: {e}"))?;
        inner
            .load(path.to_str().ok_or_else(|| anyhow!("invalid path"))?)
            .map_err(|e| anyhow!("usearch load: {e}"))?;
        let size = inner.size();
        Ok(Self {
            inner,
            next_vid: size as u64,
        })
    }

    /// Save the index to disk for persistence across restarts.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        self.inner
            .save(path.to_str().ok_or_else(|| anyhow!("invalid path"))?)
            .map_err(|e| anyhow!("usearch save: {e}"))?;
        Ok(())
    }

    /// Add a vector. Returns the assigned vid.
    pub fn add(&mut self, embedding: &[f32]) -> Result<u64> {
        let vid = self.next_vid;
        if self.inner.capacity() <= self.inner.size() {
            self.inner
                .reserve(self.inner.capacity() * 2 + 1)
                .map_err(|e| anyhow!("usearch reserve grow: {e}"))?;
        }
        self.inner
            .add(vid, embedding)
            .map_err(|e| anyhow!("usearch add: {e}"))?;
        self.next_vid += 1;
        Ok(vid)
    }

    /// Add with a specific vid (used when rebuilding from rusqlite).
    pub fn add_with_vid(&mut self, vid: u64, embedding: &[f32]) -> Result<()> {
        if self.inner.capacity() <= self.inner.size() {
            self.inner
                .reserve(self.inner.capacity() * 2 + 1)
                .map_err(|e| anyhow!("usearch reserve grow: {e}"))?;
        }
        self.inner
            .add(vid, embedding)
            .map_err(|e| anyhow!("usearch add_with_vid: {e}"))?;
        if vid >= self.next_vid {
            self.next_vid = vid + 1;
        }
        Ok(())
    }

    /// Search for the k nearest neighbors. Returns (vid, cosine_similarity).
    /// usearch Cos metric returns distance (0=identical), so similarity = 1 - distance.
    pub fn search(&self, embedding: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
        let results = self
            .inner
            .search(embedding, k)
            .map_err(|e| anyhow!("usearch search: {e}"))?;
        Ok(results
            .keys
            .iter()
            .zip(results.distances.iter())
            .map(|(&vid, &dist)| (vid, 1.0 - dist))
            .collect())
    }

    pub fn size(&self) -> usize {
        self.inner.size()
    }
}
