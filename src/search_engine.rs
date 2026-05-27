//! LanceDB search engine with FTS (default) and optional hybrid RRF search.
//!
//! By default, uses Lance native full-text search (FTS) for keyword search
//! only. When an embedding API is configured, adds vector columns and enables
//! hybrid search with Reciprocal Rank Fusion (RRF).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use arrow_array::{Array, Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::index::Index;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::query::{ExecutableQuery, QueryBase};
use regex::Regex;
use tokio::sync::{RwLock, watch};
use tracing::{error, info, warn};

use crate::embeddings::EmbeddingService;
use crate::indexer::HelpContentIndexer;
use crate::models::{BuildState, BuildStatus, BuildStrategy, IncrementalStats};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// RRF constant (from the original RRF paper).
const RRF_K: f64 = 60.0;

// Natural language query weights
const WEIGHT_TITLE_VECTOR: f64 = 2.0;
const WEIGHT_CONTENT_VECTOR: f64 = 1.0;
const WEIGHT_FTS_KEYWORD: f64 = 1.5;
const WEIGHT_TITLE_MATCH: f64 = 3.0;
const WEIGHT_BREADCRUMB_MATCH: f64 = 2.0;

// Identifier query weights
const WEIGHT_TITLE_VECTOR_ID: f64 = 0.5;
const WEIGHT_CONTENT_VECTOR_ID: f64 = 0.5;
const WEIGHT_FTS_KEYWORD_ID: f64 = 3.0;
const WEIGHT_TITLE_MATCH_ID: f64 = 4.0;
const WEIGHT_BREADCRUMB_MATCH_ID: f64 = 3.0;

/// Pages per chunk during index build.
const BUILD_CHUNK_SIZE: usize = 5000;

static IDENTIFIER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*$").unwrap());

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Find the largest char boundary <= `max_bytes` in `s`.
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    let mut pos = max_bytes.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    &s[..pos]
}

/// Detect if a query looks like a technical identifier (PascalCase, snake_case, etc.).
fn is_identifier_query(query: &str) -> bool {
    let words: Vec<&str> = query.split_whitespace().collect();
    !words.is_empty() && words.len() <= 2 && words.iter().all(|w| IDENTIFIER_RE.is_match(w))
}

/// Sanitize a query string for FTS — strip special chars and optionally reserved keywords.
fn sanitize_query(query: &str) -> String {
    sanitize_query_with_mode(query, is_identifier_query(query))
}

fn sanitize_query_with_mode(query: &str, identifier_mode: bool) -> String {
    let mut s = query.to_string();
    for ch in "\"'*:(){}^+[]-/%_".chars() {
        s = s.replace(ch, " ");
    }
    if identifier_mode {
        // For identifiers, only strip FTS special chars, keep all terms
        s.split_whitespace()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        let reserved: std::collections::HashSet<&str> =
            ["and", "or", "not", "near"].into_iter().collect();
        s.split_whitespace()
            .filter(|t| t.len() >= 2 && !reserved.contains(&t.to_lowercase().as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Generate a text snippet (~160 chars) around the first matching query term.
fn generate_snippet(content: &str, query: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut sanitized = query.to_string();
    for ch in "\"'*:(){}^+[]-/%_".chars() {
        sanitized = sanitized.replace(ch, " ");
    }
    let terms: Vec<&str> = sanitized
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .collect();

    if !terms.is_empty() {
        let lower = content.to_lowercase();
        let mut best_pos = content.len();
        for term in &terms {
            if let Some(pos) = lower.find(&term.to_lowercase())
                && pos < best_pos
            {
                best_pos = pos;
            }
        }
        if best_pos < content.len() {
            let raw_start = best_pos.saturating_sub(40);
            let start = safe_truncate(content, raw_start).len();
            let end = safe_truncate(content, best_pos + 120).len();
            let prefix = if start > 0 { "..." } else { "" };
            let suffix = if end < content.len() { "..." } else { "" };
            return Some(format!("{prefix}{}{suffix}", &content[start..end]));
        }
    }

    let snippet = safe_truncate(content, 160);
    let suffix = if content.len() > snippet.len() {
        "..."
    } else {
        ""
    };
    Some(format!("{snippet}{suffix}"))
}

/// Build a safe SQL WHERE clause for category filtering.
fn build_category_filter(category: Option<&str>) -> Option<String> {
    let cat = category?.trim();
    if cat.is_empty() {
        return None;
    }
    // Strip non-word chars for safety
    let safe: String = cat
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '.' || *c == '-' || *c == '_')
        .collect();
    Some(format!("lower(category) = '{}'", safe.to_lowercase()))
}

// ---------------------------------------------------------------------------
// Build status (shared via Arc<RwLock>)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct InternalBuildStatus {
    state: BuildState,
    build_type: String,
    phase: String,
    pages_total: usize,
    pages_processed: usize,
    started_at: Option<Instant>,
    completed_at: Option<Instant>,
    error: Option<String>,
    incremental_stats: Option<IncrementalStats>,
    embeddings_enabled: bool,
}

impl InternalBuildStatus {
    fn to_public(&self) -> BuildStatus {
        let elapsed = match (self.started_at, self.completed_at) {
            (Some(s), Some(c)) => Some(c.duration_since(s).as_secs_f64()),
            (Some(s), None) => Some(s.elapsed().as_secs_f64()),
            _ => None,
        };
        BuildStatus {
            state: self.state.to_string(),
            build_type: self.build_type.clone(),
            phase: self.phase.clone(),
            pages_total: self.pages_total,
            pages_processed: self.pages_processed,
            started_at: None, // We use elapsed_seconds instead
            completed_at: None,
            elapsed_seconds: elapsed,
            error: self.error.clone(),
            incremental_stats: self.incremental_stats.clone(),
            embeddings_enabled: self.embeddings_enabled,
        }
    }
}

// ---------------------------------------------------------------------------
// Search engine
// ---------------------------------------------------------------------------

pub struct HelpSearchEngine {
    db_path: PathBuf,
    db: lancedb::Connection,
    indexer: Arc<HelpContentIndexer>,
    embedder: Option<Arc<EmbeddingService>>,
    pub _embeddings_enabled: bool,

    status: Arc<RwLock<InternalBuildStatus>>,
    ready_tx: watch::Sender<bool>,
    ready_rx: watch::Receiver<bool>,
    fts_ready_tx: watch::Sender<bool>,
    #[allow(dead_code)]
    fts_ready_rx: watch::Receiver<bool>,

    metadata_path: PathBuf,
    build_strategy: BuildStrategy,
}

const TABLE_NAME: &str = "help_pages";
const STAGING_TABLE: &str = "help_pages_staging";

impl HelpSearchEngine {
    /// Create a new search engine. The constructor is fast — it detects the
    /// build strategy but does not start building. Call [`initialize`] to
    /// build or load the index (typically in a background task).
    pub async fn new(
        db_path: &Path,
        indexer: Arc<HelpContentIndexer>,
        force_rebuild: bool,
        embedder: Option<Arc<EmbeddingService>>,
    ) -> anyhow::Result<Self> {
        let db_path = db_path.to_path_buf();
        std::fs::create_dir_all(&db_path)?;

        let db = lancedb::connect(db_path.to_str().unwrap_or("."))
            .execute()
            .await?;

        let metadata_path = db_path.join("_index_metadata.json");
        let embeddings_enabled = embedder.is_some();

        let (ready_tx, ready_rx) = watch::channel(false);
        let (fts_ready_tx, fts_ready_rx) = watch::channel(false);

        let status = Arc::new(RwLock::new(InternalBuildStatus {
            state: BuildState::Initializing,
            build_type: String::new(),
            phase: String::new(),
            pages_total: 0,
            pages_processed: 0,
            started_at: None,
            completed_at: None,
            error: None,
            incremental_stats: None,
            embeddings_enabled,
        }));

        // Detect build strategy
        let strategy = if force_rebuild {
            BuildStrategy::Full
        } else {
            detect_build_strategy(
                &db,
                &metadata_path,
                embeddings_enabled,
                embedder.as_deref(),
                &indexer,
            )
            .await
        };

        {
            let mut s = status.write().await;
            s.build_type = format!("{:?}", strategy).to_lowercase();
        }

        Ok(Self {
            db_path,
            db,
            indexer,
            embedder,
            _embeddings_enabled: embeddings_enabled,
            status,
            ready_tx,
            ready_rx,
            fts_ready_tx,
            fts_ready_rx,
            metadata_path,
            build_strategy: strategy,
        })
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    /// Build or load the search index.
    pub async fn initialize(&self) -> anyhow::Result<()> {
        {
            let mut s = self.status.write().await;
            s.started_at = Some(Instant::now());
            s.state = BuildState::Building;
            s.pages_total = self.indexer.pages.len();
        }

        let result = match self.build_strategy {
            BuildStrategy::Full => {
                info!("Building new search index (full)...");
                if self._embeddings_enabled {
                    self.build_index_two_phase().await
                } else {
                    self.build_fts_index().await
                }
            }
            BuildStrategy::Incremental => {
                {
                    let mut s = self.status.write().await;
                    s.state = BuildState::FtsReady;
                }
                let _ = self.fts_ready_tx.send(true);
                info!("Performing incremental index update (keyword search available)...");
                self.incremental_update().await
            }
            BuildStrategy::None => {
                info!("Loading existing search index...");
                self.load_index().await?;
                let _ = self.fts_ready_tx.send(true);
                Ok(())
            }
        };

        match result {
            Ok(()) => {
                let mut s = self.status.write().await;
                s.state = BuildState::Ready;
                s.phase = "complete".to_string();
                s.completed_at = Some(Instant::now());
                let _ = self.ready_tx.send(true);
                let _ = self.fts_ready_tx.send(true);
            }
            Err(ref e) => {
                let mut s = self.status.write().await;
                if s.state == BuildState::FtsReady {
                    // Phase 2 failed but FTS is working — degrade gracefully
                    warn!(
                        "Embedding phase failed, falling back to keyword-only search: {}",
                        e
                    );
                    s.phase = "fts_only (embedding failed)".to_string();
                    s.error = Some(format!(
                        "Embedding phase failed: {}. Keyword search is available.",
                        e
                    ));
                    let _ = self.fts_ready_tx.send(true);
                } else {
                    s.state = BuildState::Error;
                    s.error = Some(e.to_string());
                    error!("Search index initialization failed: {}", e);
                }
            }
        }

        result
    }

    /// Whether the index is fully ready (hybrid or keyword).
    pub fn ready(&self) -> bool {
        *self.ready_rx.borrow()
    }

    /// Whether FTS keyword search is available.
    #[allow(dead_code)]
    pub fn fts_ready(&self) -> bool {
        *self.fts_ready_rx.borrow()
    }

    /// Current build status snapshot.
    pub async fn build_status(&self) -> BuildStatus {
        self.status.read().await.to_public()
    }

    /// Wait until the index is ready.
    #[allow(dead_code)]
    pub async fn wait_until_ready(&self) -> bool {
        let mut rx = self.ready_rx.clone();
        loop {
            if *rx.borrow() {
                return true;
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }

    // ------------------------------------------------------------------
    // Search
    // ------------------------------------------------------------------

    /// Search for help pages using RRF fusion (hybrid or keyword-only).
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        search_in_content: bool,
        category: Option<&str>,
    ) -> anyhow::Result<Vec<HashMap<String, serde_json::Value>>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let table = self.db.open_table(TABLE_NAME).execute().await?;
        let where_clause = build_category_filter(category);

        let use_vectors = self._embeddings_enabled && self.ready();

        // Choose weights based on query type
        let is_id = is_identifier_query(query);
        let (w_title_vec, w_content_vec, w_fts, w_title_match, w_breadcrumb) = if is_id {
            (
                WEIGHT_TITLE_VECTOR_ID,
                WEIGHT_CONTENT_VECTOR_ID,
                WEIGHT_FTS_KEYWORD_ID,
                WEIGHT_TITLE_MATCH_ID,
                WEIGHT_BREADCRUMB_MATCH_ID,
            )
        } else {
            (
                WEIGHT_TITLE_VECTOR,
                WEIGHT_CONTENT_VECTOR,
                WEIGHT_FTS_KEYWORD,
                WEIGHT_TITLE_MATCH,
                WEIGHT_BREADCRUMB_MATCH,
            )
        };

        let fetch_limit = (limit * 3).min(100);
        let mut rrf_scores: HashMap<String, f64> = HashMap::new();
        let mut page_data: HashMap<String, HashMap<String, serde_json::Value>> = HashMap::new();

        // Leg 1 + 2: Vector search (if hybrid mode is ready)
        if use_vectors
            && let Some(ref embedder) = self.embedder
            && let Ok(query_vector) = embedder.embed_text(query).await
        {
            // Title vector search
            if let Ok(title_results) = vector_search(
                &table,
                &query_vector,
                "title_vector",
                fetch_limit,
                where_clause.as_deref(),
            )
            .await
            {
                for (rank, row) in title_results.iter().enumerate() {
                    let pid = row
                        .get("page_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    *rrf_scores.entry(pid.clone()).or_default() +=
                        w_title_vec / (RRF_K + rank as f64 + 1.0);
                    page_data.entry(pid).or_insert_with(|| row.clone());
                }
            }

            // Content vector search
            if search_in_content
                && let Ok(content_results) = vector_search(
                    &table,
                    &query_vector,
                    "content_vector",
                    fetch_limit,
                    where_clause.as_deref(),
                )
                .await
            {
                for (rank, row) in content_results.iter().enumerate() {
                    let pid = row
                        .get("page_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    *rrf_scores.entry(pid.clone()).or_default() +=
                        w_content_vec / (RRF_K + rank as f64 + 1.0);
                    page_data.entry(pid).or_insert_with(|| row.clone());
                }
            }
        }

        // Leg 3: FTS keyword search
        let sanitized = sanitize_query(query);
        if !sanitized.is_empty()
            && let Ok(fts_results) =
                fts_search(&table, &sanitized, fetch_limit, where_clause.as_deref()).await
        {
            for (rank, row) in fts_results.iter().enumerate() {
                let pid = row
                    .get("page_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                *rrf_scores.entry(pid.clone()).or_default() += w_fts / (RRF_K + rank as f64 + 1.0);
                page_data.entry(pid).or_insert_with(|| row.clone());
            }
        }

        // Leg 4: Title match bonus (per-term matching for NL, full-string for identifiers)
        let query_lower = query.trim().to_lowercase();
        let query_terms: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|t| t.len() >= 2)
            .collect();
        let num_query_terms = query_terms.len().max(1);

        let mut title_matches: Vec<(String, f64)> = page_data
            .iter()
            .filter_map(|(pid, data)| {
                let title = data.get("title")?.as_str()?;
                let title_lower = title.to_lowercase();

                if is_id {
                    // For identifiers, keep full-string matching
                    if title_lower.contains(&query_lower) {
                        let coverage = if title_lower == query_lower { 1.0 } else { 0.8 };
                        Some((pid.clone(), coverage))
                    } else {
                        None
                    }
                } else {
                    // For NL queries, per-term matching with coverage score
                    let matched = query_terms
                        .iter()
                        .filter(|t| title_lower.contains(**t))
                        .count();
                    if matched > 0 {
                        let mut coverage = matched as f64 / num_query_terms as f64;
                        // Bonus for exact full-query match
                        if title_lower.contains(&query_lower) {
                            coverage = (coverage + 1.0) / 2.0 + 0.5;
                        }
                        // Slight preference for shorter (tighter) titles
                        let len_penalty = 1.0 - (title.len() as f64 / 500.0).min(0.2);
                        Some((pid.clone(), coverage * len_penalty))
                    } else {
                        None
                    }
                }
            })
            .collect();
        title_matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (pid, _coverage)) in title_matches.iter().enumerate() {
            *rrf_scores.entry(pid.clone()).or_default() +=
                w_title_match / (RRF_K + rank as f64 + 1.0);
        }

        // Leg 5: Breadcrumb retrieval
        if let Ok(bc_results) =
            breadcrumb_retrieval(&table, query, fetch_limit, where_clause.as_deref()).await
        {
            for (rank, row) in bc_results.iter().enumerate() {
                let pid = row
                    .get("page_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                *rrf_scores.entry(pid.clone()).or_default() +=
                    w_breadcrumb / (RRF_K + rank as f64 + 1.0);
                page_data.entry(pid).or_insert_with(|| row.clone());
            }
        }

        // Apply section page demotion
        for (pid, score) in rrf_scores.iter_mut() {
            if let Some(data) = page_data.get(pid) {
                let is_section = data.get("is_section").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
                if is_section {
                    *score *= 0.75;
                }
            }
        }

        // Sort by RRF score and take top results
        let mut sorted: Vec<_> = rrf_scores.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Deduplicate by file_path — keep highest-scoring entry for each HTML file
        let mut seen_files: HashMap<String, usize> = HashMap::new();
        let mut deduped: Vec<(String, f64)> = Vec::new();
        for (pid, score) in sorted {
            let file_path = page_data
                .get(&pid)
                .and_then(|d| d.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if file_path.is_empty() || !seen_files.contains_key(&file_path) {
                seen_files.insert(file_path, deduped.len());
                deduped.push((pid, score));
            }
        }

        let search_mode = if use_vectors { "hybrid" } else { "keyword" };

        let results: Vec<HashMap<String, serde_json::Value>> = deduped
            .into_iter()
            .take(limit)
            .filter_map(|(pid, score)| {
                let row = page_data.get(&pid)?;
                let content = row.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let snippet = generate_snippet(content, query);

                let mut result = HashMap::new();
                result.insert("page_id".to_string(), serde_json::json!(pid));
                result.insert(
                    "title".to_string(),
                    row.get("title").cloned().unwrap_or(serde_json::json!("")),
                );
                result.insert(
                    "file_path".to_string(),
                    row.get("file_path")
                        .cloned()
                        .unwrap_or(serde_json::json!("")),
                );
                result.insert(
                    "help_id".to_string(),
                    row.get("help_id")
                        .cloned()
                        .unwrap_or(serde_json::json!(null)),
                );
                result.insert(
                    "is_section".to_string(),
                    serde_json::json!(
                        row.get("is_section").and_then(|v| v.as_i64()).unwrap_or(0) != 0
                    ),
                );
                result.insert(
                    "breadcrumb_path".to_string(),
                    row.get("breadcrumb_path")
                        .cloned()
                        .unwrap_or(serde_json::json!(null)),
                );
                result.insert(
                    "category".to_string(),
                    row.get("category")
                        .cloned()
                        .unwrap_or(serde_json::json!(null)),
                );
                result.insert("score".to_string(), serde_json::json!(score));
                result.insert("snippet".to_string(), serde_json::json!(snippet));
                result.insert("search_mode".to_string(), serde_json::json!(search_mode));
                Some(result)
            })
            .collect();

        info!(
            "Search for '{}' (cat={:?}, mode={}) returned {} results",
            query,
            category,
            search_mode,
            results.len()
        );
        Ok(results)
    }

    // ------------------------------------------------------------------
    // Index building (FTS-only)
    // ------------------------------------------------------------------

    async fn build_fts_index(&self) -> anyhow::Result<()> {
        let start = Instant::now();
        let all_pages: Vec<(String, crate::models::HelpPage)> = self
            .indexer
            .pages
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let total = all_pages.len();

        // Drop existing table
        let _ = self.db.drop_table(TABLE_NAME, &[]).await;

        {
            let mut s = self.status.write().await;
            s.pages_total = total;
            s.pages_processed = 0;
        }

        // Process in chunks
        let indexer = self.indexer.clone();
        let total_chunks = total.div_ceil(BUILD_CHUNK_SIZE);
        let mut table_created = false;

        for (chunk_idx, chunk) in all_pages.chunks(BUILD_CHUNK_SIZE).enumerate() {
            let chunk_num = chunk_idx + 1;
            {
                let mut s = self.status.write().await;
                s.phase = format!("extracting text (chunk {chunk_num}/{total_chunks})");
            }

            // Extract text in parallel via rayon
            let records: Vec<PageRecord> = {
                let indexer_ref = indexer.clone();
                let chunk_owned: Vec<_> = chunk.to_vec();
                tokio::task::spawn_blocking(move || {
                    use rayon::prelude::*;
                    chunk_owned
                        .par_iter()
                        .map(|(pid, page)| extract_text_for_page(&indexer_ref, pid, page))
                        .collect()
                })
                .await?
            };

            // Build Arrow RecordBatch
            let batch = records_to_fts_batch(&records)?;

            {
                let mut s = self.status.write().await;
                s.phase = format!("saving (chunk {chunk_num}/{total_chunks})");
            }

            if !table_created {
                self.db.create_table(TABLE_NAME, batch).execute().await?;
                table_created = true;
            } else {
                let table = self.db.open_table(TABLE_NAME).execute().await?;
                table.add(batch).execute().await?;
            }

            let processed = (chunk_idx + 1) * BUILD_CHUNK_SIZE;
            let processed = processed.min(total);
            {
                let mut s = self.status.write().await;
                s.pages_processed = processed;
            }
            let pct = processed * 100 / total;
            let elapsed = start.elapsed().as_secs_f64();
            let rate = processed as f64 / elapsed;
            info!("Phase 1: {pct}% ({processed}/{total} pages, {rate:.0} pages/s)");
        }

        // Create FTS index
        {
            let mut s = self.status.write().await;
            s.phase = "creating FTS index".to_string();
        }
        info!("Creating FTS index...");
        let table = self.db.open_table(TABLE_NAME).execute().await?;
        create_fts_index(&table).await?;

        // Vectors are ready only when NOT in a two-phase build (i.e. FTS-only mode).
        // When called from build_index_two_phase, Phase 2 will overwrite this with true.
        self.save_metadata(!self._embeddings_enabled).await;

        let _ = self.fts_ready_tx.send(true);
        info!(
            "FTS search index built in {:.1}s ({total} documents)",
            start.elapsed().as_secs_f64()
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Two-phase build (embeddings enabled)
    // ------------------------------------------------------------------

    async fn build_index_two_phase(&self) -> anyhow::Result<()> {
        // Phase 1: FTS with zero vectors
        self.build_fts_index().await?;

        {
            let mut s = self.status.write().await;
            s.state = BuildState::FtsReady;
        }
        let _ = self.fts_ready_tx.send(true);
        info!("Phase 1 complete — keyword search is now available");

        // Phase 2: Embed + write to staging table
        let embedder = match &self.embedder {
            Some(e) => e.clone(),
            None => return Ok(()),
        };

        let all_pages: Vec<(String, crate::models::HelpPage)> = self
            .indexer
            .pages
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let total = all_pages.len();
        let total_chunks = total.div_ceil(BUILD_CHUNK_SIZE);

        info!("Phase 2: Embedding {total} pages via API (chunked)...");
        let phase2_start = Instant::now();

        // Clean up any leftover staging table
        let _ = self.db.drop_table(STAGING_TABLE, &[]).await;
        let mut staging_created = false;

        let indexer = self.indexer.clone();

        for (chunk_idx, chunk) in all_pages.chunks(BUILD_CHUNK_SIZE).enumerate() {
            let chunk_num = chunk_idx + 1;
            {
                let mut s = self.status.write().await;
                s.phase = format!("extracting + embedding (chunk {chunk_num}/{total_chunks})");
            }

            // Extract text
            let records: Vec<PageRecord> = {
                let indexer_ref = indexer.clone();
                let chunk_owned: Vec<_> = chunk.to_vec();
                tokio::task::spawn_blocking(move || {
                    use rayon::prelude::*;
                    chunk_owned
                        .par_iter()
                        .map(|(pid, page)| extract_text_for_page(&indexer_ref, pid, page))
                        .collect()
                })
                .await?
            };

            // Embed titles
            let titles: Vec<String> = records
                .iter()
                .map(|r| {
                    if r.breadcrumb_path.is_empty() {
                        r.title.clone()
                    } else {
                        format!("{} | {}", r.title, r.breadcrumb_path)
                    }
                })
                .collect();
            let title_vectors = embedder.embed_batch(&titles, false).await?;

            // Embed content (reuse title vector for pages without content)
            let mut content_vectors = title_vectors.clone();
            let content_texts: Vec<(usize, String)> = records
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.content.is_empty())
                .map(|(i, r)| {
                    let text = if r.breadcrumb_path.is_empty() {
                        r.content.clone()
                    } else {
                        format!("{} | {}", r.breadcrumb_path, r.content)
                    };
                    (i, text)
                })
                .collect();
            if !content_texts.is_empty() {
                let texts: Vec<String> = content_texts.iter().map(|(_, t)| t.clone()).collect();
                let embedded = embedder.embed_batch(&texts, false).await?;
                for ((idx, _), vec) in content_texts.iter().zip(embedded) {
                    content_vectors[*idx] = vec;
                }
            }

            // Build Arrow batch with vectors
            let batch = records_to_hybrid_batch(
                &records,
                &title_vectors,
                &content_vectors,
                embedder.dimension(),
            )?;

            if !staging_created {
                self.db.create_table(STAGING_TABLE, batch).execute().await?;
                staging_created = true;
            } else {
                let tbl = self.db.open_table(STAGING_TABLE).execute().await?;
                tbl.add(batch).execute().await?;
            }

            let embedded = ((chunk_idx + 1) * BUILD_CHUNK_SIZE).min(total);
            {
                let mut s = self.status.write().await;
                s.pages_processed = embedded;
            }
            let pct = embedded * 100 / total;
            let elapsed = phase2_start.elapsed().as_secs_f64();
            let rate = embedded as f64 / elapsed;
            info!("Phase 2: {pct}% ({embedded}/{total} pages, {rate:.0} pages/s)");
        }

        // Swap staging → final
        self.swap_staging_table().await?;
        self.save_metadata(true).await;

        info!(
            "Phase 2 complete — full hybrid search ready in {:.1}s ({total} documents)",
            phase2_start.elapsed().as_secs_f64()
        );
        Ok(())
    }

    async fn swap_staging_table(&self) -> anyhow::Result<()> {
        let _ = self.fts_ready_tx.send(false);
        {
            let mut s = self.status.write().await;
            s.state = BuildState::Building;
            s.phase = "swapping staging table".to_string();
        }
        info!("Swapping staging table → final...");

        // Drop old main table
        let _ = self.db.drop_table(TABLE_NAME, &[]).await;

        // Try filesystem rename
        let staging_dir = self.db_path.join(format!("{STAGING_TABLE}.lance"));
        let final_dir = self.db_path.join(format!("{TABLE_NAME}.lance"));
        let swapped = if staging_dir.is_dir() {
            match std::fs::rename(&staging_dir, &final_dir) {
                Ok(()) => {
                    info!("Staging table swapped via filesystem rename");
                    true
                }
                Err(e) => {
                    warn!("Filesystem rename failed ({}), falling back to copy", e);
                    false
                }
            }
        } else {
            false
        };

        if !swapped {
            // Fallback: read staging data and recreate
            let staging = self.db.open_table(STAGING_TABLE).execute().await?;
            let data: Vec<_> = staging.query().execute().await?.try_collect().await?;
            if let Some(first) = data.first() {
                self.db
                    .create_table(TABLE_NAME, first.clone())
                    .execute()
                    .await?;
                let tbl = self.db.open_table(TABLE_NAME).execute().await?;
                for batch in &data[1..] {
                    tbl.add(batch.clone()).execute().await?;
                }
            }
            let _ = self.db.drop_table(STAGING_TABLE, &[]).await;
            info!("Staging table swapped via Arrow copy");
        }

        // Rebuild FTS on final table
        {
            let mut s = self.status.write().await;
            s.phase = "rebuilding FTS index".to_string();
        }
        let table = self.db.open_table(TABLE_NAME).execute().await?;
        create_fts_index(&table).await?;
        let _ = self.fts_ready_tx.send(true);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Incremental update
    // ------------------------------------------------------------------

    async fn incremental_update(&self) -> anyhow::Result<()> {
        let start = Instant::now();

        let old_fps = load_old_fingerprints(&self.metadata_path);
        let new_fps = self.indexer.get_page_fingerprints();

        let old_ids: std::collections::HashSet<&String> = old_fps.keys().collect();
        let new_ids: std::collections::HashSet<&String> = new_fps.keys().collect();

        let added: Vec<&String> = new_ids.difference(&old_ids).copied().collect();
        let removed: Vec<&String> = old_ids.difference(&new_ids).copied().collect();
        let changed: Vec<&String> = old_ids
            .intersection(&new_ids)
            .filter(|pid| old_fps.get(**pid) != new_fps.get(**pid))
            .copied()
            .collect();

        let to_upsert: std::collections::HashSet<&String> =
            added.iter().chain(changed.iter()).copied().collect();
        let to_delete: std::collections::HashSet<&String> =
            removed.iter().chain(changed.iter()).copied().collect();

        {
            let mut s = self.status.write().await;
            s.incremental_stats = Some(IncrementalStats {
                added: added.len(),
                removed: removed.len(),
                changed: changed.len(),
                unchanged: new_fps.len() - to_upsert.len(),
            });
            s.pages_total = to_upsert.len();
            s.phase = "computing diff".to_string();
        }

        info!(
            "Incremental diff: {} added, {} removed, {} changed, {} unchanged",
            added.len(),
            removed.len(),
            changed.len(),
            new_fps.len() - to_upsert.len()
        );

        if to_upsert.is_empty() && to_delete.is_empty() {
            info!("No page-level changes detected — skipping update");
            // Preserve the current vectors_ready state from the existing metadata.
            let prev_vectors_ready = self.load_vectors_ready_flag();
            self.save_metadata(prev_vectors_ready).await;
            return Ok(());
        }

        // Fall back to full rebuild if >50% changed
        if to_upsert.len() > new_fps.len() / 2 {
            info!(
                "Too many changes ({}/{}) — falling back to full rebuild",
                to_upsert.len(),
                new_fps.len()
            );
            let _ = self.fts_ready_tx.send(false);
            if self._embeddings_enabled {
                return self.build_index_two_phase().await;
            } else {
                return self.build_fts_index().await;
            }
        }

        let table = self.db.open_table(TABLE_NAME).execute().await?;

        // Delete removed/changed rows
        if !to_delete.is_empty() {
            let delete_list: Vec<&str> = to_delete.iter().map(|s| s.as_str()).collect();
            for batch in delete_list.chunks(500) {
                let ids: String = batch
                    .iter()
                    .map(|id| format!("'{}'", id.replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(", ");
                table.delete(&format!("page_id IN ({ids})")).await?;
            }
            info!("Deleted {} rows from index", to_delete.len());
        }

        // Extract + insert new/changed pages
        if !to_upsert.is_empty() {
            let indexer = self.indexer.clone();
            let pages_to_index: Vec<(String, crate::models::HelpPage)> = to_upsert
                .iter()
                .filter_map(|pid| {
                    indexer
                        .pages
                        .get(*pid)
                        .map(|p| (pid.to_string(), p.clone()))
                })
                .collect();

            let records: Vec<PageRecord> = {
                let indexer_ref = indexer.clone();
                tokio::task::spawn_blocking(move || {
                    use rayon::prelude::*;
                    pages_to_index
                        .par_iter()
                        .map(|(pid, page)| extract_text_for_page(&indexer_ref, pid, page))
                        .collect()
                })
                .await?
            };

            if self._embeddings_enabled {
                if let Some(ref embedder) = self.embedder {
                    let title_texts: Vec<String> = records
                        .iter()
                        .map(|r| format!("{} {}", r.title, r.breadcrumb_path))
                        .collect();
                    let content_texts: Vec<String> =
                        records.iter().map(|r| r.content.clone()).collect();

                    let title_vecs = embedder
                        .embed_batch(&title_texts, false)
                        .await
                        .map_err(|e| anyhow::anyhow!("Title embedding failed: {e}"))?;
                    let content_vecs = embedder
                        .embed_batch(&content_texts, false)
                        .await
                        .map_err(|e| anyhow::anyhow!("Content embedding failed: {e}"))?;
                    let dim = embedder.dimension();

                    let batch = records_to_hybrid_batch(&records, &title_vecs, &content_vecs, dim)?;
                    table.add(batch).execute().await?;
                } else {
                    let batch = records_to_fts_batch(&records)?;
                    table.add(batch).execute().await?;
                }
            } else {
                let batch = records_to_fts_batch(&records)?;
                table.add(batch).execute().await?;
            }
            info!("Added {} rows to index", records.len());
        }

        // Rebuild FTS index
        {
            let mut s = self.status.write().await;
            s.phase = "rebuilding FTS index".to_string();
        }
        create_fts_index(&table).await?;
        // After an incremental update the table retains whatever columns it had
        // before, so vectors_ready matches whether embeddings were (and remain) active.
        self.save_metadata(self._embeddings_enabled).await;

        info!(
            "Incremental update complete in {:.1}s (+{} -{} ~{} pages)",
            start.elapsed().as_secs_f64(),
            added.len(),
            removed.len(),
            changed.len()
        );
        Ok(())
    }

    // ------------------------------------------------------------------
    // Load existing index
    // ------------------------------------------------------------------

    async fn load_index(&self) -> anyhow::Result<()> {
        let table = self.db.open_table(TABLE_NAME).execute().await?;
        let count = table.count_rows(None).await?;
        let mode = if self._embeddings_enabled {
            "hybrid"
        } else {
            "FTS-only"
        };
        info!("Loaded search index with {count} documents ({mode})");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Metadata
    // ------------------------------------------------------------------

    /// Save index metadata. `vectors_ready` should be `true` only after Phase 2
    /// (embeddings) successfully completes. Pass `false` after Phase 1 so that
    /// a restart with embeddings enabled will trigger a full rebuild to complete
    /// Phase 2 rather than treating the FTS-only table as a finished hybrid index.
    async fn save_metadata(&self, vectors_ready: bool) {
        let mut metadata = serde_json::json!({
            "xml_hash": self.indexer.get_xml_hash(),
            "indexed_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            "page_count": self.indexer.pages.len(),
            "help_id_count": self.indexer.help_id_map.len(),
            "embeddings_enabled": self._embeddings_enabled,
            "vectors_ready": vectors_ready,
            "fts_config": {
                "stem": true,
                "remove_stop_words": true,
                "ascii_folding": true,
                "with_position": false,
                "language": "English",
                "search_text_boost": "title_3x_breadcrumb_2x"
            },
            "page_fingerprints": self.indexer.get_page_fingerprints(),
        });
        if let Some(ref emb) = self.embedder {
            metadata["embedding_model"] = serde_json::Value::String(emb.model_name.clone());
            metadata["embedding_dimensions"] = serde_json::Value::Number(emb.dimension().into());
        }
        if let Err(e) = std::fs::write(
            &self.metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        ) {
            warn!("Failed to save metadata: {}", e);
        }
    }

    fn load_vectors_ready_flag(&self) -> bool {
        let text = match std::fs::read_to_string(&self.metadata_path) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let metadata: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return false,
        };
        metadata
            .get("vectors_ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Free functions for search legs
// ---------------------------------------------------------------------------

async fn vector_search(
    table: &lancedb::Table,
    query_vector: &[f32],
    column_name: &str,
    limit: usize,
    where_clause: Option<&str>,
) -> anyhow::Result<Vec<HashMap<String, serde_json::Value>>> {
    let mut builder = table.vector_search(query_vector)?;
    builder = builder.column(column_name).limit(limit);
    if let Some(wc) = where_clause {
        builder = builder.only_if(wc);
    }
    let batches: Vec<_> = builder.execute().await?.try_collect().await?;
    Ok(batches_to_rows(&batches))
}

async fn fts_search(
    table: &lancedb::Table,
    query: &str,
    limit: usize,
    where_clause: Option<&str>,
) -> anyhow::Result<Vec<HashMap<String, serde_json::Value>>> {
    let fts_query = lancedb::index::scalar::FullTextSearchQuery::new(query.to_owned());
    let mut builder = table.query().full_text_search(fts_query).limit(limit);
    if let Some(wc) = where_clause {
        builder = builder.only_if(wc);
    }
    let batches: Vec<_> = builder.execute().await?.try_collect().await?;
    Ok(batches_to_rows(&batches))
}

async fn breadcrumb_retrieval(
    table: &lancedb::Table,
    query: &str,
    limit: usize,
    where_clause: Option<&str>,
) -> anyhow::Result<Vec<HashMap<String, serde_json::Value>>> {
    let mut sanitized = query.to_string();
    for ch in "\"'*:(){}^+[]-/%_".chars() {
        sanitized = sanitized.replace(ch, " ");
    }
    let reserved: std::collections::HashSet<&str> =
        ["and", "or", "not", "near"].into_iter().collect();
    let terms: Vec<String> = sanitized
        .split_whitespace()
        .filter(|t| t.len() >= 2 && !reserved.contains(&t.to_lowercase().as_str()))
        .map(|t| t.to_lowercase())
        .collect();

    if terms.is_empty() {
        return Ok(Vec::new());
    }

    let bc_conditions: Vec<String> = terms
        .iter()
        .map(|t| format!("lower(breadcrumb_path) LIKE '%{t}%'"))
        .collect();
    let mut filter = bc_conditions.join(" AND ");
    if let Some(wc) = where_clause {
        filter = format!("({filter}) AND ({wc})");
    }

    let scan_limit = limit.max(200);
    let builder = table.query().only_if(&filter).limit(scan_limit);
    let batches: Vec<_> = builder.execute().await?.try_collect().await?;
    let mut rows = batches_to_rows(&batches);

    // Sort by breadcrumb match quality
    rows.sort_by(|a, b| {
        let bc_a = a
            .get("breadcrumb_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let bc_b = b
            .get("breadcrumb_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let hits_a: usize = terms.iter().filter(|t| bc_a.contains(t.as_str())).count();
        let hits_b: usize = terms.iter().filter(|t| bc_b.contains(t.as_str())).count();
        hits_b
            .cmp(&hits_a)
            .then_with(|| bc_a.len().cmp(&bc_b.len()))
    });
    rows.truncate(limit);
    Ok(rows)
}

/// Convert Arrow RecordBatches to Vec of JSON-like row maps.
fn batches_to_rows(batches: &[RecordBatch]) -> Vec<HashMap<String, serde_json::Value>> {
    let mut rows = Vec::new();
    for batch in batches {
        let schema = batch.schema();
        for row_idx in 0..batch.num_rows() {
            let mut map = HashMap::new();
            for (col_idx, field) in schema.fields().iter().enumerate() {
                let col = batch.column(col_idx);
                let val = match field.data_type() {
                    DataType::Utf8 => {
                        let arr = col.as_any().downcast_ref::<StringArray>().unwrap();
                        if arr.is_null(row_idx) {
                            serde_json::Value::Null
                        } else {
                            serde_json::json!(arr.value(row_idx))
                        }
                    }
                    DataType::Int32 => {
                        let arr = col.as_any().downcast_ref::<Int32Array>().unwrap();
                        serde_json::json!(arr.value(row_idx))
                    }
                    _ => serde_json::Value::Null, // Skip vector columns etc.
                };
                map.insert(field.name().clone(), val);
            }
            rows.push(map);
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Index creation
// ---------------------------------------------------------------------------

async fn create_fts_index(table: &lancedb::Table) -> anyhow::Result<()> {
    table
        .create_index(
            &["search_text"],
            Index::FTS(
                FtsIndexBuilder::default()
                    .stem(true)
                    .remove_stop_words(true)
                    .ascii_folding(true)
                    .with_position(false),
            ),
        )
        .execute()
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Build strategy detection
// ---------------------------------------------------------------------------

async fn detect_build_strategy(
    db: &lancedb::Connection,
    metadata_path: &Path,
    embeddings_enabled: bool,
    embedder: Option<&EmbeddingService>,
    indexer: &HelpContentIndexer,
) -> BuildStrategy {
    // Check if table exists
    let table_exists = match db.open_table(TABLE_NAME).execute().await {
        Ok(t) => t.count_rows(None).await.unwrap_or(0) > 0,
        Err(_) => false,
    };

    if !table_exists {
        return BuildStrategy::Full;
    }

    // Check metadata
    if !metadata_path.exists() {
        return BuildStrategy::Full;
    }

    let metadata = match std::fs::read_to_string(metadata_path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(_) => return BuildStrategy::Full,
        },
        Err(_) => return BuildStrategy::Full,
    };

    // Embedding mode changed
    let stored_embeddings = metadata
        .get("embeddings_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if stored_embeddings != embeddings_enabled {
        info!("Embedding mode changed — full rebuild required");
        return BuildStrategy::Full;
    }

    // Embeddings enabled but Phase 2 never completed (e.g. interrupted)
    if embeddings_enabled {
        let vectors_ready = metadata
            .get("vectors_ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !vectors_ready {
            info!(
                "Embeddings enabled but vectors not ready (Phase 2 incomplete) — full rebuild required"
            );
            return BuildStrategy::Full;
        }
    }

    // Embedding model changed
    if embeddings_enabled && let Some(emb) = embedder {
        let stored_model = metadata
            .get("embedding_model")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if stored_model != emb.model_name {
            info!("Embedding model changed — full rebuild required");
            return BuildStrategy::Full;
        }
    }

    // FTS config changed
    let current_fts_config = serde_json::json!({
        "stem": true,
        "remove_stop_words": true,
        "ascii_folding": true,
        "with_position": false,
        "language": "English",
        "search_text_boost": "title_3x_breadcrumb_2x"
    });
    if let Some(stored_fts) = metadata.get("fts_config")
        && *stored_fts != current_fts_config
    {
        info!("FTS config changed — full rebuild required");
        return BuildStrategy::Full;
    }

    // XML unchanged
    let stored_hash = metadata
        .get("xml_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if stored_hash == indexer.get_xml_hash() {
        return BuildStrategy::None;
    }

    // XML changed — incremental if fingerprints available
    if metadata
        .get("page_fingerprints")
        .and_then(|v| v.as_object())
        .is_some()
    {
        info!("XML changed — incremental update possible");
        return BuildStrategy::Incremental;
    }

    info!("XML changed but no page fingerprints — full rebuild required");
    BuildStrategy::Full
}

fn load_old_fingerprints(metadata_path: &Path) -> HashMap<String, String> {
    let text = match std::fs::read_to_string(metadata_path) {
        Ok(t) => t,
        Err(_) => return HashMap::new(),
    };
    let metadata: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    metadata
        .get("page_fingerprints")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Text extraction + Arrow conversion
// ---------------------------------------------------------------------------

struct PageRecord {
    page_id: String,
    title: String,
    content: String,
    file_path: String,
    help_id: String,
    is_section: i32,
    breadcrumb_path: String,
    category: String,
}

fn extract_text_for_page(
    indexer: &HelpContentIndexer,
    page_id: &str,
    page: &crate::models::HelpPage,
) -> PageRecord {
    let plain_text = indexer
        .extract_plain_text_no_cache(page)
        .unwrap_or_default();
    let breadcrumb_path = indexer.get_breadcrumb_string(page_id);
    let breadcrumb = indexer.get_breadcrumb(page_id);
    let category = breadcrumb
        .first()
        .map(|p| p.text.clone())
        .unwrap_or_default();

    PageRecord {
        page_id: page_id.to_string(),
        title: page.text.clone(),
        content: plain_text,
        file_path: page.file_path.clone(),
        help_id: page.help_id.clone().unwrap_or_default(),
        is_section: if page.is_section { 1 } else { 0 },
        breadcrumb_path,
        category,
    }
}

fn records_to_fts_batch(records: &[PageRecord]) -> anyhow::Result<RecordBatch> {
    let schema = Arc::new(fts_schema());

    let page_ids = StringArray::from(
        records
            .iter()
            .map(|r| r.page_id.as_str())
            .collect::<Vec<_>>(),
    );
    let titles = StringArray::from(records.iter().map(|r| r.title.as_str()).collect::<Vec<_>>());
    let contents = StringArray::from(
        records
            .iter()
            .map(|r| r.content.as_str())
            .collect::<Vec<_>>(),
    );
    let search_texts = StringArray::from(
        records
            .iter()
            .map(|r| {
                format!(
                    "{t} {t} {t} {bc} {bc} {c}",
                    t = r.title,
                    bc = r.breadcrumb_path,
                    c = r.content
                )
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
    );
    let file_paths = StringArray::from(
        records
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>(),
    );
    let help_ids = StringArray::from(
        records
            .iter()
            .map(|r| r.help_id.as_str())
            .collect::<Vec<_>>(),
    );
    let is_sections = Int32Array::from(records.iter().map(|r| r.is_section).collect::<Vec<_>>());
    let breadcrumbs = StringArray::from(
        records
            .iter()
            .map(|r| r.breadcrumb_path.as_str())
            .collect::<Vec<_>>(),
    );
    let categories = StringArray::from(
        records
            .iter()
            .map(|r| r.category.as_str())
            .collect::<Vec<_>>(),
    );

    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_ids),
            Arc::new(titles),
            Arc::new(contents),
            Arc::new(search_texts),
            Arc::new(file_paths),
            Arc::new(help_ids),
            Arc::new(is_sections),
            Arc::new(breadcrumbs),
            Arc::new(categories),
        ],
    )?)
}

fn records_to_hybrid_batch(
    records: &[PageRecord],
    title_vectors: &[Vec<f32>],
    content_vectors: &[Vec<f32>],
    dim: usize,
) -> anyhow::Result<RecordBatch> {
    use arrow_array::FixedSizeListArray;
    use arrow_array::types::Float32Type;

    let schema = Arc::new(hybrid_schema(dim));

    let page_ids = StringArray::from(
        records
            .iter()
            .map(|r| r.page_id.as_str())
            .collect::<Vec<_>>(),
    );
    let titles = StringArray::from(records.iter().map(|r| r.title.as_str()).collect::<Vec<_>>());
    let contents = StringArray::from(
        records
            .iter()
            .map(|r| r.content.as_str())
            .collect::<Vec<_>>(),
    );
    let search_texts = StringArray::from(
        records
            .iter()
            .map(|r| {
                format!(
                    "{t} {t} {t} {bc} {bc} {c}",
                    t = r.title,
                    bc = r.breadcrumb_path,
                    c = r.content
                )
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>(),
    );
    let file_paths = StringArray::from(
        records
            .iter()
            .map(|r| r.file_path.as_str())
            .collect::<Vec<_>>(),
    );
    let help_ids = StringArray::from(
        records
            .iter()
            .map(|r| r.help_id.as_str())
            .collect::<Vec<_>>(),
    );
    let is_sections = Int32Array::from(records.iter().map(|r| r.is_section).collect::<Vec<_>>());
    let breadcrumbs = StringArray::from(
        records
            .iter()
            .map(|r| r.breadcrumb_path.as_str())
            .collect::<Vec<_>>(),
    );
    let categories = StringArray::from(
        records
            .iter()
            .map(|r| r.category.as_str())
            .collect::<Vec<_>>(),
    );

    let title_vec_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        title_vectors
            .iter()
            .map(|v| Some(v.iter().map(|&f| Some(f)).collect::<Vec<_>>())),
        dim as i32,
    );
    let content_vec_array = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        content_vectors
            .iter()
            .map(|v| Some(v.iter().map(|&f| Some(f)).collect::<Vec<_>>())),
        dim as i32,
    );

    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(page_ids),
            Arc::new(titles),
            Arc::new(contents),
            Arc::new(search_texts),
            Arc::new(file_paths),
            Arc::new(help_ids),
            Arc::new(is_sections),
            Arc::new(breadcrumbs),
            Arc::new(categories),
            Arc::new(title_vec_array),
            Arc::new(content_vec_array),
        ],
    )?)
}

fn fts_schema() -> Schema {
    Schema::new(vec![
        Field::new("page_id", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("search_text", DataType::Utf8, false),
        Field::new("file_path", DataType::Utf8, false),
        Field::new("help_id", DataType::Utf8, true),
        Field::new("is_section", DataType::Int32, false),
        Field::new("breadcrumb_path", DataType::Utf8, true),
        Field::new("category", DataType::Utf8, true),
    ])
}

fn hybrid_schema(dim: usize) -> Schema {
    let mut fields = fts_schema().fields().to_vec();
    fields.push(Arc::new(Field::new(
        "title_vector",
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dim as i32,
        ),
        true,
    )));
    fields.push(Arc::new(Field::new(
        "content_vector",
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            dim as i32,
        ),
        true,
    )));
    Schema::new(fields)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_identifier_query() {
        assert!(is_identifier_query("MC_MoveAbsolute"));
        assert!(is_identifier_query("X20DI9371"));
        assert!(is_identifier_query("mapp.Motion"));
        assert!(!is_identifier_query("how to move an axis"));
        assert!(!is_identifier_query(""));
        assert!(!is_identifier_query("a b c d"));
    }

    #[test]
    fn test_sanitize_query() {
        assert_eq!(sanitize_query("hello world"), "hello world");
        assert_eq!(sanitize_query("\"test\" AND (foo)"), "test foo");
        // Single char "a" is treated as an identifier — kept in identifier mode
        assert_eq!(sanitize_query("a"), "a");
        // NL-mode filtering via sanitize_query_with_mode
        assert_eq!(sanitize_query_with_mode("a", false), "");
    }

    #[test]
    fn test_sanitize_query_like_wildcards() {
        // % and _ should be stripped to prevent LIKE wildcard injection
        let result = sanitize_query("test%wildcard");
        assert!(!result.contains('%'));
        let result = sanitize_query("test_wildcard");
        assert!(!result.contains('_'));
        let result = sanitize_query("%_%%__");
        assert_eq!(result, "");
    }

    #[test]
    fn test_generate_snippet() {
        let content =
            "This is a long text about MC_MoveAbsolute function block that does motion control.";
        let snippet = generate_snippet(content, "MC_MoveAbsolute").unwrap();
        assert!(snippet.contains("MC_MoveAbsolute"));
    }

    #[test]
    fn test_generate_snippet_utf8_boundary() {
        // German text with multi-byte characters (ä=2 bytes, ö=2 bytes, ü=2 bytes, ß=2 bytes)
        let content =
            "Übersicht der Sicherheitstechnik mit Änderungen für Größenberechnung und Maße";
        let snippet = generate_snippet(content, "Sicherheitstechnik").unwrap();
        assert!(snippet.contains("Sicherheitstechnik"));

        // Content with multi-byte chars near truncation boundary at 160
        let content = "A".repeat(158) + "ä"; // 158 + 2 = 160 bytes, but 159 chars
        let snippet = generate_snippet(&content, "nomatch").unwrap();
        // Should not panic and should contain the full string
        assert!(snippet.contains("ä"));

        // Multi-byte right at 160 byte boundary
        let content = "A".repeat(159) + "ä"; // 159 + 2 = 161 bytes
        let snippet = generate_snippet(&content, "nomatch").unwrap();
        // Should truncate safely without panicking
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn test_safe_truncate() {
        // ASCII: trivial case
        assert_eq!(safe_truncate("hello", 3), "hel");
        assert_eq!(safe_truncate("hello", 100), "hello");

        // Multi-byte: ä is 2 bytes (0xC3 0xA4)
        let s = "aäb"; // bytes: [97, 195, 164, 98] = 4 bytes
        assert_eq!(safe_truncate(s, 1), "a");
        assert_eq!(safe_truncate(s, 2), "a"); // mid-ä, walks back
        assert_eq!(safe_truncate(s, 3), "aä");
        assert_eq!(safe_truncate(s, 4), "aäb");

        // Multi-byte chars at position 99-101 (server.rs truncation scenario)
        let s = "A".repeat(99) + "ü" + "rest"; // 99 + 2 + 4 = 105 bytes
        let trunc = safe_truncate(&s, 100);
        assert_eq!(trunc.len(), 99); // can't fit the ü (starts at 99, ends at 101)

        let s = "A".repeat(99) + "B" + "rest";
        let trunc = safe_truncate(&s, 100);
        assert_eq!(trunc.len(), 100); // exact ASCII boundary
    }

    #[test]
    fn test_build_category_filter() {
        assert_eq!(
            build_category_filter(Some("Motion")),
            Some("lower(category) = 'motion'".to_string())
        );
        assert_eq!(build_category_filter(None), None);
        assert_eq!(build_category_filter(Some("")), None);
    }
}
