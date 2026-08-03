//! rqmd-llm — inference backend abstraction and llama-cpp-2 implementation.
//!
//! Feature flags:
//!   (default)    — LlamaCppBackend via llama-cpp-2 (GGUF, Metal/CUDA/Vulkan)
//!   ort-backend  — OrtBackend via ONNX Runtime (CoreML/CUDA/DirectML/CPU)
//!
//! Backend selection at runtime (read by `create_backend()`):
//!   RRQMD_INFERENCE_BACKEND=llama|ort   (default: llama)
//!   RRQMD_ORT_EP=auto|coreml|cuda|directml|cpu
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
//! `RRQMD_MODEL_IDLE_TTL` seconds of inactivity (default 300; `0` disables) —
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
/// unit-normalized contract. Mirrors `ort_backend::l2_normalize`; duplicated rather
/// than shared because `ort_backend` is feature-gated and not built by default.
fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|&x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

// ── InferenceBackend trait ────────────────────────────────────────────────────

/// Core inference operations needed by qmd's search pipeline.
pub trait InferenceBackend: Send {
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

/// Return the cache status for all three models without loading any weights.
/// All repos/revisions come from `config` so they match what the downloader
/// uses exactly — `cache.model(repo)` (implicit "main" revision) would
/// under-report once downloads are pinned to a specific commit, since the
/// cache stores each revision under its own `refs/<revision>` entry.
pub fn model_cache_report(config: &LlamaCppConfig) -> ModelCacheReport {
    // from_env() honours HF_HOME; falls back to ~/.cache/huggingface/hub.
    let cache = Cache::from_env();
    let cached = |repo: &str, revision: &str, file: &str| -> bool {
        cache
            .repo(Repo::with_revision(
                repo.to_string(),
                RepoType::Model,
                revision.to_string(),
            ))
            .get(file)
            .is_some()
    };
    ModelCacheReport {
        cache_root: cache.path().clone(),
        embed_cached: cached(
            &config.embed_repo,
            &config.embed_revision,
            &config.embed_file,
        ),
        rerank_cached: cached(
            &config.rerank_repo,
            &config.rerank_revision,
            &config.rerank_file,
        ),
        generate_cached: cached(
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

/// Fetch `filename` from `repo_id` pinned at `revision`, from cache if
/// present, otherwise downloading it. SHA-256 is verified only when the file
/// was not already cached: a cached file was already verified the run it was
/// first downloaded, and re-hashing a multi-gigabyte GGUF on every process
/// start would be a real latency cost (this is the same trust-on-first-use
/// model `git-lfs` itself uses — it checks the blob hash on checkout, not on
/// every subsequent read).
async fn get_verified(
    api: &Api,
    cache: &Cache,
    repo_id: &str,
    revision: &str,
    filename: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let repo = Repo::with_revision(repo_id.to_string(), RepoType::Model, revision.to_string());
    let already_cached = cache.repo(repo.clone()).get(filename).is_some();
    let path = api
        .repo(repo)
        .get(filename)
        .await
        .with_context(|| format!("download {repo_id}/{filename}@{revision}"))?;
    if !already_cached {
        verify_sha256(&path, expected_sha256)
            .with_context(|| format!("{repo_id}/{filename}@{revision}"))?;
    }
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
    let api = ApiBuilder::from_env().build().context("hf-hub API init")?;
    let cache = Cache::from_env();

    let ep = get_verified(
        &api,
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
        // Honour RRQMD_FORCE_CPU=1: disable Metal/CUDA offload for both models.
        // Matches the TS original's RRQMD_FORCE_CPU contract documented in README.
        let force_cpu = std::env::var("RRQMD_FORCE_CPU")
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
///   RRQMD_INFERENCE_BACKEND=llama|ort  (default: llama)
///   RRQMD_ORT_EP=auto|coreml|cuda|directml|cpu
#[derive(Debug, Clone)]
pub enum BackendKind {
    Llama,
    #[cfg(feature = "ort-backend")]
    Ort,
}

impl BackendKind {
    pub fn from_env() -> Self {
        match std::env::var("RRQMD_INFERENCE_BACKEND")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            #[cfg(feature = "ort-backend")]
            "ort" => Self::Ort,
            _ => Self::Llama,
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
            let ep = std::env::var("RRQMD_ORT_EP")
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
}
