# Soyo Harness F-09 受控多 Agent 协作（Subagents）

**开发线增量说明：当前保持 `version = 0.3.0`，未创建或推送版本标签。F-09 已完成核心运行时、受控工作区隔离与三方合并、CLI/TUI/Goal 集成、helper IPC 与桌面端 `Experimental` 开关实现，并在本机（macOS 15 + Seatbelt）完成对应集成测试、Rust Workspace 检查、工具回归、桌面端检查与 release CLI smoke 验证；Linux Bubblewrap 与其余平台的远程四平台 CI 矩阵暂未在本轮单独触发。**

## 中文

### 已实现能力

- **默认关闭（opt-in）与零漂移**：`[subagents]` 默认 `enabled = false`。关闭时提示词、工具注册表、默认系统前缀、配置指纹与未启用会话行为完全不变；已注册模型伪造的 `subagent_*` 调用继续以 `unknown_tool` 拒绝。
- **受控单层协作（`max_depth = 1`）**：提供 `subagent_spawn`、`subagent_status`、`subagent_wait`、`subagent_message`、`subagent_cancel`、`subagent_diff`、`subagent_merge` 七个内置工具，支持同步与后台并行执行、续跑引导、取消清理、差异检查与选择性合并；子 Agent 不得再委派孙任务。
- **严格非提权与 `PLAN` 约束**：
  - 子模式不可越过父模式；`PLAN` 与只读运行时仅允许 `read_only` 检查且禁用 `allow_exec`；切换父级至 `PLAN` 前自动取消全部运行中的可变或可执行子 Agent。
  - 父级显式 `deny`/`ask` 规则及工作区绝对路径 `path_glob` 自动重映射至子工作区；即使在 `Bypass` 模式下，隔离与 `allowed_paths` 子 Agent 仍严格受限于授权子路径；`allow_exec = false`（默认）在所有权限模式下禁用 `exec`；`allow_exec = true` 继承父 `SandboxPolicy` 并把 `write_roots` 收缩为子工作区。
  - 继承 `SecretRegistry` 与 `known_secrets` 并统一脱敏快照与步骤轨迹；不继承 `Preauthorizations`；`js_repl` 明确拒绝桥接 `subagent_*`。
- **工作区隔离与三方冲突检测合并**：优先采用 `git worktree add --detach`（叠加未提交且未忽略变更，失败自动清理），非 Git 或无提交仓库回退有界 `DirectoryCopy`；基于 Fork 基线、父工作区与子工作区三方 SHA-256 摘要检查冲突，`subagent_conflict` 时 fail-closed 保留父工作区原状，支持按 `paths` 选择性原子合并、生成 `SubagentMergeReceipt`，并在取消／结束时彻底清理临时目录与 Git worktree 注册信息。
- **共享预算、Metrics 与 Goal 集成**：共享父 `SharedBudget` 与父工具调用计数器，单源增量汇总执行次数与 Token 用量，串联取消信号；`[subagents]` 纳入 Goal 配置指纹并在 `StepVerifier` 独立核验前通过 `subagent_merge` 交付产物。
- **CLI / TUI / 桌面端展示与实验开关**：CLI 审批界面隐藏原始提示正文，TUI 与 `soyo-ipc` 结构化呈现步痕、变更与合并结果；桌面端 `Settings › Experimental` 新增 `subagents_enabled` 开关并通过 `toml_edit` 无损写回 `[subagents].enabled`（要求全部会话空闲）。

### 兼容性与已知限制

- 仅支持 `max_depth = 1`（父 → 子单跳）；多层递归委派、跨会话长期驻留子 Agent 与远程分布式 worker 不在本期范围。
- 未改变既有默认配置、提示词前缀、会话存储格式或默认沙箱策略；旧配置文件无需修改即可继续使用。
- 本机原生沙箱端到端集成测试已在当前 macOS Seatbelt 环境运行通过；Linux Bubblewrap 与四平台远程 CI 构建发布流水线待后续按需触发验证。

---

## English

### Implemented Capabilities

- **Strictly Opt-In with Zero Drift (`enabled = false` by default)**: `[subagents]` is disabled by default. When disabled, system prompts, registered tools, schemas, and Goal configuration fingerprints remain unchanged, and fabricated `subagent_*` calls fail with `unknown_tool`.
- **Controlled Single-Hop Delegation (`max_depth = 1`)**: Exposes 7 built-in tools (`subagent_spawn`, `subagent_status`, `subagent_wait`, `subagent_message`, `subagent_cancel`, `subagent_diff`, `subagent_merge`) for synchronous or parallel background delegation, bounded follow-up steering, cancellation, diff inspection, and selective merging. Children cannot spawn grandchildren.
- **Strict Non-Escalation & `PLAN` Enforcement**:
  - Child work modes never exceed the parent; `PLAN` and read-only runtimes permit only `read_only` inspection with `allow_exec = false`; switching an active parent run to `PLAN` cancels running mutating/exec-enabled subagents before transitioning.
  - Parent `deny`/`ask` rules and workspace-scoped `path_glob` matchers are remapped onto the child workspace; `Bypass` remains contained inside the isolated workspace / `allowed_paths` (`enforce_roots_in_bypass`); `allow_exec = false` denies `exec` even under `Bypass`; `allow_exec = true` inherits the parent `SandboxPolicy` with `write_roots` narrowed to the child workspace.
  - Inherits `SecretRegistry` and `known_secrets` with output/summary/trace redaction; never clones parent `Preauthorizations`; `js_repl` explicitly blocks bridging `subagent_*`.
- **Workspace Isolation & 3-Way SHA-256 Conflict-Checked Merge**: Prefers `git worktree add --detach` (propagating uncommitted non-ignored changes and cleaning up cleanly on setup failures), with a bounded `DirectoryCopy` fallback for non-Git workspaces. `subagent_merge` performs 3-way SHA-256 comparison across fork base, current parent, and child, fails closed with `subagent_conflict` when the parent diverged on a target file, supports selective `paths` merging with atomic writes and `SubagentMergeReceipt`, and tears down worktrees and scratch directories on discard, cancel, or run shutdown.
- **Shared Budgets, Metrics Rollup & Goal Integration**: Enforces chained `SharedBudget` timeouts and shared parent model/tool-call caps, rolls up single-source child `Metrics` once per turn/completion, propagates parent cancellation, includes `[subagents]` in `goal_profile_fingerprint`, and supports `StepVerifier`-gated Goal delivery via `subagent_merge`.
- **CLI / TUI / Desktop Presentation & Experimental Toggle**: CLI approvals redact raw prompts; TUI and `soyo-ipc::tool_format` render step traces, changed files, and merge status; Desktop `Settings › Experimental` adds a `subagents_enabled` toggle persisted safely to `[subagents].enabled` via `toml_edit` under idle checks.

### Compatibility & Known Limits

- Depth is intentionally capped at `max_depth = 1` (parent-to-child only); recursive multi-depth trees, persistent cross-run subagents, and remote distributed workers remain out of scope.
- Existing configuration files, default prompt prefixes, session schemas, and sandbox defaults remain backwards-compatible with zero behavioral changes when `[subagents]` is disabled.
- Native sandbox CLI integration was verified locally on macOS Seatbelt along with full workspace checks, Python tooling regressions, desktop frontend/Rust checks, and release CLI smoke; Linux Bubblewrap and multi-platform remote CI matrix verification were not triggered in this pass.
