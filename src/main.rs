//! Entry point for the B&R Automation Studio Help MCP server.

mod config;
mod embeddings;
mod indexer;
mod models;
mod search_engine;
mod server;

use std::sync::Arc;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use config::{AppConfig, CliArgs, Transport};
use embeddings::EmbeddingService;
use indexer::HelpContentIndexer;
use search_engine::HelpSearchEngine;
use server::HelpServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file (ignore errors — env vars may come from Docker / system)
    let _ = dotenvy::dotenv();

    // Parse CLI args
    let cli = CliArgs::parse();

    // Build config (env vars + CLI overrides)
    let config = AppConfig::from_env_and_cli(&cli)?;

    // Initialize tracing (logs to stderr so stdio transport stays clean)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    info!("=== B&R Help Server Startup ===");
    info!("Help root: {}", config.help_root.display());
    info!("AS Version: {}", config.as_version);
    info!("Online help base: {}", config.online_help_base_url);
    info!("Database: {}", config.db_path.display());
    info!("Force rebuild: {}", config.force_rebuild);
    info!(
        "Embeddings: {}",
        if config.create_embeddings {
            "enabled"
        } else {
            "disabled (FTS only)"
        }
    );

    // 1. Initialize indexer (parses XML structure)
    info!("Initializing help indexer...");
    let mut indexer = HelpContentIndexer::new(
        &config.help_root,
        Some(config.metadata_dir.as_path()),
    )?;
    indexer.parse_xml_structure()?;
    indexer.extract_help_ids()?;
    info!(
        "Indexed {} pages ({} HelpIDs)",
        indexer.pages.len(),
        indexer.help_id_map.len()
    );

    // Log categories
    let categories = indexer.get_top_level_categories();
    info!("Top-level categories ({}):", categories.len());
    for cat in &categories {
        info!("  - {}", cat.title);
    }

    let indexer = Arc::new(indexer);

    // 2. Optionally create embedding service
    let embedder: Option<Arc<EmbeddingService>> = if let Some(ref emb_cfg) = config.embedding {
        info!("Initializing embedding API client...");
        Some(Arc::new(EmbeddingService::new(emb_cfg)?))
    } else {
        None
    };

    // 3. Initialize search engine
    info!("Initializing search engine...");
    let search_engine = HelpSearchEngine::new(
        &config.db_path,
        indexer.clone(),
        config.force_rebuild,
        embedder,
    )
    .await?;

    let search_engine = Arc::new(search_engine);

    // 4. Build/load index in background
    let se_bg = search_engine.clone();
    let build_handle = tokio::spawn(async move {
        if let Err(e) = se_bg.initialize().await {
            tracing::error!("Search index initialization failed: {e}");
        }
    });

    info!("=== Server ready (index building in background) ===");

    // 5. Create MCP handler
    let handler = HelpServer::new(indexer, search_engine, &config);

    // 6. Start transport
    match config.transport {
        Transport::Stdio => {
            info!("Starting MCP server on stdio transport...");
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            let server = rmcp::ServiceExt::serve(handler, (stdin, stdout)).await?;
            // Wait for the server to finish
            server.waiting().await?;
        }
        Transport::StreamableHttp => {
            info!(
                "Starting MCP server on streamable-http at {}:{}...",
                config.host, config.port
            );
            let bind_addr = format!("{}:{}", config.host, config.port);
            let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

            use rmcp::transport::streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
            };
            let session_manager = Arc::new(LocalSessionManager::default());
            let config_http = StreamableHttpServerConfig::default()
                .with_allowed_hosts(vec!["localhost".to_string(), "127.0.0.1".to_string()]);
            let service = StreamableHttpService::new(
                move || Ok(handler.clone()),
                session_manager,
                config_http,
            );
            let app = axum::Router::new().fallback_service(tower::service_fn(
                move |req: axum::http::Request<axum::body::Body>| {
                    let mut svc = service.clone();
                    async move { tower_service::Service::call(&mut svc, req).await }
                },
            ));
            axum::serve(listener, app).await?;
        }
    }

    // Wait for background build to finish
    let _ = build_handle.await;

    info!("Server shut down.");
    Ok(())
}
