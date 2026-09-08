pub mod cache;
pub mod identity;
pub mod onnx;
pub mod voyage;

// Re-export the transient-exhausted marker so the pipeline can classify errors.
pub use voyage::TransientEmbedExhausted;

/// Input type hint for the embedding API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputType {
    Document,
    Query,
}

impl InputType {
    pub fn as_str(&self) -> &'static str {
        match self {
            InputType::Document => "document",
            InputType::Query => "query",
        }
    }
}

/// Provider-neutral asynchronous embedding client boundary.
#[async_trait::async_trait]
pub trait EmbeddingClient: Send + Sync {
    async fn embed(&self, texts: &[String], input_type: InputType)
    -> anyhow::Result<Vec<Vec<f32>>>;
    async fn embed_batch(
        &self,
        texts: &[String],
        input_type: InputType,
    ) -> anyhow::Result<Vec<Vec<f32>>>;
    async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn provider(&self) -> voyage::Provider;
    fn model(&self) -> &str;
    fn dimensions(&self) -> Option<u32>;
}
