# DuckAnything 实施工作计划

## 概述

依据 `docs/design/duckanything-design.md`（定稿）拆解为 TDD 步骤。目标：交付 MCP server（stdio，4 tools）+ CLI（query/describe）双模式，core 层共享。每个子步骤 = 一个 commit，完成后代码可编译、可测试。

## 步骤 0：项目脚手架与依赖基线

### 0a：Cargo.toml + 目录骨架 + clap 骨架

- 新建 `Cargo.toml`：duckdb（`bundled` + `json` + `parquet`）、rust-mcp-sdk 2.x（`default-features = false`，features = `server`/`stdio`/`macros`）、tokio、clap(derive)、serde/serde_json、thiserror；dev-deps: tempfile。
- 创建目录骨架：`src/core/{mod,engine,format,security,error}.rs`、`src/mcp/{mod,server,tools}.rs`，全部建为空 stub（后续 Wave 并行 lane 不再触碰共享的 mod.rs）。
- `main.rs`：clap 定义 `serve`/`query`/`describe` 三子命令 + 全局 `--format`；serve 暂返回「未实现」错误。

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | help 列出三个子命令 | `--help` 输出含 serve/query/describe |
| 2 | serve 未实现报错 | serve 调用返回明确「未实现」错误信息 |

#### 🟢 绿 — 实现
Cargo.toml 依赖、模块骨架、clap 子命令解析。

#### 🧪 测试 → 🔍 类型检查
`cargo build && cargo test`（首次 bundled 编译 5–15 分钟，需 C++ 工具链）

#### 📦 提交
`chore: 项目脚手架与依赖基线`

## 步骤 1：core/error.rs 统一错误

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | 各变体 Display 文本 | 路径不存在/不支持格式/只读拦截/超时等错误信息可读且稳定 |
| 2 | 超时与行数超限区分 | Timeout 是错误；RowLimit 是信息性类型 |

#### 🟢 绿 — 实现
`DuckAnythingError` 枚举（thiserror）：Io、PathValidation、UnsupportedFormat、ReadOnlyViolation、Sql、Timeout、Config。

#### 🧪 测试 → 🔍 类型检查
`cargo test -p duckanything core::error`

#### 📦 提交
`feat(core): 统一错误类型`

## 步骤 2a：core/security.rs（Wave 2 Lane A）

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | SELECT/WITH/FROM 前缀通过 | 含尾部空白与分号 trim |
| 2 | 非 SELECT 拒绝 | INSERT/UPDATE/DROP/CREATE/PRAGMA/CALL 全拒 |
| 3 | 大小写与空白变体 | `  select 1` / `WITH x AS` 通过 |
| 4 | 路径校验：存在且是文件 | 目录、不存在路径拒绝 |
| 5 | allow-dir 白名单 | 白名单外拒绝；`..` 穿越拒绝 |
| 6 | 默认不限制目录 | 未配置 allow-dir 时仅要求文件存在可读 |
| 7 | 配置三层覆盖 | 默认 100/30 → env 覆盖 → CLI 覆盖（单测试函数内串行改 env，避免并行竞态） |

#### 🟢 绿 — 实现
`is_readonly_sql`、`validate_path`、`Config::load(default, env, cli)`。

#### 🧪 测试 → 🔍 类型检查
`cargo test core::security`

#### 📦 提交
`feat(core): 安全校验与分层配置`

## 步骤 2b：core/format.rs（Wave 2 Lane B）

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | NULL → null | 空值映射 |
| 2 | 小整数保真 / 大整数转字符串 | ≥2^53 转 string |
| 3 | DECIMAL/浮点转字符串 | 规避 IEEE754 噪声 |
| 4 | 时间戳转 ISO 8601 | 日期/时间字符串 |
| 5 | LIST/STRUCT → 数组/对象；BLOB → hex | 嵌套与二进制 |
| 6 | json 输出结构 | 每行一个对象的数组 |
| 7 | table 对齐 | CLI 文本表格列对齐 |
| 8 | markdown / csv 输出 | 格式正确性 |
| 9 | 截断元信息 | max_rows 截断后 truncated=true、returned_rows 准确 |

#### 🟢 绿 — 实现
`Format` 枚举、`format_rows` 入口、类型映射器（四种输出共享映射）。

#### 🧪 测试 → 🔍 类型检查
`cargo test core::format`

#### 📦 提交
`feat(core): 结果格式化与类型映射`

## 步骤 3：core/engine.rs

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | 挂载 CSV/JSON 成功 | 视图可查，列类型正确 |
| 2 | 挂载 Parquet 成功 | 测试内用 duckdb `COPY ... TO` 生成 fixture |
| 3 | 不支持扩展名报错 | `.xlsx`/`.txt` → UnsupportedFormat |
| 4 | 视图名派生：常规 | `data.csv` → `data`；`my-data.json` → `my_data` |
| 5 | 视图名派生：中文/空格清理为空 → 回退 | `2023Q4 报表.csv` → `view_1` |
| 6 | 视图名派生：数字开头回退 | `2023.csv` → `view_N` |
| 7 | 重名冲突追加 `_2` | 同名不同目录文件 |
| 8 | 保留字冲突 | 与 DuckDB 保留字撞名时加后缀 |
| 9 | alias 校验 | 非法 alias 直接报错不静默清理 |
| 10 | 查询 happy path | select 返回 Arrow batches → json |
| 11 | 多语句注入被拒 | `select 1; drop view x` 引擎级报错（prepare） |
| 12 | 非 SELECT 被拒 | 与 security 集成 |
| 13 | 超时触发 interrupt | 慢查询 + 短超时 → Timeout 错误，连接随后仍可用 |
| 14 | list_views | 返回视图名 + 文件路径 + 格式 |

#### 🟢 绿 — 实现
`Engine`（连接 + Mutex）、`mount_file`、`run_query`（prepare 单语句 + spawn_blocking + interrupt 超时）、`describe_view`（列名/类型 + 3 样例值）、`derive_view_name`。

#### 🧪 测试 → 🔍 类型检查
`cargo test core::engine`

#### 📦 提交
`feat(core): DuckDB 引擎封装`

## 步骤 4a：mcp/tools.rs（Wave 4 Lane A）

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | 四个 tool 的 inputSchema | load_file/describe/run_sql/list_files 参数定义正确 |
| 2 | load_file 信封 | ok:true、data.view_name/file_path/format、metadata.columns |
| 3 | describe 信封 | 列名/类型/3 样例值 |
| 4 | run_sql 信封 | data 行数组、metadata.returned_rows/elapsed_ms |
| 5 | run_sql 截断信封 | truncated:true + total_rows |
| 6 | 错误路径信封 | ok:false + 错误信息（路径非法/只读拦截/SQL 错误） |
| 7 | list_files 信封 | 视图列表含 columns |

#### 🟢 绿 — 实现
信封构造、tool schema 定义、handler 分发（Arc<Engine> + spawn_blocking）。

#### 🧪 测试 → 🔍 类型检查
`cargo test mcp::tools`

#### 📦 提交
`feat(mcp): tools 定义与处理器`

## 步骤 4b：mcp/server.rs（Wave 4 Lane A，接 4a）

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | list_tools 返回 4 个工具 | 名称与 schema 齐全 |
| 2 | call_tool 分发 | 未知工具名报错；已知工具走对应 handler |
| 3 | run_server 可构造 | Handler 能以 Arc<Engine> 构造 |

#### 🟢 绿 — 实现
`ServerHandler` 实现（handle_list_tools_request / handle_call_tool_request）、stdio transport 装配、`run_server(config)` 入口。

#### 🧪 测试 → 🔍 类型检查
`cargo test mcp::server`

#### 📦 提交
`feat(mcp): server 装配`

## 步骤 4c：main.rs CLI（Wave 4 Lane B，与 Lane A 并行，仅触碰 main.rs）

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | query happy path | `query fixture.csv "select * from fixture"` 输出对齐表格 |
| 2 | query --format json | JSON 数组输出 |
| 3 | query 非 SELECT 报错退出 | 退出码非 0 + 明确错误 |
| 4 | describe 输出 schema | 列名/类型/样例值 |
| 5 | env 配置生效 | DUCKANYTHING_MAX_ROWS=1 时截断提示 |

#### 🟢 绿 — 实现
main.rs 分发：query/describe 走 core（临时挂载 → 执行 → 输出）。

#### 🧪 测试 → 🔍 类型检查
`cargo test --test cli`（用 `CARGO_BIN_EXE_duckanything` 起子进程）

#### 📦 提交
`feat(cli): query/describe 子命令`

## 步骤 5：serve 接线 + 端到端验收

#### 🔴 红 — 测试
| # | 测试用例 | 简明描述 |
|---|---------|---------|
| 1 | initialize 握手 | 子进程 JSON-RPC initialize 返回 serverInfo |
| 2 | tools/list = 4 | 工具齐全 |
| 3 | load_file → describe → run_sql 全链路 | 对 fixture CSV 完成三步 |
| 4 | 多语句注入拒绝 | `select 1; drop view x` → ok:false |
| 5 | 截断元信息 | max_rows 触发 total_rows |
| 6 | 结构化错误不 panic | 路径不存在 → ok:false，进程存活可继续服务 |

#### 🟢 绿 — 实现
main.rs serve 子命令接 `run_server`。

#### 🧪 测试 → 🔍 类型检查
`cargo test --test mcp_e2e && cargo test`

#### 📦 提交
`feat(mcp): serve 接线与端到端验收`

## Wave 并行策略

### Wave 0：脚手架（1 任务，串行）
任务 A: 0a

### Wave 1：基础类型（1 任务，串行）
任务 A: 1（error 是所有后续模块的依赖）

### Wave 2：core 独立模块（2 任务，并行）
任务 A: 2a security.rs
任务 B: 2b format.rs
（两者只触碰各自文件；mod.rs 骨架已在 0a 建好）

### Wave 3：引擎（1 任务，串行）
任务 A: 3（依赖 security + format）

### Wave 4：MCP 与 CLI（2 Lane 并行）
Lane A: 4a → 4b（mcp/tools.rs → mcp/server.rs）
Lane B: 4c（main.rs CLI）
（文件级无交集：Lane A 只碰 src/mcp/*，Lane B 只碰 main.rs）

### Wave 5：接线与验收（1 任务，串行）
任务 A: 5

## 依赖关系总览

```
0a ─→ 1 ─→ 2a ─→ 3 ─→ 4a ─→ 4b ─→ 5
        └─→ 2b ──┘        └─→ 4c ──┘
```

## 测试覆盖汇总

| Wave | 并行数 | 模块/包 | 步骤 | 测试用例 |
|------|--------|---------|------|---------|
| 0 | 1 | 脚手架 | 0a | 2 |
| 1 | 1 | core/error | 1 | 2 |
| 2 | 2 | core/security, core/format | 2a, 2b | 7 + 9 |
| 3 | 1 | core/engine | 3 | 14 |
| 4 | 2 lanes | mcp/tools, mcp/server, cli | 4a, 4b, 4c | 7 + 3 + 5 |
| 5 | 1 | 端到端 | 5 | 6 |

合计 55 个测试用例，覆盖设计验收标准 §7 全部 6 条。

## 备注

- **首次构建慢**：duckdb `bundled` 从源码编译 C++，5–15 分钟，一次性成本；Wave 0 完成后增量构建正常。
- **Parquet fixture**：测试内用 duckdb `COPY (SELECT ...) TO 'x.parquet'` 生成，不引入外部工具。
- **env 配置测试竞态**：`std::env::set_var` 全局生效，配置类测试放在单测试函数内串行执行。
- **超时测试**：用笛卡尔积大范围 `range()` 查询制造慢查询，断言只看错误类型与连接可用性，不做耗时精确断言（避免 flaky）。
- **rust-mcp-sdk feature**：必须 `default-features = false`，否则拖入 auth/reqwest/sse 依赖树。
- **已知限制**（有意不做，见设计 §6）：HTTP/SSE transport、写操作、远程路径、Excel、持久化 ATTACH、认证。
