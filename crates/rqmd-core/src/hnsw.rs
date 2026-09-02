//! usearch HNSW vector index wrapper.
//!
//! Each vector is keyed by `vid` (a u64 stored in content_vectors.vid in rusqlite).
//! The reverse mapping vid→document is in rusqlite; this module only handles
//! the vector similarity search itself.

use anyhow::{Result, anyhow, bail};
use std::path::Path;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use rqmd_llm::EMBED_DIM;

pub struct VectorIndex {
    inner: Index,
    next_vid: u64,
    read_only: bool,
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
        Ok(Self {
            inner,
            next_vid: 0,
            read_only: false,
        })
    }

    /// Load from a previously saved file, reading it fully into memory.
    /// Required for indexing (`add`/`add_with_vid`/`save`).
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
            read_only: false,
        })
    }

    /// Open a previously saved file as a memory-mapped, read-only view.
    /// Mutation (`add`/`add_with_vid`/`save`) is undocumented behavior in the
    /// usearch binding once a file is viewed, so it's rejected in Rust before
    /// ever reaching the C++ layer.
    pub fn view(path: &Path) -> Result<Self> {
        let opts = Self::make_opts();
        let inner = Index::new(&opts).map_err(|e| anyhow!("usearch new: {e}"))?;
        inner
            .view(path.to_str().ok_or_else(|| anyhow!("invalid path"))?)
            .map_err(|e| anyhow!("usearch view: {e}"))?;
        let size = inner.size();
        Ok(Self {
            inner,
            next_vid: size as u64,
            read_only: true,
        })
    }

    /// Save the index to disk for persistence across restarts.
    pub fn save(&self, path: &Path) -> Result<()> {
        if self.read_only {
            bail!("cannot save a read-only (mmap'd) VectorIndex");
        }
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
        if self.read_only {
            bail!("cannot add to a read-only (mmap'd) VectorIndex");
        }
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

    /// Raise `next_vid` to at least `floor` without adding a vector.
    ///
    /// Called during `Store::open` to reconcile `next_vid` with `MAX(content_vectors.vid)`
    /// from SQLite.  When the HNSW file and the DB diverge (e.g. a corrupt load fell back to
    /// an empty index with `next_vid=0`, or orphan-vid gaps accumulated from duplicate-hash
    /// drift), this guarantees freshly-issued vids will never collide with existing DB rows.
    pub fn ensure_next_vid_at_least(&mut self, floor: u64) {
        if floor > self.next_vid {
            self.next_vid = floor;
        }
    }

    /// Add with a specific vid (used when rebuilding from rusqlite).
    pub fn add_with_vid(&mut self, vid: u64, embedding: &[f32]) -> Result<()> {
        if self.read_only {
            bail!("cannot add to a read-only (mmap'd) VectorIndex");
        }
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

    /// Remove a vector by vid. `next_vid` is never decremented or reused —
    /// callers must not rely on a removed vid becoming available again.
    /// usearch tombstones rather than compacts on removal, so the on-disk
    /// file size does not shrink; that's expected, not a bug.
    pub fn remove(&mut self, vid: u64) -> Result<usize> {
        if self.read_only {
            bail!("cannot remove from a read-only (mmap'd) VectorIndex");
        }
        self.inner
            .remove(vid)
            .map_err(|e| anyhow!("usearch remove: {e}"))
    }

    /// Fetch the stored vector for a given vid. Works on both writable and
    /// `view()`-opened (mmap, read-only) indexes — `usearch::Index::get`
    /// takes `&Index`, so reads never require mutable access.
    pub fn get_vector(&self, vid: u64) -> Result<Vec<f32>> {
        let mut buf = vec![0.0_f32; EMBED_DIM];
        let found = self
            .inner
            .get(vid, &mut buf)
            .map_err(|e| anyhow!("usearch get: {e}"))?;
        if found == 0 {
            bail!("no vector found for vid {vid}");
        }
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_rejects_mutation_with_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hnsw.usearch");

        let mut writable = VectorIndex::new().unwrap();
        let embedding = vec![0.1_f32; EMBED_DIM];
        writable.add(&embedding).unwrap();
        writable.save(&path).unwrap();

        let mut viewed = VectorIndex::view(&path).unwrap();
        assert!(viewed.read_only);
        assert_eq!(viewed.size(), 1);

        // Reads still work on a view.
        assert_eq!(viewed.search(&embedding, 1).unwrap().len(), 1);

        // Mutation must fail cleanly (Err), never panic.
        assert!(viewed.add(&embedding).is_err());
        assert!(viewed.add_with_vid(99, &embedding).is_err());
        assert!(viewed.save(&path).is_err());
    }

    /// Load-bearing for `rqmd similar`: it must be able to read back a
    /// stored vector from a `view()`-opened (mmap, read-only) index without
    /// ever loading a model or mutating the index.
    #[test]
    fn get_vector_works_on_view_opened_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hnsw.usearch");

        let mut writable = VectorIndex::new().unwrap();
        let embedding: Vec<f32> = (0..EMBED_DIM).map(|i| i as f32 * 0.001).collect();
        let vid = writable.add(&embedding).unwrap();
        writable.save(&path).unwrap();

        let viewed = VectorIndex::view(&path).unwrap();
        let fetched = viewed.get_vector(vid).unwrap();
        assert_eq!(fetched.len(), EMBED_DIM);
        for (a, b) in fetched.iter().zip(embedding.iter()) {
            assert!((a - b).abs() < 1e-4, "expected {b}, got {a}");
        }

        assert!(viewed.get_vector(vid + 1).is_err());
    }

    #[test]
    fn remove_evicts_vector_from_search_results() {
        let mut index = VectorIndex::new().unwrap();
        let a: Vec<f32> = (0..EMBED_DIM).map(|i| i as f32 * 0.001).collect();
        let b: Vec<f32> = (0..EMBED_DIM)
            .map(|i| (EMBED_DIM - i) as f32 * 0.001)
            .collect();
        let vid_a = index.add(&a).unwrap();
        let vid_b = index.add(&b).unwrap();

        index.remove(vid_a).unwrap();

        let results = index.search(&a, 10).unwrap();
        assert!(
            results.iter().all(|(vid, _)| *vid != vid_a),
            "removed vid must not appear in search results"
        );
        assert!(results.iter().any(|(vid, _)| *vid == vid_b));
    }

    #[test]
    fn remove_rejects_on_read_only_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hnsw.usearch");

        let mut writable = VectorIndex::new().unwrap();
        let embedding = vec![0.1_f32; EMBED_DIM];
        let vid = writable.add(&embedding).unwrap();
        writable.save(&path).unwrap();

        let mut viewed = VectorIndex::view(&path).unwrap();
        assert!(viewed.remove(vid).is_err());
    }
}
