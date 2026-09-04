# AGENTS.md

## What this is

`duckanything`: Rust CLI + MCP server that mounts local CSV/JSON/Parquet files as read-only DuckDB views and runs SQL against them. One binary, four subcommands: `serve` (MCP over stdio), `query`, `describe`.

## Commands

```sh
cargo build
cargo test            # 47 unit tests, fast, no external services needed
cargo run -- query <file> "SELECT ..."          # run one SQL query
cargo run -- describe <file>                    # print schema
cargo run -- serve                              # MCP stdio server
```

- Query output format: `--format json|table|markdown|csv` (default `table`).
- The view name is the file stem: `/tmp/sales.csv` → `FROM sales` (not `FROM sales.csv`).
- Unsupported extensions are rejected with a clear error listing supported formats.

### Environment quirk (this machine)

`CARGO_TARGET_DIR` is set, so build artifacts land in `$CARGO_TARGET_DIR/debug/` (here: `/Users/laonger/.cargo/temp/debug/duckanything`), **not** `./target`. Don't be confused when `target/` doesn't exist after a successful build.

## MCP layer gotchas (rust-mcp-sdk 2.0)

Hard-earned findings from tracing the SDK source — these are not obvious from our code:

- **Protocol `2026_07_28` removed `initialize` and made `_meta` mandatory on every request.** The SDK's schema crate defaults to the `latest` (2026-07-28) protocol, where `InitializeRequest` no longer exists (replaced by `server/discover`). Additionally, every request (including `tools/call`) requires `params._meta` with prefixed keys `io.modelcontextprotocol/clientCapabilities` and `io.modelcontextprotocol/protocolVersion`. Any request failing these checks falls through the untagged serde enum to `Custom` and gets `-32601 "No handler is implemented for custom requests."` — the server *looks* broken but isn't. Options: implement `handle_custom_request` on the handler for legacy clients, or enable an older schema feature (`2025_06_18` etc.) on `rust-mcp-schema`.
- **Logging goes to stderr, never stdout.** stdout is the JSON-RPC channel in serve mode. `tracing_subscriber::fmt()` defaults to stdout — `src/core/logging.rs` must keep the explicit `.with_writer(std::io::stderr)`. Level is controlled by `DUCKANYTHING_LOG` (default `warn`, silent by default).
- SDK dependency is pinned with `default-features = false, features = ["server", "stdio", "macros"]` on purpose — defaults pull in `auth` (reqwest/jsonwebtoken) and HTTP transports we don't need.
- `ServerHandler` impls require `#[async_trait]` (hence the `async-trait` dep).
- Tool handlers return `CallToolResult`; `handle_call_tool_request` must wrap them in `ServerResult::CallToolResult(...)` (compile error otherwise).

## Layout

- `src/main.rs` — clap entrypoint dispatching subcommands.
- `src/core/` — DuckDB engine, errors, output formatting, security config. Engine is sync; mount/query don't need tokio.
- `src/mcp/` — MCP server assembly (`server.rs`) and tool enum (`tools.rs`). Tools convert from `CallToolRequestParams` via `TryFrom` and call into the shared `Engine`.
- Tests use `tempfile` for fixtures; MCP handler tests exercise the engine boundary, not the stdio wire.
