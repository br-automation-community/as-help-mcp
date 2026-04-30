//! Optional HTTP-based embedding service for hybrid search.
//!
//! Calls any OpenAI-compatible `/embeddings` endpoint. Only used when
//! `CREATE_EMBEDDINGS=true`.

use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::EmbeddingConfig;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("input exceeds model context length")]
    TooLarge(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API returned status {status}: {body}")]
    Api { status: u16, body: String },
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Calls an OpenAI-compatible `/embeddings` endpoint.
pub struct EmbeddingService {
    client: reqwest::Client,
    url: String,
    pub model_name: String,
    dim: usize,
    pub batch_size: usize,
    pub max_chars: usize,
    pub max_workers: usize,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a [String],
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    index: usize,
    embedding: Vec<f32>,
}

impl EmbeddingService {
    /// Create from resolved config.
    pub fn new(cfg: &EmbeddingConfig) -> anyhow::Result<Self> {
        let base = cfg.api_endpoint.trim_end_matches('/');
        let url = if base.ends_with("/embeddings") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/embeddings")
        } else {
            format!("{base}/v1/embeddings")
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {}", cfg.api_key))?,
                );
                h.insert(
                    reqwest::header::CONTENT_TYPE,
                    reqwest::header::HeaderValue::from_static("application/json"),
                );
                h
            })
            .build()?;

        info!(
            "EmbeddingService configured: endpoint={} model={} dim={} batch={} workers={}",
            cfg.api_endpoint, cfg.model, cfg.dimensions, cfg.batch_size, cfg.max_workers
        );

        Ok(Self {
            client,
            url,
            model_name: cfg.model.clone(),
            dim: cfg.dimensions,
            batch_size: cfg.batch_size,
            max_chars: cfg.max_chars,
            max_workers: cfg.max_workers,
        })
    }

    /// Configured embedding dimension.
    pub fn dimension(&self) -> usize {
        self.dim
    }

    /// Embed a single text.
    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if text.trim().is_empty() {
            return Ok(vec![0.0; self.dim]);
        }
        let truncated = truncate(text, self.max_chars);
        let texts = vec![truncated];
        let result = self.call_api(&texts).await?;
        Ok(result.into_iter().next().unwrap_or_else(|| vec![0.0; self.dim]))
    }

    /// Embed a batch of texts with concurrent workers.
    pub async fn embed_batch(
        &self,
        texts: &[String],
        show_progress: bool,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let total = texts.len();
        let truncated: Vec<String> = texts
            .iter()
            .map(|t| truncate(t, self.max_chars))
            .collect();

        // Split into batches
        let batches: Vec<Vec<String>> = truncated
            .chunks(self.batch_size)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|t| if t.trim().is_empty() { " ".to_string() } else { t.clone() })
                    .collect()
            })
            .collect();

        if show_progress {
            info!(
                "Embedding {} texts (batch_size={}, workers={}, model={})...",
                total, self.batch_size, self.max_workers, self.model_name
            );
        }
        let start = std::time::Instant::now();

        let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(total);
        let workers = self.max_workers.min(batches.len());

        if workers > 1 {
            // Concurrent batches via JoinSet
            use tokio::task::JoinSet;
            let mut batch_results: Vec<Option<Vec<Vec<f32>>>> = vec![None; batches.len()];
            let mut set = JoinSet::new();

            // Limit concurrency to max_workers via a semaphore
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(workers));

            for (idx, batch) in batches.into_iter().enumerate() {
                let sem = semaphore.clone();
                let client = self.client.clone();
                let url = self.url.clone();
                let model = self.model_name.clone();
                let dim = self.dim;
                let max_chars = self.max_chars;

                set.spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    let result = call_api_static(&client, &url, &model, dim, max_chars, &batch).await;
                    (idx, result)
                });
            }

            while let Some(join_result) = set.join_next().await {
                let (idx, result) = join_result.map_err(|e| EmbeddingError::Api {
                    status: 0,
                    body: format!("Task join error: {e}"),
                })?;
                batch_results[idx] = Some(result?);
            }

            for br in batch_results {
                all_embeddings.extend(br.unwrap_or_default());
            }
        } else {
            // Sequential
            for (i, chunk) in batches.into_iter().enumerate() {
                let result = self.embed_one_batch(&chunk).await?;
                all_embeddings.extend(result);
                if show_progress {
                    let done = ((i + 1) * self.batch_size).min(total);
                    let elapsed = start.elapsed().as_secs_f64();
                    let rate = done as f64 / elapsed;
                    info!("  Progress: {done}/{total} ({:.0}%, {rate:.0} texts/s)", done as f64 * 100.0 / total as f64);
                }
            }
        }

        if show_progress {
            let elapsed = start.elapsed().as_secs_f64();
            info!(
                "Embedded {} texts in {:.1}s ({:.0} texts/s)",
                total,
                elapsed,
                total as f64 / elapsed
            );
        }

        Ok(all_embeddings)
    }

    /// Embed a single batch with binary-split fallback on context overflow.
    fn embed_one_batch<'a>(&'a self, chunk: &'a [String]) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>, EmbeddingError>> + Send + 'a>> {
        Box::pin(async move {
            match self.call_api(chunk).await {
                Ok(vecs) => Ok(vecs),
                Err(EmbeddingError::TooLarge(_)) => {
                    if chunk.len() == 1 {
                        warn!(
                            "Text too large for model ({} chars) — using zero vector",
                            chunk[0].len()
                        );
                        return Ok(vec![vec![0.0; self.dim]]);
                    }
                    let mid = chunk.len() / 2;
                    let mut left = self.embed_one_batch(&chunk[..mid]).await?;
                    let right = self.embed_one_batch(&chunk[mid..]).await?;
                    left.extend(right);
                    Ok(left)
                }
                Err(e) => Err(e),
            }
        })
    }

    /// Single API call with retry on transient errors.
    async fn call_api(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        call_api_static(&self.client, &self.url, &self.model_name, self.dim, self.max_chars, texts).await
    }
}

/// Truncate text to at most `max_chars` characters.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

/// Static helper so it can be used from spawned tasks without `&self`.
async fn call_api_static(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    dim: usize,
    _max_chars: usize,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    let payload = EmbedRequest {
        input: texts,
        model,
        dimensions: if dim > 0 { Some(dim) } else { None },
    };

    let max_retries = 3u32;
    for attempt in 0..=max_retries {
        let resp = client.post(url).json(&payload).send().await?;
        let status = resp.status();

        if status == StatusCode::OK {
            let data: EmbedResponse = resp.json().await?;
            let mut sorted = data.data;
            sorted.sort_by_key(|d| d.index);
            return Ok(sorted.into_iter().map(|d| d.embedding).collect());
        }

        if matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        ) && attempt < max_retries
        {
            let wait = 2u64.pow(attempt);
            warn!(
                "Embedding API returned {} (attempt {}/{}), retrying in {}s...",
                status.as_u16(),
                attempt + 1,
                max_retries,
                wait
            );
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }

        let body = resp.text().await.unwrap_or_default();

        if status == StatusCode::BAD_REQUEST && body.to_lowercase().contains("context length") {
            return Err(EmbeddingError::TooLarge(body));
        }

        return Err(EmbeddingError::Api {
            status: status.as_u16(),
            body,
        });
    }

    Err(EmbeddingError::Api {
        status: 0,
        body: "Exhausted retries".to_string(),
    })
}
