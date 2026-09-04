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
///
/// Uses the SDK's BYO-server assembly (`mcp_routes` + `McpHttpHandler`) instead
/// of the turnkey `create_axum_server`, because the turnkey path exposes no
/// middleware hook — we need one to auto-fill the SEP-2243 headers
/// ([`crate::mcp::http_compat::MethodHeaderInjector`]). `enable_json_response`
/// makes `POST /mcp` answer with plain JSON instead of an SSE stream.
pub async fn run_http_server(
    config: &Config,
    host: &str,
    port: u16,
) -> Result<(), DuckAnythingError> {
    use rust_mcp_sdk::id_generator::{FastIdGenerator, UuidGenerator};
    use rust_mcp_sdk::mcp_http::{
        resolve_dns_middleware, DnsRebindingOptions, McpAppState, McpHttpHandler, Middleware,
        DEFAULT_MAX_LISTEN_STREAMS,
    };
    use rust_mcp_sdk::TransportOptions;
    use std::time::Duration;

    let engine = Arc::new(Engine::new(config)?);
    let handler = DuckServerHandler::new(engine);

    tracing::info!(
        "MCP HTTP server listening on http://{host}:{port} (Streamable HTTP: http://{host}:{port}/mcp, SSE: http://{host}:{port}/sse)"
    );

    let state: Arc<McpAppState> = Arc::new(McpAppState {
        id_generator: Arc::new(UuidGenerator {}),
        stream_id_gen: Arc::new(FastIdGenerator::new(Some("s_"))),
        server_details: Arc::new(build_server_details()),
        handler: handler.to_mcp_server_handler(),
        ping_interval: Duration::from_secs(12),
        transport_options: Arc::new(TransportOptions::default()),
        enable_json_response: true,
        message_observer: None,
        extensions: Arc::new(tokio::sync::RwLock::new(None)),
        active_listen_streams: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        max_listen_streams: DEFAULT_MAX_LISTEN_STREAMS,
    });

    // DNS rebinding protection (same as the turnkey path), then our header injector.
    let mut middlewares: Vec<Arc<dyn Middleware>> = vec![];
    if let Some(dns) = resolve_dns_middleware(&mut DnsRebindingOptions::default(), host, port) {
        middlewares.push(Arc::new(dns));
    }
    middlewares.push(Arc::new(crate::mcp::http_compat::MethodHeaderInjector));

    // No auth provider. The SDK `auth` feature is enabled transitively via
    // rust-mcp-axum's dependency, so the 3-arg `new` is the one that exists.
    let http_handler = McpHttpHandler::new(None, middlewares, None);

    let mount = rust_mcp_axum::McpMountOptions {
        streamable_http_endpoint: "/mcp".into(),
        sse_endpoint: "/sse".into(),
        sse_messages_endpoint: "/messages".into(),
        health_endpoint: Some("/health".into()),
        ..Default::default()
    };

    let app = rust_mcp_axum::mcp_routes(state, &mount, http_handler);

    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| DuckAnythingError::Io(std::io::Error::other(e.to_string())))?;
    axum::serve(listener, app)
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

    /// Regression: a client that omits the SEP-2243 `Mcp-Method` header still
    /// gets a valid JSON response — the server injects the header from the body.
    /// (Port 18924: distinct from the bind test's 18923 so the two tests can
    /// run concurrently without fighting over the listener.)
    ///
    /// Multi-thread flavor: the test blocks on `std::net::TcpStream` reads, and
    /// on a current-thread runtime that would starve the spawned server task.
    #[tokio::test(flavor = "multi_thread")]
    async fn http_server_accepts_requests_without_mcp_method_header() {
        use std::io::{Read, Write};

        let config = Config::default();
        let port = 18924u16;
        let task = tokio::spawn(async move {
            let _ = run_http_server(&config, "127.0.0.1", port).await;
        });

        // Wait until the listener is up (poll up to ~5s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stream = loop {
            match std::net::TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => panic!("server never became reachable on port {port}: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();

        // Raw HTTP/1.1 POST without any Mcp-Method / Mcp-Name header.
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
        let request = format!(
            "POST /mcp HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Content-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();

        // Read until EOF (Connection: close) or the 2s read timeout, whichever first.
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(e) => panic!("error reading HTTP response: {e}"),
            }
        }
        let response = String::from_utf8_lossy(&response);

        task.abort();

        assert!(
            response.contains("200 OK"),
            "expected 200 OK, got: {response}"
        );
        assert!(
            response.contains("load_file"),
            "expected load_file tool in response, got: {response}"
        );
        assert!(
            response.contains("run_sql"),
            "expected run_sql tool in response, got: {response}"
        );
    }
}
