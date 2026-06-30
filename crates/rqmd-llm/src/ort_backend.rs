//! OrtBackend — ONNX Runtime inference backend (ort 2.0.0-rc.12).
//!
//! Provides `embed` and `embed_batch` via ort v2 with pluggable EPs:
//!   - CoreML  (macOS ANE/GPU) — fastest for embedding-sized models on Apple Silicon
//!   - CUDA    (NVIDIA GPU)
//!   - DirectML (Windows GPU)
//!   - CPU     (universal fallback)
//!
//! Default model: BAAI/bge-base-en-v1.5 (768-dim, matches LlamaCpp embed dim).
//! Reranking and GBNF generation require LlamaCppBackend.

use anyhow::{Context, Result};
use ndarray::Ix3;
use std::path::PathBuf;

use crate::InferenceBackend;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const DEFAULT_ORT_EMBED_REPO: &str = "BAAI/bge-base-en-v1.5";
pub const DEFAULT_ORT_EMBED_FILE: &str = "onnx/model.onnx";
pub const DEFAULT_ORT_TOKENIZER_FILE: &str = "tokenizer.json";

// ── Execution provider ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrtEp {
    /// Choose best available EP: CoreML on macOS, CPU elsewhere.
    Auto,
    /// Apple Neural Engine / GPU via CoreML (macOS only).
    CoreMl,
    /// NVIDIA GPU via CUDA (requires ort `cuda` feature + CUDA toolkit).
    Cuda,
    /// Windows GPU via DirectML (Windows only).
    DirectMl,
    /// CPU-only; useful for benchmarking.
    Cpu,
}

impl OrtEp {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "coreml" | "core-ml" | "core_ml" => Some(Self::CoreMl),
            "cuda" => Some(Self::Cuda),
            "directml" | "direct-ml" | "direct_ml" => Some(Self::DirectMl),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

pub struct OrtConfig {
    pub embed_repo: String,
    pub embed_file: String,
    pub tokenizer_file: String,
    pub ep: OrtEp,
    /// Embedding dimension; must match the ONNX model's output.
    pub embed_dim: usize,
    pub hf_cache_dir: Option<PathBuf>,
    pub max_seq_len: usize,
}

impl Default for OrtConfig {
    fn default() -> Self {
        Self {
            embed_repo: DEFAULT_ORT_EMBED_REPO.to_string(),
            embed_file: DEFAULT_ORT_EMBED_FILE.to_string(),
            tokenizer_file: DEFAULT_ORT_TOKENIZER_FILE.to_string(),
            ep: OrtEp::Auto,
            embed_dim: 768,
            hf_cache_dir: None,
            max_seq_len: 512,
        }
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct OrtBackend {
    session: ort::session::Session,
    tokenizer: tokenizers::Tokenizer,
    embed_dim: usize,
    has_token_type_ids: bool,
    /// True if model outputs a pooled `sentence_embedding` (skip mean-pool step).
    has_sentence_embedding: bool,
    model_name: String,
}

impl OrtBackend {
    pub fn new(config: OrtConfig) -> Result<Self> {
        use hf_hub::api::tokio::Api;

        // Download model and tokenizer via hf-hub (async, but new() is sync)
        let rt = tokio::runtime::Runtime::new().context("tokio runtime")?;
        let (model_path, tokenizer_path) = rt.block_on(async {
            let api = Api::new().context("hf-hub API")?;
            let repo = api.model(config.embed_repo.clone());
            let model = repo
                .get(&config.embed_file)
                .await
                .context("model download")?;
            let tok = repo
                .get(&config.tokenizer_file)
                .await
                .context("tokenizer download")?;
            Ok::<_, anyhow::Error>((model, tok))
        })?;

        let ep = resolve_ep(config.ep);

        // Cap ORT native logging. Default: Warning (suppress Info/Verbose model-loader
        // noise). With RRQMD_VERBOSE=1 (set by --verbose), allow Verbose output.
        let ort_log_level = if std::env::var("RRQMD_VERBOSE").is_ok() {
            ort::logging::LogLevel::Verbose
        } else {
            ort::logging::LogLevel::Warning
        };

        // ort builder returns Error<SessionBuilder> (not std::error::Error), so use map_err
        let session = ort::session::Session::builder()
            .map_err(|e| anyhow::anyhow!("ORT session builder: {e:?}"))?
            .with_execution_providers([ep])
            .map_err(|e| anyhow::anyhow!("ORT EP registration: {e:?}"))?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("ORT opt level: {e:?}"))?
            .with_log_level(ort_log_level)
            .map_err(|e| anyhow::anyhow!("ORT log level: {e:?}"))?
            .commit_from_file(&model_path)
            .context("ORT session load")?;

        // Introspect model I/O to detect optional inputs/outputs
        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();
        let output_names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        let has_token_type_ids = input_names.iter().any(|n| n == "token_type_ids");
        let has_sentence_embedding = output_names.iter().any(|n| n == "sentence_embedding");

        // Load tokenizer and configure padding + truncation
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("tokenizer load: {e}"))?;

        tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
        }));
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: config.max_seq_len,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
                stride: 0,
                direction: tokenizers::TruncationDirection::Right,
            }))
            .map_err(|e| anyhow::anyhow!("tokenizer truncation: {e}"))?;

        let model_name = format!("{}/{}", config.embed_repo, config.embed_file);

        Ok(Self {
            session,
            tokenizer,
            embed_dim: config.embed_dim,
            has_token_type_ids,
            has_sentence_embedding,
            model_name,
        })
    }

    fn run_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        use ort::value::Tensor;

        let batch = texts.len();

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;

        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(1);

        // Build flat input vectors
        let mut flat_ids = vec![0i64; batch * seq_len];
        let mut flat_mask = vec![0i64; batch * seq_len];
        let mut flat_type = vec![0i64; batch * seq_len];

        for (i, enc) in encodings.iter().enumerate() {
            for (j, (&id, &mask)) in enc
                .get_ids()
                .iter()
                .zip(enc.get_attention_mask().iter())
                .enumerate()
            {
                flat_ids[i * seq_len + j] = id as i64;
                flat_mask[i * seq_len + j] = mask as i64;
            }
            if self.has_token_type_ids {
                for (j, &t) in enc.get_type_ids().iter().enumerate() {
                    flat_type[i * seq_len + j] = t as i64;
                }
            }
        }

        let shape = [batch as i64, seq_len as i64];

        // Clone flat_mask before moving into tensor — we need it again for mean-pooling weights.
        let flat_mask_for_pool = flat_mask.clone();
        let ids_tensor = Tensor::<i64>::from_array((shape, flat_ids)).context("ids tensor")?;
        let mask_tensor = Tensor::<i64>::from_array((shape, flat_mask)).context("mask tensor")?;

        let outputs = if self.has_token_type_ids {
            let type_tensor =
                Tensor::<i64>::from_array((shape, flat_type)).context("type_ids tensor")?;
            self.session.run(ort::inputs![
                "input_ids"      => ids_tensor,
                "attention_mask" => mask_tensor,
                "token_type_ids" => type_tensor,
            ])
        } else {
            self.session.run(ort::inputs![
                "input_ids"      => ids_tensor,
                "attention_mask" => mask_tensor,
            ])
        }
        .context("ORT inference")?;

        // Extract embeddings
        if self.has_sentence_embedding {
            // Model outputs pre-pooled embeddings [batch, dim]
            let arr = outputs["sentence_embedding"]
                .try_extract_array::<f32>()
                .context("sentence_embedding extract")?;
            let arr2 = arr
                .into_dimensionality::<ndarray::Ix2>()
                .context("expected 2D sentence_embedding")?;
            Ok((0..batch)
                .map(|i| l2_normalize(arr2.row(i).to_vec()))
                .collect())
        } else {
            // Mean-pool last_hidden_state [batch, seq, dim] with attention mask
            let arr = outputs["last_hidden_state"]
                .try_extract_array::<f32>()
                .context("last_hidden_state extract")?;
            let arr3 = arr
                .into_dimensionality::<Ix3>()
                .context("expected 3D last_hidden_state")?;
            let dim = self.embed_dim;
            Ok((0..batch)
                .map(|i| {
                    // attention mask weight and sum
                    let mask_sum = (0..seq_len)
                        .map(|j| flat_mask_for_pool[i * seq_len + j] as f32)
                        .sum::<f32>()
                        .max(1.0);
                    let mut pooled = vec![0.0f32; dim];
                    for j in 0..seq_len {
                        let w = flat_mask_for_pool[i * seq_len + j] as f32;
                        for (k, p) in pooled.iter_mut().enumerate().take(dim) {
                            *p += arr3[[i, j, k]] * w;
                        }
                    }
                    for v in &mut pooled {
                        *v /= mask_sum;
                    }
                    l2_normalize(pooled)
                })
                .collect())
        }
    }
}

impl InferenceBackend for OrtBackend {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        Ok(self.run_batch(&[text])?.remove(0))
    }

    fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        self.run_batch(texts)
    }

    fn rerank(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f32>> {
        anyhow::bail!("OrtBackend: reranking not supported — use LlamaCppBackend for hybrid query")
    }

    fn generate(&mut self, _p: &str) -> Result<String> {
        anyhow::bail!("OrtBackend: generation requires LlamaCppBackend")
    }

    fn embed_model_name(&self) -> &str {
        &self.model_name
    }

    fn rerank_model_name(&self) -> &str {
        "none (OrtBackend)"
    }

    fn generate_model_name(&self) -> &str {
        "none (OrtBackend)"
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|&x| x * x).sum::<f32>().sqrt().max(1e-12);
    for x in &mut v {
        *x /= norm;
    }
    v
}

fn resolve_ep(ep: OrtEp) -> ort::ep::ExecutionProviderDispatch {
    use ort::ep::{DirectML, CPU, CUDA};
    match ep {
        OrtEp::Cuda => CUDA::default().build(),
        OrtEp::DirectMl => DirectML::default().build(),
        OrtEp::Cpu => CPU::default().build(),
        OrtEp::CoreMl | OrtEp::Auto => {
            #[cfg(target_os = "macos")]
            {
                ort::ep::CoreML::default().build()
            }
            #[cfg(not(target_os = "macos"))]
            {
                CPU::default().build()
            }
        }
    }
}
