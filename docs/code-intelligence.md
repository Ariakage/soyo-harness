# 项目索引与沙箱 LSP 代码智能（F-17，D-087）

**状态：已在 `soyo-core`、`soyo-app`、`soyo-cli`、`soyo-tui`、`soyo-ipc`、`soyo-host` 与 `desktop` 实现，当前为严格默认关闭（opt-in，`[code_intel].enabled = false`、`[code_intel.lsp].enabled = false`）。本文记录外部 SQLite 符号索引、沙箱化 LSP 3.17 客户端、6 个只读代码工具及验证边界。**

## 定位与双层隔离架构

`F-17` 为 `CODE`、`BUILD` 与 `PLAN` 提供结构化项目符号检索和语言服务器语义分析，同时把持久索引与语言服务器构建副作用完全隔离在目标工作区之外：

- **默认零漂移（opt-in）**：`[code_intel].enabled = false` 时，不建库、不注册 `code_*` 工具、不注入 [code-intel 提示段](../crates/soyo-core/prompts/v1/code-intel.md) 或 `soyo_code_intel_v1` 上下文信封；模型伪造的 `code_*` 调用以 `unknown_tool` 拒绝，既有运行行为保持字节级一致。
- **Layer 1（工作区外 SQLite 增量符号索引）**：`WorkspaceIndex` 将数据库建立在 `<cache_dir>/index/<sha256(workspace)>/project-index.sqlite3`（目录 `0700`、DB `0600`），通过 `validate_external_cache_dir` 拒绝任何与 `<workspace>` 重叠的路径——即使在 `PLAN` 模式下也保持工作区只读干净。
- **Layer 2（沙箱化 LSP 3.17 JSON-RPC 2.0 客户端）**：`LspManager` 通过 `PreparedEnvironment`（macOS Seatbelt / Linux Bubblewrap）启动配置的语言服务器子进程，工作区只出现在 `read_roots` 与 `write_denied_paths`，仅 `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/` 可写，网络策略固定为 `NetworkPolicy::Deny`。

## Layer 1：忽略规则、权限防线与增量失效

- **过滤与路径防线**：扫描工作区采用 `ignore::WalkBuilder` 遵守 `.gitignore` 与 `.ignore`；**强制跳过** `.git` 仓库控制目录（`is_repository_control_path`）以及 `ReadTools::check_private_path` 命中的保护路径（如凭据、`env`、私钥与外部缓存路径），即使在 `Bypass` 权限模式下也有效。索引建表、文件读取与 DB 打开均拒绝符号链接。
- **支持语言与符号提取**：内建词法/结构抽取覆盖 `.rs`（`rust`）、`.ts`/`.tsx`（`typescript`/`typescriptreact`）、`.js`/`.mjs`/`.cjs`/`.jsx`（`javascript`/`javascriptreact`）、`.py`/`.pyi`（`python`）、`.go`（`go`）、`.md`/`.markdown`（`markdown`）、`.toml`（`toml`）、`.json`（`json`）、`.yaml`/`.yml`（`yaml`），提取 `module`/`struct`/`enum`/`trait`/`interface`/`class`/`function`/`method`/`const`/`static`/`type_alias`/`variable`/`heading`。
- **增量刷新与自动失效**：按 `(size, mtime_ns, sha256)` 仅重解析发生变更的文件并清理已删除条目；在运行时 `write_file`、`edit_file`、`apply_patch`、`exec` 与 `subagent_merge` 执行后触发路径级/全量失效，并同步通知已启动的 LSP 会话更新/重新校验文档。

## Layer 2：沙箱化 LSP 进程与 URI 安全校验

- **环境重定向**：启动语言服务器时将 `CARGO_TARGET_DIR`、`XDG_CACHE_HOME`、`HOME`、`TMPDIR` 全部重定向到外部 `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/`，保留只读 `RUSTUP_HOME` / `CARGO_HOME` 与配置声明的只读 `read_roots`（支持诸如 `rust-analyzer` 读取标准库/工具链源码），同时设 `OPENSSL_CONF=/dev/null`（并在 macOS Seatbelt 允许只读 `/private/etc/ssl/openssl.cnf`）。
- **帧与生命周期边界**：单帧限制 8 MiB、stderr 尾部有界 64 KiB、单次响应位置列表上限 100 条；通过 `ManagedProcess` 与取消信号在超时、`PLAN` 切换、Run 退出或 shutdown 时发 `shutdown`/`exit` 并回收进程组。
- **URI 返回过滤**：LSP 返回的全部诊断、定义、引用及编辑建议均重新经 `file://` 解析、工作区包含检查、符号链接拒绝、`.git` 控制路径拒绝与 `ReadTools::check_private_path` 过滤。

## `[code_intel]` 与 `[code_intel.lsp]` 配置边界

配置类型 `CodeIntelSettings` / `LspSettings` / `LspServerSettings`（`crates/soyo-core/src/config/code_intel.rs`）均使用 `#[serde(default, deny_unknown_fields)]`：

| 字段 | 默认值 | 允许上限 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `false` | — | Layer 1 索引与 `code_*` 工具总开关 |
| `inject_context` | `true` | — | 启用时是否按需向头部历史注入 `soyo_code_intel_v1` 低信任导航摘要 |
| `max_context_bytes` | `8192` (8 KiB) | `512..=65536` | `soyo_code_intel_v1` 摘要最大字节数 |
| `max_files` | `10000` | `1..=100000` | 索引遍历与入库文件数上限 |
| `max_file_bytes` | `1048576` (1 MiB) | `1024..=16777216` | 单文件最大参与索引字节数 |
| `max_total_index_bytes` | `67108864` (64 MiB) | `65536..=536870912` | 索引覆盖源码总字节上限 |
| `max_symbols_per_file` | `512` | `1..=4096` | 单文件提取符号上限 |
| `max_query_results` | `50` | `1..=200` | 查询返回条数默认/硬上限 |
| `lsp.enabled` | `false` | — | Layer 2 LSP 子进程开关（需先启用 `code_intel.enabled` 或单独关闭） |
| `lsp.request_timeout_seconds` | `15` | `1..=120` | 单次 LSP 请求超时秒数 |
| `lsp.startup_timeout_seconds` | `30` | `1..=120` | `initialize`/`initialized` 启动握手上界 |
| `lsp.max_diagnostics_per_file` | `100` | `1..=500` | 单文件保留诊断条数上限 |
| `lsp.max_open_documents` | `64` | `1..=256` | 同时向 LSP 打开的文档 LRU 上限 |
| `lsp.max_edit_files` | `32` | `1..=128` | `code_edit_suggestions` 单次涉及文件数上限 |

每个 `[code_intel.lsp.servers.<server_id>]` 声明 `enabled`、`argv`、`languages`、`extensions`、`root_markers`、`env`、`read_roots`（仅只读挂载）与 `initialization_options`。示例见 [examples/config.example.toml](../examples/config.example.toml) 与 [配置组件](configuration.md)。

## 内置代码智能工具（6 个全只读工具）

启用 `[code_intel]` 后，`ToolRegistry` 按 `ToolEffect::FileRead` 注册并在 `Operation::code_intel` 权限边界与并行只读批处理中统一管控下列工具（全部在 `PLAN` 模式下同样只读可用）：

| 工具名 | 核心参数 | 说明 |
| --- | --- | --- |
| `code_search_symbols` | `query`、可选 `kind`/`path`/`limit`/`use_lsp` | 从外部 SQLite 索引（或启用 LSP 时结合 `workspace/symbol`）检索符号 |
| `code_file_outline` | `path` | 返回指定工作区文件的结构化符号大纲与行号范围 |
| `code_definition` | `path`、`line`、`column` | 通过沙箱 LSP `textDocument/definition`（未配 LSP 时优雅降级索引回退）定位定义 |
| `code_references` | `path`、`line`、`column`、可选 `include_declaration` | 通过沙箱 LSP `textDocument/references` 返回工作区内受控引用位置 |
| `code_diagnostics` | `path` | 查询文件当前 LSP `publishDiagnostics` 诊断与上下文切片 |
| `code_edit_suggestions` | `path`、`line`、`column`、可选 `end_line`/`end_column`/`new_name`/`only`/`expected_file_sha256` | 请求重命名或 code action 并在宿主计算 `diffy` 统一 diff 预览；校验当前磁盘 SHA-256，若文件已修改则以 `STALE_INDEX` 拒绝，**永不写入工作区** |

## 低信任上下文注入、CLI 与桌面端入口

- **`soyo_code_intel_v1` 导航信封**：启用 `inject_context = true` 时，运行时在头部历史按需插入携带 `sha256` 与 `summary` 的 `soyo_code_intel_v1` JSON 数据块，保持系统静态提示词前缀不变；提示段 [code-intel.md](../crates/soyo-core/prompts/v1/code-intel.md) 明确模型须将其视为低信任线索，写入前仍需以工具读取并核对磁盘最新版本。
- **CLI 命令**：
  - `soyo index build`：全量/增量构建外部项目符号索引。
  - `soyo index status`：查看外部数据库路径、大小、文件数、符号数及是否发生截断。
  - `soyo index query <query> [--kind <kind>] [--path <path>] [--limit <N>]`：离线检索符号（支持 `--output json|jsonl`）。
- **桌面端实验开关**：`Settings › Experimental` 提供 `code_intel_enabled` 开关，经 `soyo host` 以 `toml_edit` 安全写回 `[code_intel].enabled`（要求所有会话空闲）。

## 当前验证与已知限制

- 本地 macOS Seatbelt 环境已通过 `WorkspaceIndex`、`CodeIntelState`、6 个只读工具、`.git`/私有路径/符号链接拦截、`STALE_INDEX` 过期拒绝、`edit_file` 失效同步、沙箱写拒绝验证（mock LSP 无法写工作区，仅能写外部 cache 目录）与 CLI `soyo index build|query|status` 集成回归。
- `crates/soyo-core/tests/code_intel.rs` 包含真实 `rust-analyzer` 沙箱测试；若当前运行环境未在活动工具链安装 `rust-analyzer` 组件，该项测试按预设显式打印跳过（本机 `~/.cargo/bin/rust-analyzer` 为无组件的 rustup proxy，故该真实二进制子测在本机跳过）。Linux Bubblewrap 与四平台远程 CI 矩阵待按需触发验证。
