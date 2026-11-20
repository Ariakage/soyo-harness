# Soyo Harness F-16 模型路由与 F-17 项目索引／沙箱 LSP

**开发线增量说明：当前保持 `version = 0.3.0`，未创建或推送版本标签。F-16 与 F-17 已完成核心运行时、`soyo-app`、CLI/TUI、`soyo-ipc`/`soyo-host`、桌面端 `Experimental` 开关与设计稿／渲染截图同步，并在本机（macOS 15 + Seatbelt）完成 Rust Workspace 全量检查、工具回归、桌面端检查与 release CLI smoke 验证；真实 `rust-analyzer` 端到端测试在本机因活动工具链未安装 `rust-analyzer` 组件而条件跳过，Linux Bubblewrap 与其余平台的远程四平台 CI 矩阵暂未在本轮单独触发。**

## 中文

### F-16 模型路由与故障切换（D-086）

- **默认关闭（opt-in）与零漂移**：`[routing]` 默认 `enabled = false`。关闭时保持既有单执行 profile 与可选独立审核配置行为，提示词、工具表与配置指纹完全不变。
- **能力筛选与角色分配**：支持 `[profiles.<name>.capabilities]`（`tools`、`reasoning`、`task_types`）、多模态（`image`/`audio`/`video`）、上下文窗口与 `[routing.roles]`（`execution`/`planning`/`review`/`compaction` 及 `plan`/`build`/`code`/`work` 覆盖列表），结合 `[profiles.<name>.pricing]`、`prefer_lowest_cost`、`max_run_cost_microunits` 与 `require_pricing_for_budget` 选择候选，并生成可解释的 `ExcludedCandidate` 淘汰清单。
- **预流（Pre-Stream）故障切换与熔断器**：仅当 `!received && is_transient_model_error()` 时按阈值（`circuit_breaker_failures`）与冷却窗（`cooldown_seconds`）切换已配置候选或 `fallback_profiles`；一旦已开始输出文本或执行过工具副作用，绝不重放或静默换模型。
- **不透明协议状态（`OpaqueHistoryPolicy`）**：显式区分 `reject`（默认 fail-closed）、`migrate_compatible`（剥离端点绑定 `ProviderItem` 并完整保留语义历史与工具调用／结果）及 `require_new_session`（经 `session::fork_migrated` / `soyo resume --migrate-history` 非破坏性分叉新建会话）。
- **可观测性与 CLI／桌面端**：持久化 `EventPayload::ModelRouted` 与 `RunMetrics`（`by_profile`、`route_decisions`），支持 Goal `Planning` 路由与配置指纹校验；新增 `soyo routing explain` 与桌面端 `Settings › Experimental` 的 `routing_enabled` 开关。

### F-17 项目索引与沙箱 LSP 代码智能（D-087）

- **默认关闭与外部索引（Layer 1）**：`[code_intel]` 与 `[code_intel.lsp]` 默认均为 `enabled = false`。索引 SQLite 严格保存在工作区外的 `<cache_dir>/index/<sha256(workspace)>/project-index.sqlite3`（`0700`/`0600`，拒绝与工作区重叠），遵守 `.gitignore`/`.ignore`，强制排除 `.git` 控制路径与 `ReadTools` 私有保护路径（`Bypass` 下亦同），拒绝符号链接并基于 `(size, mtime_ns, sha256)` 增量刷新与写后失效。
- **沙箱化 LSP 3.17 客户端（Layer 2）**：在 `PreparedEnvironment`（Seatbelt／Bubblewrap）内运行语言服务器；工作区只读，仅 `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/` 可写，`NetworkPolicy::Deny`、`OPENSSL_CONF=/dev/null`、`CARGO_TARGET_DIR`/`XDG_CACHE_HOME`/`HOME`/`TMPDIR` 重定向外部缓存并按 `ManagedProcess`  clean shutdown。返回 URI 经工作区包含与私有路径复核。
- **6 个只读代码工具与 `STALE_INDEX` 校验**：提供 `code_search_symbols`、`code_file_outline`、`code_definition`、`code_references`、`code_diagnostics` 与 `code_edit_suggestions`（`diffy` 统一 diff 预览与 SHA-256 校验，永不写入工作区）；注入受控 `code-intel.md` 与 `soyo_code_intel_v1` 低信任导航信封；提供 `soyo index build|query|status` 与桌面端 `code_intel_enabled` 开关。

### 兼容性与已知限制

- 未启用 `[routing]` 与 `[code_intel]` 时，既定配置、提示词前缀、工具目录及会话格式保持向后兼容、行为不变。
- `crates/soyo-core/tests/code_intel.rs` 的真实 `rust-analyzer` 测试在本机因未安装工具链 `rust-analyzer` 组件而条件跳过；Mock LSP 已在真实 macOS Seatbelt 沙箱下验证全工具路径、写入隔离、`STALE_INDEX` 与失效同步。Linux Bubblewrap 与远程四平台 CI 矩阵暂未在本轮触发。

---

## English

### F-16 Model Routing & Failover (D-086)

- **Opt-In with Zero Drift**: `[routing]` defaults to `enabled = false`. Existing single-profile runs, optional review profile behavior, system prompts, tool registries, and config fingerprints remain unchanged when disabled.
- **Capability & Budget-Aware Role Selection**: Evaluates `[profiles.<name>.capabilities]` (`tools`, `reasoning`, `task_types`), media support, context windows, `[routing.roles]` (`execution`, `planning`, `review`, `compaction`, plus per-mode overrides `plan`, `build`, `code`, `work`), pricing bounds (`prefer_lowest_cost`, `max_run_cost_microunits`, `cost_currency`, `require_pricing_for_budget`), and emits structured `ExcludedCandidate` explanations.
- **Bounded Pre-Stream Failover & Circuit Breaker**: Retries fallback candidates only when `!received && is_transient_model_error()`, enforcing `circuit_breaker_failures` and `cooldown_seconds` without ever replaying streamed output or executed tool side effects.
- **Explicit Opaque History Handling (`OpaqueHistoryPolicy`)**: Supports `reject` (fail-closed default), `migrate_compatible` (strips endpoint-bound `ProviderItem`s while retaining semantic turns and tool calls/results), and `require_new_session` via non-destructive `session::fork_migrated` (`soyo resume --migrate-history`).
- **Observability, CLI & Desktop**: Emits `EventPayload::ModelRouted`, records `RunMetrics.by_profile` and `RunMetrics.route_decisions`, wires Goal `Planning` routing & config fingerprints, adds `soyo routing explain`, and exposes `routing_enabled` in Desktop `Settings › Experimental`.

### F-17 External Project Index & Sandboxed LSP Code Intelligence (D-087)

- **Opt-In External SQLite Index (Layer 1)**: Both `[code_intel]` and `[code_intel.lsp]` default to `enabled = false`. Stores symbol indexes strictly outside `<workspace>` at `<cache_dir>/index/<sha256(workspace)>/project-index.sqlite3` (`0700`/`0600`), rejects cache/workspace overlap, honors `.gitignore`/`.ignore`, blocks `.git` control paths and protected host/private paths even under `Bypass`, refuses symlinks, and updates incrementally via `(size, mtime_ns, sha256)` plus write invalidation.
- **Sandboxed LSP 3.17 Client (Layer 2)**: Spawns language servers inside `PreparedEnvironment` with `<workspace>` read-only, write access confined to `<cache_dir>/lsp/<sha256(workspace)>/<server_id>/`, `NetworkPolicy::Deny`, `OPENSSL_CONF=/dev/null`, and external `CARGO_TARGET_DIR`/`XDG_CACHE_HOME`/`HOME`/`TMPDIR` redirection, cleanly shutting down via `ManagedProcess`. All returned LSP URIs undergo workspace containment, symlink, and private-path checks.
- **Six Read-Only Code Tools & `STALE_INDEX` Verification**: Registers `code_search_symbols`, `code_file_outline`, `code_definition`, `code_references`, `code_diagnostics`, and `code_edit_suggestions` (read-only `diffy` unified diff previews with SHA-256 verification and `STALE_INDEX` rejection), injects low-trust `soyo_code_intel_v1` hints & `code-intel.md`, adds `soyo index build|query|status`, and adds `code_intel_enabled` to Desktop `Settings › Experimental`.

### Compatibility & Known Limits

- Default configurations keep existing prompts, tool schemas, session schemas, and single-profile execution identical when `[routing]` and `[code_intel]` remain disabled.
- The real `rust-analyzer` integration test inside `crates/soyo-core/tests/code_intel.rs` was conditionally skipped on this host because no `rust-analyzer` component is installed under the active toolchain; sandboxed Seatbelt isolation, all 6 tools, `STALE_INDEX`, and edit invalidation were verified against the local mock LSP server. Linux Bubblewrap and remote 4-platform CI runs were not triggered in this pass.
