# AS Help MCP Server

MCP server for B&R Automation Studio help documentation search. Provides keyword search by default using LanceDB's native full-text search (FTS), and optional hybrid semantic + keyword search using Reciprocal Rank Fusion (RRF) when an embedding API is configured.

## Features

- **Keyword search** (default): Fast full-text search using LanceDB's native FTS — no external dependencies
- **Hybrid search** (optional): RRF fusion of vector similarity and keyword matching when embeddings are enabled
- **API-based embeddings**: Works with any OpenAI-compatible endpoint (Ollama, OpenAI, Azure OpenAI, GitHub Models, LiteLLM) — no local ML models required
- **Smart ranking**: Query-type detection shifts weights between FTS and vectors (identifiers like `MC_MoveAbsolute` favor exact match; natural language favors semantic similarity)
- Category filtering and hierarchical browsing
- Auto-generated links to B&R online help (AS4/AS6)
- HelpID lookup for context-sensitive help integration
- Incremental reindexing — only changed pages are re-processed

## Prerequisites

- B&R Automation Studio installed (with help documentation)
- VS Code with GitHub Copilot extension
- **For standalone binary:** Download `as-help-server.exe` from [Releases](../../releases) — no build tools required
- **For building from source:** [Rust 1.85+](https://www.rust-lang.org/tools/install)
- **Optional** (for hybrid search): An OpenAI-compatible embedding API (e.g., [Ollama](https://ollama.com/) with `nomic-embed-text`)

## Quick Start (VS Code)

Add to `.vscode/mcp.json` in your workspace:

### Option 1: Standalone Binary (Recommended)

Download the `.exe` from [Releases](../../releases) and place it in `%APPDATA%\as-help-mcp\`.

```json
{
  "servers": {
    "as-help": {
      "command": "${env:APPDATA}\\as-help-mcp\\as-help-server.exe",
      "args": [
        "--help-root",
        "C:\\Program Files (x86)\\BRAutomation\\AS6\\Help-en\\Data",
        "--db-path",
        "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_lance",
        "--metadata-dir",
        "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_metadata",
        "--as-version",
        "6"
      ]
    }
  }
}
```

Update `--help-root` to match your AS installation:
- **AS 4.x:** `C:\\BRAutomation\\AS412\\Help-en\\Data`
- **AS 6.x:** `C:\\Program Files (x86)\\BRAutomation\\AS6\\Help-en\\Data`

### Option 2: Build from Source

```bash
git clone <repository-url>
cd as-help-mcp
cargo build --release
```

The binary is at `target/release/as-help-server.exe`.

```json
{
  "servers": {
    "as-help": {
      "command": "C:\\path\\to\\as-help-mcp\\target\\release\\as-help-server.exe",
      "args": [
        "--help-root",
        "C:\\Program Files (x86)\\BRAutomation\\AS6\\Help-en\\Data",
        "--db-path",
        "C:\\path\\to\\data\\.ashelp_lance",
        "--metadata-dir",
        "C:\\path\\to\\data\\.ashelp_metadata",
        "--as-version",
        "6"
      ]
    }
  }
}
```

---

Restart VS Code, then test in Copilot Chat: *"Search AS help for mapp Motion"*

**First run takes 2-3 minutes** to build the keyword search index. Subsequent starts are instant (~3s).

---

## Enabling Hybrid Search (Optional)

By default, the server uses keyword-only search (FTS). To enable hybrid semantic + keyword search, configure an OpenAI-compatible embedding API.

### Example: Ollama (Local, Free)

1. Install [Ollama](https://ollama.com/) and pull an embedding model:

```bash
ollama pull nomic-embed-text
```

2. Add `--create-embeddings true` and embedding environment variables to your MCP config:

```json
{
  "servers": {
    "as-help": {
      "command": "${env:APPDATA}\\as-help-mcp\\as-help-server.exe",
      "args": [
        "--help-root", "C:\\Program Files (x86)\\BRAutomation\\AS6\\Help-en\\Data",
        "--db-path", "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_lance",
        "--metadata-dir", "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_metadata",
        "--as-version", "6",
        "--create-embeddings", "true"
      ],
      "env": {
        "EMBEDDING_API_ENDPOINT": "http://localhost:11434",
        "EMBEDDING_API_KEY": "ollama",
        "EMBEDDING_MODEL": "nomic-embed-text",
        "EMBEDDING_DIMENSIONS": "768",
        "EMBEDDING_BATCH_SIZE": "100",
        "EMBEDDING_MAX_CHARS": "4000"
      }
    }
  }
}
```

Any OpenAI-compatible endpoint works — OpenAI, Azure OpenAI, GitHub Models, LiteLLM, etc.

### How Hybrid Search Works

When embeddings are enabled, the server uses **Reciprocal Rank Fusion (RRF)** to combine four search signals:

| Signal | NL Weight | ID Weight | Description |
|--------|-----------|-----------|-------------|
| Title vector | 2.0 | 0.5 | Semantic similarity between query and title+breadcrumb embeddings |
| Content vector | 1.0 | 0.5 | Semantic similarity between query and breadcrumb+content embeddings |
| FTS keyword | 1.5 | 3.0 | Lance native full-text search on title+breadcrumb+content |
| Title match | 3.0 | 4.0 | Exact/substring match of query in page titles |

**Query-type detection** automatically selects weights: identifier queries (e.g., `MC_MoveAbsolute`, `X20DI9371`) shift toward FTS + title match; natural language queries favor vector similarity.

For a deep dive into the RAG architecture, see **[RAG.md](RAG.md)**.

---

## CLI Arguments

Run `as-help-server --help` for full details.

| Argument | Env Var Equivalent | Description |
|----------|--------------------|-------------|
| `--help-root` | `AS_HELP_ROOT` | Path to AS Help Data folder |
| `--db-path` | `AS_HELP_DB_PATH` | Path to the LanceDB directory |
| `--metadata-dir` | `AS_HELP_METADATA_DIR` | Path to the indexing metadata directory |
| `--as-version` | `AS_HELP_VERSION` | AS version for online help (`4` or `6`) |
| `--force-rebuild` | `AS_HELP_FORCE_REBUILD` | Force a full index rebuild |
| `--create-embeddings` | `CREATE_EMBEDDINGS` | Enable API-based embeddings for hybrid search |

### Embedding Configuration (Environment Variables)

These are only needed when `--create-embeddings true` is set:

| Variable | Default | Description |
|----------|---------|-------------|
| `EMBEDDING_API_ENDPOINT` | *(required)* | Base URL of OpenAI-compatible API |
| `EMBEDDING_API_KEY` | *(required)* | API key / bearer token |
| `EMBEDDING_MODEL` | *(required)* | Model name (e.g., `nomic-embed-text`, `text-embedding-3-small`) |
| `EMBEDDING_DIMENSIONS` | *(required)* | Vector dimensions (e.g., `768`, `1536`) |
| `EMBEDDING_BATCH_SIZE` | `100` | Texts per API call |
| `EMBEDDING_MAX_CHARS` | `8000` | Truncate input texts to this length |

---

## Development

### Building

```bash
cargo build          # Debug build
cargo build --release  # Optimized release build
```

### Testing

```bash
cargo test
```

### Project Structure

```
src/
  main.rs           # Entry point, transport setup (stdio + StreamableHTTP)
  server.rs         # FastMCP server, tool/prompt handlers
  indexer.rs        # XML parsing, HTML text extraction, breadcrumbs
  search_engine.rs  # LanceDB FTS + hybrid search with RRF
  embeddings.rs     # Optional API-based embedding service
  config.rs         # CLI args + env var configuration
  models.rs         # Shared data types
```

---

## Performance

| Operation | Time | Notes |
|-----------|------|-------|
| XML parse | ~2s | 58K+ pages in-memory |
| First index build (FTS-only) | ~2-3 min | Parallel HTML extraction + FTS indexing |
| First index build (hybrid) | 15-20 min | + embedding via API |
| Subsequent startup | ~3s | Load existing index |
| Search query | 10-50ms | RRF hybrid or FTS keyword |

---

## Tools

| Tool | Description |
|------|-------------|
| `search_help` | Hybrid semantic + keyword search with RRF ranking and optional category filter |
| `get_categories` | List top-level categories for filtering |
| `browse_section` | Navigate help tree hierarchically |
| `get_page_by_id` | Get full page content |
| `get_page_by_help_id` | Retrieve page by numeric HelpID |
| `get_breadcrumb` | Get navigation path |
| `get_help_statistics` | Get content and index build statistics |

## Prompts

| Prompt | Description |
|--------|-------------|
| `help_search` | Structured search with page IDs, breadcrumbs, and HelpIDs |
| `help_details` | Deep research with content synthesis from multiple pages |

---

## Multiple AS Versions

```json
{
  "servers": {
    "as-help-4": {
      "command": "${env:APPDATA}\\as-help-mcp\\as-help-server.exe",
      "args": [
        "--help-root", "C:\\BRAutomation\\AS412\\Help-en\\Data",
        "--db-path", "${env:APPDATA}\\as-help-mcp\\data\\as4\\.ashelp_lance",
        "--metadata-dir", "${env:APPDATA}\\as-help-mcp\\data\\as4\\.ashelp_metadata",
        "--as-version", "4"
      ]
    },
    "as-help-6": {
      "command": "${env:APPDATA}\\as-help-mcp\\as-help-server.exe",
      "args": [
        "--help-root", "C:\\Program Files (x86)\\BRAutomation\\AS6\\Help-en\\Data",
        "--db-path", "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_lance",
        "--metadata-dir", "${env:APPDATA}\\as-help-mcp\\data\\as6\\.ashelp_metadata",
        "--as-version", "6"
      ]
    }
  }
}
```

## License

MIT
