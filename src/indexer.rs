//! XML parser and HTML text extractor for B&R Automation Studio help content.

use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use tracing::{debug, error, info, warn};

use crate::models::HelpPage;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Section/page child returned by navigation helpers.
pub struct SectionChildEntry {
    pub id: String,
    pub title: String,
    pub file_path: String,
    pub is_section: bool,
}

/// Top-level category entry.
pub struct CategoryEntry {
    pub id: String,
    pub title: String,
    pub file_path: String,
}

// ---------------------------------------------------------------------------
// Indexer
// ---------------------------------------------------------------------------

/// Indexes B&R Automation Studio help content from `brhelpcontent.xml`.
pub struct HelpContentIndexer {
    pub help_root: PathBuf,
    xml_path: PathBuf,
    #[allow(dead_code)]
    metadata_dir: PathBuf,
    metadata_path: PathBuf,

    /// All pages keyed by page ID.
    pub pages: HashMap<String, HelpPage>,
    /// HelpID → page_id mapping.
    pub help_id_map: HashMap<String, String>,
    /// Pre-computed breadcrumbs (page_id → list from root to page).
    breadcrumb_cache: HashMap<String, Vec<String>>,
    /// Track duplicate IDs: original_id → list of titles seen.
    duplicate_ids: HashMap<String, Vec<String>>,
    /// Counter for generating synthetic IDs per original ID.
    dup_id_counter: HashMap<String, usize>,
}

impl HelpContentIndexer {
    /// Create a new indexer.
    ///
    /// # Errors
    /// Returns an error if `brhelpcontent.xml` is not found under `help_root`.
    pub fn new(help_root: &Path, metadata_dir: Option<&Path>) -> anyhow::Result<Self> {
        let help_root = help_root.to_path_buf();
        let xml_path = help_root.join("brhelpcontent.xml");
        let metadata_dir = metadata_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| help_root.join(".ashelp_metadata"));
        let metadata_path = metadata_dir.join("index_metadata.json");

        fs::create_dir_all(&help_root)?;
        fs::create_dir_all(&metadata_dir)?;

        if !xml_path.exists() {
            anyhow::bail!(
                "brhelpcontent.xml not found at: {}. \
                 Ensure the B&R Help 'Data' folder is at the configured path.",
                xml_path.display()
            );
        }

        Ok(Self {
            help_root,
            xml_path,
            metadata_dir,
            metadata_path,
            pages: HashMap::new(),
            help_id_map: HashMap::new(),
            breadcrumb_cache: HashMap::new(),
            duplicate_ids: HashMap::new(),
            dup_id_counter: HashMap::new(),
        })
    }

    // ------------------------------------------------------------------
    // XML parsing
    // ------------------------------------------------------------------

    /// Parse `brhelpcontent.xml` to extract the full page/section tree.
    pub fn parse_xml_structure(&mut self) -> anyhow::Result<()> {
        info!("Parsing {}", self.xml_path.display());
        let start = std::time::Instant::now();

        let file = fs::File::open(&self.xml_path)?;
        let reader = BufReader::new(file);
        let mut xml = Reader::from_reader(reader);
        xml.config_mut().trim_text(true);

        let mut buf = Vec::new();

        // Stack tracks (element_tag, page_id_or_empty, depth).
        // We use an iterative approach to avoid deep recursion on 100k+ pages.
        let mut parent_stack: Vec<Option<String>> = Vec::new(); // stack of parent IDs
        let mut depth: usize = 0; // XML nesting depth
        let mut root_seen = false;

        loop {
            match xml.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    match tag {
                        "Section" | "S" => {
                            if !root_seen {
                                // First start element should be the root container — skip
                                // Actually the root is something like <HelpContent>
                                // Sections at root level are top-level categories
                            }
                            let parent_id = parent_stack.last().cloned().flatten();
                            let page_id = self.process_element(e, parent_id.as_deref(), true);
                            parent_stack.push(page_id);
                            depth += 1;
                        }
                        "Page" | "P" => {
                            let parent_id = parent_stack.last().cloned().flatten();
                            self.process_element(e, parent_id.as_deref(), false);
                            // Pages can have children in B&R XML (rare), push placeholder
                            parent_stack.push(None);
                            depth += 1;
                        }
                        _ => {
                            if !root_seen && depth == 0 {
                                root_seen = true;
                                parent_stack.push(None); // root has no parent
                                depth += 1;
                            } else {
                                depth += 1;
                                parent_stack.push(parent_stack.last().cloned().flatten());
                            }
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    // Self-closing elements like <P ... /> or <H ... />
                    let name = e.name();
                    let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    match tag {
                        "Section" | "S" => {
                            let parent_id = parent_stack.last().cloned().flatten();
                            self.process_element(e, parent_id.as_deref(), true);
                        }
                        "Page" | "P" => {
                            let parent_id = parent_stack.last().cloned().flatten();
                            self.process_element(e, parent_id.as_deref(), false);
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(_)) if depth > 0 => {
                    parent_stack.pop();
                    depth -= 1;
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    error!(
                        "XML parse error at position {}: {}",
                        xml.error_position(),
                        e
                    );
                    anyhow::bail!("Failed to parse XML: {}", e);
                }
                _ => {}
            }
            buf.clear();
        }

        let elapsed = start.elapsed();
        info!(
            "Indexed {} pages and sections in {:.2}s",
            self.pages.len(),
            elapsed.as_secs_f64()
        );

        if !self.duplicate_ids.is_empty() {
            info!(
                "Resolved {} duplicate IDs with synthetic IDs",
                self.duplicate_ids.len()
            );
        }

        // Pre-compute breadcrumbs
        info!("Pre-computing breadcrumbs for all pages...");
        self.precompute_breadcrumbs();
        info!(
            "Breadcrumb cache populated: {} entries",
            self.breadcrumb_cache.len()
        );

        Ok(())
    }

    /// Process a Section or Page element, handling duplicates.
    /// Returns the effective page_id (possibly synthetic) if this was a section.
    fn process_element(
        &mut self,
        e: &BytesStart<'_>,
        parent_id: Option<&str>,
        is_section: bool,
    ) -> Option<String> {
        let id = self.attr_str(e, b"Id");
        let text = self
            .attr_str(e, b"Text")
            .or_else(|| self.attr_str(e, b"t"))
            .unwrap_or_default();
        let file_path = self
            .attr_str(e, b"File")
            .or_else(|| self.attr_str(e, b"p"))
            .unwrap_or_default();

        let id = id?;

        // Check for HelpID in nested Identifiers/I > HelpID/H elements.
        // NOTE: quick-xml SAX parsing doesn't let us look ahead into children here.
        // HelpID extraction is handled separately via the Identifiers/I element events.
        // For this SAX approach, we store a pending state. However, B&R XML stores
        // Identifiers as child elements. We'll use a two-pass or post-processing approach.
        // Actually — for quick-xml with SAX, we need a different strategy.
        // Let's parse the inner XML of section/page elements in a sub-reader.
        // For now, create the page without help_id and add it later.

        // Handle duplicate IDs
        if self.pages.contains_key(&id) {
            let existing_title = self.pages[&id].text.clone();
            self.duplicate_ids
                .entry(id.clone())
                .or_insert_with(|| vec![existing_title])
                .push(text.clone());

            let count = self.dup_id_counter.entry(id.clone()).or_insert(0);
            *count += 1;
            let synthetic_id = format!("{}__dup_{}", id, count);

            let page = HelpPage {
                id: synthetic_id.clone(),
                text,
                file_path,
                help_id: None,
                parent_id: parent_id.map(String::from),
                is_section,
            };
            self.pages.insert(synthetic_id.clone(), page);

            if is_section { Some(synthetic_id) } else { None }
        } else {
            let page = HelpPage {
                id: id.clone(),
                text,
                file_path,
                help_id: None,
                parent_id: parent_id.map(String::from),
                is_section,
            };
            self.pages.insert(id.clone(), page);

            if is_section { Some(id) } else { None }
        }
    }

    /// Extract a UTF-8 attribute value from an XML element.
    fn attr_str(&self, e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
        e.attributes()
            .filter_map(|a| a.ok())
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| {
                a.normalized_value(XmlVersion::Implicit1_0)
                    .ok()
                    .map(|v| v.to_string())
            })
    }

    // ------------------------------------------------------------------
    // HelpID extraction — second pass or inline
    // ------------------------------------------------------------------

    // NOTE: The SAX approach above doesn't extract HelpIDs from child elements.
    // We need a second-pass approach: after initial parse, re-parse the XML
    // looking specifically for Identifiers/HelpID elements to assign help_ids.
    // Alternatively, we use a DOM parser for just the Identifiers subtree.
    //
    // For the initial implementation, let's do a full second pass with quick-xml
    // that tracks which Section/Page we're inside and extracts HelpIDs.

    /// Second pass to extract HelpIDs from Identifiers/I > HelpID/H elements.
    pub fn extract_help_ids(&mut self) -> anyhow::Result<()> {
        let file = fs::File::open(&self.xml_path)?;
        let reader = BufReader::new(file);
        let mut xml = Reader::from_reader(reader);
        xml.config_mut().trim_text(true);

        let mut buf = Vec::new();
        // Track current Section/Page ID at each nesting level
        let mut current_page_id: Vec<Option<String>> = Vec::new();
        let mut in_identifiers = false;
        let mut dup_counters: HashMap<String, usize> = HashMap::new();

        loop {
            match xml.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let name = e.name();
                    let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");

                    match tag {
                        "Section" | "S" | "Page" | "P" => {
                            let id = self.attr_str(e, b"Id");
                            if let Some(ref raw_id) = id {
                                let counter = dup_counters.entry(raw_id.clone()).or_insert(0);
                                let effective_id = if *counter == 0 {
                                    raw_id.clone()
                                } else {
                                    format!("{}__dup_{}", raw_id, counter)
                                };
                                *counter += 1;
                                current_page_id.push(Some(effective_id));
                            } else {
                                current_page_id.push(None);
                            }
                        }
                        "Identifiers" | "I" => {
                            in_identifiers = true;
                        }
                        "HelpID" | "H" if in_identifiers => {
                            let value = self
                                .attr_str(e, b"Value")
                                .or_else(|| self.attr_str(e, b"v"));
                            if let Some(help_id) = value
                                && let Some(Some(page_id)) = current_page_id.last()
                            {
                                if let Some(page) = self.pages.get_mut(page_id) {
                                    page.help_id = Some(help_id.clone());
                                }
                                self.help_id_map.insert(help_id, page_id.clone());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    let name = e.name();
                    let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    match tag {
                        "Section" | "S" | "Page" | "P" => {
                            // Self-closing: track for dup counting but do NOT push to stack
                            let id = self.attr_str(e, b"Id");
                            if let Some(ref raw_id) = id {
                                let counter = dup_counters.entry(raw_id.clone()).or_insert(0);
                                *counter += 1;
                            }
                        }
                        "Identifiers" | "I" => {
                            // Self-closing <Identifiers/> or <I/> — no-op
                        }
                        "HelpID" | "H" if in_identifiers => {
                            let value = self
                                .attr_str(e, b"Value")
                                .or_else(|| self.attr_str(e, b"v"));
                            if let Some(help_id) = value
                                && let Some(Some(page_id)) = current_page_id.last()
                            {
                                if let Some(page) = self.pages.get_mut(page_id) {
                                    page.help_id = Some(help_id.clone());
                                }
                                self.help_id_map.insert(help_id, page_id.clone());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let name = e.name();
                    let tag = std::str::from_utf8(name.as_ref()).unwrap_or("");
                    match tag {
                        "Section" | "S" | "Page" | "P" => {
                            current_page_id.pop();
                        }
                        "Identifiers" | "I" => {
                            in_identifiers = false;
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        info!("Extracted {} HelpID mappings", self.help_id_map.len());
        Ok(())
    }

    // ------------------------------------------------------------------
    // Breadcrumbs
    // ------------------------------------------------------------------

    fn precompute_breadcrumbs(&mut self) {
        let page_ids: Vec<String> = self.pages.keys().cloned().collect();
        for page_id in &page_ids {
            if !self.breadcrumb_cache.contains_key(page_id) {
                let bc = self.compute_breadcrumb(page_id);
                self.breadcrumb_cache.insert(page_id.clone(), bc);
            }
        }
    }

    fn compute_breadcrumb(&self, page_id: &str) -> Vec<String> {
        let mut breadcrumb = Vec::new();
        let mut current_id = Some(page_id.to_string());
        let mut visited = std::collections::HashSet::new();

        while let Some(ref cid) = current_id {
            if visited.contains(cid) {
                debug!(
                    "Duplicate ID detected in breadcrumb for '{}': stopping at {}",
                    page_id, cid
                );
                break;
            }
            visited.insert(cid.clone());

            let page = match self.pages.get(cid) {
                Some(p) => p,
                None => {
                    debug!("Breadcrumb traversal stopped: page_id '{}' not found", cid);
                    break;
                }
            };

            breadcrumb.push(cid.clone());
            current_id = page.parent_id.clone();

            if breadcrumb.len() > 100 {
                error!(
                    "Breadcrumb depth exceeded 100 levels for '{}' — stopping",
                    page_id
                );
                break;
            }
        }

        breadcrumb.reverse();
        breadcrumb
    }

    /// Get breadcrumb trail as a list of `HelpPage` references (root → page).
    pub fn get_breadcrumb(&self, page_id: &str) -> Vec<&HelpPage> {
        let ids = self
            .breadcrumb_cache
            .get(page_id)
            .cloned()
            .unwrap_or_else(|| self.compute_breadcrumb(page_id));

        ids.iter().filter_map(|id| self.pages.get(id)).collect()
    }

    /// Get breadcrumb as `"Root > Section > Page"` string.
    pub fn get_breadcrumb_string(&self, page_id: &str) -> String {
        self.get_breadcrumb(page_id)
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join(" > ")
    }

    // ------------------------------------------------------------------
    // HTML text extraction
    // ------------------------------------------------------------------

    /// Safely resolve a file path within help_root, preventing path traversal.
    fn safe_resolve_path(&self, file_path: &str) -> Option<PathBuf> {
        if file_path.is_empty() {
            return None;
        }

        let path = Path::new(file_path);

        // Reject absolute paths
        if path.is_absolute() {
            warn!("Rejecting absolute file path: {}", file_path);
            return None;
        }

        // Reject paths with .. components
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                warn!(
                    "Rejecting path with parent directory traversal: {}",
                    file_path
                );
                return None;
            }
        }

        let resolved = self.help_root.join(file_path);

        // Double-check via canonicalization (handles symlinks)
        if let (Ok(canonical_root), Ok(canonical_file)) =
            (self.help_root.canonicalize(), resolved.canonicalize())
            && !canonical_file.starts_with(&canonical_root)
        {
            warn!(
                "Path escapes help root: {} -> {}",
                file_path,
                canonical_file.display()
            );
            return None;
        }

        Some(resolved)
    }

    /// Read raw HTML content for a page from disk (no caching).
    pub fn extract_html_content(&self, page_id: &str) -> Option<String> {
        let page = self.pages.get(page_id)?;
        let html_file = self.safe_resolve_path(&page.file_path)?;
        if !html_file.exists() {
            debug!("HTML file not found: {}", html_file.display());
            return None;
        }
        // Try UTF-8 first, fall back to Windows-1252 via encoding_rs
        match fs::read(&html_file) {
            Ok(bytes) => Some(decode_html_bytes(&bytes)),
            Err(e) => {
                error!("Failed to read HTML file {}: {}", html_file.display(), e);
                None
            }
        }
    }

    /// Extract plain text from HTML without caching (for bulk indexing).
    pub fn extract_plain_text_no_cache(&self, page: &HelpPage) -> Option<String> {
        let html_file = self.safe_resolve_path(&page.file_path)?;
        if !html_file.exists() {
            return None;
        }
        let bytes = fs::read(&html_file).ok()?;
        let html_str = decode_html_bytes(&bytes);
        extract_text_from_html(&html_str)
    }

    /// Extract plain text for a page by ID.
    pub fn extract_plain_text(&self, page_id: &str) -> Option<String> {
        let page = self.pages.get(page_id)?;
        self.extract_plain_text_no_cache(page)
    }

    // ------------------------------------------------------------------
    // Page lookups
    // ------------------------------------------------------------------

    pub fn get_page_by_id(&self, page_id: &str) -> Option<&HelpPage> {
        self.pages.get(page_id)
    }

    pub fn get_page_by_help_id(&self, help_id: &str) -> Option<&HelpPage> {
        let page_id = self.help_id_map.get(help_id)?;
        self.pages.get(page_id)
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    /// Get root-level sections (top-level categories), sorted by title.
    pub fn get_top_level_categories(&self) -> Vec<CategoryEntry> {
        let mut cats: Vec<CategoryEntry> = self
            .pages
            .values()
            .filter(|p| p.parent_id.is_none() && p.is_section)
            .map(|p| CategoryEntry {
                id: p.id.clone(),
                title: p.text.clone(),
                file_path: p.file_path.clone(),
            })
            .collect();
        cats.sort_by_key(|a| a.title.to_lowercase());
        cats
    }

    /// Get immediate children of a section (sections first, then pages, both alphabetical).
    pub fn get_section_children(&self, section_id: &str) -> Vec<SectionChildEntry> {
        if !self.pages.contains_key(section_id) {
            warn!("Section '{}' not found", section_id);
            return Vec::new();
        }

        let mut sections = Vec::new();
        let mut pages = Vec::new();

        for page in self.pages.values() {
            if page.parent_id.as_deref() == Some(section_id) {
                let entry = SectionChildEntry {
                    id: page.id.clone(),
                    title: page.text.clone(),
                    file_path: page.file_path.clone(),
                    is_section: page.is_section,
                };
                if page.is_section {
                    sections.push(entry);
                } else {
                    pages.push(entry);
                }
            }
        }

        sections.sort_by_key(|a| a.title.to_lowercase());
        pages.sort_by_key(|a| a.title.to_lowercase());
        sections.extend(pages);
        sections
    }

    // ------------------------------------------------------------------
    // Fingerprints & metadata
    // ------------------------------------------------------------------

    /// Compute MD5 hash of `brhelpcontent.xml` for change detection.
    pub fn get_xml_hash(&self) -> String {
        match fs::read(&self.xml_path) {
            Ok(bytes) => {
                let mut hasher = Md5::new();
                hasher.update(&bytes);
                format!("{:x}", hasher.finalize())
            }
            Err(_) => String::new(),
        }
    }

    /// Compute per-page fingerprints for incremental update detection.
    pub fn get_page_fingerprints(&self) -> HashMap<String, String> {
        self.pages
            .iter()
            .map(|(id, page)| {
                let key = format!(
                    "{}|{}|{}|{}|{}",
                    page.text,
                    page.file_path,
                    page.parent_id.as_deref().unwrap_or(""),
                    page.help_id.as_deref().unwrap_or(""),
                    page.is_section
                );
                let mut hasher = Md5::new();
                hasher.update(key.as_bytes());
                (id.clone(), format!("{:x}", hasher.finalize()))
            })
            .collect()
    }

    /// Check whether the XML has changed since the last index.
    #[allow(dead_code)]
    pub fn needs_reindex(&self) -> bool {
        let metadata = self.load_metadata();
        match metadata {
            Some(m) => {
                let current = self.get_xml_hash();
                let stored = m.get("xml_hash").and_then(|v| v.as_str()).unwrap_or("");
                if current != stored {
                    info!("XML file has changed — reindex required");
                    true
                } else {
                    info!("XML file unchanged — can use existing index");
                    false
                }
            }
            None => {
                info!("No metadata found — full index required");
                true
            }
        }
    }

    #[allow(dead_code)]
    fn load_metadata(&self) -> Option<serde_json::Value> {
        if !self.metadata_path.exists() {
            return None;
        }
        let data = fs::read_to_string(&self.metadata_path).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn save_metadata(&self) {
        let metadata = serde_json::json!({
            "xml_hash": self.get_xml_hash(),
            "indexed_at": chrono_now_iso(),
            "page_count": self.pages.len(),
            "help_id_count": self.help_id_map.len(),
            "help_root": self.help_root.display().to_string(),
        });
        if let Err(e) = fs::write(
            &self.metadata_path,
            serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        ) {
            warn!("Failed to save metadata: {}", e);
        } else {
            info!(
                "Saved metadata: {} pages, {} HelpIDs",
                self.pages.len(),
                self.help_id_map.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// HTML helpers
// ---------------------------------------------------------------------------

/// Decode bytes to a string, trying UTF-8 first, falling back to Windows-1252.
fn decode_html_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
            cow.into_owned()
        }
    }
}

/// Extract plain text from an HTML string using `scraper`.
///
/// Removes `<script>` and `<style>` content, collects text nodes,
/// and adds spacing after block-level elements.
fn extract_text_from_html(html: &str) -> Option<String> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);

    // Selectors for elements to skip
    let skip_selector = Selector::parse("script, style").ok()?;
    let skip_ids: std::collections::HashSet<_> =
        document.select(&skip_selector).map(|e| e.id()).collect();

    let block_tags: std::collections::HashSet<&str> = [
        "p",
        "div",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "li",
        "td",
        "th",
        "tr",
        "table",
        "blockquote",
        "pre",
    ]
    .into_iter()
    .collect();

    let mut text_parts: Vec<String> = Vec::new();

    // Walk all text nodes in document order
    for node in document.tree.nodes() {
        use scraper::node::Node;
        match node.value() {
            Node::Text(t) => {
                // Check if any ancestor is in the skip set
                let mut skip = false;
                let mut parent_id = node.parent();
                while let Some(pid) = parent_id {
                    if skip_ids.contains(&pid.id()) {
                        skip = true;
                        break;
                    }
                    parent_id = pid.parent();
                }
                if !skip {
                    text_parts.push(t.text.to_string());
                }
            }
            Node::Element(el) if block_tags.contains(el.name()) => {
                text_parts.push(" ".to_string());
            }
            _ => {}
        }
    }

    let text = text_parts.join("");
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Simple ISO-8601 timestamp (no external chrono dependency).
fn chrono_now_iso() -> String {
    // Use std::time for a rough timestamp
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Return Unix timestamp as string (good enough for metadata)
    secs.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_sample_xml(dir: &Path, content: &str) {
        fs::write(dir.join("brhelpcontent.xml"), content).unwrap();
    }

    fn create_sample_html(dir: &Path, rel_path: &str, html: &str) {
        let path = dir.join(rel_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, html).unwrap();
    }

    #[test]
    fn test_parse_full_format_xml() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="hw" Text="Hardware" File="hardware/index.html">
    <Identifiers><HelpID Value="10000"/></Identifiers>
    <Page Id="x20" Text="X20DI9371" File="hardware/x20di9371.html">
      <Identifiers><HelpID Value="12345"/></Identifiers>
    </Page>
  </Section>
  <Section Id="mot" Text="Motion" File="motion/overview.html">
    <Section Id="mapp" Text="mapp Motion" File="motion/mapp.html">
      <Page Id="mc" Text="MC_BR_MoveAbsolute" File="motion/mc_move.html">
        <Identifiers><HelpID Value="20100"/></Identifiers>
      </Page>
    </Section>
  </Section>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();
        indexer.extract_help_ids().unwrap();

        assert_eq!(indexer.pages.len(), 5); // hw, x20, mot, mapp, mc
        assert_eq!(indexer.help_id_map.get("12345"), Some(&"x20".to_string()));
        assert_eq!(indexer.help_id_map.get("20100"), Some(&"mc".to_string()));

        // Breadcrumb
        let bc = indexer.get_breadcrumb_string("mc");
        assert_eq!(bc, "Motion > mapp Motion > MC_BR_MoveAbsolute");
    }

    #[test]
    fn test_parse_abbreviated_format_xml() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <S Id="hw" t="Hardware" p="hardware/index.html">
    <I><H v="10000"/></I>
    <P Id="x20" t="X20DI9371" p="hardware/x20di9371.html">
      <I><H v="12345"/></I>
    </P>
  </S>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();
        indexer.extract_help_ids().unwrap();

        assert_eq!(indexer.pages.len(), 2);
        assert_eq!(indexer.pages["hw"].text, "Hardware");
        assert_eq!(indexer.pages["x20"].text, "X20DI9371");
        assert_eq!(indexer.help_id_map.get("12345"), Some(&"x20".to_string()));
    }

    #[test]
    fn test_duplicate_id_handling() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="dup" Text="First" File="first.html"/>
  <Section Id="dup" Text="Second" File="second.html"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        assert!(indexer.pages.contains_key("dup"));
        assert!(indexer.pages.contains_key("dup__dup_1"));
        assert_eq!(indexer.pages["dup"].text, "First");
        assert_eq!(indexer.pages["dup__dup_1"].text, "Second");
    }

    #[test]
    fn test_html_text_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Page Id="p1" Text="Test" File="test.html"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);
        create_sample_html(
            dir.path(),
            "test.html",
            "<html><body><h1>Title</h1><p>Hello world</p><script>var x=1;</script></body></html>",
        );

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        let text = indexer.extract_plain_text("p1").unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world"));
        assert!(!text.contains("var x=1"));
    }

    #[test]
    fn test_breadcrumb_cycle_detection() {
        let dir = tempfile::tempdir().unwrap();
        // Manually create pages that would form a cycle
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="a" Text="A" File="a.html">
    <Section Id="b" Text="B" File="b.html">
      <Page Id="c" Text="C" File="c.html"/>
    </Section>
  </Section>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        // Normal breadcrumb should work
        let bc = indexer.get_breadcrumb("c");
        assert_eq!(bc.len(), 3); // A > B > C
    }

    #[test]
    fn test_categories_and_children() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="hw" Text="Hardware" File="hw.html">
    <Page Id="p1" Text="Module A" File="a.html"/>
    <Page Id="p2" Text="Module B" File="b.html"/>
    <Section Id="sub" Text="Subsection" File="sub.html"/>
  </Section>
  <Section Id="sw" Text="Software" File="sw.html"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        let cats = indexer.get_top_level_categories();
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0].title, "Hardware");
        assert_eq!(cats[1].title, "Software");

        let children = indexer.get_section_children("hw");
        assert_eq!(children.len(), 3);
        // Sections first
        assert!(children[0].is_section);
        assert_eq!(children[0].title, "Subsection");
        // Then pages alphabetically
        assert!(!children[1].is_section);
        assert_eq!(children[1].title, "Module A");
    }

    #[test]
    fn test_helpid_not_assigned_to_self_closing_page() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="sec" Text="Section" File="sec.html">
    <Page Id="pg1" Text="Page1" File="pg1.html"/>
    <Identifiers><HelpID Value="99"/></Identifiers>
  </Section>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();
        indexer.extract_help_ids().unwrap();

        // HelpID "99" belongs to "sec", not the self-closing "pg1"
        assert_eq!(indexer.help_id_map.get("99"), Some(&"sec".to_string()));
        assert_eq!(indexer.pages["sec"].help_id, Some("99".to_string()));
    }

    #[test]
    fn test_helpid_after_self_closing_section() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Section Id="empty" Text="Empty" File="empty.html"/>
  <Section Id="real" Text="Real" File="real.html">
    <Identifiers><HelpID Value="100"/></Identifiers>
  </Section>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();
        indexer.extract_help_ids().unwrap();

        // HelpID "100" belongs to "real", not "empty"
        assert_eq!(indexer.help_id_map.get("100"), Some(&"real".to_string()));
        assert_eq!(indexer.pages["real"].help_id, Some("100".to_string()));
        assert_eq!(indexer.pages["empty"].help_id, None);
    }

    #[test]
    fn test_safe_resolve_rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Page Id="evil" Text="Evil" File="../../etc/passwd"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        assert!(indexer.extract_html_content("evil").is_none());
        let page = indexer.get_page_by_id("evil").unwrap();
        assert!(indexer.extract_plain_text_no_cache(page).is_none());
    }

    #[test]
    fn test_safe_resolve_rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let abs_path = if cfg!(windows) {
            "C:\\Windows\\System32\\config\\sam"
        } else {
            "/etc/passwd"
        };
        let xml = format!(
            r#"<?xml version="1.0"?>
<HelpContent>
  <Page Id="abs" Text="Absolute" File="{}"/>
</HelpContent>"#,
            abs_path
        );
        create_sample_xml(dir.path(), &xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        assert!(indexer.extract_html_content("abs").is_none());
    }

    #[test]
    fn test_safe_resolve_allows_normal_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Page Id="p1" Text="Test" File="hardware/x20.html"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);
        create_sample_html(
            dir.path(),
            "hardware/x20.html",
            "<html><body><p>Content</p></body></html>",
        );

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        let html = indexer.extract_html_content("p1");
        assert!(html.is_some());
        let text = indexer.extract_plain_text("p1").unwrap();
        assert!(text.contains("Content"));
    }

    #[test]
    fn test_page_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<?xml version="1.0"?>
<HelpContent>
  <Page Id="p1" Text="Test" File="test.html"/>
</HelpContent>"#;
        create_sample_xml(dir.path(), xml);

        let mut indexer = HelpContentIndexer::new(dir.path(), None).unwrap();
        indexer.parse_xml_structure().unwrap();

        let fp = indexer.get_page_fingerprints();
        assert!(fp.contains_key("p1"));
        assert!(!fp["p1"].is_empty());
    }
}
