//! HTTP compatibility middleware: auto-fill SEP-2243 headers from the JSON-RPC body.
//!
//! The SDK's Streamable HTTP handler requires every JSON-RPC request to carry
//! `Mcp-Method` (and `Mcp-Name` for object-addressing methods). Plain clients
//! (curl, minimal SDKs) often omit these headers. This middleware parses the
//! request body and injects the missing headers *before* the SDK's header
//! validation runs, so header-less clients work while strict clients keep
//! their own headers (and the SDK's mismatch checks still apply).

use std::sync::Arc;

use async_trait::async_trait;

use rust_mcp_sdk::mcp_http::http::{HeaderName, HeaderValue, Request, Response};
use rust_mcp_sdk::mcp_http::{GenericBody, McpAppState, McpHttpResult, Middleware, MiddlewareNext};

/// Injects `Mcp-Method` / `Mcp-Name` headers derived from the JSON-RPC body
/// when the client did not send them. Headers already present are left
/// untouched (the SDK's own validation then applies to them as usual).
#[derive(Debug, Default, Clone)]
pub struct MethodHeaderInjector;

#[async_trait]
impl Middleware for MethodHeaderInjector {
    async fn handle<'req>(
        &self,
        mut req: Request<&'req str>,
        state: Arc<McpAppState>,
        next: MiddlewareNext<'req>,
    ) -> McpHttpResult<Response<GenericBody>> {
        if let Some((method, name)) = sep2243_headers_from_body(req.body()) {
            let headers = req.headers_mut();
            // HeaderMap lookups are case-insensitive, so this also matches an
            // explicit `Mcp-Method` / `Mcp-Name` sent by a strict client.
            if !headers.contains_key("mcp-method") {
                if let Ok(value) = HeaderValue::from_str(&method) {
                    headers.insert(HeaderName::from_static("mcp-method"), value);
                    tracing::debug!("injected missing Mcp-Method header: {method}");
                }
            }
            if let Some(name) = name {
                if !headers.contains_key("mcp-name") {
                    if let Ok(value) = HeaderValue::from_str(&name) {
                        headers.insert(HeaderName::from_static("mcp-name"), value);
                        tracing::debug!("injected missing Mcp-Name header: {name}");
                    }
                }
            }
        }
        next(req, state).await
    }
}

/// Parse the JSON-RPC body and return the SEP-2243 header values as
/// `(method, name)`, where `name` is `Some` only for methods that address a
/// specific object (`tools/call` / `prompts/get` → `params.name`,
/// `resources/read` → `params.uri`).
///
/// Returns `None` for bodies that are not valid JSON or carry no `method`
/// field — such requests pass through untouched.
fn sep2243_headers_from_body(body: &str) -> Option<(String, Option<String>)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let method = value.get("method")?.as_str()?.to_string();
    let name = match method.as_str() {
        "tools/call" | "prompts/get" => value
            .pointer("/params/name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        "resources/read" => value
            .pointer("/params/uri")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        _ => None,
    };
    Some((method, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_method_and_name_for_tools_call() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"load_file","arguments":{}}}"#;
        let (method, name) = sep2243_headers_from_body(body).unwrap();
        assert_eq!(method, "tools/call");
        assert_eq!(name.as_deref(), Some("load_file"));
    }

    #[test]
    fn extracts_uri_for_resources_read() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"file:///tmp/a.csv"}}"#;
        let (method, name) = sep2243_headers_from_body(body).unwrap();
        assert_eq!(method, "resources/read");
        assert_eq!(name.as_deref(), Some("file:///tmp/a.csv"));
    }

    #[test]
    fn no_name_for_plain_methods() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let (method, name) = sep2243_headers_from_body(body).unwrap();
        assert_eq!(method, "tools/list");
        assert!(name.is_none());
    }

    #[test]
    fn invalid_json_or_missing_method_passes_through() {
        assert!(sep2243_headers_from_body("not json").is_none());
        assert!(sep2243_headers_from_body(r#"{"jsonrpc":"2.0","id":1}"#).is_none());
        assert!(sep2243_headers_from_body(r#"{"method":42}"#).is_none());
    }
}