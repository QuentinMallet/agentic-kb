//! Embedder trait and implementations
#![allow(unsafe_code)] // candle VarBuilder::from_mmaped_safetensors requires unsafe

use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Trait for text embedding.
pub trait Embedder: Send + Sync {
    /// Generate an embedding vector for the given text.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Whether this is a no-op embedder (skips embedding storage).
    fn is_noop(&self) -> bool {
        false
    }
}

/// No-op embedder that returns empty vectors (for KB_NO_EMBED=1 or tests).
pub struct NoopEmbedder;

impl Embedder for NoopEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Ok(vec![])
    }

    fn is_noop(&self) -> bool {
        true
    }
}

/// Candle-based embedder using BAAI/bge-small-en-v1.5.
/// Lazily loads the model on first embed() call.
pub struct CandleEmbedder {
    cache_dir: PathBuf,
    inner: Mutex<Option<CandleInner>>,
}

struct CandleInner {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
}

// SAFETY: CandleInner (BertModel + Tokenizer) may contain non-Send raw pointers
// internally, but all access is serialized through the Mutex<Option<CandleInner>>
// in embed(). The MCP server and CLI are single-threaded; the Mutex ensures no
// concurrent access even if callers change in the future.
unsafe impl Send for CandleEmbedder {}
unsafe impl Sync for CandleEmbedder {}

impl CandleEmbedder {
    /// Create a new CandleEmbedder. Does NOT load the model yet.
    pub fn new(cache_dir: &Path) -> Self {
        Self {
            cache_dir: cache_dir.to_path_buf(),
            inner: Mutex::new(None),
        }
    }

    /// Returns true if the model has been loaded.
    pub fn is_loaded(&self) -> bool {
        self.inner.lock().unwrap().is_some()
    }

    /// Download a single file from HuggingFace Hub, caching locally.
    /// Uses ureq with auto-redirect following (handles relative Location headers).
    fn download_hf_file(model_id: &str, filename: &str, cache_dir: &Path) -> Result<PathBuf> {
        let safe_id = model_id.replace('/', "--");
        let dir = cache_dir.join(&safe_id);
        std::fs::create_dir_all(&dir)?;
        let out_path = dir.join(filename);
        if out_path.exists() {
            return Ok(out_path);
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}",
            model_id, filename
        );
        eprintln!("kb: downloading {} from HuggingFace...", filename);
        let agent = ureq::AgentBuilder::new().build();
        let resp = agent
            .get(&url)
            .call()
            .map_err(|e| anyhow::anyhow!("download {filename}: {e}"))?;
        let mut reader = resp.into_reader();
        let tmp = out_path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::copy(&mut reader, &mut f)?;
            f.flush()?;
        }
        std::fs::rename(&tmp, &out_path)?;
        Ok(out_path)
    }

    fn load_model(cache_dir: &Path) -> Result<CandleInner> {
        use candle_core::Device;
        use candle_nn::VarBuilder;

        let model_id = "BAAI/bge-small-en-v1.5";
        let config_path = Self::download_hf_file(model_id, "config.json", cache_dir)?;
        let tokenizer_path = Self::download_hf_file(model_id, "tokenizer.json", cache_dir)?;
        let weights_path = Self::download_hf_file(model_id, "model.safetensors", cache_dir)?;

        let config_data = std::fs::read_to_string(&config_path)?;
        let config: candle_transformers::models::bert::Config =
            serde_json::from_str(&config_data)?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("tokenizer load: {e}"))?;

        let device = Device::Cpu;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[weights_path],
                candle_core::DType::F32,
                &device,
            )?
        };
        let model = candle_transformers::models::bert::BertModel::load(vb, &config)?;

        Ok(CandleInner { model, tokenizer })
    }
}

impl Embedder for CandleEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        use candle_core::{Device, Tensor};

        let mut guard = self.inner.lock().map_err(|e| anyhow::anyhow!("lock: {e}"))?;
        if guard.is_none() {
            *guard = Some(Self::load_model(&self.cache_dir)?);
        }
        let inner = guard.as_ref().unwrap();

        let encoding = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;

        let ids = encoding.get_ids().to_vec();
        let type_ids = encoding.get_type_ids().to_vec();

        // Truncate to BAAI/bge-small-en-v1.5 max sequence length (512 tokens).
        // Without truncation the model panics on index-out-of-bounds for long inputs.
        const MAX_SEQ_LEN: usize = 512;
        let ids: Vec<u32> = ids.into_iter().take(MAX_SEQ_LEN).collect();
        let type_ids: Vec<u32> = type_ids.into_iter().take(MAX_SEQ_LEN).collect();
        let len = ids.len();

        let device = Device::Cpu;
        let input_ids = Tensor::new(vec![ids], &device)?;
        let type_ids = Tensor::new(vec![type_ids], &device)?;

        let output = inner.model.forward(&input_ids, &type_ids, None)?;

        // Mean pooling over sequence length dimension
        let sum = output.sum(1)?;
        let count = Tensor::new(vec![len as f32], &device)?.reshape((1, 1))?;
        let mean = sum.broadcast_div(&count)?;

        // L2 normalize
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normalized = mean.broadcast_div(&norm)?;

        let result: Vec<f32> = normalized.squeeze(0)?.to_vec1()?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_embedder_returns_empty() {
        let embedder = NoopEmbedder;
        let result = embedder.embed("anything").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_noop_embedder_is_noop() {
        let embedder = NoopEmbedder;
        assert!(embedder.is_noop());
    }

    #[test]
    fn test_candle_embedder_is_not_noop() {
        let embedder = CandleEmbedder::new(Path::new("/tmp/test-cache"));
        assert!(!embedder.is_noop());
    }

    #[test]
    fn test_candle_embedder_new_does_not_load_model() {
        // Construction should succeed without downloading anything
        let embedder = CandleEmbedder::new(Path::new("/tmp/nonexistent-cache"));
        assert!(!embedder.is_loaded());
    }
}
