//! Neural sentence-embedding model — `all-MiniLM-L6-v2` (BERT) run with
//! [Candle], Hugging Face's pure-Rust ML framework.
//!
//! Unlike ONNX-based embedders this has **no C/C++ dependency**: inference is
//! pure Rust, so it builds identically on every platform. The ~90 MB model
//! weights are fetched once from Hugging Face and cached on disk; the loaded
//! model is shared process-wide (one instance for all agents).
//!
//! [Candle]: https://github.com/huggingface/candle

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::Tokenizer;

use crate::error::MemoryError;

/// Output dimensionality of `all-MiniLM-L6-v2`.
pub const NEURAL_DIM: usize = 384;

/// Identifier stored alongside vectors — a change here triggers a re-embed.
pub const NEURAL_ID: &str = "minilm-l6-v2";

/// Base URL for the model files on the Hugging Face hub.
const HF_BASE: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";

/// BERT position embeddings cap the sequence length; truncate well within it.
const MAX_TOKENS: usize = 256;

/// A loaded neural sentence embedder. Cheap to clone (it is always held in an
/// `Arc`); construction downloads + loads the model.
pub struct NeuralEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

/// Process-wide shared embedder — loaded at most once.
static SHARED: OnceLock<Result<Arc<NeuralEmbedder>, String>> = OnceLock::new();

impl NeuralEmbedder {
    /// The process-wide embedder, initialising (download + load) on first use.
    /// Subsequent calls are free. An initialisation failure is cached so the
    /// expensive attempt is not repeated.
    ///
    /// `load` does blocking HTTP, and `reqwest::blocking` creates then drops
    /// its own Tokio runtime — which panics if done inside another runtime's
    /// async context (and the daemon calls this from an async task). So the
    /// load runs on a dedicated OS thread with no ambient runtime; the caller
    /// simply blocks on it once, at startup.
    pub fn shared() -> Result<Arc<NeuralEmbedder>, MemoryError> {
        SHARED
            .get_or_init(|| {
                std::thread::spawn(Self::load)
                    .join()
                    .unwrap_or_else(|_| {
                        Err(MemoryError::Embedding(
                            "model-load thread panicked".to_string(),
                        ))
                    })
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .clone()
            .map_err(MemoryError::Embedding)
    }

    /// Download (if needed) and load the model.
    fn load() -> Result<Self, MemoryError> {
        let dir = model_cache_dir()?;
        let config_path = ensure_file(&dir, "config.json")?;
        let tokenizer_path = ensure_file(&dir, "tokenizer.json")?;
        let weights_path = ensure_file(&dir, "model.safetensors")?;

        let device = Device::Cpu;
        let config: Config = serde_json::from_slice(&std::fs::read(&config_path)?)
            .map_err(|e| MemoryError::Embedding(format!("model config: {e}")))?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| MemoryError::Embedding(format!("tokenizer: {e}")))?;

        // SAFETY: mmap of a model file we just wrote/verified; standard for
        // Candle weight loading.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)
                .map_err(|e| MemoryError::Embedding(format!("weights: {e}")))?
        };
        let model = BertModel::load(vb, &config)
            .map_err(|e| MemoryError::Embedding(format!("model load: {e}")))?;

        tracing::info!("neural embedder ready ({NEURAL_ID}, {NEURAL_DIM}-dim)");
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embed one text into a 384-dim, L2-normalised vector by running BERT and
    /// mean-pooling the token embeddings.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let ce = |e: candle_core::Error| MemoryError::Embedding(e.to_string());

        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| MemoryError::Embedding(format!("tokenize: {e}")))?;
        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        ids.truncate(MAX_TOKENS);
        if ids.is_empty() {
            return Ok(vec![0.0; NEURAL_DIM]);
        }
        let n = ids.len();

        let input_ids = Tensor::new(ids.as_slice(), &self.device)
            .and_then(|t| t.reshape((1, n)))
            .map_err(ce)?;
        let token_type_ids = input_ids.zeros_like().map_err(ce)?;

        // [1, n_tokens, 384]
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, None)
            .map_err(|e| MemoryError::Embedding(format!("bert forward: {e}")))?;

        // Mean-pool over the token axis → [1, 384].
        let pooled = hidden
            .sum(1)
            .and_then(|t| t.affine(1.0 / n as f64, 0.0))
            .and_then(|t| t.flatten_all())
            .map_err(ce)?;
        let vec = pooled.to_vec1::<f32>().map_err(ce)?;
        Ok(l2_normalize(vec))
    }
}

/// `{AXOCOATL_DATA_DIR or ./data}/models/all-MiniLM-L6-v2/`.
fn model_cache_dir() -> Result<PathBuf, MemoryError> {
    let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let dir = PathBuf::from(data_dir)
        .join("models")
        .join("all-MiniLM-L6-v2");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Return `dir/name`, downloading it from the Hugging Face hub if absent.
fn ensure_file(dir: &Path, name: &str) -> Result<PathBuf, MemoryError> {
    let path = dir.join(name);
    if path.exists() {
        return Ok(path);
    }
    let url = format!("{HF_BASE}/{name}");
    tracing::info!(%url, "downloading embedding-model file (one-time)");
    let bytes = reqwest::blocking::get(&url)
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| MemoryError::Embedding(format!("downloading {name}: {e}")))?;
    // Write atomically so an interrupted download can't leave a corrupt file.
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Scale a vector to unit L2 length so cosine similarity is a plain dot product.
fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dot product — for L2-normalised vectors this is cosine similarity.
    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    #[ignore = "downloads the ~90MB all-MiniLM-L6-v2 model from Hugging Face"]
    fn neural_embeddings_capture_meaning() {
        let emb = NeuralEmbedder::shared().expect("model should download + load");

        let a = emb.embed("I prefer terse, concise answers").unwrap();
        assert_eq!(a.len(), NEURAL_DIM, "MiniLM is 384-dimensional");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "embeddings must be L2-normalised"
        );

        // The whole point of the upgrade: similarity tracks *meaning*, not
        // shared words. These two phrases share almost no vocabulary.
        let related = dot(
            &a,
            &emb.embed("keep your responses short and to the point")
                .unwrap(),
        );
        let unrelated = dot(
            &a,
            &emb.embed("the weather in Tokyo is rainy today").unwrap(),
        );
        assert!(
            related > unrelated,
            "meaning-related text must outscore unrelated (related={related}, unrelated={unrelated})"
        );
        assert!(
            related > 0.4,
            "semantically close text should score clearly high (got {related})"
        );
    }
}
