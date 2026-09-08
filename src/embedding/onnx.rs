use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tokenizers::Tokenizer;
use tract_onnx::prelude::*;
use tract_onnx::tract_hir::infer::{Factoid, GenericFactoid};

use super::voyage::Provider;
use super::{EmbeddingClient, InputType};

#[derive(Clone, Copy)]
enum OnnxInput {
    InputIds,
    AttentionMask,
    TokenTypeIds,
}

#[derive(Clone)]
pub struct OnnxEmbeddingClient {
    model: Arc<SimplePlan<TypedFact, Box<dyn TypedOp>, TypedModel>>,
    tokenizer: Arc<Tokenizer>,
    model_name: String,
    dimensions: usize,
    inputs: Vec<OnnxInput>,
}

impl OnnxEmbeddingClient {
    pub fn new(model_path: impl AsRef<Path>, tokenizer_path: impl AsRef<Path>) -> Result<Self> {
        let model_path = model_path.as_ref();
        let tokenizer_path = tokenizer_path.as_ref();
        let model_bytes = fs::read(model_path)?;
        let tokenizer_bytes = fs::read(tokenizer_path)?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow!("failed to load tokenizer: {e}"))?;
        let model = tract_onnx::onnx()
            .model_for_path(model_path)
            .with_context(|| format!("failed to load ONNX model {}", model_path.display()))?;
        let outlets = model.input_outlets()?;
        if outlets.is_empty() {
            bail!("ONNX model has no inputs")
        }
        let mut inputs = Vec::with_capacity(outlets.len());
        let mut seen_ids = false;
        let mut seen_mask = false;
        for outlet in outlets {
            let name = model.node(outlet.node).name.as_str();
            let input = match name {
                "input_ids" => {
                    seen_ids = true;
                    OnnxInput::InputIds
                }
                "attention_mask" => {
                    seen_mask = true;
                    OnnxInput::AttentionMask
                }
                "token_type_ids" => OnnxInput::TokenTypeIds,
                _ => bail!(
                    "ONNX model has unsupported required input `{name}` (found {:?})",
                    outlets
                        .iter()
                        .map(|o| model.node(o.node).name.clone())
                        .collect::<Vec<_>>()
                ),
            };
            let fact = model.outlet_fact(*outlet)?;
            if fact.shape.is_concrete() && fact.shape.rank() != GenericFactoid::Only(2) {
                bail!("ONNX input `{name}` must have rank 2, got {:?}", fact.shape);
            }
            inputs.push(input);
        }
        if !seen_ids {
            bail!("ONNX model missing named input input_ids")
        }
        if !seen_mask {
            bail!("ONNX model missing named input attention_mask")
        }
        let plan = model.into_optimized()?.into_runnable()?;
        let output_fact = plan
            .model()
            .output_fact(0)
            .context("ONNX model has no output")?;
        if output_fact.shape.len() != 3 {
            bail!("ONNX output must have rank 3, got {:?}", output_fact.shape);
        }
        let dimensions = output_fact.shape[2]
            .to_i64()
            .map_err(|_| anyhow!("ONNX output hidden dimension must be concrete"))?;
        if dimensions <= 0 {
            bail!("ONNX output embedding dimension must be positive")
        }
        let dimensions = dimensions as usize;
        let fingerprint = fingerprint_for_bytes(&model_bytes, &tokenizer_bytes);
        let model_name = format!("{}#{fingerprint}", model_path.display());
        Ok(Self {
            model: Arc::new(plan),
            tokenizer: Arc::new(tokenizer),
            model_name,
            dimensions,
            inputs,
        })
    }

    fn run_one(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow!("tokenization failed: {e}"))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let len = ids.len();
        let ids = tract_ndarray::Array2::from_shape_vec((1, len), ids)?
            .into_tensor()
            .into_tvalue();
        let mask_tensor = tract_ndarray::Array2::from_shape_vec((1, len), mask.clone())?
            .into_tensor()
            .into_tvalue();
        let token_types = tract_ndarray::Array2::<i64>::zeros((1, len))
            .into_tensor()
            .into_tvalue();
        let inputs = self
            .inputs
            .iter()
            .map(|input| match input {
                OnnxInput::InputIds => ids.clone(),
                OnnxInput::AttentionMask => mask_tensor.clone(),
                OnnxInput::TokenTypeIds => token_types.clone(),
            })
            .collect::<TVec<_>>();
        let outputs = self.model.run(inputs)?;
        let tensor = outputs[0].to_array_view::<f32>()?;
        let shape = tensor.shape();
        if shape.len() != 3 || shape[0] != 1 || shape[1] != len || shape[2] != self.dimensions {
            bail!(
                "ONNX output shape {:?} does not match [1, {}, {}]",
                shape,
                len,
                self.dimensions
            );
        }
        let rows = (0..len)
            .map(|i| {
                (0..self.dimensions)
                    .map(|d| tensor[[0, i, d]])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        masked_mean_l2(&rows, &mask)
    }
}

fn fingerprint_for_bytes(model: &[u8], tokenizer: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(model);
    hash.update(tokenizer);
    hash.update(b"mean-pool-attention-mask-l2-normalize-v1");
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn masked_mean_l2(rows: &[Vec<f32>], mask: &[i64]) -> Result<Vec<f32>> {
    if rows.is_empty() || rows.len() != mask.len() {
        bail!("ONNX output rows do not match attention mask")
    }
    let dimensions = rows[0].len();
    let mut pooled = vec![0.0f32; dimensions];
    let mut count = 0.0f32;
    for (row, &m) in rows.iter().zip(mask) {
        if row.len() != dimensions {
            bail!("ONNX output rows have inconsistent dimensions")
        }
        if m == 0 {
            continue;
        }
        count += 1.0;
        for (out, value) in pooled.iter_mut().zip(row) {
            *out += value;
        }
    }
    if count == 0.0 {
        bail!("attention mask contains no active tokens")
    }
    let norm = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        bail!("ONNX embedding has zero norm")
    }
    for v in &mut pooled {
        *v /= norm;
    }
    Ok(pooled)
}

#[async_trait]
impl EmbeddingClient for OnnxEmbeddingClient {
    async fn embed(&self, texts: &[String], _input_type: InputType) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.run_one(t)).collect()
    }
    async fn embed_batch(&self, texts: &[String], input_type: InputType) -> Result<Vec<Vec<f32>>> {
        self.embed(texts, input_type).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.run_one(text)
    }
    fn provider(&self) -> Provider {
        Provider::Onnx
    }
    fn model(&self) -> &str {
        &self.model_name
    }
    fn dimensions(&self) -> Option<u32> {
        Some(self.dimensions as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_mean_excludes_padding_and_normalizes() {
        let out = masked_mean_l2(
            &[vec![3.0, 0.0], vec![0.0, 4.0], vec![99.0, 99.0]],
            &[1, 1, 0],
        )
        .unwrap();
        assert!((out[0] - 0.6).abs() < 1e-6);
        assert!((out[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn fingerprint_changes_when_model_or_tokenizer_changes() {
        let base = fingerprint_for_bytes(b"model", b"tokenizer");
        assert_ne!(base, fingerprint_for_bytes(b"model!", b"tokenizer"));
        assert_ne!(base, fingerprint_for_bytes(b"model", b"tokenizer!"));
    }

    #[test]
    fn loader_reports_missing_model() {
        let err = match OnnxEmbeddingClient::new(
            "/definitely/missing/model.onnx",
            "/definitely/missing/tokenizer.json",
        ) {
            Ok(_) => panic!("missing model unexpectedly loaded"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("No such file") || err.to_string().contains("os error"));
    }
}
