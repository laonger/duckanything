# DuckAnything

使用 Rust 实现的 DuckDB 数据查询工具：把本地 CSV / JSON / Parquet 文件挂载为只读 DuckDB 视图，然后用 SQL 查询。

提供两种使用方式：

- **CLI**：单条命令直接查询文件，适合临时分析、脚本管道
- **MCP Server**：以 stdio MCP 服务运行，供 Claude Desktop / Cursor 等 MCP 客户端调用，让 AI 直接查询本地数据

## 安装

需要 Rust 工具链（rustup）。

```sh
git clone <repo-url>
cd DuckAnything
cargo build --release
# 二进制位于 target/release/duckanything（若设置了 CARGO_TARGET_DIR 则在 $CARGO_TARGET_DIR/release/）
```

## 快速上手（CLI）

```sh
# 查看文件 schema（视图名 = 文件名去扩展名）
duckanything describe sales.csv

# 运行 SQL —— 表名直接用文件名 stem
duckanything query sales.csv "SELECT region, sum(amount) AS total FROM sales GROUP BY region"

# 切换输出格式：table（默认）/ json / markdown / csv
duckanything query sales.csv "SELECT * FROM sales LIMIT 10" --format json
duckanything query sales.csv "SELECT * FROM sales LIMIT 10" --format csv
```

示例输出：

```
$ duckanything describe demo.csv
view: demo
region	VARCHAR	east,west,east
amount	BIGINT	10,20,30

$ duckanything query demo.csv "SELECT region, sum(amount) AS total FROM demo GROUP BY region ORDER BY total DESC"
region total
------ -----
east   40
west   20
```

说明：

- 支持 `.csv` / `.json` / `.parquet` 三种扩展名，其他格式会报错并列出支持的格式
- 文件以**只读**方式挂载，SQL 仅允许 `SELECT` / `WITH` / `FROM`，写入与 DDL 会被拒绝
- 视图名即文件名去扩展名（`/tmp/sales.csv` → `FROM sales`），挂载多个文件时用各自的名字查询

## MCP Server 模式

```sh
duckanything serve
```

通过 stdio 通信，客户端配置示例（以 Claude Desktop 为例）：

```json
{
  "mcpServers": {
    "duckanything": {
      "command": "/path/to/duckanything",
      "args": ["serve"]
    }
  }
}
```

暴露给 MCP 客户端的四个工具：

| 工具 | 说明 |
|------|------|
| `load_file` | 挂载本地数据文件（CSV/JSON/Parquet）为只读视图 |
| `describe` | 查看已挂载视图的列名、DuckDB 类型及每列最多三个样本值 |
| `run_sql` | 对已挂载视图执行只读 SQL（SELECT/WITH/FROM） |
| `list_files` | 列出当前已挂载的文件及其视图名 |

典型流程：AI 先调 `load_file` 挂载文件 → `describe` 看结构 → `run_sql` 查询。

> **注意**：当前基于 rust-mcp-sdk 2.0（MCP 协议 2026-07-28），该版本协议已用 `server/discover` 取代 `initialize` 握手。部分仍发送 `initialize` 的旧版 MCP 客户端可能握手失败，需等客户端适配新版协议。

## 开发

```sh
cargo test          # 运行全部单元测试
cargo run -- query demo.csv "SELECT 1"   # 本地调试 CLI
```
