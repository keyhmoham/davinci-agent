//! A hosted embedding model behind an OpenAI-compatible `/embeddings` endpoint (Vercel AI Gateway serves `voyage/voyage-4-lite`,
//! `openai/text-embedding-3-small`, `alibaba/qwen3-embedding-*`, ...).
//!
//! It is the memory's **semantic** vector, next to the hash vector every item always has: paraphrases find each other (measured on
//! paraphrase pairs: recall@1 0.65 with the hash embedder, 1.00 with `voyage-4-lite`), while the hash vector keeps working when the
//! model cannot be reached, and stays the measure of how alike two requests are in *shape* (a skill replays on that).

use std::time::Duration;

#[derive(Clone)]
pub struct RemoteEmbedder {
    http: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
    dim: usize,
}

impl RemoteEmbedder {
    /// `base` is the API root (`https://ai-gateway.vercel.sh/v1`). `dim` is the model's output size (it names the storage column, so
    /// a model of another size never mixes with it); it is checked against every reply.
    pub fn new(base: &str, model: &str, key: Option<String>, dim: usize, timeout: Duration) -> Option<Self> {
        let http = reqwest::Client::builder().timeout(timeout).build().ok()?;
        Some(Self { http, url: format!("{}/embeddings", base.trim_end_matches('/')), model: model.into(), key, dim })
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One vector per text, in order, L2-normalised.
    pub async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let input: Vec<String> = texts.iter().map(|t| t.chars().take(8000).collect()).collect();
        let mut rb = self.http.post(&self.url).json(&serde_json::json!({"model": self.model, "input": input}));
        if let Some(k) = &self.key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HTTP {status}: {}", resp.text().await.unwrap_or_default().chars().take(200).collect::<String>()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let data = v["data"].as_array().ok_or("no data in the reply")?;
        let mut out = vec![Vec::new(); texts.len()];
        for (pos, d) in data.iter().enumerate() {
            let i = d["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
            let e: Vec<f32> = d["embedding"].as_array().ok_or("no embedding")?.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect();
            if e.len() != self.dim {
                return Err(format!("{} returned {} dimensions, {} were configured", self.model, e.len(), self.dim));
            }
            if i < out.len() {
                out[i] = normalise(e);
            }
        }
        if out.iter().any(|e| e.is_empty()) {
            return Err("an input got no embedding".into());
        }
        Ok(out)
    }

    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, String> {
        self.embed(&[text]).await.map(|mut v| v.remove(0))
    }
}

fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}
