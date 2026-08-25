//! Neural sentence-embedding model — `all-MiniLM-L6-v2` (BERT) run with
//! [Candle], Hugging Face's pure-Rust ML framework.
//!
//! Unlike ONNX-based embedders this has **no C/C++ dependency**: inference is
//! pure Rust, so it builds identically on every platform. The ~90 MB model
//! weights are fetched once from Hugging Face and cached on disk; the loaded
//! model is shared process-wide (one instance for all agents).
//!
//! [Candle]: https://github.com/huggingface/candle

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use axocoatl_core::secure_fs::SecureDir;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::error::MemoryError;

/// Output dimensionality of `all-MiniLM-L6-v2`.
pub const NEURAL_DIM: usize = 384;

/// Identifier stored alongside vectors — a change here triggers a re-embed.
pub const NEURAL_ID: &str = "minilm-l6-v2";

/// Immutable Hugging Face revision whose exact files are accepted below.
/// Never use the mutable `main` ref for executable model input.
const HF_REVISION: &str = "c315f904dfc467d8b9c40ab4ed50b3a8d0866c15";
const HF_BASE: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve";

#[derive(Clone, Copy)]
struct ModelFileSpec {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

const CONFIG_FILE: ModelFileSpec = ModelFileSpec {
    name: "config.json",
    size: 612,
    sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
};
const TOKENIZER_FILE: ModelFileSpec = ModelFileSpec {
    name: "tokenizer.json",
    size: 466_247,
    sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
};
const WEIGHTS_FILE: ModelFileSpec = ModelFileSpec {
    name: "model.safetensors",
    size: 90_868_376,
    sha256: "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
};

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
        let root = model_cache_root()?;
        Self::shared_in(&root)
    }

    /// Initialise the shared model cache beneath the daemon's already-opened
    /// control-plane root instead of reopening `AXOCOATL_DATA_DIR` by name.
    pub fn shared_in(data_root: &SecureDir) -> Result<Arc<NeuralEmbedder>, MemoryError> {
        let data_root = data_root.clone();
        SHARED
            .get_or_init(move || {
                std::thread::spawn(move || Self::load_in(&data_root))
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

    fn load_in(data_root: &SecureDir) -> Result<Self, MemoryError> {
        let dir = data_root.child("models/all-MiniLM-L6-v2")?;
        let config_bytes = ensure_file(&dir, CONFIG_FILE)?;
        let tokenizer_bytes = ensure_file(&dir, TOKENIZER_FILE)?;
        let weights_bytes = ensure_file(&dir, WEIGHTS_FILE)?;

        let device = Device::Cpu;
        let config: Config = serde_json::from_slice(&config_bytes)
            .map_err(|e| MemoryError::Embedding(format!("model config: {e}")))?;
        let tokenizer = Tokenizer::from_bytes(&tokenizer_bytes)
            .map_err(|e| MemoryError::Embedding(format!("tokenizer: {e}")))?;

        // Buffered loading keeps every byte bound to the retained SecureDir
        // capability. Reopening an ambient cache path for mmap would let a
        // post-bootstrap path replacement substitute different model input.
        let vb = VarBuilder::from_buffered_safetensors(weights_bytes, DTYPE, &device)
            .map_err(|e| MemoryError::Embedding(format!("weights: {e}")))?;
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

/// `{AXOCOATL_DATA_DIR or ./data}` compatibility root.
fn model_cache_root() -> Result<SecureDir, MemoryError> {
    let data_dir = std::env::var("AXOCOATL_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    Ok(SecureDir::open_or_create_all(PathBuf::from(data_dir))?)
}

/// Return exact verified bytes, downloading the immutable artifact if absent
/// or if an existing cache entry fails its size/hash contract.
fn ensure_file(dir: &SecureDir, spec: ModelFileSpec) -> Result<Vec<u8>, MemoryError> {
    if dir.is_file(spec.name)? {
        match read_verified_file(dir, spec.name, spec.size, spec.sha256) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                tracing::warn!(
                    file = spec.name,
                    error = %error,
                    "cached embedding-model file failed verification; fetching the pinned artifact"
                );
            }
        }
    }
    let url = format!("{HF_BASE}/{HF_REVISION}/{}", spec.name);
    tracing::info!(%url, "downloading embedding-model file (one-time)");
    let mut response = reqwest::blocking::get(&url)
        .and_then(|response| response.error_for_status())
        .map_err(|e| MemoryError::Embedding(format!("downloading {}: {e}", spec.name)))?;
    if response
        .content_length()
        .is_some_and(|length| length != spec.size)
    {
        return Err(MemoryError::Embedding(format!(
            "downloading {}: expected {} bytes, response advertised {:?}",
            spec.name,
            spec.size,
            response.content_length()
        )));
    }
    let mut bytes = Vec::with_capacity(spec.size as usize);
    response
        .by_ref()
        .take(spec.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| MemoryError::Embedding(format!("downloading {}: {e}", spec.name)))?;
    verify_file_bytes(spec.name, &bytes, spec.size, spec.sha256)?;
    // The anchored writer uses an unpredictable create-new temp and refuses a
    // symlink at the final target. Old predictable `.part` names are ignored.
    dir.atomic_write(spec.name, &bytes)?;
    // Re-read through the retained handle so the loaded value is exactly the
    // durable cache entry that future process starts will verify.
    read_verified_file(dir, spec.name, spec.size, spec.sha256)
}

fn read_verified_file(
    dir: &SecureDir,
    name: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<Vec<u8>, MemoryError> {
    let size = dir.file_len(name)?;
    if size != expected_size {
        return Err(MemoryError::Embedding(format!(
            "model file {name} is {size} bytes; expected {expected_size}"
        )));
    }
    let bytes = dir.read(name)?;
    verify_file_bytes(name, &bytes, expected_size, expected_sha256)?;
    Ok(bytes)
}

fn verify_file_bytes(
    name: &str,
    bytes: &[u8],
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), MemoryError> {
    let actual_sha256 = format!("{:x}", Sha256::digest(bytes));
    if bytes.len() as u64 != expected_size || actual_sha256 != expected_sha256 {
        return Err(MemoryError::Embedding(format!(
            "model file {name} failed pinned size/SHA-256 verification"
        )));
    }
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn predictable_part_symlink_is_not_followed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let cache = SecureDir::open(root.path()).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"safe").unwrap();
        symlink(outside.path(), root.path().join("config.part")).unwrap();
        cache.atomic_write("config.json", b"owned").unwrap();
        assert_eq!(cache.read("config.json").unwrap(), b"owned");
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn verified_model_read_stays_bound_to_opened_root_after_path_swap() {
        let parent = tempfile::tempdir().unwrap();
        let configured = parent.path().join("cache");
        let opened = parent.path().join("opened-cache");
        std::fs::create_dir(&configured).unwrap();
        std::fs::write(configured.join("fixture"), b"trusted model bytes").unwrap();
        let cache = SecureDir::open(&configured).unwrap();
        let expected = format!("{:x}", Sha256::digest(b"trusted model bytes"));

        std::fs::rename(&configured, &opened).unwrap();
        std::fs::create_dir(&configured).unwrap();
        std::fs::write(configured.join("fixture"), b"replacement bytes!").unwrap();

        assert_eq!(
            read_verified_file(&cache, "fixture", 19, &expected).unwrap(),
            b"trusted model bytes"
        );
    }

    #[test]
    fn verified_model_read_rejects_modified_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("fixture"), b"modified").unwrap();
        let cache = SecureDir::open(root.path()).unwrap();
        let expected = format!("{:x}", Sha256::digest(b"trusted"));

        assert!(matches!(
            read_verified_file(&cache, "fixture", 8, &expected),
            Err(MemoryError::Embedding(_))
        ));
    }

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
