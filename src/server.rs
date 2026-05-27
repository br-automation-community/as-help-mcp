//! MCP server — exposes tools, prompts, and resources for B&R Help search.
//!
//! Uses the `rmcp` crate with `#[tool_router]` for automatic JSON Schema
//! generation from Serde/Schemars-annotated parameter types.

use std::sync::Arc;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, GetPromptRequestParams, GetPromptResult, ListPromptsResult,
    PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, PromptMessageRole,
    ServerCapabilities, ServerInfo,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, ServerHandler};
use rmcp::{RoleServer, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::config::AppConfig;
use crate::indexer::HelpContentIndexer;
use crate::search_engine::HelpSearchEngine;

// ---------------------------------------------------------------------------
// Server instructions (guides the LLM's usage of our tools)
// ---------------------------------------------------------------------------

const SERVER_INSTRUCTIONS: &str = "\
B&R Automation Studio Help Server - 100k+ pages of technical documentation.\n\n\
CRITICAL: content_preview is ~100 chars. NEVER answer from previews alone. \
You MUST call get_page_by_id to read actual documentation.\n\n\
*** RESEARCH WORKFLOW ***\n\n\
1. search_help — Find pages by keyword or meaning. Returns titles/page_ids only, NO content.\n\
2. get_page_by_id — Get FULL content. Use breadcrumb_path from results to pick relevant pages \
and skip obvious mismatches (e.g., wrong library variant).\n\
3. REPEAT — Search with different keywords or synonyms. Complex questions need 2-5 page retrievals.\n\
4. get_page_by_help_id — Use when you have a numeric HelpID (e.g., from error codes or context help).\n\n\
*** DISCOVERY & BROWSING ***\n\n\
- get_categories — List top-level categories. Call BEFORE using the category filter in search_help.\n\
- browse_section — Navigate into a category/section to see its children. \
Prefer search_help for direct lookups; use browse_section only to explore structure or find siblings.\n\n\
TIPS:\n\
- Use specific identifiers when known (e.g., 'MC_BR_MoveAbsolute', 'X20DI9371')\n\
- Try keyword variations: 'axis move' vs 'motion control' vs 'positioning'\n\
- get_help_statistics — Check index build progress if search returns empty results";

// ---------------------------------------------------------------------------
// Tool parameter structs (Serde + JsonSchema → automatic MCP schemas)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchHelpParams {
    /// Search query — use specific identifiers (e.g., 'MC_BR_MoveAbsolute')
    /// or natural language (e.g., 'how to move an axis').
    query: String,
    /// Max results (default 5). Use smaller limits with multiple searches.
    #[serde(default = "default_limit")]
    limit: usize,
    /// True = search titles + content. False = titles only (faster).
    #[serde(default = "default_true")]
    content_search: bool,
    /// Filter by top-level category. Call get_categories() first.
    #[serde(default)]
    category: Option<String>,
}

fn default_limit() -> usize {
    5
}
fn default_true() -> bool {
    true
}
fn default_false() -> bool {
    false
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetPageByIdParams {
    /// Page ID from search results.
    page_id: String,
    /// Include raw HTML (only for rendering / link extraction).
    #[serde(default = "default_false")]
    include_html: bool,
    /// Include full plain text content.
    #[serde(default = "default_true")]
    include_text: bool,
    /// Include navigation breadcrumb path.
    #[serde(default = "default_true")]
    include_breadcrumb: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetPageByHelpIdParams {
    /// Numeric HelpID (e.g., '3002099'). Found in error messages and context help.
    help_id: String,
    /// Include raw HTML.
    #[serde(default = "default_false")]
    include_html: bool,
    /// Include plain text.
    #[serde(default = "default_true")]
    include_text: bool,
    /// Include breadcrumb.
    #[serde(default = "default_true")]
    include_breadcrumb: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetBreadcrumbParams {
    /// Unique page ID from search results.
    page_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BrowseSectionParams {
    /// Section ID from get_categories or a previous browse_section call.
    section_id: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
struct PromptTopicArg {
    topic: String,
}

// ---------------------------------------------------------------------------
// Server handler
// ---------------------------------------------------------------------------

/// Shared application context held by the MCP handler.
#[derive(Clone)]
pub struct HelpServer {
    indexer: Arc<HelpContentIndexer>,
    search_engine: Arc<HelpSearchEngine>,
    online_help_base_url: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl HelpServer {
    pub fn new(
        indexer: Arc<HelpContentIndexer>,
        search_engine: Arc<HelpSearchEngine>,
        config: &AppConfig,
    ) -> Self {
        Self {
            indexer,
            search_engine,
            online_help_base_url: config.online_help_base_url.clone(),
            tool_router: Self::tool_router(),
        }
    }

    /// Build an online help URL from a relative file path.
    fn build_online_help_url(&self, file_path: &str) -> Option<String> {
        if file_path.is_empty() {
            return None;
        }
        let normalized = file_path.replace('\\', "/");
        let encoded: String = normalized
            .split('/')
            .map(|seg| utf8_percent_encode(seg, NON_ALPHANUMERIC).to_string())
            .collect::<Vec<_>>()
            .join("/");
        Some(format!("{}{}", self.online_help_base_url, encoded))
    }
}

// ---------------------------------------------------------------------------
// Tool implementations via #[tool] macro
// ---------------------------------------------------------------------------

#[tool_router]
impl HelpServer {
    /// Find help pages by keyword or meaning. Returns page_ids and metadata ONLY — no actual content.
    /// IMPORTANT: You MUST call get_page_by_id to read content. Previews (~100 chars) are NOT enough.
    /// Use breadcrumb_path in results to assess relevance before retrieving.
    #[tool(
        name = "search_help",
        description = "Find help pages by keyword or meaning. Returns page_ids and metadata ONLY — no actual content. You MUST call get_page_by_id to read content."
    )]
    async fn search_help(
        &self,
        Parameters(params): Parameters<SearchHelpParams>,
    ) -> Result<CallToolResult, McpError> {
        let status = self.search_engine.build_status().await;

        // Return status message if index not yet queryable
        if status.state != "ready" && status.state != "fts_ready" {
            let msg = if let Some(err) = &status.error {
                format!("Index build failed: {err}")
            } else {
                format!(
                    "Search index is building ({}): {} - {}/{} pages. Call get_help_statistics to check progress.",
                    status.build_type, status.phase, status.pages_processed, status.pages_total
                )
            };
            let result = serde_json::json!({
                "query": params.query,
                "results": [],
                "total": 0,
                "status_message": msg
            });
            return Ok(CallToolResult::success(vec![Content::json(result)?]));
        }

        let limit = if params.limit == 0 { 5 } else { params.limit };
        let results = self
            .search_engine
            .search(
                &params.query,
                limit,
                params.content_search,
                params.category.as_deref(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let search_mode = if self.search_engine._embeddings_enabled && self.search_engine.ready() {
            "hybrid"
        } else {
            "keyword"
        };

        let search_results: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                // Intentionally short preview to force get_page_by_id
                let preview = r.get("snippet").and_then(|v| v.as_str()).map(|s| {
                    let trimmed = s.trim();
                    if trimmed.len() > 100 {
                        let safe_end = crate::search_engine::safe_truncate(trimmed, 100);
                        format!("{safe_end}...")
                    } else {
                        format!("{trimmed}...")
                    }
                });

                // Round score to 3 decimal places to save tokens
                let score = r
                    .get("score")
                    .and_then(|v| v.as_f64())
                    .map(|s| (s * 1000.0).round() / 1000.0);

                // Omit empty help_id
                let help_id = r
                    .get("help_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());

                let mut entry = serde_json::json!({
                    "page_id": r.get("page_id"),
                    "title": r.get("title"),
                    "breadcrumb_path": r.get("breadcrumb_path"),
                    "category": r.get("category"),
                    "is_section": r.get("is_section"),
                    "content_preview": preview,
                });

                // Only include non-null optional fields
                if let Some(s) = score {
                    entry["score"] = serde_json::json!(s);
                }
                if let Some(hid) = help_id {
                    entry["help_id"] = serde_json::json!(hid);
                }

                entry
            })
            .collect();

        let mut result = serde_json::json!({
            "query": params.query,
            "results": search_results,
            "total": search_results.len(),
            "search_mode": search_mode,
        });

        // Only include status_message when present
        if search_mode == "keyword" && self.search_engine._embeddings_enabled {
            result["status_message"] = serde_json::json!(
                "Semantic search is still loading — results are keyword-only. Retry later for better relevance."
            );
        }

        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }

    /// Get all top-level categories from the help documentation.
    /// Use this to discover valid category names for filtering search_help and to browse the structure.
    #[tool(
        name = "get_categories",
        description = "Get all top-level categories. Call BEFORE using the category filter in search_help."
    )]
    async fn get_categories(&self) -> Result<CallToolResult, McpError> {
        let categories = self.indexer.get_top_level_categories();
        let cats: Vec<serde_json::Value> = categories
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "file_path": c.file_path,
                })
            })
            .collect();
        let result = serde_json::json!({
            "categories": cats,
            "total": cats.len(),
        });
        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }

    /// Browse children of a section in the help tree.
    /// Use browse_section to explore structure or find sibling/related pages.
    #[tool(
        name = "browse_section",
        description = "Browse children of a section. Use to explore structure or find siblings. Prefer search_help for direct lookups."
    )]
    async fn browse_section(
        &self,
        Parameters(params): Parameters<BrowseSectionParams>,
    ) -> Result<CallToolResult, McpError> {
        let parent = self.indexer.get_page_by_id(&params.section_id);
        let parent = match parent {
            Some(p) => p,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Section '{}' not found.",
                    params.section_id
                ))]));
            }
        };
        let parent_title = parent.text.clone();

        let children = self.indexer.get_section_children(&params.section_id);
        let child_items: Vec<serde_json::Value> = children
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "title": c.title,
                    "file_path": c.file_path,
                    "is_section": c.is_section,
                })
            })
            .collect();
        let result = serde_json::json!({
            "section_id": params.section_id,
            "section_title": parent_title,
            "children": child_items,
            "total": child_items.len(),
        });
        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }

    /// Get the COMPLETE content of a help page.
    /// Before calling, check the breadcrumb_path from search results to confirm relevance.
    /// For thorough answers, retrieve 2–5 pages.
    #[tool(
        name = "get_page_by_id",
        description = "Get FULL content of a help page. Use breadcrumb_path from search to pick relevant pages. Retrieve 2-5 pages for thorough answers."
    )]
    async fn get_page_by_id(
        &self,
        Parameters(params): Parameters<GetPageByIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let page = match self.indexer.get_page_by_id(&params.page_id) {
            Some(p) => p.clone(),
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Page '{}' not found.",
                    params.page_id
                ))]));
            }
        };

        let html_content = if params.include_html {
            self.indexer.extract_html_content(&params.page_id)
        } else {
            None
        };

        let plain_text = if params.include_text {
            self.indexer.extract_plain_text(&params.page_id)
        } else {
            None
        };

        let breadcrumb: Vec<String> = if params.include_breadcrumb {
            self.indexer
                .get_breadcrumb(&params.page_id)
                .into_iter()
                .map(|p| p.text.clone())
                .collect()
        } else {
            Vec::new()
        };

        let online_url = self.build_online_help_url(&page.file_path);

        let result = serde_json::json!({
            "page_id": page.id,
            "title": page.text,
            "file_path": page.file_path,
            "online_help_url": online_url,
            "help_id": page.help_id,
            "html_content": html_content,
            "plain_text": plain_text,
            "breadcrumb": breadcrumb,
        });
        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }

    /// Retrieve a help page by its numeric HelpID.
    /// Use when you have a HelpID from error codes, context-sensitive help links, or AS project references.
    #[tool(
        name = "get_page_by_help_id",
        description = "Retrieve a help page by its numeric HelpID. Use for error codes and context help."
    )]
    async fn get_page_by_help_id(
        &self,
        Parameters(params): Parameters<GetPageByHelpIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let page = match self.indexer.get_page_by_help_id(&params.help_id) {
            Some(p) => p.clone(),
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "No page found for HelpID '{}'.",
                    params.help_id
                ))]));
            }
        };

        let html_content = if params.include_html {
            self.indexer.extract_html_content(&page.id)
        } else {
            None
        };

        let plain_text = if params.include_text {
            self.indexer.extract_plain_text(&page.id)
        } else {
            None
        };

        let breadcrumb: Vec<String> = if params.include_breadcrumb {
            self.indexer
                .get_breadcrumb(&page.id)
                .into_iter()
                .map(|p| p.text.clone())
                .collect()
        } else {
            Vec::new()
        };

        let online_url = self.build_online_help_url(&page.file_path);

        let result = serde_json::json!({
            "page_id": page.id,
            "title": page.text,
            "file_path": page.file_path,
            "online_help_url": online_url,
            "help_id": page.help_id,
            "html_content": html_content,
            "plain_text": plain_text,
            "breadcrumb": breadcrumb,
        });
        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }

    /// Get detailed navigation breadcrumb for a help page.
    /// WARNING: Rarely needed! Search results already include breadcrumb_path as a string.
    #[tool(
        name = "get_breadcrumb",
        description = "Get navigation breadcrumb for a page. Rarely needed — search results include breadcrumb_path."
    )]
    async fn get_breadcrumb(
        &self,
        Parameters(params): Parameters<GetBreadcrumbParams>,
    ) -> Result<CallToolResult, McpError> {
        let crumbs = self.indexer.get_breadcrumb(&params.page_id);
        let items: Vec<serde_json::Value> = crumbs
            .iter()
            .map(|p| {
                serde_json::json!({
                    "page_id": p.id,
                    "title": p.text,
                    "file_path": p.file_path,
                    "is_section": p.is_section,
                })
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::json(items)?]))
    }

    /// Check index build progress and get content statistics.
    /// Call this when search_help returns empty results to see if the index is still building.
    #[tool(
        name = "get_help_statistics",
        description = "Check index build progress and content statistics. Call when search returns empty results."
    )]
    async fn get_help_statistics(&self) -> Result<CallToolResult, McpError> {
        let total_pages = self.indexer.pages.len();
        let total_sections = self.indexer.pages.values().filter(|p| p.is_section).count();
        let total_help_ids = self.indexer.help_id_map.len();
        let pages_with_parents = self
            .indexer
            .pages
            .values()
            .filter(|p| p.parent_id.is_some())
            .count();
        let root_items = total_pages - pages_with_parents;

        let build_status = self.search_engine.build_status().await;

        let mut result = serde_json::json!({
            "total_pages": total_pages,
            "total_sections": total_sections,
            "regular_pages": total_pages - total_sections,
            "help_id_mappings": total_help_ids,
            "pages_with_parents": pages_with_parents,
            "root_items": root_items,
            "index_status": {
                "state": build_status.state,
                "build_type": build_status.build_type,
                "phase": build_status.phase,
                "pages_total": build_status.pages_total,
                "pages_processed": build_status.pages_processed,
                "elapsed_seconds": build_status.elapsed_seconds,
            },
        });

        if let Some(stats) = &build_status.incremental_stats {
            result["index_status"]["incremental_stats"] = serde_json::json!(stats);
        }
        if let Some(err) = &build_status.error {
            result["index_status"]["error"] = serde_json::json!(err);
        }

        Ok(CallToolResult::success(vec![Content::json(result)?]))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler trait implementation
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for HelpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "B&R Automation Studio Help Server",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    // -- Prompts --

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            meta: None,
            prompts: vec![
                Prompt::new(
                    "help_search",
                    Some(
                        "Search for a topic in B&R help and return comprehensive results with locations and HelpIDs.",
                    ),
                    Some(vec![
                        PromptArgument::new("topic")
                            .with_description("The topic to search for")
                            .with_required(true),
                    ]),
                ),
                Prompt::new(
                    "help_details",
                    Some(
                        "Deep research a topic — retrieves and synthesizes content from multiple pages.",
                    ),
                    Some(vec![
                        PromptArgument::new("topic")
                            .with_description("The topic to research")
                            .with_required(true),
                    ]),
                ),
            ],
            next_cursor: None,
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let topic = request
            .arguments
            .as_ref()
            .and_then(|m| m.get("topic"))
            .and_then(|v| v.as_str())
            .unwrap_or("<topic>")
            .to_string();

        match request.name.as_str() {
            "help_search" => Ok(GetPromptResult::new(vec![
                PromptMessage::new_text(
                    PromptMessageRole::User,
                    format!(
                        "Search the B&R Automation Studio help documentation comprehensively for: \"{topic}\"\n\n\
                         ## Instructions\n\n\
                         1. Use the `search_help` tool to find all relevant pages (use limit=10)\n\
                         2. If the first search doesn't cover all aspects, search with alternative keywords\n\
                         3. Use the `get_page_by_id` tool to retrieve full content for the content summary\n\
                         4. For each relevant result, provide: Page ID, Help Path, Online Help URL, HelpID, Content Summary"
                    ),
                ),
            ]).with_description("Search B&R help documentation")),
            "help_details" => Ok(GetPromptResult::new(vec![
                PromptMessage::new_text(
                    PromptMessageRole::User,
                    format!(
                        "Perform DEEP RESEARCH on the B&R Automation Studio help documentation for: \"{topic}\"\n\n\
                         ## Research Workflow\n\n\
                         1. Use `search_help` with limit=10 to find all relevant pages\n\
                         2. Call `get_page_by_id` for the TOP 3-5 most relevant results\n\
                         3. Search for related terms (error codes, examples, related FBs)\n\
                         4. Call `get_page_by_id` for additional relevant pages\n\
                         5. Synthesize information from ALL retrieved pages\n\n\
                         ## Output: Overview, Key Details, Parameters, Usage Examples, Error Handling"
                    ),
                ),
            ]).with_description("Deep research a B&R help topic")),
            _ => Err(McpError::invalid_params(
                format!("Unknown prompt: {}", request.name),
                None,
            )),
        }
    }
}
