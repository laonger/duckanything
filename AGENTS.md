# DuckAnything — AGENTS.md

## Project Identity

Rust DuckDB MCP server. Clients pass a file path (CSV, Parquet, JSON, etc.) and query/manipulate it via DuckDB SQL over the MCP protocol.

Two modes (per README):
- **Server mode**: runs as an MCP protocol server
- **CLI mode**: runs as a CLI tool

## Current State

**Early stage.** `Cargo.toml` and `src/main.rs` (Hello World boilerplate) exist. No dependencies, no tests, no CI, no commits yet.

## Build & Run

```bash
cargo build
cargo run
```

## Tech Stack

- **Rust edition 2024** (`edition = "2024"` in Cargo.toml). Requires a Rust toolchain that supports it (1.85+). Do not downgrade to edition 2021.
- **Toolchain**: rustc 1.96.0, cargo 1.96.0 (as of repo setup)
- Expected key deps: `duckdb` crate, `mcp` or similar server SDK

## Repo Conventions

- `.visp/rules/` for repo-local visp rules; `.visp/skills/` for custom skills (both currently empty).
- `.gitignore` only contains `/target` — extend as needed.
- No Cargo.lock tracked yet (no commits).

## What to Remember

- README.md is the authoritative project description (currently bilingual CN/EN).
- Edition 2024 is non-negotiable — don't rewrite to 2021.
- MCP tool schemas require strict JSON Schema; DuckDB results can contain arbitrary types — plan serialization carefully.
- DuckDB version matters for file format support (Parquet, CSV, Excel, etc.).