use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;

mod batch_transport;
mod batching;
mod http_transport;
mod query_transport;
mod retry;

#[cfg(test)]
pub use retry::TRANSIENT_RETRY_LIMIT_FOR_TEST;
pub use retry::TransientEmbedExhausted;

const VOYAGE_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";
const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/embeddings";
const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434/api/embed";
pub const MAX_BATCH_SIZE: usize = 128;
const MAX_BATCH_BYTES: usize = 1_500_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Voyage,
    OpenAI,
    Ollama,
    Onnx,
}

impl Provider {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Ok(Self::OpenAI),
            "ollama" => Ok(Self::Ollama),
            "voyage" => Ok(Self::Voyage),
            "onnx" => Ok(Self::Onnx),
            other => anyhow::bail!("unsupported embedding provider: {other}"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Voyage => "voyage",
            Self::OpenAI => "openai",
            Self::Ollama => "ollama",
            Self::Onnx => "onnx",
        }
    }

    fn default_endpoint(self) -> &'static str {
        match self {
            Self::Voyage => VOYAGE_ENDPOINT,
            Self::OpenAI => OPENAI_ENDPOINT,
            Self::Ollama => OLLAMA_ENDPOINT,
            Self::Onnx => "",
        }
    }
}

pub fn new_embedding_client(
    config: &crate::config::EmbeddingConfig,
) -> Result<Arc<dyn crate::embedding::EmbeddingClient>> {
    config.validate_for_provider()?;
    let provider = Provider::parse(&config.provider)?;
    match provider {
        Provider::Onnx => {
            let model = config
                .onnx_model_path
                .as_ref()
                .context("onnx_model_path is required for ONNX")?;
            let tokenizer = config
                .onnx_tokenizer_path
                .as_ref()
                .context("onnx_tokenizer_path is required for ONNX")?;
            Ok(Arc::new(crate::embedding::onnx::OnnxEmbeddingClient::new(
                model, tokenizer,
            )?))
        }
        Provider::Voyage | Provider::OpenAI => Ok(Arc::new(VoyageClient::new_for_provider(
            provider,
            config.model.clone(),
            config.api_keys.clone(),
            config.voyage_base_url.as_deref(),
            config.dimensions,
        )?)),
        Provider::Ollama => Ok(Arc::new(VoyageClient::new_for_provider(
            provider,
            config.model.clone(),
            config.api_keys.clone(),
            config.ollama_base_url.as_deref(),
            config.dimensions,
        )?)),
    }
}
pub fn embedding_url(provider: Provider, base: Option<&str>) -> String {
    let raw = base.unwrap_or_default().trim();
    if raw.is_empty() {
        return provider.default_endpoint().to_owned();
    }
    let trimmed = raw.trim_end_matches('/');
    if provider == Provider::Ollama {
        if trimmed.ends_with("/api/embed") {
            trimmed.to_owned()
        } else {
            format!("{trimmed}/api/embed")
        }
    } else if trimmed.ends_with("/embeddings") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/embeddings")
    }
}

/// Backward-compatible URL resolver using the Voyage default endpoint.
pub fn voyage_url(base: Option<&str>) -> String {
    embedding_url(Provider::Voyage, base)
}

#[derive(Clone)]
pub struct VoyageClient {
    pub(super) inner: Arc<VoyageInner>,
}

pub(super) struct VoyageInner {
    pub(super) http: Client,
    pub(super) query_http: Client,
    pub(super) provider: Provider,
    pub(super) model: String,
    pub(super) api_keys: Vec<String>,
    pub(super) endpoint: String,
    pub(super) dimensions: Option<u32>,
    pub(super) key_cursor: AtomicUsize,
}

impl VoyageClient {
    pub fn new(model: String, api_keys: Vec<String>, base_url: Option<&str>) -> Result<Self> {
        Self::new_for_provider(Provider::Voyage, model, api_keys, base_url, None)
    }

    pub fn new_for_provider(
        provider: Provider,
        model: String,
        api_keys: Vec<String>,
        base_url: Option<&str>,
        dimensions: Option<u32>,
    ) -> Result<Self> {
        if api_keys.is_empty() && provider != Provider::Ollama {
            bail!("embedding client requires at least one API key");
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("build embedding reqwest client")?;
        let query_http = http.clone();
        Ok(Self {
            inner: Arc::new(VoyageInner {
                http,
                query_http,
                provider,
                model,
                api_keys,
                endpoint: embedding_url(provider, base_url),
                dimensions,
                key_cursor: AtomicUsize::new(0),
            }),
        })
    }

    pub fn provider(&self) -> Provider {
        self.inner.provider
    }

    pub fn model(&self) -> &str {
        &self.inner.model
    }

    pub fn dimensions(&self) -> Option<u32> {
        self.inner.dimensions
    }
}

#[async_trait::async_trait]
impl crate::embedding::EmbeddingClient for VoyageClient {
    async fn embed(
        &self,
        texts: &[String],
        input_type: crate::embedding::InputType,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        VoyageClient::embed(self, texts, input_type).await
    }

    async fn embed_batch(
        &self,
        texts: &[String],
        input_type: crate::embedding::InputType,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        VoyageClient::embed_batch(self, texts, input_type).await
    }

    async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        VoyageClient::embed_query(self, text).await
    }

    fn provider(&self) -> Provider {
        self.provider()
    }
    fn model(&self) -> &str {
        self.model()
    }
    fn dimensions(&self) -> Option<u32> {
        self.dimensions()
    }
}

#[cfg(test)]
mod tests;
