# 受控子 Agent 协作（F-09，D-085）

**状态：已在 `soyo-contract`、`soyo-core`、`soyo-app`、`soyo-cli`、`soyo-tui`、`soyo-ipc`、`soyo-host` 与 `desktop` 实现，当前为严格默认关闭（opt-in，`[subagents].enabled = false`）。本文记录当前已落地的单层受控父子委派边界；多层递归委派与跨进程分布式子 Agent 仍属后续范围。**

## 定位与开关边界

`F-09` 允许单个父 `AgentRuntime` 将独立子任务委派给有界子运行时执行，同时维持宿主在模式、权限、沙箱、凭据、预算与取消上的最终裁决权：

- **默认零漂移（opt-in）**：`[subagents] enabled = false` 为默认值。关闭时，`ModelRequest.tools` 不注册 `subagent_*` 工具、`prompt.rs` 不注入 [subagents 提示段](../crates/soyo-core/prompts/v1/subagents.md)、`ToolDispatcher` 拒绝任何模型伪造的 `subagent_*` 调用并返回 `unknown_tool`，且既有配置指纹、系统提示词前缀与行为保持零变化。
- **深度固定为 `max_depth = 1`**：只有顶层父会话在启用 `[subagents]` 时构建 `SubagentSupervisor`；子 `AgentRuntime` 绝不调用 `with_subagents(...)`，模型侧看不到 `subagent_*` 工具定义，并在派发期 fail-closed 拒绝递归生成孙子任务。
- **非托管外部进程**：子 Agent 在进程内受监督任务中复用 `AgentRuntime` 与宿主统一工具／沙箱管道，不启动独立绕过权限的外部编码 Agent。

## `[subagents]` 配置与限额

配置类型 `SubagentSettings`（`crates/soyo-core/src/subagent/mod.rs`）使用 `#[serde(default, deny_unknown_fields)]`，即使 `enabled = false` 也会在加载期校验所有数值范围（`0`、倒挂区间或超过硬上界返回 `invalid_config`）：

| 字段 | 默认值 | 允许上限 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `false` | — | 总开关；仅 `true` 时注入工具与提示段 |
| `max_concurrent` | `4` | `16` | 同时处于 `running` 状态的子 Agent 上限（`1..=max_total`） |
| `max_total` | `16` | `64` | 单个父 Run 累计生成的子 Agent 总数上限 |
| `max_model_requests` | `32` | `256` | 单个子 Agent 累计模型请求上限 |
| `max_tool_calls` | `128` | `1024` | 单个子 Agent 累计工具调用上限 |
| `default_timeout_seconds` | `300` | `max_timeout_seconds` | 未传 `timeout_seconds` 时的默认执行超时 |
| `max_timeout_seconds` | `900` | `1800` | 单次 `spawn`/`wait`/`message` 允许请求的最大超时 |
| `max_workspace_files` | `10000` | `50000` | 工作区隔离拷贝与差异扫描的文件数上限 |
| `max_workspace_bytes` | `67108864` (64 MiB) | `536870912` (512 MiB) | 工作区拷贝与基线扫描总字节上限 |
| `max_changed_files` | `256` | `2048` | `subagent_diff` / `subagent_merge` 单次追踪变更文件上限 |
| `max_patch_bytes` | `262144` (256 KiB) | `2097152` (2 MiB) | `subagent_diff` 统一补丁输出上限（超限置 `patch_truncated = true`） |
| `max_summary_bytes` | `32768` (32 KiB) | `262144` (256 KiB) | 回传给父级 `SubagentSnapshot.summary` 的截断与脱敏上限 |

详细 TOML 示例见 [examples/config.example.toml](../examples/config.example.toml) 与[配置组件](configuration.md)。

## 工具契约（7 个内置工具）

启用 `[subagents]` 后，工具注册表按当前工作模式投影以下工具：

| 工具名 | 副作用分类（`ToolEffect`） | 核心输入参数 | 说明 |
| --- | --- | --- | --- |
| `subagent_spawn` | `Control` | `label`、`task`、可选 `mode`/`workspace_isolation`/`allow_exec`/`allowed_paths`/`background`/`timeout_seconds` | 创建子 Agent；`background = false` 同步等待完成并返回快照，`true` 立即返回 `running` 状态 |
| `subagent_status` | `Control` | `subagent_id` | 读取脱敏后的子 Agent 当前快照（状态、步痕 `step_traces`、变更文件、合并回执） |
| `subagent_wait` | `Control` | `subagent_ids`、`wait_all`（默认 `true`）、可选 `timeout_seconds` | 等待一个或全部子 Agent 终态或超时 |
| `subagent_message` | `Control` | `subagent_id`、`message`、可选 `timeout_seconds` | 向已终止但未合并／丢弃且未耗尽预算的子 Agent 发送后续指示 |
| `subagent_cancel` | `Control` | `subagent_id`、可选 `discard`（默认 `false`） | 触发子级本地取消并等待退出；`discard = true` 时立即清理隔离工作区 |
| `subagent_diff` | `Control` | `subagent_id`、可选 `include_patch`（默认 `true`） | 对照基础清单、父工作区与子工作区生成变更摘要、冲突标记与有界 unified diff |
| `subagent_merge` | `FileWrite` | `subagent_id`、可选 `paths`（子集选择性合并）、可选 `discard_after`（默认 `true`） | 三方冲突检测并原子合入父工作区；在 `PLAN` 或只读运行时中从模型工具列表省略且派发拒绝 |

对应专用边界错误码见 [soyo-contract](../crates/soyo-contract/src/contract.rs)：`subagent_not_found`、`subagent_conflict`、`subagent_limit`、`subagent_workspace_error`、`subagent_error`。

## 安全与非提权边界

1. **工作模式继承与 `PLAN` 清理**：
   - 父模式为 `WorkMode::Plan` 或运行时为 `read_only` 时，只允许 `mode = "plan"`、`workspace_isolation = "read_only"` 且 `allow_exec = false`；试图委派非 `PLAN`、`isolated` 或可 `exec` 的子 Agent 一律返回 `mode_denied`。
   - `PLAN` 模式下模型侧工具列表隐藏 `subagent_merge`，且 `Policy::evaluate` 与 `Runtime` 派发双层拒绝该调用。
   - 运行期间父级通过 `request_mode(WorkMode::Plan)` 切换到 `PLAN` 时，[RuntimeServices::cleanup_for_plan](../crates/soyo-core/src/runtime_services.rs) 在进入 `PLAN` 屏障前调用 `SubagentSupervisor::cancel_mutating()`，取消并等待全部正在运行的 `isolated` 或 `allow_exec` 子 Agent 收敛。
2. **规则重映射与 `Bypass` 隔离约束（`Policy::derive_for_subagent`）**：
   - 继承父策略全部显式 `deny`/`ask`/`allow` 规则；对指向父工作区绝对路径或其前缀的 `path_glob`，额外复制映射一份针对隔离工作区真实路径的规则，确保父级对敏感路径（如 `secrets/**`）的 `deny`/`ask` 规则不会因进入独立 worktree 而失效。
   - 当隔离模式为 `isolated` 或指定了 `allowed_paths` 时，子策略启用 `enforce_roots_in_bypass = true`：即使在 `PermissionMode::Bypass` 下，子 Agent 的文件读写与可执行工作目录也严格受限于子工作区根目录或指定的相对 `allowed_paths` 子树（`path_denied` / `permission_denied`）。
   - 子 Agent 不继承父级的一次性缓存或 `Preauthorizations`（永远从空授权集启动）。在 `Manual Approval` 或 `ask` 规则下，审批提示经 `[subagent:<id>:<label>]` 前缀清晰标识来源子任务。CLI 审批渲染（[crates/soyo-cli/src/approval.rs](../crates/soyo-cli/src/approval.rs)）隐去原始 `task`/`message` 文本，仅展示长度与元信息；`subagent_merge` 的操作指纹绑定候选目标文件集合、SHA-256 评审摘要与 `discard_after` 参数。
3. **命令执行与原生沙箱收缩**：
   - `allow_exec` 默认 `false`：通过 `dispatcher.restrict_exec_disabled()` 与 `Policy::with_exec_denied()` 双重封锁，子 Agent 既看不到 `exec` 工具，在 `Bypass` 模式下执行也会被明确拒绝（`unknown_tool` / `permission_denied`）。
   - 显式 `allow_exec = true` 时，子运行时的 `ExecConfig.execution` 继承父 `SandboxPolicy`（macOS Seatbelt / Linux Bubblewrap+Seccomp / 显式 Host 测试后端），并由 `derive_sandbox_for_subagent` 将 `write_roots` 收缩为隔离工作区（及其 `allowed_paths` 子集），把其他工作区的私有目录或其余隔离工作区加入受保护范围。
4. **凭据隔离与 `js_repl` 禁止桥接**：
   - 子 Agent 共享父级 `SecretRegistry` 与静态 `known_secrets`；返回的 `summary`、错误消息、`step_traces` 与 diff 文本统一经过已知凭据脱敏。
   - [crates/soyo-core/src/js_repl/mod.rs](../crates/soyo-core/src/js_repl/mod.rs) 显式将 `subagent_*` 排除在 `soyo.callTool` 桥接白名单之外，JS 脚本无法绕过编排或审批边界触发子 Agent。

## 工作区隔离与三方冲突检测合并

`isolated` 模式在 [crates/soyo-core/src/subagent/workspace.rs](../crates/soyo-core/src/subagent/workspace.rs) 中分配系统临时目录（优先 `std::env::temp_dir()`、回退 `/var/tmp`）下的私有 `0700` 目录（与父工作区及受保护路径不重叠）：

1. **Git Worktree 优先，有界 Directory Copy 回退**：
   - 当父工作区是合法 Git 仓库且存在可检出 `HEAD` 时，通过 `git worktree add --detach` 创建临时 worktree，并把父工作区当前已修改或未跟踪但未被 `.gitignore` 忽略的文件与删除/重命名叠加到子 worktree 中，确保未提交改动对子任务可见并纳入基线 SHA-256 清单；创建或叠加半途失败自动回滚清理已注册 worktree 与临时目录。
   - 非 Git 工作区或无初始提交的仓库回退到有界目录拷贝（排除 `.git`、私有保护路径、符号链接穿越，受 `max_workspace_files` 与 `max_workspace_bytes` 限制）。
2. **三方 SHA-256 冲突检测与原子合入**：
   - 依据 Fork 基线摘要（`base`）、当前父工作区实际摘要（`parent`）与子工作区当前摘要（`subagent`）进行三方比对：若父工作区在 Fork 之后自行修改或删除了待合并路径且与目标状态冲突，立即 fail-closed 返回 `subagent_conflict`，附带结构化冲突清单（`path`、`reason`、`base_sha256`、`parent_sha256`、`subagent_sha256`），不部分写入已冲突的集合作业。
   - 未冲突变更经临时文件 + `fsync` 原子写入父工作区，逐文件记录 `SubagentMergeReceipt`（`operation`、`pre_parent_sha256`、`post_parent_sha256`），并在 parent `RunMetrics` 的 artifacts 列表中留下可审计追踪记录。
   - 支持 `paths` 选择性合并跳过冲突文件；合并后默认 `discard_after = true` 或通过 `subagent_cancel(discard = true)`／父 Run `shutdown()` 自动卸载临时 worktree 与清理临时目录，不残留孤儿目录或 `.git/worktrees/*` 条目。

## 共享预算、Metrics 与 Goal 集成

- **共享时间与调用上限**：子 Agent 接收从父 `SharedBudget` 派生的受限预算（同时受子级 `timeout_seconds`、父级剩余活跃时间与共享父 `model_requests` 计数约束），并共享原子父工具调用计数器，防止通过并行扇出突破父级 Run 预算。
- **单源 Metrics 汇总**：`SubagentStore` 在每次子工具完成与模型请求后直接向子 `MeasuredModel` 与记录表累积明细；子任务返回时仅通过 `Metrics::merge_child` 增量回滚进父 `Metrics` 一次（支持多轮 `subagent_message` 续跑而不重复计算）。
- **取消链路**：通过 `SubagentCancel` 串联父 `Cancellation` 与局部取消信号，父 Run 取消、Ctrl-C、超时或 helper 断线会立即终止运行中的子 Agent 并清理临时工作区。
- **Goal 指纹集成**：[goal_profile_fingerprint](../crates/soyo-app/src/goal.rs) 纳入 `&config.subagents`；`goal start --auto` 在执行期发现 `[subagents]` 开关或限额漂移时明确拒绝继续，步骤产物经 `subagent_merge` 落入父工作区后由宿主 `StepVerifier` 独立核验。

## CLI、TUI 与桌面端呈现

- **CLI**：工具事件流正常发出 `tool.requested`/`tool.finished`，审批交互隐藏原始 `task`/`message` 正文。
- **TUI**（[tool_summary.rs](../crates/soyo-tui/src/tool_summary.rs)、[tool_render.rs](../crates/soyo-tui/src/tool_render.rs)）：单行摘要呈现动作、标签、状态、`wait` 完成状态（`completed` / `timed out`）与变更文件数；详情折叠展示 `Mode`、`Isolation`、摘要、嵌套 `step_traces`、变更文件状态（冲突标红）与合并结果。
- **Desktop GUI**：
  - `soyo-ipc::tool_format` 为 `subagent_*` 生成摘要与结构化列表详情（步痕、变更文件、`Merged` / `Conflicts`）。
  - `Settings › Experimental`（[ExperimentalPanel.tsx](../desktop/src/components/ExperimentalPanel.tsx)）新增 `subagents_enabled` 开关，经 `get_experimental_settings` / `set_subagents_enabled`（`soyo-host/src/peripheral.rs`）以 `toml_edit` 无损持久化 `[subagents].enabled`（要求全部会话空闲，新建会话生效）。
