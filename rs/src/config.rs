//! Application configuration from environment variables and CLI arguments.

use std::path::PathBuf;

use clap::Parser;

/// B&R Automation Studio Help MCP Server
#[derive(Parser, Debug)]
#[command(name = "as-help-server", version, about)]
pub struct CliArgs {
    /// Path to AS Help Data folder (AS_HELP_ROOT).
    /// Example: 'C:\BRAutomation\AS412\Help-en\Data'
    #[arg(long = "help-root")]
    pub help_root: Option<PathBuf>,

    /// Path to database file (AS_HELP_DB_PATH).
    #[arg(long = "db-path")]
    pub db_path: Option<PathBuf>,

    /// Path to metadata directory (AS_HELP_METADATA_DIR).
    #[arg(long = "metadata-dir")]
    pub metadata_dir: Option<PathBuf>,

    /// Force index rebuild (AS_HELP_FORCE_REBUILD).
    #[arg(long = "force-rebuild")]
    pub force_rebuild: Option<bool>,

    /// AS version for online help: 4 or 6 (AS_HELP_VERSION).
    #[arg(long = "as-version", value_parser = ["4", "6"])]
    pub as_version: Option<String>,

    /// Enable embedding via API (CREATE_EMBEDDINGS).
    #[arg(long = "create-embeddings")]
    pub create_embeddings: Option<bool>,
}

/// Resolved application configuration (env vars merged with CLI overrides).
#[derive(Debug, Clone)]
pub struct AppConfig {
    // Core paths
    pub help_root: PathBuf,
    pub db_path: PathBuf,
    pub metadata_dir: PathBuf,

    // Behaviour
    pub force_rebuild: bool,
    pub as_version: String,
    pub online_help_base_url: String,

    // Embeddings (optional)
    pub create_embeddings: bool,
    pub embedding: Option<EmbeddingConfig>,

    // Transport
    pub transport: Transport,
    pub host: String,
    pub port: u16,
    pub disable_dns_rebinding_protection: bool,
}

/// Embedding API configuration.
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub api_endpoint: String,
    pub api_key: String,
    pub model: String,
    pub dimensions: usize,
    pub batch_size: usize,
    pub max_chars: usize,
    pub max_workers: usize,
}

/// MCP transport mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    StreamableHttp,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

impl AppConfig {
    /// Build configuration by merging environment variables with CLI overrides.
    pub fn from_env_and_cli(cli: &CliArgs) -> anyhow::Result<Self> {
        // Help root
        let help_root = cli
            .help_root
            .clone()
            .or_else(|| std::env::var("AS_HELP_ROOT").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/data/help"));
        let help_root = std::fs::canonicalize(&help_root).unwrap_or(help_root);

        // Determine if we're in a Docker-like setup
        let is_docker = help_root.starts_with("/data/");

        // DB path
        let default_db = if is_docker {
            PathBuf::from("/data/db/.ashelp_lance")
        } else {
            help_root.join(".ashelp_lance")
        };
        let db_path = cli
            .db_path
            .clone()
            .or_else(|| std::env::var("AS_HELP_DB_PATH").ok().map(PathBuf::from))
            .unwrap_or(default_db);

        // Metadata dir
        let default_meta = if is_docker {
            PathBuf::from("/data/db/.ashelp_metadata")
        } else {
            help_root.join(".ashelp_metadata")
        };
        let metadata_dir = cli
            .metadata_dir
            .clone()
            .or_else(|| std::env::var("AS_HELP_METADATA_DIR").ok().map(PathBuf::from))
            .unwrap_or(default_meta);

        // Force rebuild
        let force_rebuild = cli.force_rebuild.unwrap_or_else(|| env_bool("AS_HELP_FORCE_REBUILD", false));

        // AS version
        let as_version = cli
            .as_version
            .clone()
            .unwrap_or_else(|| env_or("AS_HELP_VERSION", "4"));
        let online_help_base_url = match as_version.as_str() {
            "6" => "https://help.br-automation.com/#/en/6/".to_string(),
            _ => "https://help.br-automation.com/#/en/4/".to_string(),
        };

        // Embeddings
        let create_embeddings = cli
            .create_embeddings
            .unwrap_or_else(|| env_bool("CREATE_EMBEDDINGS", false));

        let embedding = if create_embeddings {
            Some(EmbeddingConfig::from_env()?)
        } else {
            None
        };

        // Transport
        let transport_str = env_or("MCP_TRANSPORT", "stdio");
        let transport = if transport_str == "streamable-http" {
            Transport::StreamableHttp
        } else {
            Transport::Stdio
        };

        let host = env_or("MCP_HOST", "127.0.0.1");
        let port = env_usize("MCP_PORT", 8000) as u16;
        let disable_dns_rebinding_protection =
            env_bool("MCP_DISABLE_DNS_REBINDING_PROTECTION", false);

        Ok(Self {
            help_root,
            db_path,
            metadata_dir,
            force_rebuild,
            as_version,
            online_help_base_url,
            create_embeddings,
            embedding,
            transport,
            host,
            port,
            disable_dns_rebinding_protection,
        })
    }
}

impl EmbeddingConfig {
    fn from_env() -> anyhow::Result<Self> {
        let api_endpoint = std::env::var("EMBEDDING_API_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("EMBEDDING_API_ENDPOINT is required when embeddings are enabled"))?;
        let api_key = std::env::var("EMBEDDING_API_KEY")
            .map_err(|_| anyhow::anyhow!("EMBEDDING_API_KEY is required when embeddings are enabled"))?;
        let model = std::env::var("EMBEDDING_MODEL")
            .map_err(|_| anyhow::anyhow!("EMBEDDING_MODEL is required when embeddings are enabled"))?;
        let dimensions: usize = std::env::var("EMBEDDING_DIMENSIONS")
            .map_err(|_| anyhow::anyhow!("EMBEDDING_DIMENSIONS is required when embeddings are enabled"))?
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("EMBEDDING_DIMENSIONS must be a positive integer"))?;
        if dimensions == 0 {
            anyhow::bail!("EMBEDDING_DIMENSIONS must be a positive integer");
        }

        Ok(Self {
            api_endpoint: api_endpoint.trim_end_matches('/').to_string(),
            api_key,
            model,
            dimensions,
            batch_size: env_usize("EMBEDDING_BATCH_SIZE", 200),
            max_chars: env_usize("EMBEDDING_MAX_CHARS", 8000),
            max_workers: env_usize("EMBEDDING_MAX_WORKERS", 4),
        })
    }
}
