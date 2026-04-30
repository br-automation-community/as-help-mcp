//! Shared data types used across the server.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core domain types (internal)
// ---------------------------------------------------------------------------

/// A single help page or section in the B&R documentation tree.
#[derive(Debug, Clone)]
pub struct HelpPage {
    pub id: String,
    pub text: String,
    pub file_path: String,
    pub help_id: Option<String>,
    pub parent_id: Option<String>,
    pub is_section: bool,
}

// ---------------------------------------------------------------------------
// MCP response models (serialised to the client)
// ---------------------------------------------------------------------------

/// A single search result — metadata only. Call `get_page_by_id` for content.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    /// REQUIRED: Use this with get_page_by_id to get actual page content
    pub page_id: String,
    /// Title of the help page
    pub title: String,
    /// Relative path to the HTML file
    pub file_path: String,
    /// Direct link to B&R online help for this page
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online_help_url: Option<String>,
    /// HelpID if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_id: Option<String>,
    /// Whether this is a section (true) or page (false)
    pub is_section: bool,
    /// Search relevance score (higher is better, RRF fusion)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Navigation path like "Section > Subsection > Page"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb_path: Option<String>,
    /// Top-level category (e.g., "Motion", "Hardware", "Safety")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// First ~100 chars only. NOT enough to answer questions — call get_page_by_id!
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_preview: Option<String>,
}

/// Collection of search results.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResults {
    /// The search query
    pub query: String,
    /// List of matching pages
    pub results: Vec<SearchResult>,
    /// Total number of results returned
    pub total: usize,
    /// "hybrid" (semantic + keyword) or "keyword" (FTS only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_mode: Option<String>,
    /// Status message when index is not ready
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
}

/// Full content of a help page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PageContent {
    /// Unique identifier for the page
    pub page_id: String,
    /// Title of the help page
    pub title: String,
    /// Relative path to the HTML file
    pub file_path: String,
    /// Direct link to B&R online help
    #[serde(skip_serializing_if = "Option::is_none")]
    pub online_help_url: Option<String>,
    /// HelpID if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_id: Option<String>,
    /// Full HTML content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html_content: Option<String>,
    /// Plain text content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plain_text: Option<String>,
    /// Breadcrumb trail (list of titles)
    #[serde(default)]
    pub breadcrumb: Vec<String>,
}

/// A single item in a breadcrumb trail.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BreadcrumbItem {
    /// Unique identifier
    pub page_id: String,
    /// Page/section title
    pub title: String,
    /// Relative path to HTML file
    pub file_path: String,
    /// Whether this is a section
    pub is_section: bool,
}

/// A top-level category in the help documentation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CategoryInfo {
    /// Unique identifier (use with browse_section)
    pub id: String,
    /// Display name of the category
    pub title: String,
    /// Relative path to HTML file
    pub file_path: String,
}

/// List of all top-level categories.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CategoriesResult {
    /// List of top-level categories
    pub categories: Vec<CategoryInfo>,
    /// Total number of categories
    pub total: usize,
}

/// A child item within a section.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SectionChild {
    /// Unique identifier (browse_section for sections, get_page_by_id for pages)
    pub id: String,
    /// Display name
    pub title: String,
    /// Relative path to HTML file
    pub file_path: String,
    /// True if section (browseable), false if page
    pub is_section: bool,
}

/// Children of a section in the help tree.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SectionChildren {
    /// ID of the parent section
    pub section_id: String,
    /// Title of the parent section
    pub section_title: String,
    /// Child sections and pages (sections listed first)
    pub children: Vec<SectionChild>,
    /// Total number of children
    pub total: usize,
}

// ---------------------------------------------------------------------------
// Search engine internal types
// ---------------------------------------------------------------------------

/// Build strategy for the search index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStrategy {
    /// No existing index — full build required.
    Full,
    /// XML changed, page fingerprints available — partial update.
    Incremental,
    /// Interrupted build detected — resume.
    Resume,
    /// Index up-to-date — just load.
    None,
}

/// Current state of the search index build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildState {
    Initializing,
    Building,
    FtsReady,
    Ready,
    Error,
}

impl std::fmt::Display for BuildState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initializing => write!(f, "initializing"),
            Self::Building => write!(f, "building"),
            Self::FtsReady => write!(f, "fts_ready"),
            Self::Ready => write!(f, "ready"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Snapshot of the build progress, returned to MCP clients.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildStatus {
    pub state: String,
    pub build_type: String,
    pub phase: String,
    pub pages_total: usize,
    pub pages_processed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incremental_stats: Option<IncrementalStats>,
    pub embeddings_enabled: bool,
}

/// Statistics from an incremental index update.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IncrementalStats {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub unchanged: usize,
}
