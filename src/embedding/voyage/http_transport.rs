use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::embedding::InputType;

use super::retry::{EmbedError, is_send_error_transient};
use super::{Provider, VoyageClient};

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    input_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    data: Vec<EmbedData>,
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

impl VoyageClient {
    pub(super) async fn try_embed_with_key(
        &self,
        key: &str,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.try_embed_with_key_using(&self.inner.http, key, texts, input_type)
            .await
    }

    pub(super) async fn try_embed_query_with_key(
        &self,
        key: &str,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.try_embed_with_key_using(&self.inner.query_http, key, texts, input_type)
            .await
    }

    pub(super) async fn try_embed_with_key_using(
        &self,
        client: &Client,
        key: &str,
        texts: &[String],
        input_type: InputType,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let (input_type, dimensions) = match self.inner.provider {
            Provider::Voyage => (Some(input_type.as_str()), None),
            Provider::OpenAI | Provider::Ollama => (None, self.inner.dimensions),
            Provider::Onnx => {
                return Err(EmbedError::Other(anyhow::anyhow!(
                    "ONNX provider requires local embedding client"
                )));
            }
        };
        let body = EmbedRequest {
            model: &self.inner.model,
            input: texts,
            input_type,
            dimensions,
        };
        let request = client.post(&self.inner.endpoint);
        let request = if self.inner.provider == Provider::Ollama && key.is_empty() {
            request
        } else {
            request.bearer_auth(key)
        };
        let response = request.json(&body).send().await.map_err(|error| {
            if is_send_error_transient(&error) {
                EmbedError::Transient(error.into())
            } else {
                EmbedError::Other(error.into())
            }
        })?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(EmbedError::RateLimited);
        }
        if !status.is_success() {
            return Err(EmbedError::Other(anyhow::anyhow!(
                "embedding error {}: {}",
                status,
                response.text().await.unwrap_or_default()
            )));
        }
        let response: EmbedResponse = response
            .json()
            .await
            .map_err(|error| EmbedError::Transient(error.into()))?;
        let embeddings = if self.inner.provider == Provider::Ollama {
            response.embeddings
        } else {
            response
                .data
                .into_iter()
                .map(|item| item.embedding)
                .collect()
        };
        if embeddings.len() != texts.len() {
            return Err(EmbedError::Other(anyhow::anyhow!(
                "embedding returned {} vectors for {} inputs",
                embeddings.len(),
                texts.len()
            )));
        }
        Ok(embeddings)
    }
}

#[cfg(test)]
pub(super) fn request_json(
    provider: Provider,
    input_type: InputType,
    dimensions: Option<u32>,
) -> serde_json::Value {
    let (input_type, dimensions) = match provider {
        Provider::Voyage => (Some(input_type.as_str()), None),
        Provider::OpenAI | Provider::Ollama | Provider::Onnx => (None, dimensions),
    };
    let texts = vec!["hi".to_string()];
    serde_json::to_value(EmbedRequest {
        model: "m",
        input: &texts,
        input_type,
        dimensions,
    })
    .expect("serialize body")
}
