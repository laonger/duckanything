//! MCP server assembly (Wave 4 Lane B): stdio transport + DuckTools.

use std::sync::Arc;

use async_trait::async_trait;

use rust_mcp_sdk::mcp_server::{server_runtime, McpServerOptions, ServerHandler, ServerRuntime};
use rust_mcp_sdk::schema::{
    schema_utils::CallToolError, CallToolRequestParams, Implementation, ListToolsResult,
    ListToolsResultCacheScope, PaginatedRequestParams, RpcError, ServerCapabilities,
    ServerCapabilitiesTools, ServerResult,
};
use rust_mcp_sdk::{
    McpServer, RequestContext, ServerDetails, StdioTransport, ToMcpServerHandler, TransportOptions,
};

use crate::core::engine::Engine;
use crate::core::error::DuckAnythingError;
use crate::core::security::Config;
use crate::mcp::tools::DuckTools;

/// Handler bridging MCP requests to the shared [`Engine`].
pub struct DuckServerHandler {
    engine: Arc<Engine>,
}

impl DuckServerHandler {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl ServerHandler for DuckServerHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _context: &RequestContext,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            cache_scope: ListToolsResultCacheScope::Private,
            result_type: "complete".to_string(),
            ttl_ms: 0,
            meta: None,
            next_cursor: None,
            tools: DuckTools::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _context: &RequestContext,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ServerResult, CallToolError> {
        let tool: DuckTools = DuckTools::try_from(params).map_err(CallToolError::new)?;
        let result = match tool {
            DuckTools::LoadFileTool(t) => t.call_tool(&self.engine).await,
            DuckTools::DescribeTool(t) => t.call_tool(&self.engine).await,
            DuckTools::RunSqlTool(t) => t.call_tool(&self.engine).await,
            DuckTools::ListFilesTool(t) => t.call_tool(&self.engine).await,
        };
        Ok(ServerResult::CallToolResult(result))
    }
}

/// Server identity, capabilities and instructions shared by stdio and HTTP transports.
fn build_server_details() -> ServerDetails {
    ServerDetails {
        server_info: Implementation {
            name: "duckanything".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("DuckAnything".into()),
            description: Some("Query local data files (CSV/JSON/Parquet) with DuckDB".into()),
            icons: vec![],
            website_url: None,
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Mount local CSV/JSON/Parquet files as DuckDB views, then run read-only SQL."
                .into(),
        ),
    }
}

/// Build and run the MCP server over stdio until the client disconnects.
pub async fn run_server(config: &Config) -> Result<(), DuckAnythingError> {
    let engine = Arc::new(Engine::new(config)?);
    let handler = DuckServerHandler::new(engine);

    let transport = StdioTransport::new(TransportOptions::default())
        .map_err(|e| DuckAnythingError::Io(std::io::Error::other(e.to_string())))?;

    let server: Arc<ServerRuntime> = server_runtime::create_server(McpServerOptions {
        server_details: build_server_details(),
        transport,
        handler: handler.to_mcp_server_handler(),
        message_observer: None,
    });

    server
        .start()
        .await
        .map_err(|e| DuckAnythingError::Io(std::io::Error::other(e.to_string())))?;
    Ok(())
}

/// Build and run the MCP server over HTTP: Streamable HTTP at `/mcp` and SSE at `/sse`.
pub async fn run_http_server(
    config: &Config,
    host: &str,
    port: u16,
) -> Result<(), DuckAnythingError> {
    let engine = Arc::new(Engine::new(config)?);
    let handler = DuckServerHandler::new(engine);

    tracing::info!(
        "MCP HTTP server listening on http://{host}:{port} (Streamable HTTP: http://{host}:{port}/mcp, SSE: http://{host}:{port}/sse)"
    );

    let server = rust_mcp_axum::create_axum_server(
        build_server_details(),
        handler.to_mcp_server_handler(),
        rust_mcp_axum::AxumServerOptions {
            host: host.into(),
            port,
            ..Default::default()
        },
    );

    server
        .start()
        .await
        .map_err(|e| DuckAnythingError::Io(std::io::Error::other(e.to_string())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::format::Format;

    /// End-to-end smoke (MCP boundary excluded): mount CSV -> run_query -> json.
    #[tokio::test]
    async fn mount_query_json_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("sales.csv");
        std::fs::write(&csv, "region,amount\neast,10\nwest,20\n").unwrap();

        let config = Config::default();
        let engine = Arc::new(Engine::new(&config).unwrap());
        let mount = engine.mount_file(&csv, None).unwrap();
        assert_eq!(mount.view_name, "sales");

        let result = engine.run_query("SELECT region, amount FROM sales ORDER BY amount").unwrap();
        assert_eq!(result.columns, vec!["region".to_string(), "amount".to_string()]);
        let mut result = result;
        let json = crate::core::format::format_result(&mut result, Format::Json, 100).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["region"], "east");
        assert_eq!(parsed[1]["amount"], 20);
    }

    /// Smoke test: the HTTP server actually binds and listens on the given port.
    #[tokio::test]
    async fn http_server_binds_and_listens() {
        let config = Config::default();
        let port = 18923u16;
        let task = tokio::spawn(async move {
            let _ = run_http_server(&config, "127.0.0.1", port).await;
        });

        // Give the server a moment to bind the listener.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let connected = std::net::TcpStream::connect(("127.0.0.1", port)).is_ok();
        task.abort();

        assert!(connected, "HTTP server should be listening on port {port}");
    }
}
