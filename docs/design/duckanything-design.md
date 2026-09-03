# DuckAnything 技术设计文档

## 1. 目标

一句话：**让任何 MCP 客户端（如 Claude Desktop、Cursor）通过自然语言驱动 DuckDB 分析本地数据文件。**

详细描述：DuckAnything 是一个 Rust 实现的 MCP server，客户端把本地文件路径（CSV/JSON/Parquet）传给 server，server 用 DuckDB 引擎执行 SQL 查询文件内容。同时提供 CLI 模式，不依赖 MCP 客户端即可直接查询。

### 1.1 设计原则

- **文件是数据源，DuckDB 是引擎**：不导入数据，DuckDB 直接查询文件（`read_parquet`/`read_csv` 等），利用 DuckDB 原生能力，简单可靠。
- **一个进程一个 DuckDB 实例**：in-memory 数据库 + 视图挂载文件，避免多连接管理复杂性。
- **CLI 和 MCP 共享同一核心库**：CLI 是核心库的薄封装，两种模式行为一致，核心逻辑只写一次。
- **安全边界**：路径校验 + 只读 + 结果行数上限 + 查询超时。

## 2. 模块划分

```
src/
├── main.rs          # 入口：解析 CLI 参数，分发到 MCP server 或 CLI 模式
├── core/
│   ├── mod.rs       # 模块导出
│   ├── engine.rs    # DuckDB 引擎封装（连接管理、视图挂载、查询执行）
│   ├── format.rs    # 查询结果格式化（JSON / table / markdown / csv）
│   ├── security.rs  # 路径校验、行数上限、查询超时
│   └── error.rs     # 统一错误类型
└── mcp/
    ├── mod.rs       # 模块导出
    ├── server.rs    # MCP server 装配（rust-mcp-sdk、StdioTransport）
    └── tools.rs     # MCP tools 定义与分发
```

## 3. 各模块详细设计

### 3.1 core/engine.rs — DuckDB 引擎封装

**职责**：持有 DuckDB 连接，提供「挂载文件为视图」「执行 SQL」「列出已挂载文件」三类核心能力。

**关键行为**：

- **挂载文件**：根据扩展名判断格式，DuckDB 原生支持 CSV/JSON/Parquet（`read_csv_auto`/`read_json_auto`/`read_parquet`）。挂载 = `CREATE VIEW name AS SELECT * FROM read_xxx('path')`。视图名由文件名派生（如 `data.csv` → `data`）。Excel 支持推迟（见第 6 节，需要动态加载扩展，二进制体积和兼容性问题多），不在第一版关键行为内。
- **视图名规则（讨论定稿，方案 A）**：取文件名去扩展名，非 ASCII 字母/数字/下划线的字符转 `_`，连续 `_` 合并，trim 首尾 `_`，整体转小写。若结果为空或以数字开头，回退 `view_<序号>`。与已挂载视图或 DuckDB 保留字冲突时追加 `_2`、`_3`。`load_file` 的 `alias` 参数走同一校验（不合法直接报错，不做静默清理）。
- **执行查询**：任意 SELECT 语句在已挂载视图上执行。支持用 `query_arrow` 拿到 Arrow batches，再统一格式化输出。
- **可选持久化**：默认 in-memory；如果用户传入 db 文件路径，`ATTACH` 它，查询结果可落表。这个是可选增强，第一版可以不做。

**设计决策**：
- 为什么用视图而不是 `CREATE TABLE AS`：视图是惰性读取，不占内存，多次查询同一文件时 DuckDB 自身有缓存；CTAS 会把数据物化进内存，大文件会 OOM。
- 为什么单连接：MCP server 是单客户端场景（stdio），单连接 + Mutex 串行执行即可，无需连接池。

### 3.2 core/format.rs — 结果格式化

**职责**：把 Arrow batches / DuckDB rows 转成不同格式的输出。

- `json`（默认）：数组形式，每行一个对象。MCP 模式默认用这个。
- `table`：对齐文本表格，CLI 模式默认用这个（人眼友好）。
- `markdown` / `csv`：可选，CLI 模式下方便复制粘贴。
- 统一由 `format(rows, format, max_rows)` 入口分发。

**类型映射原则（讨论定稿，方案 A）**：NULL → `null`；整数按 Arrow 类型保真，超出 JS 安全整数范围（|n| ≥ 2^53）转字符串；DECIMAL 与浮点转字符串（规避 IEEE754 精度噪声）；日期/时间戳转 ISO 8601 字符串；LIST/STRUCT → JSON 数组/对象；BLOB → hex 字符串。json/table/markdown/csv 四种输出格式共享同一映射。

### 3.3 core/security.rs — 安全与限制

**职责**：三种保护机制，全部在 core 层强制，CLI 和 MCP 都走这套。

- **路径校验**：文件必须存在、可读；可选白名单目录（通过 CLI 参数 `--allow-dir` 配置）。拒绝目录穿越。默认行为以「配置来源」小节定稿为准：默认不限制路径，仅要求文件存在可读。
- **行数上限**：查询结果默认最多返回 N 行（如 100），超出截断并提示总行数。防止 LLM 上下文爆炸。
- **查询超时**：单条查询超时时间（默认 30s）。实现方式（讨论定稿）：spawn_blocking 执行 + tokio timeout 包裹，超时后另起任务调用 DuckDB 的 `Connection::interrupt` 真正取消查询（查询以错误结束，中断结果丢弃）。若 interrupt 实测有坑，fallback 为「连接作废 + 下次请求重建连接和视图」。
- **只读保证**：连接以只读语义使用——不执行 `CREATE TABLE`/`DROP` 等写操作（第一版拒绝非 SELECT 语句；后续可引入可写模式）。

**只读校验规则（讨论定稿）**：前缀白名单（SELECT/WITH/FROM，检查前先 trim 尾部空白与分号）+ 执行走 `prepare()` 单语句强制（duckdb-rs 的 prepare 遇多语句 SQL 直接报错，引擎级保证）。不用字符串级分号检查（会误伤字符串字面量内的分号）。

**设计决策**：第一版做「只读 + 附加白名单可选」，不做用户认证（MCP stdio 场景无意义）。

**配置来源（讨论定稿，方案 A 分层配置）**：所有限制参数统一走「内置默认值 → 环境变量 → CLI 参数」三层覆盖。环境变量：`DUCKANYTHING_MAX_ROWS`（默认 100）、`DUCKANYTHING_TIMEOUT_SECS`（默认 30）、`DUCKANYTHING_ALLOW_DIR`（默认不限制路径，仅要求文件存在可读）。CLI 参数优先级最高；MCP 模式下生效默认值与环境变量（MCP 客户端配置原生支持 env 注入），无配置文件。

### 3.4 core/error.rs — 统一错误

**职责**：定义 `DuckAnythingError` 枚举，覆盖：IO 错误、路径校验失败、不支持的文件格式、SQL 执行错误、超时、行数超限（信息性，非错误）。用 `thiserror` 派生。MCP 层和 CLI 层各自 map 到自己的错误表示。

### 3.5 mcp/server.rs — MCP server 装配

**职责**：用 rust-mcp-sdk 装配 server。

- transport：**第一版只支持 StdioTransport**（Claude Desktop / Cursor 都是 stdio）。HTTP/SSE transport 后续按需加。
- 注册 `ServerHandler`，实现 `handle_list_tools_request` / `handle_call_tool_request`。
- server 需要访问 core 引擎，通过 `Arc<Engine>` 注入 handler（handler 是 async，DuckDB 调用需要 `spawn_blocking` 包裹，避免阻塞 tokio runtime）。

### 3.6 mcp/tools.rs — MCP tools 定义

用 `mcp_tool` 宏定义，暴露以下 tools：

| Tool | 输入 | 输出 | 说明 |
|------|------|------|------|
| `load_file` | `path`, `alias`（可选） | 视图名 + schema 摘要 | 挂载文件为视图，返回视图名和列信息 |
| `run_sql` | `sql`（MCP 模式无 `format` 参数，data 固定 JSON 数组；CLI 对应能力用 `--format`） | 格式化结果 + 元信息 | 在已挂载视图上执行 SELECT |
| `list_files` | 无 | 视图名列表 + 各自 schema | 列出当前挂载的所有文件 |
| `describe` | `view` | 列名 + 类型 + 样例值 | 查看视图 schema，帮 LLM 写 SQL |

**设计决策**：
- **不提供 `unload_file`**：MCP server 生命周期 = 进程生命周期，客户端重启即清空，单独卸载是伪需求。如果后续有需求再加。
- `run_sql` 接受任意 SQL（校验只读后执行），而不是拆成多个细粒度 tools（filter/aggregate/group）——细粒度 tools 会把 LLM 的表达力锁死，任意 SQL 才是 DuckDB 的正确打开方式。
- **不提供 `list_dir`**：路径由 LLM 从用户对话上下文拿，不做文件系统枚举（减少攻击面）。

**返回结构（讨论定稿，方案 C 混合信封）**：
- 所有 tool 统一返回信封：`ok`（布尔）、`data`、`metadata`。
- `load_file`：`data` 为 JSON 对象（`view_name`、`file_path`、`format`），`metadata.columns` 为列名/类型数组。**不返回行数**（讨论定稿，方案 A）：挂载保持轻快，行数由 LLM 需要时自行 count。
- `describe`：`data` 为 JSON 数组，每元素含列名、类型、最多 3 个样例值；`metadata.view_name`。
- `run_sql`：`data` 为 JSON 数组（每行一个对象），`metadata` 含 `truncated` 标志、`returned_rows`，截断时必含 `total_rows`（通过一条 count(*) 包裹原查询获取，仅在截断场景发生）、`elapsed_ms`。
- `list_files`：`data` 为 JSON 数组，每元素含 `view_name`、`file_path`、`format`、`columns`。

### 3.7 CLI 模式

```
duckanything serve                    # MCP server 模式（stdio）
duckanything query <file> "<sql>"     # 单次查询，结果输出到 stdout
duckanything describe <file>          # 查看文件 schema（列名/类型/样例值）
```

CLI 直接调 core 层，格式默认 `table`，`--format` 可切换。用 `clap` derive 风格定义子命令。`describe` 与 MCP `describe` tool 共享同一核心能力（CLI 侧临时挂载 → describe，用户无感），命名与语义完全对齐（讨论定稿，方案 A）。

## 4. 依赖关系

```
main.rs → mcp/server.rs → mcp/tools.rs → core/engine.rs
                              ↓                ↓
                        core/format.rs ← core/security.rs
                              ↓
                        core/error.rs
```

- `main.rs` 用 clap 解析子命令后，`serve` 走 MCP 装配，`query`/`describe` 直接调 core。
- `mcp/tools.rs` 依赖 core 的 Engine + format + security；core 不反向依赖 mcp。

模块依赖关系（分层描述）：
- `main.rs` 依赖 `mcp/server.rs`（serve 子命令）与 `core`（query/describe 子命令）。
- `mcp/server.rs` 依赖 `mcp/tools.rs`（工具注册）与 `core/engine.rs`（Engine 构造与注入）。
- `mcp/tools.rs` 依赖 `core/engine.rs`、`core/format.rs`、`core/security.rs`、`core/error.rs`。
- `core/format.rs` 与 `core/security.rs` 依赖 `core/error.rs`；core 内部无相互反向依赖。

## 5. 核心数据流

### MCP 模式：load_file

```
客户端 → MCP request → tools.rs（参数校验）
  → security.rs（路径校验、格式判断）
  → engine.rs（CREATE VIEW）
  → 返回视图名 + schema 摘要（列名/类型，供 LLM 后续写 SQL）
```

### MCP 模式：run_sql

```
客户端 → MCP request（SQL 文本）
  → tools.rs → security.rs（只读校验：语句必须以 SELECT/WITH/FROM 开头）
  → engine.rs（spawn_blocking 执行，query_arrow 拿 batches）
  → format.rs（默认 json，截断到 max_rows）
  → CallToolResult 返回
```

### CLI 模式：query

```
命令行参数 → security.rs（校验） → engine.rs（挂载 + 查询）
  → format.rs（默认 table） → stdout
```

## 6. 不做什么（第一版）

- ❌ HTTP/SSE transport（只做 stdio）
- ❌ 写操作（INSERT/CREATE TABLE/UPDATE/DELETE 全拒绝）
- ❌ 多文件 JOIN 之外的复杂会话管理（同一进程内多视图 JOIN 天然支持，无需额外设计）
- ❌ 远程文件（http/s3 路径）——DuckDB 本身支持，但第一版只允许本地路径，减少安全审查面
- ❌ 持久化 / db 文件 ATTACH
- ❌ Excel 格式（需要动态加载扩展，二进制体积和兼容性问题多，后续加）
- ❌ 认证、多租户

## 7. 验收标准

1. `cargo run -- serve` 后，Claude Desktop 配置 stdio MCP 能完成：load_file → describe → run_sql 三步查询 CSV/JSON/Parquet 文件。
2. `cargo run -- query data.csv "SELECT * FROM data LIMIT 5"` 输出对齐的文本表格。
3. 非 SELECT 语句（如 `DROP TABLE`）被拒绝并返回明确错误。
4. 超过 max_rows 的结果被截断并提示总行数。
5. 路径不存在 / 不支持的扩展名 → 返回结构化错误，进程不 panic。
6. `cargo test` 覆盖：engine（挂载/查询/错误）、security（路径穿越/只读拦截）、format（各格式输出）。

## 8. 构建注意（调研发现）

- duckdb crate 的 `json` / `parquet` feature 均强制启用 `bundled`（从源码编译 DuckDB C++），首次构建预计 5–15 分钟，需要 C++ 工具链（macOS clang / Linux gcc）。此为一次性成本，后续增量构建不受影响。
- duckdb crate 内置的 Excel 支持（vtab-excel）已标记 deprecated，与「Excel 推迟」决策一致。
- rust-mcp-sdk 2.0.0 建议 `default-features = false`，只启用 `server` + `stdio` + `macros`，避免拖入 auth/reqwest/HTTP 传输栈（default 开了 sse/streamable-http/auth 等一堆无关特性）。
