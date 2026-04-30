# Plan: Rust Rewrite of B&R Help MCP Server

Rewrite the Python MCP server as a single Rust binary using `rmcp` (official MCP SDK v1.5), `lancedb` (Rust crate for FTS + vectors), `quick-xml` for XML parsing, and `scraper` for HTML text extraction. Full feature parity including optional hybrid search. Phased approach — each phase is independently verifiable.

---

## Technology Stack

| Layer | Python (current) | Rust (target) |
|-------|-------------------|---------------|
| MCP SDK | FastMCP (`mcp[cli]`) | `rmcp` 1.5+ (`transport-io` + `transport-streamable-http-server`) |
| Search/DB | `lancedb` 0.29 (Python) | `lancedb` 0.27+ (Rust crate — same underlying Lance engine) |
| Arrow | `pyarrow` | `arrow-rs` (`arrow-array`, `arrow-schema`) |
| XML parser | `defusedxml` | `quick-xml` (SAX-style streaming, fast, safe) |
| HTML parser | `lxml` | `scraper` (html5ever-based) |
| HTTP client | `httpx` | `reqwest` (async, tokio-native) |
| Async runtime | asyncio + ThreadPoolExecutor | `tokio` (multi-threaded) |
| Serialization | Pydantic | `serde` + `schemars` |
| Env/config | `python-dotenv` | `dotenvy` |
| CLI args | `argparse` | `clap` |
| Logging | Python `logging` | `tracing` + `tracing-subscriber` (to stderr) |

---

## Project Structure (Single Crate)

```
as-help-mcp-rs/
├── Cargo.toml
├── .env.example
├── src/
│   ├── main.rs           # Entry point, CLI, transport selection
│   ├── server.rs         # MCP tool definitions via #[tool_router], AppState
│   ├── indexer.rs        # XML parsing, HelpPage, breadcrumbs, HTML extraction
│   ├── search_engine.rs  # LanceDB FTS + hybrid search, RRF fusion
│   ├── embeddings.rs     # Optional HTTP-based embedding client (reqwest)
│   ├── models.rs         # Shared data types (HelpPage, SearchResult, etc.)
│   └── config.rs         # Environment/CLI configuration (clap + dotenvy)
└── tests/
    ├── common/mod.rs     # Fixtures (sample XML, HTML, mock embedder)
    ├── test_indexer.rs
    ├── test_search.rs
    ├── test_embeddings.rs
    └── test_server.rs
```

---

## Phases

### Phase 1: Scaffolding & Core Types

1. `cargo init`, set up `Cargo.toml` with all dependencies
2. Define `config.rs` — all env vars (`AS_HELP_ROOT`, `CREATE_EMBEDDINGS`, etc.) + CLI via `clap`
3. Define `models.rs` — `HelpPage`, `SearchResult`, `PageContent`, `BreadcrumbItem`, `CategoryInfo`, `SectionChild` (all `Serialize` + `JsonSchema`)
4. Set up `tracing` logging to stderr

### Phase 2: Indexer (*first critical path*)

1. Implement `HelpContentIndexer` struct with `HashMap<String, HelpPage>`, `help_id_map`, `breadcrumb_cache`
2. SAX-style XML parsing via `quick-xml::Reader` with explicit stack (not recursion) — handle both `Section`/`S`, `Page`/`P`, `Text`/`t`, `File`/`p`, `Identifiers`/`I`, `HelpID`/`H`, `Value`/`v`
3. Duplicate ID handling with synthetic `"{id}__dup_{n}"`
4. Breadcrumb pre-computation (cycle detection, depth limit 100)
5. HTML text extraction via `scraper` — remove script/style, collect text, normalize whitespace
6. Page fingerprints (MD5), metadata sidecar load/save
7. Navigation: `get_top_level_categories()`, `get_section_children()`

### Phase 3: Search Engine (*depends on Phase 2*)

1. `HelpSearchEngine` struct with `lancedb::Connection`, `Arc<RwLock<BuildStatus>>`, `tokio::sync::watch` for readiness
2. Arrow schemas (FTS 9-col, Hybrid 11-col) via `arrow-schema`
3. Build strategy detection (Full/Incremental/Resume/None)
4. `initialize()` — background build via `tokio::task::spawn_blocking` + `rayon` for parallel text extraction
5. FTS index creation: `FtsIndexBuilder::default().stem(true).remove_stop_words(true).ascii_folding(true).language("English")`
6. Two-phase hybrid build (Phase 1: FTS with zero vectors → Phase 2: embed + staging table swap)
7. `search()` — 5-leg RRF fusion, query classification, sanitization, snippet generation
8. File-based lock management (instance + build locks)

### Phase 4: Embedding Client (*parallel with Phase 3*)

1. `EmbeddingService` struct with `reqwest::Client`
2. `embed_text()`, `embed_batch()` with `tokio::task::JoinSet` concurrency
3. Retry on 429/5xx (exponential backoff, max 3 attempts)
4. Binary-split fallback on context overflow
5. Endpoint URL construction (`/v1/embeddings` appending logic)

### Phase 5: MCP Server Layer (*depends on Phase 3 + 4*)

1. `AsHelpServer` struct (Clone, holds `Arc`s) with `#[tool_router(server_handler)]`
2. Define param structs per tool (with `schemars::JsonSchema` descriptions)
3. Implement all 7 tools: `search_help`, `get_page_by_id`, `get_page_by_help_id`, `get_breadcrumb`, `get_categories`, `browse_section`, `get_help_statistics`
4. `ServerHandler` impl with `get_info()` (capabilities, instructions text)
5. `main()`: clap args → dotenvy → init indexer → init search engine (background) → select transport (stdio vs streamable-http) → serve
6. Graceful shutdown on ctrl-c

### Phase 6: Testing (*incrementally during each phase*)

1. Test fixtures: sample XML (full + abbreviated), sample HTML files, mock embedding service
2. Unit tests per module (same scenarios as Python: duplicate IDs, breadcrumb cycles, query sanitization, RRF scoring)
3. Integration test: parse XML → build index → search → retrieve page
4. CI: `cargo test` + `cargo clippy -- -D warnings` + `cargo fmt --check`

### Phase 7: Build & Distribution (*after Phase 5*)

1. Static linking config for `x86_64-pc-windows-msvc` (primary), `x86_64-unknown-linux-musl`
2. GitHub Actions release workflow (multi-platform matrix)
3. Update `mcp.json` to reference binary path
4. Migration guide (env vars unchanged, swap binary for `uv run as-help-server`)

---

## Relevant Files (reference for implementation)

| Python source | What to port | Key functions |
|---|---|---|
| `src/server.py` | `server.rs` + `main.rs` | 7 tools, `app_lifespan()`, `_build_online_help_url()` |
| `src/indexer.py` | `indexer.rs` | `parse_xml_structure()`, `_process_section()`, `_extract_plain_text_no_cache()`, `get_breadcrumb_string()` |
| `src/search_engine.py` | `search_engine.rs` | `initialize()`, `search()`, RRF fusion, `_sanitize_query()`, `_is_identifier_query()` |
| `src/embeddings.py` | `embeddings.rs` | `embed_batch()`, `_call_api()`, retry + binary-split |
| `tests/conftest.py` | `tests/common/mod.rs` | `MockEmbeddingService`, sample XML/HTML fixtures |

---

## Key Design Decisions

### RRF Fusion (implement manually)

Implement RRF manually in Rust (query each leg separately, fuse scores) rather than relying on `execute_hybrid()`. This gives more control and matches the current Python logic exactly.

**5 retrieval legs:**
1. **Title vector search** (weight 2.0 NL / 0.5 identifier) — semantic similarity on title+breadcrumb vectors
2. **Content vector search** (weight 1.0 NL / 0.5 identifier) — semantic similarity on breadcrumb+content vectors
3. **FTS keyword search** (weight 1.5 NL / 3.0 identifier) — Lance native BM25 on search_text
4. **Title match bonus** (weight 3.0 NL / 4.0 identifier) — exact/substring match in titles
5. **Breadcrumb retrieval** (weight 2.0 NL / 3.0 identifier) — AND filter on breadcrumb_path

**RRF formula:** `score += weight / (k=60 + rank + 1)`

### Concurrency Model

- Background index build via `tokio::task::spawn_blocking` (CPU/IO-bound LanceDB ops)
- Parallel text extraction via `rayon` threadpool inside `spawn_blocking`
- `Arc<RwLock<BuildStatus>>` for progress reporting
- `tokio::sync::watch` channel to signal `fts_ready` / `ready` states
- Embedding batch concurrency via `tokio::task::JoinSet` (max_workers)

### State Sharing in rmcp

```rust
#[derive(Clone)]
struct AsHelpServer {
    indexer: Arc<HelpContentIndexer>,
    search_engine: Arc<HelpSearchEngine>,
    as_version: String,
    online_help_base_url: String,
}
```

All `Arc`s — cheap to clone for rmcp's handler model. Search engine readiness checked via `watch` channel before returning results.

### Index Compatibility

LanceDB index format is shared between Python and Rust SDKs (same Lance format). An index built by Python is readable by Rust, enabling migration without full rebuild.

### Env Vars (unchanged)

All environment variables stay identical — drop-in binary replacement:
- `AS_HELP_ROOT`, `AS_HELP_VERSION`, `AS_HELP_DB_PATH`, `AS_HELP_METADATA_DIR`
- `AS_HELP_FORCE_REBUILD`, `CREATE_EMBEDDINGS`
- `EMBEDDING_API_ENDPOINT`, `EMBEDDING_API_KEY`, `EMBEDDING_MODEL`, `EMBEDDING_DIMENSIONS`
- `MCP_TRANSPORT`, `MCP_HOST`, `MCP_PORT`

### Encoding Fallback

Use `encoding_rs` crate for HTML file reading — handles Windows-1252 fallback (some B&R HTML files aren't UTF-8).

---

## Verification Criteria

| Check | Target |
|---|---|
| `cargo test` | All unit + integration tests pass |
| Parse real XML (107k pages) | Complete in <3s |
| FTS index build | Complete, results match Python |
| Binary size | <20MB stripped |
| Startup (existing index) | <3s |
| Search latency | <50ms per query |
| MCP compliance | Tools discoverable from VS Code Copilot |
| Windows paths | Works with `C:\BRAutomation\AS412\Help-en\Data` |
| `cargo clippy -- -D warnings` | Clean |
| Cross-platform build | Windows (MSVC) + Linux (musl) |

---

## Scope Exclusions

- Docker image (add after binary works)
- PyInstaller build scripts (replaced by cargo build)
- Python source files (kept for reference, not modified)
