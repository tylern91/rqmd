//! Local view of the Hugging Face hub cache, plus the client builders.
//!
//! hf-hub 1.0 dropped the synchronous `Cache` / `CacheRepo` types, but the
//! on-disk layout is unchanged (`models--<owner>--<name>/{refs,snapshots,blobs}`),
//! and `rqmd doctor` and the offline path need sync lookups that never touch
//! the network. This module reads and writes that layout directly.

use hf_hub::{HFClient, HFError};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoType {
    Model,
}

/// A repo id pinned at a revision.
#[derive(Debug, Clone)]
pub(crate) struct Repo {
    id: String,
    revision: String,
}

impl Repo {
    pub(crate) fn with_revision(id: String, _kind: RepoType, revision: String) -> Self {
        Self { id, revision }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn revision(&self) -> &str {
        &self.revision
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Cache {
    root: PathBuf,
}

impl Cache {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Honours `HF_HUB_CACHE`, `HUGGINGFACE_HUB_CACHE` and `HF_HOME`.
    pub(crate) fn from_env() -> Self {
        Self::new(hf_hub::resolve_cache_dir())
    }

    pub(crate) fn path(&self) -> &PathBuf {
        &self.root
    }

    pub(crate) fn repo(&self, repo: Repo) -> CacheRepo {
        let folder = format!("models--{}", repo.id.replace('/', "--"));
        CacheRepo {
            dir: self.root.join(folder),
            revision: repo.revision,
        }
    }

    /// The token file `huggingface-cli login` writes (`$HF_HOME/token`).
    pub(crate) fn token_path(&self) -> PathBuf {
        hf_hub::hf_home().join("token")
    }

    /// True when a stored token is in play without `HF_TOKEN`: an
    /// `HF_TOKEN_PATH` file or the `$HF_HOME/token` file with content.
    pub(crate) fn has_stored_token(&self) -> bool {
        let non_empty = |p: &Path| std::fs::read_to_string(p).is_ok_and(|s| !s.trim().is_empty());
        std::env::var_os("HF_TOKEN_PATH").is_some_and(|p| non_empty(Path::new(&p)))
            || non_empty(&self.token_path())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CacheRepo {
    dir: PathBuf,
    revision: String,
}

impl CacheRepo {
    /// `snapshots/<revision>`, where the pointer files for a commit live.
    pub(crate) fn pointer_path(&self, revision: &str) -> PathBuf {
        self.dir.join("snapshots").join(revision)
    }

    fn ref_path(&self, revision: &str) -> PathBuf {
        self.dir.join("refs").join(revision)
    }

    /// Resolve `file` through `refs/<revision>` of the repo this handle was
    /// made for; `None` when the ref or the snapshot entry is missing.
    pub(crate) fn get(&self, file: &str) -> Option<PathBuf> {
        let commit = std::fs::read_to_string(self.ref_path(&self.revision)).ok()?;
        let path = self.pointer_path(commit.trim()).join(file);
        path.exists().then_some(path)
    }

    pub(crate) fn create_ref(&self, revision: &str) -> std::io::Result<()> {
        let path = self.ref_path(revision);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, revision)
    }
}

/// An hf-hub client, authenticated through `token` (or its own env/file
/// lookup when `None`). `cache` pins the download directory to the one the
/// sync lookups read.
pub(crate) fn build_client(cache: &Cache, token: Option<&str>) -> Result<HFClient, HFError> {
    let mut builder = HFClient::builder().cache_dir(cache.path().clone());
    if let Some(tok) = token {
        builder = builder.token(tok);
    }
    builder.build()
}

/// True when the error says the credential was rejected, as opposed to a
/// missing resource or a server fault.
pub(crate) fn is_auth_failure(err: &HFError) -> bool {
    match err {
        HFError::AuthRequired { .. } | HFError::Forbidden { .. } => true,
        HFError::Http { context } => crate::status_is_auth_failure(context.status.as_u16()),
        _ => false,
    }
}
