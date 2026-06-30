//! qmd-llm — inference backend abstraction and llama-cpp-2 implementation.
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
//! - n_ctx=512 and n_gpu_layers=14 for reranker on Apple Silicon (448 MiB KV limit)

use anyhow::{Context, Result};
use hf_hub::{api::tokio::Api, Cache};
use llama_cpp_2::{
    context::params::{LlamaContextParams, LlamaPoolingType},
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
    send_logs_to_tracing, LogOptions,
};
use std::{num::NonZeroU32, path::PathBuf};

// ── Default model repos (mirrors qmd's llm.ts defaults) ──────────────────────

pub const DEFAULT_EMBED_REPO: &str = "ggml-org/embeddinggemma-300M-GGUF";
pub const DEFAULT_EMBED_FILE: &str = "embeddinggemma-300M-Q8_0.gguf";
pub const DEFAULT_RERANK_REPO: &str = "ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF";
pub const DEFAULT_RERANK_FILE: &str = "qwen3-reranker-0.6b-q8_0.gguf";
pub const DEFAULT_GENERATE_REPO: &str = "ggml-org/Qwen3-1.7B-GGUF";
pub const DEFAULT_GENERATE_FILE: &str = "Qwen3-1.7B-Q8_0.gguf";

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

    /// Rerank: score (query, doc) pairs. Returns a scalar score per pair.
    /// Higher = more relevant. Scores are NOT normalized across pairs.
    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;

    /// Generate free-form text from a prompt. Returns the generated string.
    /// The caller is responsible for parsing the output (e.g. stripping lex:/vec:/hyde: lines).
    fn generate(&mut self, prompt: &str) -> Result<String>;

    fn embed_model_name(&self) -> &str;
    fn rerank_model_name(&self) -> &str;
    fn generate_model_name(&self) -> &str;
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
/// All repos come from `config` so they match what the downloader uses exactly.
pub fn model_cache_report(config: &LlamaCppConfig) -> ModelCacheReport {
    // from_env() honours HF_HOME; falls back to ~/.cache/huggingface/hub.
    let cache = Cache::from_env();
    let cached =
        |repo: &str, file: &str| -> bool { cache.model(repo.to_string()).get(file).is_some() };
    ModelCacheReport {
        cache_root: cache.path().clone(),
        embed_cached: cached(&config.embed_repo, &config.embed_file),
        rerank_cached: cached(&config.rerank_repo, &config.rerank_file),
        generate_cached: cached(&config.generate_repo, &config.generate_file),
    }
}

// ── LlamaCppBackend ───────────────────────────────────────────────────────────

pub struct LlamaCppConfig {
    /// HF repo ID (e.g. "ggml-org/embeddinggemma-300M-GGUF") or local path.
    pub embed_repo: String,
    pub embed_file: String,
    pub rerank_repo: String,
    pub rerank_file: String,
    pub generate_repo: String,
    pub generate_file: String,
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
            rerank_repo: DEFAULT_RERANK_REPO.to_string(),
            rerank_file: DEFAULT_RERANK_FILE.to_string(),
            generate_repo: DEFAULT_GENERATE_REPO.to_string(),
            generate_file: DEFAULT_GENERATE_FILE.to_string(),
            embed_n_gpu_layers: 99,
            rerank_n_gpu_layers: 14,
            rerank_n_ctx: 2048,
            generate_n_gpu_layers: 99,
            generate_n_ctx: 2048,
            hf_cache_dir: None,
        }
    }
}

pub struct LlamaCppBackend {
    _backend: LlamaBackend,
    embed_model: LlamaModel,
    rerank_model: LlamaModel,
    generate_model: LlamaModel,
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
        let (embed_path, rerank_path, generate_path) =
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        let api = Api::new().context("hf-hub API init")?;
                        let ep = api
                            .model(config.embed_repo.clone())
                            .get(&config.embed_file)
                            .await
                            .context("embed model download")?;
                        let rp = api
                            .model(config.rerank_repo.clone())
                            .get(&config.rerank_file)
                            .await
                            .context("rerank model download")?;
                        let gp = api
                            .model(config.generate_repo.clone())
                            .get(&config.generate_file)
                            .await
                            .context("generate model download")?;
                        Ok::<_, anyhow::Error>((ep, rp, gp))
                    })
                })?,
                Err(_) => tokio::runtime::Runtime::new()
                    .context("tokio runtime init")?
                    .block_on(async {
                        let api = Api::new().context("hf-hub API init")?;
                        let ep = api
                            .model(config.embed_repo.clone())
                            .get(&config.embed_file)
                            .await
                            .context("embed model download")?;
                        let rp = api
                            .model(config.rerank_repo.clone())
                            .get(&config.rerank_file)
                            .await
                            .context("rerank model download")?;
                        let gp = api
                            .model(config.generate_repo.clone())
                            .get(&config.generate_file)
                            .await
                            .context("generate model download")?;
                        Ok::<_, anyhow::Error>((ep, rp, gp))
                    })?,
            };

        // Install the tracing→log bridge BEFORE LlamaBackend::init() so that
        // ggml_metal_device_init (which runs during init) routes through the bridge
        // instead of escaping to the default ggml stderr logger.  The setters are
        // global and do not require an initialized backend.
        send_logs_to_tracing(LogOptions::default().with_logs_enabled(true));

        let backend = LlamaBackend::init().context("LlamaBackend init")?;

        let embed_model = LlamaModel::load_from_file(
            &backend,
            &embed_path,
            &LlamaModelParams::default().with_n_gpu_layers(config.embed_n_gpu_layers),
        )
        .context("embed model load")?;

        let rerank_model = LlamaModel::load_from_file(
            &backend,
            &rerank_path,
            &LlamaModelParams::default().with_n_gpu_layers(config.rerank_n_gpu_layers),
        )
        .context("rerank model load")?;

        let generate_model = LlamaModel::load_from_file(
            &backend,
            &generate_path,
            &LlamaModelParams::default().with_n_gpu_layers(config.generate_n_gpu_layers),
        )
        .context("generate model load")?;

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

        Ok(Self {
            _backend: backend,
            embed_model,
            rerank_model,
            generate_model,
            embed_ctx_params,
            rerank_ctx_params,
            generate_ctx_params,
            rerank_n_ctx: config.rerank_n_ctx as usize,
            generate_n_ctx: config.generate_n_ctx as usize,
            embed_model_name: format!("{}/{}", config.embed_repo, config.embed_file),
            rerank_model_name: format!("{}/{}", config.rerank_repo, config.rerank_file),
            generate_model_name: format!("{}/{}", config.generate_repo, config.generate_file),
        })
    }
}

impl InferenceBackend for LlamaCppBackend {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        let mut tokens = self
            .embed_model
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
            .embed_model
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
        Ok(emb.to_vec())
    }

    fn rerank(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let mut scores = Vec::with_capacity(docs.len());
        for doc in docs {
            // Fresh context per pair — KV cache holds positions 0..n for seq_id=0;
            // next batch at position 0 fails with "positions not consecutive".
            let mut ctx = self
                .rerank_model
                .new_context(&self._backend, self.rerank_ctx_params.clone())
                .context("rerank context")?;
            let input = format!("Query: {query}\nDocument: {doc}");
            let mut tokens = self
                .rerank_model
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

        let mut ctx = self
            .generate_model
            .new_context(&self._backend, self.generate_ctx_params.clone())
            .context("generate context")?;

        // Qwen3 uses ChatML — BOS is embedded in the template, so AddBos::Never avoids
        // a double BOS token.  If the prompt is a raw ChatML string this is correct;
        // if a bare question is passed, AddBos::Always is equally fine (one extra token).
        let tokens = self
            .generate_model
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
            batch.add(tok, i as i32, &[0], last).context("batch add (prompt)")?;
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

            if self.generate_model.is_eog_token(tok) {
                break;
            }

            let piece = self
                .generate_model
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
            batch.add(tok, n_cur, &[0], true).context("batch add (decode)")?;
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
}

// ── NoBackend ─────────────────────────────────────────────────────────────────

/// Stub backend that errors on any ML call. Use for FTS-only commands.
pub struct NoBackend;

impl InferenceBackend for NoBackend {
    fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
        anyhow::bail!("embed called without inference backend — run `qmd embed` first")
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
