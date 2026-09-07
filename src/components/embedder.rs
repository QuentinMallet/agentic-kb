//! Embedder trait and implementations

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

// Send + Sync auto-derive from candle-core (default-features = false) + tokenizers.
// Locked in by the assertion below: if a future feature flip (e.g. enabling
// `candle-core/cuda`) reintroduces a thread-bound storage type, this fails to
// compile rather than silently allowing UB across threads.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CandleEmbedder>();
};

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
        let config: candle_transformers::models::bert::Config = serde_json::from_str(&config_data)?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("tokenizer load: {e}"))?;

        let device = Device::Cpu;
        // SAFETY: weights_path is the HuggingFace cache file written via tmp +
        // atomic rename in download_hf_file. It must not be mutated for the
        // lifetime of any Tensor mmapped against it; no code path in this crate
        // writes to the cache after download_hf_file returns.
        #[allow(unsafe_code)]
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)?
        };
        let model = candle_transformers::models::bert::BertModel::load(vb, &config)?;

        Ok(CandleInner { model, tokenizer })
    }
}

impl Embedder for CandleEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        use candle_core::{Device, Tensor};

        let mut guard = self
            .inner
            .lock()
            .map_err(|e| anyhow::anyhow!("lock: {e}"))?;
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

/// An embedder whose vectors were all resolved before a write transaction
/// opened (C1/D3).
///
/// `apply_event` embeds inside its savepoint — entry text plus up to eight
/// cues. Wrapping a batch in one outer transaction would otherwise hold a
/// SQLite write transaction across up to nine model calls: hundreds of
/// milliseconds, a growing WAL, and a worse busy-checkpoint problem for the
/// rebuild swap.
///
/// [`PrefetchedEmbedder::seal`] is called immediately after `BEGIN`. After
/// that a cache miss is a loud error rather than a silent model call, so "no
/// write transaction is held across an embedder call" is enforced rather than
/// documented.
pub struct PrefetchedEmbedder<'a> {
    inner: &'a dyn Embedder,
    cache: std::collections::HashMap<String, std::result::Result<Vec<f32>, String>>,
    sealed: std::sync::atomic::AtomicBool,
}

impl<'a> PrefetchedEmbedder<'a> {
    /// Resolve every text the caller expects to need. A no-op embedder skips
    /// the work entirely — `apply_event` never calls it.
    ///
    /// Fails fast: on the write path an embedder outage must abort *before*
    /// the log is appended, so no gap is created in the first place.
    pub fn prefetch(inner: &'a dyn Embedder, texts: Vec<String>) -> Result<Self> {
        let mut cache = std::collections::HashMap::new();
        if !inner.is_noop() {
            for text in texts {
                if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(text) {
                    let vector = inner.embed(entry.key())?;
                    entry.insert(Ok(vector));
                }
            }
        }
        Ok(Self {
            inner,
            cache,
            sealed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// [`Self::prefetch`], but a failure is recorded and re-raised from
    /// `embed` instead of aborting the prefetch.
    ///
    /// Recovery needs this: the log is already durable, so a record that can
    /// never be embedded has to reach `apply_event` and fail *there*, where the
    /// poison policy can count the attempt and eventually quarantine it. Failing
    /// during the prefetch would instead brick every entry point, which is the
    /// exact outcome the policy exists to prevent (D3, Principle 4).
    pub fn prefetch_deferring_errors(inner: &'a dyn Embedder, texts: Vec<String>) -> Self {
        let mut cache = std::collections::HashMap::new();
        if !inner.is_noop() {
            for text in texts {
                if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(text) {
                    let resolved = inner.embed(entry.key()).map_err(|e| e.to_string());
                    entry.insert(resolved);
                }
            }
        }
        Self {
            inner,
            cache,
            sealed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Forbid any further call into the wrapped embedder.
    pub fn seal(&self) {
        self.sealed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Embedder for PrefetchedEmbedder<'_> {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match self.cache.get(text) {
            Some(Ok(vector)) => return Ok(vector.clone()),
            Some(Err(message)) => anyhow::bail!("{message}"),
            None => {}
        }
        if self.sealed.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!(
                "embedder called while the applied-cursor transaction is open: no vector was \
                 pre-resolved for {:?}. Every text apply_event needs must be resolved before \
                 BEGIN (C1/D3).",
                text.chars().take(60).collect::<String>()
            );
        }
        self.inner.embed(text)
    }

    fn is_noop(&self) -> bool {
        self.inner.is_noop()
    }
}
