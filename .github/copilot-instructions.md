# B&R Help MCP Server - Copilot Instructions

## Project Overview

This is a **Model Context Protocol (MCP) server** that provides keyword and optional semantic search + retrieval for B&R Automation Studio help documentation. Built with Rust, rmcp SDK, LanceDB for FTS + optional vector storage, and reqwest for optional API-based embeddings.

**Key Architecture Decision:** LanceDB provides full-text search (FTS) by default with no external dependencies. When `--create-embeddings true` is set, the server calls an OpenAI-compatible embedding API to create vectors and enables hybrid search (RRF = Reciprocal Rank Fusion). No local ML models — embeddings are always API-based and optional.

## Core Architecture

### Module Design

1. **`indexer.rs`** - XML Parser & HTML Extractor
   - Parses `brhelpcontent.xml` (abbreviated tags: `S`=Section, `P`=Page, `t`=Text, `p`=File, `I`=Identifiers)
   - Builds in-memory page tree with parent-child relationships
   - Extracts breadcrumbs with **cycle detection** and **depth limit (100)**
   - Uses **scraper** crate for HTML text extraction
   - Uses MD5 hash for change detection (stored in `_index_metadata.json` sidecar)
   - Two-pass parsing: structure first, then HelpID extraction

2. **`embeddings.rs`** - Optional API-Based Embedding Service
   - Only used when `--create-embeddings true`
   - Calls any **OpenAI-compatible** embedding API (OpenAI, Azure OpenAI, GitHub Models, Ollama, LiteLLM)
   - Configured via env vars: `EMBEDDING_API_ENDPOINT`, `EMBEDDING_API_KEY`, `EMBEDDING_MODEL`, `EMBEDDING_DIMENSIONS`
   - Batch embedding with configurable size and concurrent workers
   - Automatic retry on 429/5xx with exponential backoff
   - Uses **reqwest** (async HTTP client)

3. **`search_engine.rs`** - LanceDB Dual-Mode Search Engine
   - **FTS-only mode** (default):
     - Lance native full-text keyword search (BM25 ranking)
     - Tokenizer: stemming, stop-word removal, ASCII folding enabled
     - No vector columns — minimal storage overhead
   - **Hybrid mode** (`CREATE_EMBEDDINGS=true`):
     - Multiple search legs fused via RRF (k=60):
       - Title vector similarity (weight 2x)
       - Content vector similarity (weight 1x)
       - FTS keyword search (weight 1.5x)
       - Title match (weight 3x)
       - Breadcrumb match (weight 2x)
     - Query-type detection shifts weights for identifiers vs natural language
   - LanceDB directory-based storage (`.ashelp_lance/`)
   - **Query sanitization** for FTS special characters
   - Parallel text extraction using rayon/tokio spawn_blocking
   - Metadata sidecar (`_index_metadata.json`) tracks XML hash, `embeddings_enabled`, and model info

4. **`server.rs`** - rmcp MCP Server
   - Exposes tools: `search_help`, `get_page_by_id`, `get_page_by_help_id`, `get_breadcrumb`, `get_categories`, `browse_section`, `get_help_statistics`
   - **Intentionally truncated previews** (~100 chars) to force LLM to call `get_page_by_id`
   - Server instructions guide LLM to make **multiple searches and page retrievals**
   - Uses `#[tool_router]` for tool dispatch and `#[tool_handler]` for ServerHandler
   - Prompt support via `help_search` and `help_details`

5. **`config.rs`** - CLI + Environment Configuration
   - Uses **clap** for CLI argument parsing
   - Merges CLI args with env vars (CLI takes precedence)
   - Loads `.env` files via dotenvy

6. **`main.rs`** - Entry Point & Transport
   - Stdio transport for MCP client communication
   - StreamableHTTP transport support via axum

### Data Flow

```
brhelpcontent.xml → Indexer → Page Tree (in-memory)
                        ↓
                  HTML Files → scraper → Plain Text
                        ↓
             [Optional: Embedding API → Vectors]
                        ↓
                  LanceDB → Table + FTS Index [+ Vectors] → Search → MCP Tools
```

### Search Ranking (Hybrid Mode)

RRF formula: `score = Σ weight / (k + rank + 1)` where `k=60`

| Signal | NL Weight | ID Weight | Description |
|--------|-----------|-----------|-------------|
| Title vector | 2.0 | 0.5 | Query ↔ title+breadcrumb embedding similarity |
| Content vector | 1.0 | 0.5 | Query ↔ content embedding similarity |
| FTS keyword | 1.5 | 3.0 | BM25 on title+breadcrumb+content |
| Title match | 3.0 | 4.0 | Exact/substring match in titles |
| Breadcrumb match | 2.0 | 3.0 | Query terms in breadcrumb path |

## Development Workflows

### Building

```bash
cargo build          # Debug build
cargo build --release  # Optimized release (LTO, stripped)
```

### Testing

```bash
cargo test           # Run all tests
```

### Running

```bash
# With CLI arguments
cargo run -- --help-root "C:\BRAutomation\AS412\Help-en\Data" --as-version 4

# With .env file
cp .env.example .env
# Edit .env with your paths
cargo run
```

## Critical Conventions

### Environment Variables (Required)

- `AS_HELP_ROOT` - Path to `Data` folder containing `brhelpcontent.xml`
- `AS_HELP_DB_PATH` - Path to LanceDB directory (defaults to `.ashelp_lance` in help root)
- `AS_HELP_METADATA_DIR` - Path to metadata directory (defaults to `.ashelp_metadata` in help root)
- `AS_HELP_VERSION` - AS version: `4` or `6` (for online help URL generation)
- `AS_HELP_FORCE_REBUILD` - Set `true` to force full rebuild

### Embedding Variables (Optional)

- `CREATE_EMBEDDINGS` - Master switch: `true` enables API-based embeddings
- `EMBEDDING_API_ENDPOINT` - Base URL of OpenAI-compatible embedding API
- `EMBEDDING_API_KEY` - API key
- `EMBEDDING_MODEL` - Model name (e.g., `text-embedding-3-small`)
- `EMBEDDING_DIMENSIONS` - Vector dimensions (e.g., `1536`, `768`)
- `EMBEDDING_BATCH_SIZE` - Texts per API call (default: 100)
- `EMBEDDING_MAX_CHARS` - Text truncation limit (default: 8000)

### Abbreviated XML Tags (Critical!)

The B&R XML uses shortened tags — **both formats must be handled**:
- `Section` or `S` (with `Text`/`t`, `File`/`p`)
- `Page` or `P` (with `Text`/`t`, `File`/`p`)
- `Identifiers` or `I` → `HelpID` or `H` (with `Value`/`v`)

### Index Rebuild Logic

1. **No change** (most starts): XML hash matches → load existing index (<3s)
2. **Content changed**: XML hash differs → incremental or full rebuild
3. **Mode switch**: FTS↔hybrid or model changed → full rebuild

## Key Dependencies

- `rmcp` - MCP server SDK (tools, prompts, transport)
- `lancedb` - Vector + FTS database
- `arrow-array` / `arrow-schema` - Columnar data for LanceDB
- `quick-xml` - SAX-style XML parsing
- `scraper` - HTML parsing and text extraction
- `reqwest` - HTTP client for embedding API
- `tokio` - Async runtime
- `rayon` - Parallel text extraction
- `clap` - CLI argument parsing
- `serde` / `serde_json` - Serialization
- `tracing` - Structured logging

## File Structure

```
src/
  main.rs           # Entry point, transport setup
  server.rs         # MCP server, tool/prompt handlers
  indexer.rs        # XML parsing, HTML extraction, breadcrumbs
  search_engine.rs  # LanceDB FTS + hybrid search with RRF
  embeddings.rs     # Optional API-based embedding service
  config.rs         # CLI args + env var configuration
  models.rs         # Shared data types

Root:
  Cargo.toml        # Rust dependencies and build config
  .env.example      # Template for environment vars
  RAG.md            # RAG architecture deep-dive
```

## When Modifying Code

- **Adding tools**: Add method to the `#[tool_router]` impl block in `server.rs`
- **Changing XML parsing**: Update both tag formats in `indexer.rs` (`Section`/`S`, etc.)
- **Search improvements**: Adjust RRF weights in `search_engine.rs`
- **Embedding model**: Change `EMBEDDING_MODEL` env var — any OpenAI-compatible model works
- **New config options**: Add to `CliArgs` struct in `config.rs` + `AppConfig` resolution
