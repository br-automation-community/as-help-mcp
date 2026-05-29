# RAG Strategy — B&R Help MCP Server

This document describes the Retrieval-Augmented Generation (RAG) architecture used by the B&R Help MCP Server: how documentation is indexed, how search works, and the reasoning behind each design decision.

---

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Chunking Strategy](#chunking-strategy)
3. [Two-Phase Index Build](#two-phase-index-build)
4. [Embedding Model](#embedding-model)
5. [Search Modes](#search-modes)
6. [Hybrid Search & RRF Fusion](#hybrid-search--rrf-fusion)
7. [Query-Type Detection](#query-type-detection)
8. [Progressive Availability](#progressive-availability)
9. [Index Rebuild Logic](#index-rebuild-logic)
10. [Configuration Reference](#configuration-reference)
11. [Alternatives Considered](#alternatives-considered)

---

## Architecture Overview

```
brhelpcontent.xml ──► Indexer ──► Page Tree (in-memory, ~100K pages)
                                    │
                              HTML files ──► lxml ──► Plain text
                                    │
              ┌─────────────────────┴─────────────────────┐
              │                                           │
        FTS-only mode                            Hybrid mode
        (default)                        (CREATE_EMBEDDINGS=true)
              │                                           │
    LanceDB native FTS                Embedding API ─► Vectors
              │                                  │
              │                         LanceDB native FTS + Vectors
              │                                  │
              ▼                                  ▼
       Keyword search                   RRF Hybrid search
       (BM25 ranking)               (4 signals fused via RRF)
              │                                  │
              └────────────► MCP Tools ◄─────────┘
```

The server indexes B&R Automation Studio help documentation (~100K pages) into LanceDB. By default it provides full-text keyword search only, requiring zero external dependencies. When configured with an embedding API, it adds vector columns and enables hybrid search.

---

## Chunking Strategy

### One page = one chunk

Each help page or section in B&R's documentation maps directly to one search document (chunk). There is no sub-page splitting, no sliding window, and no semantic chunking.

**Why this works for B&R documentation:**

- **Pages are self-contained.** B&R documentation is structured as a hierarchy of sections and pages, each with a specific topic (a function block, a hardware module, a configuration property). Pages are naturally atomic units of information.
- **Pages are reasonably sized.** The vast majority of pages are well within embedding model context limits. Even with `nomic-embed-text`'s 8,192 token context window, most pages fit comfortably.
- **Retrieval granularity matches LLM usage.** When an LLM retrieves a page via `get_page_by_id`, it gets the full content. Sub-page chunks would add complexity without improving answer quality — the LLM can handle reading a full page and extracting the relevant part.
- **The existing structure provides hierarchy.** Breadcrumbs, parent-child relationships, and HelpIDs already exist in the XML. Splitting pages would lose this structure and require re-stitching at query time.

### What gets indexed per chunk

| Field | Content | Used for |
|-------|---------|----------|
| `title` | Page title from XML | Display, title-match ranking |
| `content` | Plain text extracted from HTML via lxml | Semantic search, snippets |
| `search_text` | `"{title} {breadcrumb} {content}"` | FTS keyword index (Lance native) |
| `breadcrumb_path` | Full path like `"Hardware > X20 System > X20DI9371"` | Category filtering, context |
| `title_vector` | Embedding of `"{title} \| {breadcrumb}"` | Vector similarity on title |
| `content_vector` | Embedding of `"{breadcrumb} \| {content}"` | Vector similarity on content |
| `category` | Top-level breadcrumb entry (e.g., "Hardware") | SQL `WHERE` filter |

**Breadcrumb enrichment:** Both vector columns prepend the breadcrumb path to give the embedding model context about *where* the page sits in the documentation hierarchy. This significantly improves retrieval for ambiguous queries — e.g., "configuration" means very different things under "Safety" vs "Motion".

### Text extraction

HTML content is extracted using **lxml** (2–3× faster than BeautifulSoup). `<script>` and `<style>` elements are stripped via XPath before calling `text_content()`. The indexer extracts text lazily and caches it on the `HelpPage` object.

### Sections vs. pages

Both sections and pages are indexed. Many B&R sections contain substantive documentation (LED tables, wiring diagrams, register descriptions) that is valuable for search. The `is_section` flag is stored but does not affect ranking.

---

## Two-Phase Index Build

When embeddings are enabled, the index is built in two phases to provide **progressive availability** — keyword search is available within minutes while embedding continues in the background for potentially 10+ minutes.

### Phase 1: Text Extraction + FTS

1. Parse `brhelpcontent.xml` into an in-memory page tree (~2s)
2. Extract plain text from HTML files using parallel `ThreadPoolExecutor` (configurable workers, chunks of 5,000 pages)
3. Write rows to LanceDB with **zero vectors** (placeholder `[0.0, 0.0, ...]`)
4. Build Lance native FTS index on the `search_text` column (with stemming, stop-word removal, ASCII folding)
5. **Set `_fts_ready` event** → keyword search is now available

### Phase 2: Chunked Embedding + Staging Table

Phase 2 is designed to be **memory-efficient** — it processes pages in chunks of 5,000 and writes to a staging table, keeping peak memory at ~200–400 MB regardless of total page count.

1. Clean up any leftover staging table from a previous interrupted build
2. For each chunk of 5,000 pages:
   a. Re-extract plain text from HTML files (parallel, fast — text was not kept from Phase 1)
   b. Embed title+breadcrumb via the configured embedding API
   c. Embed content+breadcrumb (reuses title vectors for pages without content)
   d. Write chunk to a **staging table** (`help_pages_staging`)
   e. Free memory before next chunk
3. **Brief FTS suspension**: drop original table → rename staging directory → rebuild FTS
4. **Set both `_fts_ready` and `_ready`** → hybrid search is now available

**Why a staging table?** Keyword search stays available on the original Phase 1 table during the entire embedding process (~15–20 min for 60K pages). Only the final swap (step 3) briefly suspends FTS for a few seconds.

**Why re-extract text?** Phase 1 text records are not kept in memory — this would consume 1–2 GB for 120K pages. Re-extracting from HTML (parallel lxml) takes ~30–60 seconds and keeps peak memory low.

### Memory profile

| Phase | Peak memory (120K pages) | What's in memory |
|-------|--------------------------|------------------|
| Phase 1 | ~200–400 MB | One chunk of 5K pages + zero vectors |
| Phase 2 | ~200–400 MB | One chunk of 5K pages + real vectors |
| Swap | ~50 MB | Filesystem rename (no data in memory) |
| Runtime | ~100–200 MB | Page tree + LanceDB mmap |

Two instances (AS4 + AS6) building simultaneously: **~400–800 MB peak**, safe for 16 GB laptops.

### Why two phases?

- Embedding 60K pages via a local API (Ollama) takes ~15–20 minutes. Making users wait that long for any search capability is unacceptable.
- FTS/keyword search is surprisingly effective for B&R's technical documentation, where exact identifiers (`MC_MoveAbsolute`, `X20DI9371`) are common.
- The two-phase approach lets the server be immediately useful while vectors are being computed.
- Chunked writes keep memory consumption under ~400 MB regardless of page count, enabling multiple instances (e.g., AS4 + AS6) to build simultaneously on laptops with limited RAM.

### Resume support

If the build is interrupted (crash, timeout, restart), Phase 1 progress is saved and can be resumed. The server detects an incomplete build via `_build_progress.json` and skips already-indexed pages. Phase 2 re-extracts text for resumed pages in parallel before embedding.

---

## Embedding Model

### Current choice: `nomic-embed-text`

| Property | Value |
|----------|-------|
| Dimensions | 768 |
| Context window | 8,192 tokens |
| Hosting | Ollama (local) |
| API compatibility | OpenAI-compatible `/v1/embeddings` |

**Why nomic-embed-text:**

- **Strong performance for its size.** Ranks well on MTEB benchmarks, competitive with larger models for retrieval tasks.
- **8K token context.** Fits the vast majority of B&R help pages without truncation. Pages that exceed the limit are truncated at `EMBEDDING_MAX_CHARS` (default: 4,000 characters).
- **Runs locally via Ollama.** No cloud API costs, no data leaving the network — important for industrial documentation.
- **768 dimensions.** Good balance between recall quality and storage/compute cost. Smaller than OpenAI's 1,536-dim models but sufficient for this use case.

### Embedding API flexibility

The server calls any **OpenAI-compatible** embedding endpoint. This means you can swap in:

- **OpenAI** (`text-embedding-3-small`, `text-embedding-ada-002`)
- **Azure OpenAI** (deployed embedding models)
- **GitHub Models** (hosted inference)
- **Ollama** (any embedding model: `nomic-embed-text`, `mxbai-embed-large`, etc.)
- **LiteLLM** (proxy to any provider)

Changing the model triggers a full index rebuild (the server detects model name changes via metadata).

### Truncation strategy

Texts are truncated to `EMBEDDING_MAX_CHARS` (default 4,000 characters) before being sent to the API. If a single text exceeds the model's token limit at the API level, the batch falls back to one-by-one embedding with zero-vector fallback for texts that still fail. This graceful degradation ensures the build never crashes on oversized pages.

---

## Search Modes

The server operates in one of two modes depending on configuration and index state:

### Keyword-only mode (FTS)

- **When:** `CREATE_EMBEDDINGS=false` (default), or during Phase 1 of a two-phase build
- **Engine:** Lance native FTS (built into LanceDB) with BM25 scoring
- **Index field:** `search_text` = concatenation of title, breadcrumb, and content
- **Ranking:** BM25 term frequency scoring + title-match bonus via RRF

This mode requires zero external dependencies and works well for exact-match queries on technical identifiers.

### Hybrid mode (RRF)

- **When:** `CREATE_EMBEDDINGS=true` and the index is fully built (Phase 2 complete)
- **Engine:** Four search signals fused via Reciprocal Rank Fusion
- **Ranking:** See [Hybrid Search & RRF Fusion](#hybrid-search--rrf-fusion)

Hybrid mode significantly improves results for natural-language queries like "how to configure a safety response" while maintaining excellent performance for identifier lookups.

---

## Hybrid Search & RRF Fusion

When hybrid mode is active, every query runs four search "legs" in parallel, each producing a ranked list. These lists are fused using **Reciprocal Rank Fusion (RRF)** with per-signal weights.

### The four search signals

| # | Signal | What it searches | Vector column | Weight (NL) | Weight (ID) |
|---|--------|------------------|---------------|-------------|-------------|
| 1 | **Title vector** | Semantic similarity of query vs. `title \| breadcrumb` | `title_vector` | 2.0 | 0.5 |
| 2 | **Content vector** | Semantic similarity of query vs. `breadcrumb \| content` | `content_vector` | 1.0 | 0.5 |
| 3 | **FTS keyword** | BM25 keyword match on `title + breadcrumb + content` | — (Lance FTS) | 1.5 | 3.0 |
| 4 | **Title match** | Exact/substring match of query in page titles | — (in-memory) | 3.0 | 4.0 |
| 5 | **Breadcrumb match** | Query terms found in breadcrumb path | — (in-memory) | 2.0 | 3.0 |

- **NL** = natural-language query (e.g., "how to configure axis homing")
- **ID** = identifier query (e.g., `MC_MoveAbsolute`, `X20DI9371`)

### RRF scoring formula

For each search signal $s$ with weight $w_s$, a page at rank $r$ (0-indexed) receives:

$$\text{score}_s = \frac{w_s}{k + r + 1}$$

where $k = 60$ is the standard RRF constant from the original [Cormack et al. paper](https://dl.acm.org/doi/10.1145/1571941.1572114).

The final score for a page is the sum across all signals:

$$\text{score}(page) = \sum_{s \in \text{signals}} \frac{w_s}{k + \text{rank}_s + 1}$$

Pages are sorted by descending score. Pages that appear in only one signal still receive a score from that signal alone.

### Why RRF?

- **Simple and effective.** RRF doesn't require score calibration between heterogeneous signals (BM25 scores, cosine similarity, exact-match boolean). It only uses rank positions.
- **Robust.** Performs well across query types without the fragility of learned-to-rank models.
- **Tunable.** Per-signal weights let us emphasize different signals for different query types (see [Query-Type Detection](#query-type-detection)).

### Title-match signal

The 4th signal directly rewards pages whose title contains the query as a substring. Results are sorted with exact matches first, then by title length (shorter = more specific). This captures the common case where users search for a specific function block or hardware module by name.

### Breadcrumb-match signal

The 5th signal rewards pages whose breadcrumb path (e.g., "Motion control > ACP10/ARNC0 > General information > Revision Information") contains query terms. Pages are ranked by how many distinct query terms appear in their breadcrumb — more matching terms yield a better rank. This helps surface pages with generic titles (like "Revision Information") that live under highly relevant sections (like "ACP10/ARNC0").

### FTS over-fetching

In both hybrid and FTS-only modes, the search engine fetches `limit × 3` candidates (up to 100) from the underlying search, then applies title-match and breadcrumb-match bonuses to rerank. This ensures BM25's document-length normalization (which penalizes long documents) doesn't permanently exclude relevant pages from the results.

---

## Query-Type Detection

The server classifies each query as either a **natural-language query** or a **technical identifier** and adjusts RRF weights accordingly.

### Detection heuristic

A query is classified as an identifier if:
1. It consists of 1–2 words, AND
2. Each word matches the pattern `^[A-Za-z_][A-Za-z0-9_.]*$`

Examples:
- `MC_MoveAbsolute` → identifier (PascalCase with underscores)
- `X20DI9371` → identifier (product code)
- `mapp.Motion` → identifier (dotted name)
- `how to home an axis` → natural language

### Weight shift effect

| Signal | NL weight | ID weight | Rationale |
|--------|-----------|-----------|-----------|
| Title vector | 2.0 | 0.5 | Semantic similarity helps NL queries; identifiers match better via keywords |
| Content vector | 1.0 | 0.5 | Same reasoning — vectors help with meaning, not exact names |
| FTS keyword | 1.5 | 3.0 | Lance BM25 excels at matching exact identifiers and product codes |
| Title match | 3.0 | 4.0 | Highest signal for both — direct title match is the strongest quality signal |
| Breadcrumb match | 2.0 | 3.0 | Helps surface pages under relevant sections even with generic titles |

For identifier queries, the FTS, title-match, and breadcrumb signals dominate (combined weight 10.0 vs. 1.0 for vectors). For natural-language queries, vectors get meaningful weight (3.0) alongside FTS (1.5), title match (3.0), and breadcrumb match (2.0).

---

## Progressive Availability

The server uses two `threading.Event` objects to provide progressive search availability:

| Event | Meaning | When set |
|-------|---------|----------|
| `_fts_ready` | Keyword (FTS) search is available | After Phase 1 FTS index is built |
| `_ready` | Full hybrid search is available | After Phase 2 vectors are written |

### MCP tool behavior during build

- **Before `_fts_ready`:** `search_help` returns a "search not yet available" message
- **After `_fts_ready`, before `_ready`:** `search_help` uses keyword-only mode (returns `search_mode: "keyword"`)
- **After `_ready`:** `search_help` uses hybrid RRF mode (returns `search_mode: "hybrid"`)
- **`get_page_by_id`** works as soon as the indexer has parsed the XML (within seconds)

The `get_help_statistics` tool exposes the current build state, phase, and progress so LLMs can inform the user about search capability status.

### Temporary suspension

During the Phase 2 → table overwrite step, `_fts_ready` is briefly cleared while the LanceDB table is being replaced and the FTS index is being rebuilt. This ensures no queries hit an inconsistent state.

---

## Index Rebuild Logic

A three-tier strategy minimizes unnecessary work:

### 1. No change (most starts)

**Condition:** XML hash matches stored metadata AND embedding mode/model unchanged  
**Action:** Load existing LanceDB table (<3 seconds)

### 2. Incremental update

**Condition:** XML hash changed (e.g., Automation Studio service pack), per-page fingerprints exist in metadata  
**Action:**
1. Diff page fingerprints: `hash(title | file_path | parent_id | help_id | is_section)`
2. Delete removed/changed rows from LanceDB
3. Extract, embed (if enabled), and insert new/changed rows
4. Rebuild FTS index
5. Falls back to full rebuild if >50% of pages changed

### 3. Full rebuild

**Condition:** First run, embedding mode/model changed, no fingerprints in metadata, or >50% incremental change  
**Action:** Full text extraction + embedding + table creation

### Mode switching

Changing between FTS-only and hybrid mode (or changing the embedding model) always triggers a full rebuild because the PyArrow schema differs (9 columns vs. 11 columns with vector fields).

---

## Configuration Reference

### Required

| Variable | Example | Description |
|----------|---------|-------------|
| `AS_HELP_ROOT` | `C:\BRAutomation\AS6\Help-en\Data` | Path to `Data` folder containing `brhelpcontent.xml` |

### Optional — Embedding

| Variable | Default | Description |
|----------|---------|-------------|
| `CREATE_EMBEDDINGS` | `false` | Master switch: `true` enables API-based embeddings + hybrid search |
| `EMBEDDING_API_ENDPOINT` | — | Base URL of OpenAI-compatible API (e.g., `http://localhost:11434`) |
| `EMBEDDING_API_KEY` | — | API key or token |
| `EMBEDDING_MODEL` | — | Model name (e.g., `nomic-embed-text`) |
| `EMBEDDING_DIMENSIONS` | — | Vector dimensions (e.g., `768`) |
| `EMBEDDING_BATCH_SIZE` | `50` | Texts per API call. Keep at 50 for Ollama; increase for cloud APIs |
| `EMBEDDING_MAX_CHARS` | `4000` | Truncate input texts to this many characters before embedding |

### Tuning guidance

- **`EMBEDDING_BATCH_SIZE`**: Ollama handles 50 well. Cloud APIs (OpenAI, Azure) can handle 100+. Increase only if you have confirmed the API can handle larger batches without timeouts.
- **`EMBEDDING_MAX_CHARS`**: The code default is 8,000, but 4,000 is recommended for Ollama/nomic-embed-text to avoid token-limit errors. Cloud APIs with larger context windows can use higher values. If you see `EmbeddingTooLargeError` warnings, reduce this value.

---

## Alternatives Considered

### Semantic chunking (rejected)

Splitting pages into overlapping chunks (e.g., 512-token windows) would increase the number of documents by 5–10×, complicating retrieval and requiring reassembly at query time. B&R pages are already semantically coherent — they don't benefit from sub-page splitting.

### GraphRAG (rejected)

GraphRAG builds a knowledge graph from unstructured text and uses graph traversal for retrieval. B&R documentation **already has a graph structure** — the XML hierarchy provides parent-child relationships, sections, categories, and HelpIDs. Building another graph on top would add complexity without meaningful improvement. The existing breadcrumb/category structure provides the same navigational benefit.

### LlamaParse / advanced HTML parsing (rejected)

LlamaParse and similar tools excel at parsing complex, unstructured documents (PDFs with tables, scanned images). B&R's HTML help pages are **well-structured** with standard HTML elements. lxml's `text_content()` extracts clean text reliably and is 2–3× faster than BeautifulSoup. The marginal improvement from a more sophisticated parser doesn't justify the added dependency.

### Local embedding models (rejected)

Running embedding models locally (e.g., via `sentence-transformers`) would add heavyweight dependencies (PyTorch, ~2GB+). The API-based approach keeps the server lightweight while supporting any model. Ollama fills the "local" use case without bundling ML frameworks.

### Learned-to-rank / cross-encoder reranking (deferred)

A cross-encoder reranker (e.g., `ms-marco-MiniLM`) could improve top-k precision by scoring query-document pairs directly. This is a potential future enhancement but adds latency (~50–100ms per query) and another model dependency. RRF provides good-enough ranking for the current use case.
