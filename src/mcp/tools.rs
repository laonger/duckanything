//! Wave 4 Lane A: MCP tool definitions and dispatch.
//!
//! Four tools are exposed to MCP clients — `load_file`, `describe`, `run_sql`
//! and `list_files` — all backed by the synchronous [`Engine`] API. Every tool
//! returns a uniform JSON envelope as its single text block:
//!
//! ```json
//! {"ok": true, "data": ..., "metadata": {...}}
//! ```
//!
//! Errors are reported inside the envelope (`{"ok": false, "error": "..."}`),
//! never via `CallToolResult::is_error`; `CallToolError` is only used by the
//! generated `TryFrom<CallToolRequestParams>` for protocol-level failures such
//! as argument parsing.
//!
//! The engine is synchronous (it blocks on a worker thread + `recv_timeout`
//! internally), so each tool's `call_tool` runs the engine call inside
//! `tokio::task::spawn_blocking` to avoid stalling a tokio worker.
//!
//! The MCP server (Wave 4 Lane B) is not implemented yet, so nothing in this
//! module is reachable from the binary; dead-code warnings are suppressed here
//! exactly as in the other not-yet-wired core modules.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use rust_mcp_sdk::macros::{mcp_tool, JsonSchema};
use rust_mcp_sdk::schema::{CallToolResult, TextContent};
use rust_mcp_sdk::tool_box;

use crate::core::engine::{Engine, FileFormat};
use crate::core::error::DuckAnythingError;
use crate::core::format::{format_result, Cell, Format};

/// Default row limit for `run_sql` when the caller does not pass `max_rows`.
/// Mirrors `Config::default().max_rows` (the tool layer has no config access).
const DEFAULT_MAX_ROWS: usize = 100;

/// 2^53 — the largest integer exactly representable in a JS `Number`.
/// Values with magnitude >= this are rendered as strings in JSON output.
const JS_SAFE_INT: u64 = 1 << 53;

/// Upper bound (seconds) for a per-query `timeout_secs` override. Values above
/// this are silently clamped to prevent a caller from pinning the engine.
const MAX_TOOL_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Mounts a local data file (CSV, JSON, or Parquet) as a read-only view that
/// can be queried with `run_sql`.
#[mcp_tool(
    name = "load_file",
    description = "Mounts a local data file (CSV, JSON, or Parquet) as a read-only view that can be queried with run_sql",
    title = "Load File",
    idempotent_hint = true,
    destructive_hint = false,
    open_world_hint = false,
    read_only_hint = true,
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct LoadFileTool {
    /// Path to the data file to mount as a view (csv, json, or parquet).
    file: String,
    /// Optional view name; defaults to the file stem when omitted.
    alias: Option<String>,
}

impl LoadFileTool {
    pub async fn call_tool(&self, engine: &Arc<Engine>) -> CallToolResult {
        let file = self.file.clone();
        let alias = self.alias.clone();
        let engine = Arc::clone(engine);
        tokio::task::spawn_blocking(move || {
            tracing::debug!(tool = "load_file", file = %file, alias = ?alias, "call_tool");
            let info = match engine.mount_file(Path::new(&file), alias.as_deref()) {
                Ok(info) => info,
                Err(err) => {
                    tracing::warn!(tool = "load_file", error = %err, "call_tool failed");
                    return envelope_err(&err);
                }
            };
            // Supplement the mount result with column name/type info.
            let columns = match engine.describe_view(&info.view_name) {
                Ok(desc) => desc
                    .columns
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.name,
                            "type": c.type_name,
                        })
                    })
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };
            let data = serde_json::json!({
                "view_name": info.view_name,
                "file_path": info.file_path.to_string_lossy(),
                "format": format_str(&info.format),
            });
            let metadata = serde_json::json!({ "columns": columns });
            envelope_ok(data, metadata)
        })
        .await
        .unwrap_or_else(|join_err| envelope_err_msg(format!("tool execution panicked: {join_err}")))
    }
}

/// Describes a mounted view: column names, DuckDB types, and sample values.
#[mcp_tool(
    name = "describe",
    description = "Describes a mounted view: column names, DuckDB types, and up to three sample values per column",
    title = "Describe View",
    idempotent_hint = true,
    destructive_hint = false,
    open_world_hint = false,
    read_only_hint = true,
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct DescribeTool {
    /// Name of the mounted view to describe.
    view: String,
}

impl DescribeTool {
    pub async fn call_tool(&self, engine: &Arc<Engine>) -> CallToolResult {
        let view = self.view.clone();
        let engine = Arc::clone(engine);
        tokio::task::spawn_blocking(move || {
            tracing::debug!(tool = "describe", view = %view, "call_tool");
            match engine.describe_view(&view) {
                Ok(desc) => {
                    let data = desc
                        .columns
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "name": c.name,
                                "type": c.type_name,
                                "samples": c.samples.iter().map(cell_to_json).collect::<Vec<_>>(),
                            })
                        })
                        .collect::<Vec<_>>();
                    let metadata = serde_json::json!({ "view_name": desc.view_name });
                    envelope_ok(serde_json::Value::Array(data), metadata)
                }
                Err(err) => {
                    tracing::warn!(tool = "describe", error = %err, "call_tool failed");
                    envelope_err(&err)
                }
            }
        })
        .await
        .unwrap_or_else(|join_err| envelope_err_msg(format!("tool execution panicked: {join_err}")))
    }
}

/// Runs a read-only SQL query (SELECT/WITH/FROM) against mounted views.
#[mcp_tool(
    name = "run_sql",
    description = "Runs a read-only SQL query (SELECT/WITH/FROM) against the mounted views",
    title = "Run SQL",
    idempotent_hint = true,
    destructive_hint = false,
    open_world_hint = false,
    read_only_hint = true,
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct RunSqlTool {
    /// The read-only SQL statement to execute.
    sql: String,
    /// Overrides the default row limit (100) for this query.
    ///
    /// `u64` rather than `usize`: the SDK's `JsonSchema` derive does not
    /// recognize `usize` and would treat it as a nested struct.
    max_rows: Option<u64>,
    /// Max seconds this query may run, overriding the server default (30).
    /// Use when a large scan legitimately needs longer; keep small for
    /// interactive queries.
    timeout_secs: Option<u64>,
}

impl RunSqlTool {
    pub async fn call_tool(&self, engine: &Arc<Engine>) -> CallToolResult {
        let sql = self.sql.clone();
        // `max_rows` is `u64` for the JSON schema (see field docs); the engine
        // formatting API takes `usize`, and a row limit that does not fit in
        // `usize` is not realistic, so a plain cast is fine.
        let max_rows = self.max_rows.unwrap_or(DEFAULT_MAX_ROWS as u64) as usize;
        // Per-query timeout override: clamp to a sane upper bound so a caller
        // cannot pin the engine with an absurd value. `None`/0 → server default.
        let timeout_secs = self
            .timeout_secs
            .filter(|&t| t > 0)
            .map(|t| t.min(MAX_TOOL_TIMEOUT_SECS));
        let engine = Arc::clone(engine);
        tokio::task::spawn_blocking(move || {
            tracing::debug!(
                tool = "run_sql",
                sql = %sql,
                max_rows = max_rows,
                timeout_secs = ?timeout_secs,
                "call_tool"
            );
            let start = std::time::Instant::now();
            let result = match timeout_secs {
                Some(t) => engine.run_query_with_timeout(&sql, t),
                None => engine.run_query(&sql),
            };
            let mut result = match result {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!(tool = "run_sql", error = %err, "call_tool failed");
                    return envelope_err(&err);
                }
            };
            let elapsed_ms = start.elapsed().as_millis() as u64;

            // format_result truncates `result.rows` in place and records the
            // truncation in `result.truncated`.
            let formatted = match format_result(&mut result, Format::Json, max_rows) {
                Ok(text) => text,
                Err(err) => return envelope_err(&err),
            };
            // The formatted text is JSON; parse it back so `data` is a real
            // JSON array. If parsing ever fails, fall back to the raw string.
            let data = serde_json::from_str::<serde_json::Value>(&formatted)
                .unwrap_or_else(|_| serde_json::Value::String(formatted));

            let truncated = result.truncated.as_ref();
            let mut metadata = serde_json::Map::new();
            metadata.insert(
                "truncated".to_string(),
                serde_json::Value::Bool(truncated.is_some()),
            );
            metadata.insert(
                "returned_rows".to_string(),
                serde_json::Value::from(result.rows.len()),
            );
            if let Some(rl) = truncated {
                metadata.insert("total_rows".to_string(), serde_json::Value::from(rl.total));
            }
            metadata.insert(
                "elapsed_ms".to_string(),
                serde_json::Value::from(elapsed_ms),
            );
            metadata.insert(
                "columns".to_string(),
                serde_json::Value::Array(
                    result
                        .columns
                        .iter()
                        .map(|c| serde_json::Value::String(c.clone()))
                        .collect(),
                ),
            );
            envelope_ok(data, serde_json::Value::Object(metadata))
        })
        .await
        .unwrap_or_else(|join_err| envelope_err_msg(format!("tool execution panicked: {join_err}")))
    }
}

/// Lists all currently mounted data files and their view names.
#[mcp_tool(
    name = "list_files",
    description = "Lists all currently mounted data files and their view names",
    title = "List Files",
    idempotent_hint = true,
    destructive_hint = false,
    open_world_hint = false,
    read_only_hint = true,
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, JsonSchema)]
pub struct ListFilesTool {}

impl ListFilesTool {
    pub async fn call_tool(&self, engine: &Arc<Engine>) -> CallToolResult {
        let engine = Arc::clone(engine);
        tokio::task::spawn_blocking(move || {
            tracing::debug!(tool = "list_files", "call_tool");
            let views = engine.list_views();
            let data = views
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "view_name": m.view_name,
                        "file_path": m.file_path.to_string_lossy(),
                        "format": format_str(&m.format),
                    })
                })
                .collect::<Vec<_>>();
            let metadata = serde_json::json!({ "count": data.len() });
            envelope_ok(serde_json::Value::Array(data), metadata)
        })
        .await
        .unwrap_or_else(|join_err| envelope_err_msg(format!("tool execution panicked: {join_err}")))
    }
}

// Generates `pub enum DuckTools` with variants for each tool, plus
// `DuckTools::tools()`, `DuckTools::tool_name()` and
// `TryFrom<CallToolRequestParams>` (which reports argument-parsing failures as
// `CallToolError`).
tool_box!(DuckTools, [LoadFileTool, DescribeTool, RunSqlTool, ListFilesTool]);

// ---------------------------------------------------------------------------
// Envelope helpers
// ---------------------------------------------------------------------------

/// Builds a success envelope `{"ok": true, "data": ..., "metadata": ...}` and
/// serializes it as the single text block of a [`CallToolResult`].
fn envelope_ok(data: serde_json::Value, metadata: serde_json::Value) -> CallToolResult {
    let envelope = serde_json::json!({
        "ok": true,
        "data": data,
        "metadata": metadata,
    });
    CallToolResult::text_content(vec![TextContent::from(
        serde_json::to_string(&envelope).expect("envelope is always serializable"),
    )])
}

/// Builds an error envelope `{"ok": false, "error": "<message>"}`.
fn envelope_err(err: &DuckAnythingError) -> CallToolResult {
    envelope_err_msg(err.to_string())
}

/// Builds an error envelope from a raw message (e.g. a panicked blocking task).
fn envelope_err_msg(message: String) -> CallToolResult {
    let envelope = serde_json::json!({
        "ok": false,
        "error": message,
    });
    CallToolResult::text_content(vec![TextContent::from(
        serde_json::to_string(&envelope).expect("envelope is always serializable"),
    )])
}

// ---------------------------------------------------------------------------
// Value mapping
// ---------------------------------------------------------------------------

/// Maps a [`FileFormat`] to its lowercase extension string.
fn format_str(format: &FileFormat) -> &'static str {
    match format {
        FileFormat::Csv => "csv",
        FileFormat::Json => "json",
        FileFormat::Parquet => "parquet",
    }
}

/// Maps a [`Cell`] to a JSON value following the JS-safe-number rules:
/// integers with |n| >= 2^53 become strings, floats become strings, blobs
/// become lowercase hex, and lists/structs recurse.
///
/// Note: `crate::core::format` has an identical private helper; it is not
/// exported, so this module keeps its own copy.
fn cell_to_json(cell: &Cell) -> serde_json::Value {
    match cell {
        Cell::Null => serde_json::Value::Null,
        Cell::Bool(b) => serde_json::Value::Bool(*b),
        Cell::Int(n) => {
            if n.unsigned_abs() >= JS_SAFE_INT {
                serde_json::Value::String(n.to_string())
            } else {
                serde_json::Value::Number((*n).into())
            }
        }
        Cell::UInt(n) => {
            if *n >= JS_SAFE_INT {
                serde_json::Value::String(n.to_string())
            } else {
                serde_json::Value::Number((*n).into())
            }
        }
        Cell::Float(f) => serde_json::Value::String(format!("{f}")),
        Cell::Decimal(s) => serde_json::Value::String(s.clone()),
        Cell::Text(s) => serde_json::Value::String(s.clone()),
        Cell::Date(s) | Cell::Timestamp(s) | Cell::Time(s) => serde_json::Value::String(s.clone()),
        Cell::Blob(bytes) => serde_json::Value::String(to_hex(bytes)),
        Cell::List(cells) => serde_json::Value::Array(cells.iter().map(cell_to_json).collect()),
        Cell::Struct(fields) => {
            let mut obj = serde_json::Map::new();
            for (name, value) in fields {
                obj.insert(name.clone(), cell_to_json(value));
            }
            serde_json::Value::Object(obj)
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::security::Config;
    use std::fs;
    use tempfile::tempdir;

    /// Extracts the JSON envelope from a CallToolResult's single text block.
    fn envelope(result: &CallToolResult) -> serde_json::Value {
        let text = result
            .content
            .first()
            .expect("result has one content block")
            .as_text_content()
            .expect("content is text")
            .text
            .clone();
        serde_json::from_str(&text).expect("envelope is valid JSON")
    }

    #[test]
    fn envelope_ok_has_expected_shape() {
        let result = envelope_ok(serde_json::json!({"a": 1}), serde_json::json!({"n": 2}));
        let env = envelope(&result);
        assert_eq!(env["ok"], true);
        assert_eq!(env["data"]["a"], 1_i64);
        assert_eq!(env["metadata"]["n"], 2_i64);
        assert!(env.get("error").is_none());
    }

    #[test]
    fn envelope_err_has_expected_shape() {
        let err = DuckAnythingError::PathValidation("nope".to_string());
        let result = envelope_err(&err);
        let env = envelope(&result);
        assert_eq!(env["ok"], false);
        assert!(env["error"].as_str().unwrap().contains("invalid path"));
        assert!(env.get("data").is_none());
        assert!(env.get("metadata").is_none());
    }

    #[test]
    fn cell_to_json_maps_all_variants() {
        let cell = Cell::Struct(vec![
            ("id".to_string(), Cell::Int(1)),
            ("big".to_string(), Cell::Int(1 << 53)),
            ("pi".to_string(), Cell::Float(3.5)),
            ("tags".to_string(), Cell::List(vec![Cell::Text("x".to_string())])),
            ("blob".to_string(), Cell::Blob(vec![0x0f, 0xab])),
            ("none".to_string(), Cell::Null),
        ]);
        let v = cell_to_json(&cell);
        assert_eq!(v["id"], 1_i64);
        assert_eq!(v["big"], "9007199254740992"); // |n| >= 2^53 → string
        assert_eq!(v["pi"], "3.5"); // float → string
        assert_eq!(v["tags"][0], "x");
        assert_eq!(v["blob"], "0fab");
        assert!(v["none"].is_null());
    }

    #[tokio::test]
    async fn load_file_and_run_sql_happy_path() {
        let dir = tempdir().unwrap();
        let csv = dir.path().join("data.csv");
        fs::write(&csv, "a,b\n1,2\n3,4\n").unwrap();
        let engine = Arc::new(Engine::new(&Config::default()).unwrap());

        // load_file: mounts the CSV and reports view info + column metadata.
        let load = LoadFileTool {
            file: csv.to_string_lossy().into_owned(),
            alias: None,
        };
        let result = load.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], true, "load_file failed: {env}");
        assert_eq!(env["data"]["view_name"], "data");
        assert_eq!(env["data"]["format"], "csv");
        assert_eq!(env["data"]["file_path"], csv.to_string_lossy().as_ref());
        let cols = env["metadata"]["columns"].as_array().unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0]["name"], "a");
        assert_eq!(cols[0]["type"], "BIGINT");
        assert_eq!(cols[1]["name"], "b");

        // run_sql: returns rows as a JSON array plus query metadata.
        let query = RunSqlTool {
            sql: "SELECT * FROM data".to_string(),
            max_rows: None,
            timeout_secs: None,
        };
        let result = query.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], true, "run_sql failed: {env}");
        let data = env["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["a"], 1_i64);
        assert_eq!(data[1]["b"], 4_i64);
        assert_eq!(env["metadata"]["columns"][0], "a");
        assert_eq!(env["metadata"]["returned_rows"], 2_i64);
        assert_eq!(env["metadata"]["truncated"], false);
        assert!(env["metadata"]["elapsed_ms"].is_number());
        assert!(env["metadata"].get("total_rows").is_none());
    }

    #[tokio::test]
    async fn run_sql_respects_max_rows_and_reports_truncation() {
        let dir = tempdir().unwrap();
        let csv = dir.path().join("data.csv");
        fs::write(&csv, "a,b\n1,2\n3,4\n").unwrap();
        let engine = Arc::new(Engine::new(&Config::default()).unwrap());

        let load = LoadFileTool {
            file: csv.to_string_lossy().into_owned(),
            alias: None,
        };
        let result = load.call_tool(&engine).await;
        assert_eq!(envelope(&result)["ok"], true);

        let query = RunSqlTool {
            sql: "SELECT * FROM data".to_string(),
            max_rows: Some(1),
            timeout_secs: None,
        };
        let result = query.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], true, "run_sql failed: {env}");
        assert_eq!(env["data"].as_array().unwrap().len(), 1);
        assert_eq!(env["metadata"]["truncated"], true);
        assert_eq!(env["metadata"]["returned_rows"], 1_i64);
        assert_eq!(env["metadata"]["total_rows"], 2_i64);
    }

    #[tokio::test]
    async fn run_sql_timeout_returns_actionable_error() {
        let engine = Arc::new(Engine::new(&Config::default()).unwrap());
        // A recursive CTE that will not finish within 1s (no table needed).
        let query = RunSqlTool {
            sql: "WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x < 100000000) SELECT count(*) FROM t".to_string(),
            max_rows: None,
            timeout_secs: Some(1),
        };
        let result = query.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], false, "expected timeout failure, got: {env}");
        let err = env["error"].as_str().unwrap();
        assert!(err.contains("timed out after 1s"), "error: {err}");
        assert!(err.contains("approx_count_distinct"), "error: {err}");
    }

    #[tokio::test]
    async fn run_sql_timeout_secs_clamped_to_max() {
        let engine = Arc::new(Engine::new(&Config::default()).unwrap());
        // A huge timeout is silently clamped to MAX_TOOL_TIMEOUT_SECS; a fast
        // query still succeeds (no panic, no hang).
        let query = RunSqlTool {
            sql: "SELECT 1".to_string(),
            max_rows: None,
            timeout_secs: Some(5000),
        };
        let result = query.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], true, "run_sql failed: {env}");
        assert_eq!(env["data"][0]["1"], 1_i64);
    }

    #[tokio::test]
    async fn run_sql_zero_timeout_uses_default() {
        let engine = Arc::new(Engine::new(&Config::default()).unwrap());
        // timeout_secs = 0 is treated as "use the server default" (not an
        // immediate timeout), so a fast query succeeds.
        let query = RunSqlTool {
            sql: "SELECT 1".to_string(),
            max_rows: None,
            timeout_secs: Some(0),
        };
        let result = query.call_tool(&engine).await;
        let env = envelope(&result);
        assert_eq!(env["ok"], true, "run_sql failed: {env}");
    }
}