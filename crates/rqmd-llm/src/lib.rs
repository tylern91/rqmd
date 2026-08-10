//! rqmd-llm — inference backend abstraction and llama-cpp-2 implementation.
//!
//! Feature flags:
//!   (default)    — LlamaCppBackend via llama-cpp-2 (GGUF, Metal/CUDA/Vulkan)
//!   ort-backend  — OrtBackend via ONNX Runtime (CoreML/CUDA/DirectML/CPU)
//!
//! Backend selection at runtime (read by `create_backend()`):
//!   RQMD_INFERENCE_BACKEND=llama|ort   (default: llama)
//!   RQMD_ORT_EP=auto|coreml|cuda|directml|cpu
//!
//! All API shapes validated against llama-cpp-2 v0.1.150 in spike-inference.
//! Critical gotchas (all confirmed by spike):
//! - Qwen3-Reranker is a causal decoder model → ctx.decode(), NOT ctx.encode()
//! - Reranker needs a fresh LlamaContext per (query, doc) pair (KV cache positions)
//! - LlamaContextParams is Clone but not Copy; clone before passing to new_context()
//! - n_gpu_layers=14 for reranker on Apple Silicon (448 MiB KV limit). rerank_n_ctx
//!   ships at 2048 (`LlamaCppConfig::default`) — 512 is a documented tuning option
//!   for tighter KV budgets, not the shipped default.
//!
//! Each of the three GGUF models loads lazily on first use (`ensure_embed` /
//! `ensure_rerank` / `ensure_generate`) and is evicted after
//! `RQMD_MODEL_IDLE_TTL` seconds of inactivity (default 300; `0` disables) —
//! see `release_idle`. This keeps an idle daemon from permanently holding the
//! ~2 GB generate model resident once query expansion (on by default) has
//! triggered it once.

use anyhow::{Context, Result};
use hf_hub::{
    api::tokio::{Api, ApiBuilder},
    Cache, Repo, RepoType,
};
use llama_cpp_2::{
    context::params::{LlamaContextParams, LlamaPoolingType},
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
    send_logs_to_tracing, LogOptions,
};
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// ── Default model repos (mirrors qmd's llm.ts defaults) ──────────────────────
//
// Each repo is pinned to an explicit commit (not the mutable "main" branch)
// and each file carries the SHA-256 that Hugging Face's own git-lfs tracking
// reports for that pinned commit, so a compromised upstream revision or a
// tampered/corrupted download is caught before a multi-gigabyte binary blob
// reaches llama.cpp's GGUF parser.
//
// Revisions resolved via `GET https://huggingface.co/api/models/<repo>`
// (`sha` field); file hashes via `GET .../models/<repo>?blobs=true`
// (`siblings[].lfs.sha256` — the same hash git-lfs itself verifies blobs
// against), both on 2026-08-03. If `ggml-org` publishes a new quantization
// under these filenames, bump the revision/hash together deliberately rather
// than silently trusting whatever "main" resolves to at download time.

pub const DEFAULT_EMBED_REPO: &str = "ggml-org/embeddinggemma-300M-GGUF";
pub const DEFAULT_EMBED_FILE: &str = "embeddinggemma-300M-Q8_0.gguf";
pub const DEFAULT_EMBED_REVISION: &str = "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12";
pub const DEFAULT_EMBED_SHA256: &str =
    "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63";

pub const DEFAULT_RERANK_REPO: &str = "ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF";
pub const DEFAULT_RERANK_FILE: &str = "qwen3-reranker-0.6b-q8_0.gguf";
pub const DEFAULT_RERANK_REVISION: &str = "a02f48bb4f057028298c21fa033da2b30d7742d5";
pub const DEFAULT_RERANK_SHA256: &str =
    "22c9979ce4fbcdc5acdc310c6641c32797eff1aa980b8f7a2db8a8ea23429a48";

pub const DEFAULT_GENERATE_REPO: &str = "ggml-org/Qwen3-1.7B-GGUF";
pub const DEFAULT_GENERATE_FILE: &str = "Qwen3-1.7B-Q8_0.gguf";
pub const DEFAULT_GENERATE_REVISION: &str = "daeb8e2d528a760970442092f6bf1e55c3b659eb";
pub const DEFAULT_GENERATE_SHA256: &str =
    "9860780f3a1fab1f8f909a1b549ea3e62c22d19ab1a492b3a1026b38c5bd3ec3";

// Embedding dimension for embeddinggemma-300M (confirmed in spike: dim=768)
pub const EMBED_DIM: usize = 768;

// Embed context window size (tokens).  Must stay in sync with `with_n_ctx` / `with_n_ubatch`
// in `LlamaCppBackend::new`.  encoder-mode (llama_encode) requires n_ubatch >= n_tokens —
// without truncation a token-dense 3600-char chunk can exceed 2048 tokens and trigger a
// GGML_ASSERT abort.  Guard: truncate inputs to EMBED_CONTEXT_SIZE - EMBED_TOKEN_MARGIN
// before encoding.  Mirrors qmd's truncateToContextSize (src/llm.ts:1279).
const EMBED_CONTEXT_SIZE: usize = 2048;
/// BOS/EOS overhead margin, matching qmd (src/llm.ts:1291 `maxTokens - 4`).
const EMBED_TOKEN_MARGIN: usize = 4;

/// EmbeddingGemma's documented query-side prompt template.
const EMBEDDINGGEMMA_QUERY_PREFIX: &str = "task: search result | query: ";
/// EmbeddingGemma's documented passage-side prompt template, using the documented
/// "no title" fallback — threading real chunk titles through is not worth the plumbing.
const EMBEDDINGGEMMA_PASSAGE_PREFIX: &str = "title: none | text: ";

/// L2-normalize a vector in place, matching `InferenceBackend::embed`'s documented
/// unit-normalized contract. Shared with the feature-gated `ort_backend` module —
/// this crate root is compiled unconditionally, so `pub(crate)` here is reachable
/// regardless of whether the `ort-backend` feature is on.
pub(crate) fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|&x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

// ── InferenceBackend trait ────────────────────────────────────────────────────

/// Which operations a backend actually supports. Callers (e.g. `hybrid_query`'s
/// rerank/generate steps) check this before attempting a call so an unsupported
/// operation degrades with a visible notice instead of a swallowed error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub embed: bool,
    pub rerank: bool,
    pub generate: bool,
}

/// Core inference operations needed by qmd's search pipeline.
pub trait InferenceBackend: Send {
    /// Which operations this backend actually supports. Default: full support
    /// (embed + rerank + generate), matching `LlamaCppBackend`. Backends that
    /// only implement a subset (e.g. `OrtBackend`: embed-only) must override
    /// this truthfully — callers rely on it to decide whether to attempt
    /// `rerank`/`generate` at all.
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            embed: true,
            rerank: true,
            generate: true,
        }
    }

    /// Embed a single text. Returns a unit-normalized f32 vector.
    fn embed(&mut self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of texts. Default: sequential loop — override for batched acceleration.
    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed(text)?);
        }
        Ok(out)
    }

    /// Embed text that will be matched against via `hnsw.search()` — a search query,
    /// or (for HyDE) a hypothetical document that plays the query's role in the
    /// pipeline. Default: identical to `embed`. Backends whose model documents an
    /// asymmetric query/passage prompt convention (e.g. EmbeddingGemma) override this.
    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed(text)
    }

    /// Embed text that will be stored and matched against via `hnsw.add()` — an
    /// indexed document chunk. Default: identical to `embed`. See `embed_query`.
    fn embed_passage(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed(text)
    }

    /// Batched form of `embed_passage`. Default: sequential loop — override for
    /// batched acceleration.
    fn embed_batch_passage(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.embed_passage(text)?);
        }
        Ok(out)
    }

    /// Rerank: score (query, doc) pairs. Returns a scalar score per pair.
    /// Higher = more relevant. Scores are NOT normalized across pairs.
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;

    /// Generate free-form text from a prompt. Returns the generated string.
    /// The caller is responsible for parsing the output (e.g. stripping lex:/vec:/hyde: lines).
    fn generate(&mut self, prompt: &str) -> Result<String>;

    fn embed_model_name(&self) -> &str;
    fn rerank_model_name(&self) -> &str;
    fn generate_model_name(&self) -> &str;

    /// Drop any loaded model idle for at least `ttl`. Returns how many were released.
    /// Default no-op — backends with nothing to release (`NoBackend`, `OrtBackend`)
    /// don't need to implement this.
    fn release_idle(&mut self, _ttl: Duration) -> usize {
        0
    }
}

// ── Model cache inspection (sync, no model load) ─────────────────────────────

/// On-disk cache status for the three GGUF models, resolved via the same hf-hub
/// `Cache` that `LlamaCppBackend::new` downloads into. `rqmd doctor` previously
/// rebuilt the path with `dirs::cache_dir()`, which is wrong on macOS (hf-hub
/// uses `~/.cache/huggingface`, not `~/Library/Caches`).
#[derive(Debug, Clone)]
pub struct ModelCacheReport {
    pub cache_root: std::path::PathBuf,
    pub embed_cached: bool,
    pub rerank_cached: bool,
    pub generate_cached: bool,
}

/// Existence-only check for whether `file` is present for `repo`/`revision`
/// in `cache`. Mirrors `resolve_cached`'s two lookup paths (hf-hub's own
/// `refs/<revision>` fast path, then the legacy `snapshots/<revision>/<file>`
/// layout) but never hashes or heals — `doctor` must stay instant and must
/// not mutate the cache as a side effect of a status check. Takes `cache`
/// explicitly (rather than reading `Cache::from_env()` itself) so it is
/// unit-testable against a temporary cache directory.
fn cache_has_file(cache: &Cache, repo: &str, revision: &str, file: &str) -> bool {
    let cache_repo = cache.repo(Repo::with_revision(
        repo.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));
    cache_repo.get(file).is_some() || cache_repo.pointer_path(revision).join(file).exists()
}

/// Return the cache status for all three models without loading any weights.
/// All repos/revisions come from `config` so they match what the downloader
/// uses exactly — `cache.model(repo)` (implicit "main" revision) would
/// under-report once downloads are pinned to a specific commit, since the
/// cache stores each revision under its own `refs/<revision>` entry.
pub fn model_cache_report(config: &LlamaCppConfig) -> ModelCacheReport {
    // from_env() honours HF_HOME; falls back to ~/.cache/huggingface/hub.
    let cache = Cache::from_env();
    ModelCacheReport {
        cache_root: cache.path().clone(),
        embed_cached: cache_has_file(
            &cache,
            &config.embed_repo,
            &config.embed_revision,
            &config.embed_file,
        ),
        rerank_cached: cache_has_file(
            &cache,
            &config.rerank_repo,
            &config.rerank_revision,
            &config.rerank_file,
        ),
        generate_cached: cache_has_file(
            &cache,
            &config.generate_repo,
            &config.generate_revision,
            &config.generate_file,
        ),
    }
}

// ── LlamaCppBackend ───────────────────────────────────────────────────────────

pub struct LlamaCppConfig {
    /// HF repo ID (e.g. "ggml-org/embeddinggemma-300M-GGUF") or local path.
    pub embed_repo: String,
    pub embed_file: String,
    /// Commit hash to pin the embed repo to — see the `DEFAULT_*_REVISION`
    /// doc comment for how these are resolved and why "main" is not used.
    pub embed_revision: String,
    /// Expected SHA-256 of `embed_file` at `embed_revision`.
    pub embed_sha256: String,
    pub rerank_repo: String,
    pub rerank_file: String,
    pub rerank_revision: String,
    pub rerank_sha256: String,
    pub generate_repo: String,
    pub generate_file: String,
    pub generate_revision: String,
    pub generate_sha256: String,
    /// GPU layers for embed model. 99 = all layers on Metal/CUDA.
    pub embed_n_gpu_layers: u32,
    /// GPU layers for reranker. Keep ≤14 on Apple Silicon (448 MiB KV budget).
    pub rerank_n_gpu_layers: u32,
    /// KV cache size for reranker context. Must be >= query+doc token count.
    pub rerank_n_ctx: u32,
    /// GPU layers for generation model. 99 = all layers on Metal/CUDA.
    pub generate_n_gpu_layers: u32,
    /// KV context size for generation. Limits prompt + output token count.
    pub generate_n_ctx: u32,
    pub hf_cache_dir: Option<PathBuf>,
}

impl Default for LlamaCppConfig {
    fn default() -> Self {
        Self {
            embed_repo: DEFAULT_EMBED_REPO.to_string(),
            embed_file: DEFAULT_EMBED_FILE.to_string(),
            embed_revision: DEFAULT_EMBED_REVISION.to_string(),
            embed_sha256: DEFAULT_EMBED_SHA256.to_string(),
            rerank_repo: DEFAULT_RERANK_REPO.to_string(),
            rerank_file: DEFAULT_RERANK_FILE.to_string(),
            rerank_revision: DEFAULT_RERANK_REVISION.to_string(),
            rerank_sha256: DEFAULT_RERANK_SHA256.to_string(),
            generate_repo: DEFAULT_GENERATE_REPO.to_string(),
            generate_file: DEFAULT_GENERATE_FILE.to_string(),
            generate_revision: DEFAULT_GENERATE_REVISION.to_string(),
            generate_sha256: DEFAULT_GENERATE_SHA256.to_string(),
            embed_n_gpu_layers: 99,
            rerank_n_gpu_layers: 14,
            rerank_n_ctx: 2048,
            generate_n_gpu_layers: 99,
            generate_n_ctx: 2048,
            hf_cache_dir: None,
        }
    }
}

/// Validate an `HF_ENDPOINT` value uses `https://`. Split out as a pure
/// function (rather than inlined in `validate_hf_endpoint`) so it's
/// unit-testable without mutating the process environment — env vars are
/// process-global and would race under `cargo test`'s parallel execution.
fn check_hf_endpoint_scheme(endpoint: &str) -> Result<()> {
    if !endpoint.starts_with("https://") {
        anyhow::bail!(
            "HF_ENDPOINT={endpoint:?} is not an https:// URL — refusing to allow \
             unencrypted model downloads. Set HF_ENDPOINT to an https:// mirror \
             or unset it to use the default https://huggingface.co."
        );
    }
    Ok(())
}

/// Reject a plaintext `HF_ENDPOINT`. `hf-hub`'s downloader reads this env var
/// with no scheme check, so left unvalidated it lets anyone with control of
/// the process environment silently redirect every model download to an
/// arbitrary `http://` host — no TLS, no certificate, no warning.
fn validate_hf_endpoint() -> Result<()> {
    if let Ok(endpoint) = std::env::var("HF_ENDPOINT") {
        check_hf_endpoint_scheme(&endpoint)?;
    }
    Ok(())
}

/// Compute the SHA-256 of a file, reading in fixed-size chunks so a
/// multi-gigabyte GGUF is never loaded into memory whole just to hash it.
fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("open {} to hash", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {} to hash", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Verify `path`'s SHA-256 matches `expected` (case-insensitive lowercase hex).
fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        anyhow::bail!(
            "integrity check failed for {}: expected sha256 {expected}, got {actual} — \
             the download may be corrupted or tampered with",
            path.display()
        );
    }
    Ok(())
}

/// Resolve `filename` from the local cache, tolerating caches populated
/// before revision pinning existed. hf-hub's own `CacheRepo::get` only
/// consults `refs/<revision>` (a file containing a commit hash), so a
/// snapshot that was downloaded under `refs/main` is invisible to it once we
/// pin to a specific commit SHA — even though the exact same bytes already
/// sit at `snapshots/<revision>/<file>`. When we find bytes there with no
/// matching ref, we hash them once and write `refs/<revision>` ourselves so
/// every later run takes hf-hub's fast (network-free) path.
fn resolve_cached(
    cache: &Cache,
    repo: &Repo,
    filename: &str,
    expected_sha256: &str,
) -> Result<Option<PathBuf>> {
    let cache_repo = cache.repo(repo.clone());

    // Fast path: refs/<revision> already exists. That file was written either
    // by a prior download (verified then — see `get_verified`'s trust-on-
    // first-use note) or by a previous run of this healing step, so it is
    // never re-hashed here.
    if let Some(path) = cache_repo.get(filename) {
        return Ok(Some(path));
    }

    // Legacy-layout path: the pinned snapshot exists on disk, but nothing
    // ever wrote refs/<revision> for it (pre-pinning cache, or a ref that got
    // lost). Verify before trusting it, then heal the ref so this check is
    // paid only once per model per machine.
    let pointer = cache_repo.pointer_path(repo.revision()).join(filename);
    if !pointer.exists() {
        return Ok(None);
    }
    verify_sha256(&pointer, expected_sha256)?;
    cache_repo
        .create_ref(repo.revision())
        .with_context(|| format!("heal cache ref for {}", pointer.display()))?;
    Ok(Some(pointer))
}

/// Parse an `HF_HUB_OFFLINE` value the way the official `huggingface_hub`
/// Python client does: any value other than empty/"0"/"false"/"no"
/// (case-insensitive) means offline. Split out as a pure function for the
/// same unit-testability reason as `check_hf_endpoint_scheme`.
fn offline_from_env_value(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no"
    )
}

/// True when `HF_HUB_OFFLINE` is set to a value that means "never touch the
/// network". hf-hub 0.5.0 has no built-in support for this var — despite the
/// README advertising it — so it is enforced entirely here, and only after
/// `resolve_cached` has had a chance to serve the file from a local cache.
fn hf_hub_offline() -> bool {
    std::env::var("HF_HUB_OFFLINE")
        .map(|v| offline_from_env_value(&v))
        .unwrap_or(false)
}

/// Read an override token from the environment, in the same precedence order
/// the official `huggingface_hub` Python client uses: `HF_TOKEN` first, then
/// `HUGGING_FACE_HUB_TOKEN`. hf-hub 0.5.0 reads neither on its own — only the
/// on-disk `~/.cache/huggingface/token` file — so without this an env var
/// override would silently do nothing.
fn env_hf_token() -> Option<String> {
    std::env::var("HF_TOKEN")
        .ok()
        .or_else(|| std::env::var("HUGGING_FACE_HUB_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
}

/// True for HTTP statuses that mean "the credential was rejected" rather
/// than "the resource doesn't exist" or "the server is unhappy" — split out
/// as a pure function (same reasoning as `check_hf_endpoint_scheme`) so it is
/// unit-testable without constructing a real `ApiError`.
fn status_is_auth_failure(status: u16) -> bool {
    status == 401 || status == 403
}

/// Walk an `ApiError`, including a `TooManyRetries`-boxed inner error, looking
/// for an HTTP response whose status means the credential was rejected.
fn is_auth_failure(err: &hf_hub::api::tokio::ApiError) -> bool {
    use hf_hub::api::tokio::ApiError;
    match err {
        ApiError::RequestError(e) => e
            .status()
            .map(|s| status_is_auth_failure(s.as_u16()))
            .unwrap_or(false),
        ApiError::TooManyRetries(inner) => is_auth_failure(inner),
        _ => false,
    }
}

/// Download `filename` from `repo` via `api`, retrying anonymously via
/// `anon_api` if the credentialed attempt is rejected as an auth failure.
/// rqmd's pinned model repos are public, so a stale or revoked token —
/// whether from `HF_TOKEN`/`HUGGING_FACE_HUB_TOKEN` or the cache's token
/// file — turns a request that would otherwise succeed into a 401/403;
/// dropping the token is the fix, not a privilege escalation. Integrity is
/// unaffected either way: the caller still verifies SHA-256 against the
/// pinned hash regardless of which client answered.
async fn get_with_auth_fallback(
    api: &Api,
    anon_api: Option<&Api>,
    cache: &Cache,
    repo_id: &str,
    repo: &Repo,
    filename: &str,
) -> Result<PathBuf> {
    let err = match api.repo(repo.clone()).get(filename).await {
        Ok(path) => return Ok(path),
        Err(e) => e,
    };
    if !is_auth_failure(&err) {
        return Err(err.into());
    }
    let Some(anon_api) = anon_api else {
        return Err(err.into());
    };

    tracing::warn!(
        "HuggingFace rejected the credentialed request for {repo_id}/{filename} \
         ({err}); retrying anonymously — the cached or configured token may be \
         expired or invalid"
    );
    match anon_api.repo(repo.clone()).get(filename).await {
        Ok(path) => Ok(path),
        Err(anon_err) => anyhow::bail!(
            "HuggingFace rejected both the authenticated ({err}) and anonymous \
             ({anon_err}) download of {repo_id}/{filename}. The token at {} may \
             be expired — run `huggingface-cli login` to refresh it, or remove \
             that file to download anonymously.",
            cache.token_path().display(),
        ),
    }
}

/// Fetch `filename` from `repo_id` pinned at `revision`, from cache if
/// present (see `resolve_cached`), otherwise downloading it via
/// `get_with_auth_fallback`. SHA-256 is verified only on the network path: a
/// cached file was already verified either when first downloaded or by
/// `resolve_cached`'s healing check, and re-hashing a multi-gigabyte GGUF on
/// every process start would be a real latency cost (this is the same
/// trust-on-first-use model `git-lfs` itself uses — it checks the blob hash
/// on checkout, not on every subsequent read).
async fn get_verified(
    api: &Api,
    anon_api: Option<&Api>,
    cache: &Cache,
    repo_id: &str,
    revision: &str,
    filename: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let repo = Repo::with_revision(repo_id.to_string(), RepoType::Model, revision.to_string());
    if let Some(path) = resolve_cached(cache, &repo, filename, expected_sha256)
        .with_context(|| format!("{repo_id}/{filename}@{revision}"))?
    {
        return Ok(path);
    }

    if hf_hub_offline() {
        let expected_path = cache
            .repo(repo.clone())
            .pointer_path(revision)
            .join(filename);
        anyhow::bail!(
            "{repo_id}/{filename}@{revision} is not in the local cache and \
             HF_HUB_OFFLINE is set — unset it to allow the download, or \
             pre-stage the file at {}",
            expected_path.display(),
        );
    }

    let path = get_with_auth_fallback(api, anon_api, cache, repo_id, &repo, filename)
        .await
        .with_context(|| format!("download {repo_id}/{filename}@{revision}"))?;
    verify_sha256(&path, expected_sha256)
        .with_context(|| format!("{repo_id}/{filename}@{revision}"))?;
    Ok(path)
}

/// Download (or resolve from cache) all three GGUF models. Shared between the
/// two runtime contexts in `LlamaCppBackend::new` (already inside a tokio
/// runtime vs. not) so revision pinning and SHA-256 verification are
/// implemented exactly once instead of duplicated per branch.
async fn download_models(config: &LlamaCppConfig) -> Result<(PathBuf, PathBuf, PathBuf)> {
    validate_hf_endpoint()?;
    // `from_env()` (not `Api::new()`) so HF_HOME/HF_ENDPOINT are actually
    // honoured here — `Api::new()` silently ignores both and always talks to
    // the default https://huggingface.co using the default cache directory,
    // which previously left `validate_hf_endpoint` checking an env var the
    // downloader itself never read, and left `model_cache_report` (which does
    // use `Cache::from_env()`) checking a different directory than downloads
    // actually landed in whenever HF_HOME was set.
    let cache = Cache::from_env();

    // Token precedence: HF_TOKEN / HUGGING_FACE_HUB_TOKEN env vars override
    // hf-hub's own cache-file token (which `ApiBuilder::from_env` already
    // picks up via `Cache::token()`); with neither, no token is sent.
    let env_token = env_hf_token();
    let mut builder = ApiBuilder::from_env();
    if let Some(tok) = env_token.clone() {
        builder = builder.with_token(Some(tok));
    }
    let api = builder.build().context("hf-hub API init")?;

    // Build the anonymous fallback client only when a credential is actually
    // in play — env override or cache file — since it exists purely to retry
    // without one on a 401/403 (see `get_with_auth_fallback`).
    let anon_api = if env_token.is_some() || cache.token().is_some() {
        Some(
            ApiBuilder::from_env()
                .with_token(None)
                .build()
                .context("hf-hub anonymous API init")?,
        )
    } else {
        None
    };

    let ep = get_verified(
        &api,
        anon_api.as_ref(),
        &cache,
        &config.embed_repo,
        &config.embed_revision,
        &config.embed_file,
        &config.embed_sha256,
    )
    .await
    .context("embed model download")?;
    let rp = get_verified(
        &api,
        anon_api.as_ref(),
        &cache,
        &config.rerank_repo,
        &config.rerank_revision,
        &config.rerank_file,
        &config.rerank_sha256,
    )
    .await
    .context("rerank model download")?;
    let gp = get_verified(
        &api,
        anon_api.as_ref(),
        &cache,
        &config.generate_repo,
        &config.generate_revision,
        &config.generate_file,
        &config.generate_sha256,
    )
    .await
    .context("generate model download")?;

    Ok((ep, rp, gp))
}

pub struct LlamaCppBackend {
    _backend: LlamaBackend,
    /// GPU layer counts (and other load params) for lazy `ensure_*` loads.
    config: LlamaCppConfig,
    embed_model: Option<LlamaModel>,
    rerank_model: Option<LlamaModel>,
    generate_model: Option<LlamaModel>,
    embed_path: PathBuf,
    rerank_path: PathBuf,
    generate_path: PathBuf,
    /// `None` until first use; refreshed on every `ensure_*` call. Read by
    /// `release_idle` to decide whether to drop a model.
    embed_last_used: Option<Instant>,
    rerank_last_used: Option<Instant>,
    generate_last_used: Option<Instant>,
    embed_ctx_params: LlamaContextParams,
    rerank_ctx_params: LlamaContextParams,
    generate_ctx_params: LlamaContextParams,
    /// KV context size for the reranker — used to guard against token-overflow aborts.
    rerank_n_ctx: usize,
    /// KV context size for the generation model — used to guard against overflow.
    generate_n_ctx: usize,
    embed_model_name: String,
    rerank_model_name: String,
    generate_model_name: String,
}

impl LlamaCppBackend {
    /// Download models via hf-hub and initialize. Blocks the current thread.
    pub fn new(mut config: LlamaCppConfig) -> Result<Self> {
        // Honour RQMD_FORCE_CPU=1: disable Metal/CUDA offload for both models.
        // Matches the TS original's RQMD_FORCE_CPU contract documented in README.
        let force_cpu = std::env::var("RQMD_FORCE_CPU")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if force_cpu {
            config.embed_n_gpu_layers = 0;
            config.rerank_n_gpu_layers = 0;
            config.generate_n_gpu_layers = 0;
        }

        // Run async HF downloads while keeping this fn sync.
        // Spawning a new Runtime inside an existing tokio context panics; detect and
        // use block_in_place (which yields the thread to the scheduler) instead.
        let (embed_path, rerank_path, generate_path) = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| handle.block_on(download_models(&config)))?
            }
            Err(_) => tokio::runtime::Runtime::new()
                .context("tokio runtime init")?
                .block_on(download_models(&config))?,
        };

        // Install the tracing→log bridge BEFORE LlamaBackend::init() so that
        // ggml_metal_device_init (which runs during init) routes through the bridge
        // instead of escaping to the default ggml stderr logger.  The setters are
        // global and do not require an initialized backend.
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(true));

        let backend = LlamaBackend::init().context("LlamaBackend init")?;

        let embed_ctx_params = LlamaContextParams::default()
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Mean)
            .with_n_ctx(NonZeroU32::new(EMBED_CONTEXT_SIZE as u32))
            // encoder requires n_ubatch >= n_tokens; set to match n_ctx
            .with_n_ubatch(EMBED_CONTEXT_SIZE as u32);

        let rerank_ctx_params = LlamaContextParams::default()
            .with_embeddings(true)
            .with_pooling_type(LlamaPoolingType::Rank)
            .with_n_ctx(NonZeroU32::new(config.rerank_n_ctx))
            .with_n_batch(config.rerank_n_ctx)
            .with_n_ubatch(config.rerank_n_ctx);

        // Generation context: causal (no embeddings/pooling), standard n_batch/n_ubatch=1
        // since we decode one token at a time in the generation loop.
        let generate_ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(config.generate_n_ctx))
            .with_n_batch(config.generate_n_ctx)
            .with_n_ubatch(config.generate_n_ctx);

        let rerank_n_ctx = config.rerank_n_ctx as usize;
        let generate_n_ctx = config.generate_n_ctx as usize;
        let embed_model_name = format!("{}/{}", config.embed_repo, config.embed_file);
        let rerank_model_name = format!("{}/{}", config.rerank_repo, config.rerank_file);
        let generate_model_name = format!("{}/{}", config.generate_repo, config.generate_file);

        Ok(Self {
            _backend: backend,
            config,
            embed_model: None,
            rerank_model: None,
            generate_model: None,
            embed_path,
            rerank_path,
            generate_path,
            embed_last_used: None,
            rerank_last_used: None,
            generate_last_used: None,
            embed_ctx_params,
            rerank_ctx_params,
            generate_ctx_params,
            rerank_n_ctx,
            generate_n_ctx,
            embed_model_name,
            rerank_model_name,
            generate_model_name,
        })
    }

    /// Load the embed model if not already resident; refresh `embed_last_used`
    /// either way so a burst of calls doesn't get evicted mid-use.
    fn ensure_embed(&mut self) -> Result<()> {
        if self.embed_model.is_none() {
            self.embed_model = Some(
                LlamaModel::load_from_file(
                    &self._backend,
                    &self.embed_path,
                    &LlamaModelParams::default().with_n_gpu_layers(self.config.embed_n_gpu_layers),
                )
                .context("embed model load")?,
            );
        }
        self.embed_last_used = Some(Instant::now());
        Ok(())
    }

    fn ensure_rerank(&mut self) -> Result<()> {
        if self.rerank_model.is_none() {
            self.rerank_model = Some(
                LlamaModel::load_from_file(
                    &self._backend,
                    &self.rerank_path,
                    &LlamaModelParams::default().with_n_gpu_layers(self.config.rerank_n_gpu_layers),
                )
                .context("rerank model load")?,
            );
        }
        self.rerank_last_used = Some(Instant::now());
        Ok(())
    }

    fn ensure_generate(&mut self) -> Result<()> {
        if self.generate_model.is_none() {
            self.generate_model = Some(
                LlamaModel::load_from_file(
                    &self._backend,
                    &self.generate_path,
                    &LlamaModelParams::default()
                        .with_n_gpu_layers(self.config.generate_n_gpu_layers),
                )
                .context("generate model load")?,
            );
        }
        self.generate_last_used = Some(Instant::now());
        Ok(())
    }

    // Re-borrow a model immutably after `ensure_*`, as two disjoint shared
    // borrows (model ref + `&self._backend` in `new_context`) rather than
    // keeping the `&mut self` from `ensure_*` alive. `Result` instead of
    // `.expect()`: this backend lives behind `rqmd-mcp`'s shared
    // `Arc<Mutex<Store>>`, so a panic here would poison the mutex and wedge
    // every future query — a `Result` just fails the one in-flight request.
    fn embed_model_ref(&self) -> Result<&LlamaModel> {
        self.embed_model
            .as_ref()
            .context("embed model not loaded (ensure_embed was not called)")
    }

    fn rerank_model_ref(&self) -> Result<&LlamaModel> {
        self.rerank_model
            .as_ref()
            .context("rerank model not loaded (ensure_rerank was not called)")
    }

    fn generate_model_ref(&self) -> Result<&LlamaModel> {
        self.generate_model
            .as_ref()
            .context("generate model not loaded (ensure_generate was not called)")
    }
}

/// Pure staleness check, split out so it's unit-testable without a GGUF model.
fn is_idle(last_used: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    match last_used {
        None => false,
        Some(t) => now.saturating_duration_since(t) >= ttl,
    }
}

impl InferenceBackend for LlamaCppBackend {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        self.ensure_embed()?;
        let mut tokens = self
            .embed_model_ref()?
            .str_to_token(text, AddBos::Always)
            .context("embed tokenization")?;
        // Guard: encoder (llama_encode) requires n_ubatch >= n_tokens.  Without this
        // check a token-dense chunk can exceed EMBED_CONTEXT_SIZE and trigger a fatal
        // GGML_ASSERT abort.  Mirrors qmd's truncateToContextSize (src/llm.ts:1279).
        let safe_limit = EMBED_CONTEXT_SIZE - EMBED_TOKEN_MARGIN; // 2044
        if tokens.len() > safe_limit {
            tracing::debug!(
                tokens = tokens.len(),
                limit = safe_limit,
                "embed input truncated to context window"
            );
            tokens.truncate(safe_limit);
        }
        let mut ctx = self
            .embed_model_ref()?
            .new_context(&self._backend, self.embed_ctx_params.clone())
            .context("embed context")?;
        let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
        // Mean pooling requires every token to be marked as an output so
        // llama.cpp includes it in the pooled embedding.  Using false triggers
        // "embeddings required but some input tokens were not marked as outputs
        // -> overriding" at WARN level; using true is both correct and silent.
        batch.add_sequence(&tokens, 0, true)?;
        ctx.encode(&mut batch).context("encode")?;
        let emb = ctx.embeddings_seq_ith(0).context("embedding extract")?;
        Ok(l2_normalize(emb.to_vec()))
    }

    fn embed_query(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed(&format!("{EMBEDDINGGEMMA_QUERY_PREFIX}{text}"))
    }

    fn embed_passage(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed(&format!("{EMBEDDINGGEMMA_PASSAGE_PREFIX}{text}"))
    }

    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        self.ensure_rerank()?;
        let mut scores = Vec::with_capacity(docs.len());
        for doc in docs {
            // Fresh context per pair — KV cache holds positions 0..n for seq_id=0;
            // next batch at position 0 fails with "positions not consecutive".
            let mut ctx = self
                .rerank_model_ref()?
                .new_context(&self._backend, self.rerank_ctx_params.clone())
                .context("rerank context")?;
            let input = format!("Query: {query}\nDocument: {doc}");
            let mut tokens = self
                .rerank_model_ref()?
                .str_to_token(&input, AddBos::Always)
                .context("rerank tokenization")?;
            // Guard: ctx.decode() also aborts on n_ubatch < n_tokens.
            // Truncate to the rerank context window with the same BOS/EOS margin.
            let rerank_limit = self.rerank_n_ctx.saturating_sub(EMBED_TOKEN_MARGIN);
            if tokens.len() > rerank_limit {
                tracing::debug!(
                    tokens = tokens.len(),
                    limit = rerank_limit,
                    "rerank input truncated to context window"
                );
                tokens.truncate(rerank_limit);
            }
            let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
            // Rank pooling reads the last-token logit from embeddings_seq_ith, so the
            // result is identical whether or not every token is an output.  Passing true
            // avoids the "embeddings required but some input tokens were not marked as
            // outputs -> overriding" WARN that llama.cpp emits when output_all=true and
            // any token has logits=0.
            batch.add_sequence(&tokens, 0, true)?;
            // Qwen3-Reranker is a causal decoder → decode(), not encode()
            ctx.decode(&mut batch).context("rerank decode")?;
            let score_slice = ctx.embeddings_seq_ith(0).context("rerank score extract")?;
            scores.push(score_slice.first().copied().unwrap_or(f32::NEG_INFINITY));
        }
        Ok(scores)
    }

    fn generate(&mut self, prompt: &str) -> Result<String> {
        // Maximum tokens to generate. The ChatML prompt asks for three short lines
        // (lex:/vec:/hyde:); the early-stop below exits as soon as the hyde: line
        // is complete so we rarely reach this cap.
        const MAX_EXPANSION_TOKENS: usize = 256;

        // Guard: prevent the prompt from blowing the context window.
        let prompt_token_estimate = prompt.len() / 3; // conservative char-to-token ratio
        if prompt_token_estimate + MAX_EXPANSION_TOKENS > self.generate_n_ctx {
            anyhow::bail!(
                "expansion prompt too long ({} estimated tokens, ctx={})",
                prompt_token_estimate,
                self.generate_n_ctx
            );
        }

        self.ensure_generate()?;

        let mut ctx = self
            .generate_model_ref()?
            .new_context(&self._backend, self.generate_ctx_params.clone())
            .context("generate context")?;

        // Qwen3 uses ChatML — BOS is embedded in the template, so AddBos::Never avoids
        // a double BOS token.  If the prompt is a raw ChatML string this is correct;
        // if a bare question is passed, AddBos::Always is equally fine (one extra token).
        let tokens = self
            .generate_model_ref()?
            .str_to_token(prompt, AddBos::Always)
            .context("generate tokenization")?;

        let n_prompt = tokens.len();
        if n_prompt + MAX_EXPANSION_TOKENS > self.generate_n_ctx {
            anyhow::bail!(
                "expansion prompt too long after tokenization ({n_prompt} tokens, ctx={})",
                self.generate_n_ctx
            );
        }

        // Decode the full prompt in one batch (logits only on the last token).
        let mut batch = LlamaBatch::new(n_prompt.max(1), 1);
        for (i, &tok) in tokens.iter().enumerate() {
            let last = i == n_prompt - 1;
            batch
                .add(tok, i as i32, &[0], last)
                .context("batch add (prompt)")?;
        }
        ctx.decode(&mut batch).context("generate prompt decode")?;

        // Free-form sampler chain — no GBNF grammar.
        // GBNF grammar sampling is not viable on llama-cpp-2 v0.1.150: the
        // llama.cpp grammar engine aborts with GGML_ASSERT(!stacks.empty()) when
        // a multi-byte token drives the grammar into a dead state, and that assert
        // is uncatchable across Rust FFI.  The output parser (parse_and_run_expansion)
        // is lenient line-based and never needed the grammar's hard constraint.
        //
        // Rule: dist() MUST be last — temp/top_k/top_p are filters only (they do
        // not set cur_p.selected); without dist the sampler aborts with
        // GGML_ASSERT(cur_p.selected >= 0).
        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::temp(0.7),
            LlamaSampler::top_k(40),
            LlamaSampler::top_p(0.9, 1),
            LlamaSampler::dist(1337),
        ]);

        // Accumulate decoded text; a shared Decoder handles multi-byte UTF-8
        // sequences that span token boundaries correctly.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();

        // n_cur tracks the absolute KV-cache position for each generated token.
        // It starts right after the prompt and increments once per generated token.
        for (step, _) in (0..MAX_EXPANSION_TOKENS).enumerate() {
            let n_cur = (n_prompt + step) as i32;

            // After the prompt decode the last-token logits are at the last batch slot.
            // After each single-token decode the batch holds exactly one slot (index 0).
            let batch_last = batch.n_tokens() - 1;
            let tok = sampler.sample(&ctx, batch_last);
            sampler.accept(tok);

            if self.generate_model_ref()?.is_eog_token(tok) {
                break;
            }

            let piece = self
                .generate_model_ref()?
                .token_to_piece(tok, &mut decoder, false, None)
                .context("token_to_piece")?;
            out.push_str(&piece);

            // Early stop: once the hyde: line is complete (i.e. out contains
            // "hyde:" followed by a newline) the three-line format is done.
            // EOG token and MAX_EXPANSION_TOKENS are additional backstops.
            if let Some(after_hyde) = out.split_once("hyde:").map(|(_, tail)| tail) {
                if after_hyde.contains('\n') {
                    break;
                }
            }

            // Decode the next single token.
            batch.clear();
            batch
                .add(tok, n_cur, &[0], true)
                .context("batch add (decode)")?;
            ctx.decode(&mut batch).context("generate token decode")?;
        }

        Ok(out)
    }

    fn embed_model_name(&self) -> &str {
        &self.embed_model_name
    }

    fn rerank_model_name(&self) -> &str {
        &self.rerank_model_name
    }

    fn generate_model_name(&self) -> &str {
        &self.generate_model_name
    }

    fn release_idle(&mut self, ttl: Duration) -> usize {
        let now = Instant::now();
        let mut released = 0;

        if is_idle(self.embed_last_used, now, ttl) && self.embed_model.take().is_some() {
            self.embed_last_used = None;
            released += 1;
        }
        if is_idle(self.rerank_last_used, now, ttl) && self.rerank_model.take().is_some() {
            self.rerank_last_used = None;
            released += 1;
        }
        if is_idle(self.generate_last_used, now, ttl) && self.generate_model.take().is_some() {
            self.generate_last_used = None;
            released += 1;
        }

        released
    }
}

// ── NoBackend ─────────────────────────────────────────────────────────────────

/// Stub backend that errors on any ML call. Use for FTS-only commands.
pub struct NoBackend;

impl InferenceBackend for NoBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            embed: false,
            rerank: false,
            generate: false,
        }
    }

    fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("embed called without inference backend — run `rqmd embed` first")
    }
    fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
        anyhow::bail!("rerank called without inference backend")
    }
    fn generate(&mut self, _p: &str) -> Result<String> {
        anyhow::bail!("generate called without inference backend")
    }
    fn embed_model_name(&self) -> &str {
        "none"
    }
    fn rerank_model_name(&self) -> &str {
        "none"
    }
    fn generate_model_name(&self) -> &str {
        "none"
    }
}

/// Create a boxed NoBackend (convenience for Store::open).
pub fn no_backend() -> Box<dyn InferenceBackend> {
    Box::new(NoBackend)
}

// ── OrtBackend (feature-gated) ────────────────────────────────────────────────

#[cfg(feature = "ort-backend")]
pub mod ort_backend;

#[cfg(feature = "ort-backend")]
pub use ort_backend::{OrtBackend, OrtConfig, OrtEp};

// ── Backend factory ───────────────────────────────────────────────────────────

/// Backend selection. Read by `create_backend()`.
///
///   RQMD_INFERENCE_BACKEND=llama|ort  (default: llama)
///   RQMD_ORT_EP=auto|coreml|cuda|directml|cpu
#[derive(Debug, Clone)]
pub enum BackendKind {
    Llama,
    #[cfg(feature = "ort-backend")]
    Ort,
}

impl BackendKind {
    pub fn from_env() -> Self {
        match std::env::var("RQMD_INFERENCE_BACKEND")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            #[cfg(feature = "ort-backend")]
            "ort" => Self::Ort,
            _ => Self::Llama,
        }
    }

    /// The embed-model identity (`"repo/file"`) this kind's default config would
    /// report via `InferenceBackend::embed_model_name`, without downloading or
    /// constructing anything. Mirrors `create_backend`'s embed config exactly —
    /// used to detect stale embeddings (`rqmd doctor`) without paying the cost of
    /// loading a real backend, and without hardcoding one backend's defaults.
    pub fn default_embed_model_name(&self) -> String {
        match self {
            BackendKind::Llama => {
                let cfg = LlamaCppConfig::default();
                format!("{}/{}", cfg.embed_repo, cfg.embed_file)
            }
            #[cfg(feature = "ort-backend")]
            BackendKind::Ort => {
                let cfg = OrtConfig::default();
                format!("{}/{}", cfg.embed_repo, cfg.embed_file)
            }
        }
    }
}

/// Create the inference backend configured by env vars and `kind`.
/// Prints progress to stderr.
pub fn create_backend(kind: &BackendKind) -> Result<Box<dyn InferenceBackend>> {
    match kind {
        BackendKind::Llama => {
            tracing::info!("Loading LlamaCpp backend (downloads GGUF models on first run)...");
            let b =
                LlamaCppBackend::new(LlamaCppConfig::default()).context("LlamaCpp backend init")?;
            tracing::info!("LlamaCpp backend ready.");
            Ok(Box::new(b))
        }

        #[cfg(feature = "ort-backend")]
        BackendKind::Ort => {
            use ort_backend::OrtEp;
            let ep = std::env::var("RQMD_ORT_EP")
                .ok()
                .and_then(|s| OrtEp::from_str(&s))
                .unwrap_or(OrtEp::Auto);
            tracing::info!("Loading ORT backend (ep={ep:?}, downloads ONNX model on first run)...");
            let b = OrtBackend::new(OrtConfig {
                ep,
                ..OrtConfig::default()
            })
            .context("ORT backend init")?;
            let name = b.embed_model_name().to_string();
            tracing::info!("ORT backend ready ({name})");
            Ok(Box::new(b))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `cargo test` runs tests concurrently on a thread pool within one process,
    // so being adjacent in this file gives no ordering guarantee by itself.
    // `#[serial(inference_backend_env)]` is what actually prevents two tests
    // from racing on the shared RQMD_INFERENCE_BACKEND env var; `EnvVarGuard`
    // restores the prior value on drop (including on panic) so a failing
    // assertion can't leak state into whichever test runs next.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    #[serial(inference_backend_env)]
    fn backend_kind_from_env_defaults_to_llama_when_unset() {
        let _guard = EnvVarGuard::unset("RQMD_INFERENCE_BACKEND");
        assert!(matches!(BackendKind::from_env(), BackendKind::Llama));
    }

    #[test]
    #[serial(inference_backend_env)]
    fn backend_kind_from_env_parses_llama_case_insensitively() {
        let _guard = EnvVarGuard::set("RQMD_INFERENCE_BACKEND", "LLAMA");
        assert!(matches!(BackendKind::from_env(), BackendKind::Llama));
    }

    /// This is the exact identity `create_backend(&BackendKind::Llama)` would
    /// produce for its embed model — used by the stale-fingerprint check
    /// (`rqmd doctor`) to avoid loading a real backend just to compare.
    #[test]
    fn default_embed_model_name_matches_llama_config_defaults() {
        let cfg = LlamaCppConfig::default();
        assert_eq!(
            BackendKind::Llama.default_embed_model_name(),
            format!("{}/{}", cfg.embed_repo, cfg.embed_file)
        );
    }

    // Built by forward addition from a fixed base Instant, never by subtracting
    // from `Instant::now()` — a freshly-booted CI container can have too little
    // monotonic clock uptime for `checked_sub` to be safe.
    #[test]
    fn is_idle_never_used_is_not_idle() {
        let base = Instant::now();
        assert!(!is_idle(None, base, Duration::from_secs(300)));
    }

    #[test]
    fn is_idle_false_before_ttl_elapses() {
        let base = Instant::now();
        let last_used = Some(base);
        let now = base + Duration::from_secs(100);
        assert!(!is_idle(last_used, now, Duration::from_secs(300)));
    }

    #[test]
    fn hf_endpoint_https_accepted() {
        assert!(check_hf_endpoint_scheme("https://huggingface.co").is_ok());
        assert!(check_hf_endpoint_scheme("https://internal-mirror.example.com").is_ok());
    }

    #[test]
    fn hf_endpoint_http_rejected() {
        assert!(check_hf_endpoint_scheme("http://huggingface.co").is_err());
        assert!(check_hf_endpoint_scheme("http://evil.example.com").is_err());
    }

    #[test]
    fn hf_endpoint_other_schemes_rejected() {
        assert!(check_hf_endpoint_scheme("ftp://huggingface.co").is_err());
        assert!(check_hf_endpoint_scheme("huggingface.co").is_err());
    }

    #[test]
    fn sha256_file_matches_known_vector() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, b"abc").unwrap();
        // echo -n abc | sha256sum
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_file(&path).unwrap(), expected);
        assert!(verify_sha256(&path, expected).is_ok());
        let wrong = "0".repeat(64);
        assert!(verify_sha256(&path, &wrong).is_err());
    }

    #[test]
    fn is_idle_true_once_ttl_elapses() {
        let base = Instant::now();
        let last_used = Some(base);
        let now = base + Duration::from_secs(300);
        assert!(is_idle(last_used, now, Duration::from_secs(300)));
    }

    // ── resolve_cached / cache_has_file ─────────────────────────────────────
    //
    // Every test below builds its own `Cache::new(<tmpdir>/hub)` rather than
    // `Cache::from_env()`, so none of them read or depend on the developer's
    // real `~/.cache/huggingface` — the same reproduction environment this
    // fix was diagnosed against had a live cache with real multi-GB GGUFs in
    // it, and these tests must pass identically on a machine with no cache
    // at all.

    // sha256("abc") — reuses the same known vector as `sha256_file_matches_known_vector`.
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn test_repo(revision: &str) -> Repo {
        Repo::with_revision(
            "some/repo".to_string(),
            RepoType::Model,
            revision.to_string(),
        )
    }

    /// Regression test for the actual bug: a snapshot downloaded before
    /// revision pinning existed (bytes at `snapshots/<rev>/<file>`, no
    /// `refs/<rev>`) must be adopted without a network call, and the ref must
    /// be healed so hf-hub's own fast path finds it on every later run.
    #[test]
    fn resolve_cached_adopts_legacy_snapshot_and_heals_ref() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Cache::new(dir.path().join("hub"));
        let repo = test_repo("deadbeef");
        let pointer = cache.repo(repo.clone()).pointer_path(repo.revision());
        std::fs::create_dir_all(&pointer).unwrap();
        std::fs::write(pointer.join("model.gguf"), b"abc").unwrap();

        // Before healing, hf-hub's own `CacheRepo::get` sees nothing — it
        // only ever consults refs/<rev>, which doesn't exist yet.
        assert!(cache.repo(repo.clone()).get("model.gguf").is_none());

        let resolved = resolve_cached(&cache, &repo, "model.gguf", ABC_SHA256).unwrap();
        assert!(resolved.is_some());

        // Healed: hf-hub's own fast path now finds it directly, with no help
        // from `resolve_cached`.
        assert!(cache.repo(repo.clone()).get("model.gguf").is_some());
    }

    /// A ref that already exists must short-circuit before any hashing —
    /// proven here by planting content that does *not* match the expected
    /// hash and asserting `resolve_cached` still succeeds (trust-on-first-use
    /// for content that hf-hub itself already considers cached).
    #[test]
    fn resolve_cached_fast_path_does_not_rehash() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Cache::new(dir.path().join("hub"));
        let repo = test_repo("deadbeef");
        let pointer = cache.repo(repo.clone()).pointer_path(repo.revision());
        std::fs::create_dir_all(&pointer).unwrap();
        std::fs::write(pointer.join("model.gguf"), b"not-abc-at-all").unwrap();
        cache
            .repo(repo.clone())
            .create_ref(repo.revision())
            .unwrap();

        let resolved = resolve_cached(&cache, &repo, "model.gguf", ABC_SHA256).unwrap();
        assert!(resolved.is_some());
    }

    /// A legacy-layout snapshot with the wrong bytes (corruption/tampering)
    /// must fail loudly and must NOT heal the ref — healing a bad file would
    /// make every later run trust it via the fast path forever.
    #[test]
    fn resolve_cached_rejects_legacy_snapshot_with_wrong_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Cache::new(dir.path().join("hub"));
        let repo = test_repo("deadbeef");
        let pointer = cache.repo(repo.clone()).pointer_path(repo.revision());
        std::fs::create_dir_all(&pointer).unwrap();
        std::fs::write(pointer.join("model.gguf"), b"tampered").unwrap();

        assert!(resolve_cached(&cache, &repo, "model.gguf", ABC_SHA256).is_err());
        assert!(cache.repo(repo.clone()).get("model.gguf").is_none());
    }

    #[test]
    fn resolve_cached_returns_none_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Cache::new(dir.path().join("hub"));
        let repo = test_repo("deadbeef");
        assert!(resolve_cached(&cache, &repo, "model.gguf", ABC_SHA256)
            .unwrap()
            .is_none());
    }

    /// `doctor`'s status check must see the same legacy layout `resolve_cached`
    /// adopts, but must never mutate the cache doing so.
    #[test]
    fn cache_has_file_sees_legacy_snapshot_without_mutating_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = Cache::new(dir.path().join("hub"));
        let repo = test_repo("deadbeef");
        let pointer = cache.repo(repo.clone()).pointer_path(repo.revision());
        std::fs::create_dir_all(&pointer).unwrap();
        std::fs::write(pointer.join("model.gguf"), b"abc").unwrap();

        assert!(cache_has_file(
            &cache,
            "some/repo",
            "deadbeef",
            "model.gguf"
        ));
        assert!(cache.repo(repo.clone()).get("model.gguf").is_none());
    }

    // ── auth fallback / offline predicates ──────────────────────────────────

    #[test]
    fn status_is_auth_failure_matches_401_and_403_only() {
        assert!(status_is_auth_failure(401));
        assert!(status_is_auth_failure(403));
        assert!(!status_is_auth_failure(404));
        assert!(!status_is_auth_failure(500));
        assert!(!status_is_auth_failure(200));
    }

    #[test]
    fn offline_from_env_value_parses_common_forms() {
        assert!(offline_from_env_value("1"));
        assert!(offline_from_env_value("true"));
        assert!(offline_from_env_value("TRUE"));
        assert!(offline_from_env_value("yes"));
        assert!(!offline_from_env_value(""));
        assert!(!offline_from_env_value("0"));
        assert!(!offline_from_env_value("false"));
        assert!(!offline_from_env_value("no"));
    }
}
