# 模型路由与故障切换（F-16，D-086）

**状态：已在 `soyo-contract`、`soyo-core`、`soyo-app`、`soyo-cli`、`soyo-tui`、`soyo-ipc`、`soyo-host` 与 `desktop` 实现，当前为严格默认关闭（opt-in，`[routing].enabled = false`）。本文记录当前已落地的能力筛选、角色路由、熔断预流故障切换与不透明历史迁移边界；动态在线探测与跨服务自动注册未知账号不在范围。**

## 定位与默认开关边界

`F-16` 允许在多个已显式配置的 `[profiles.<name>]` 之间，按请求角色、工作模式、能力声明、上下文余量与运行成本预算选择候选模型，同时维持宿主在预算、工具边界与不透明协议状态上的硬约束：

- **默认零漂移（opt-in）**：`[routing] enabled = false` 为默认值。关闭时，执行会话固定使用所选 execution profile，兼容既有的独立 `[defaults].review_profile` 设置，系统提示词、工具注册表与配置指纹保持零变化。
- **仅限已显式配置 profile**：路由与故障切换只在当前配置显式声明并校验通过的 `[profiles.<name>]` 中选择，绝不自动发现、猜测未配置端点或接入额外付费服务。
- **能力先于排序**：所有候选必须同时满足工具调用能力（`tools_required`）、媒体能力（`image`/`audio`/`video`）、`task_types` 标签约束、上下文窗口预算（`estimated_input_tokens + extra_safety_tokens`），以及未触发熔断冷却和货币/预算上限要求。

## `[profiles.<name>.capabilities]` 与 `[routing]` 配置

在每个 `[profiles.<name>]` 下可声明可选的 `capabilities`（配置类型 `ProfileCapabilities`，`crates/soyo-core/src/config/routing.rs`）：

| 字段 | 类型 / 默认值 | 说明 |
| --- | --- | --- |
| `capabilities.tools` | `bool` / `true` | 是否支持函数/工具调用；执行循环带工具时过滤为 `false` 的 profile |
| `capabilities.reasoning` | `bool` / `false` | 是否具备推理能力（配置 `thinking_effort` 时自动视为 `true`） |
| `capabilities.task_types` | `BTreeSet<String>` / `[]` | 可选任务类型标签（至多 32 项、每项 1..=32 ASCII 小写/数字/`_`/`-`） |

`[routing]`（配置类型 `RoutingSettings`，`#[serde(default, deny_unknown_fields)]`）在加载阶段即校验全部字段边界（即使 `enabled = false`）：

| 字段 | 默认值 | 允许边界 | 说明 |
| --- | --- | --- | --- |
| `enabled` | `false` | — | 总开关；`true` 时启用按角色候选路由与预流回退 |
| `roles.execution` / `planning` / `review` / `compaction` | `[]` | 每表至多 16 个已配置且不重复 profile | 按功能角色声明有序候选 profile 列表 |
| `roles.plan` / `build` / `code` / `work` | `[]` | 每表至多 16 个已配置且不重复 profile | 按工作模式覆盖候选 profile 列表 |
| `fallback_profiles` | `[]` | 至多 16 个已配置且不重复 profile | 全局兜底候选序列 |
| `prefer_lowest_cost` | `false` | — | 在满足前提的候选中优先按已申报估算成本从低到高排序 |
| `max_run_cost_microunits` | `None` | `>= 1`（`0` 拒绝） | 单次 Run 累计成本上限（百万分之一货币单位） |
| `cost_currency` | `None` | 3 位大写 ASCII 货币码 | 与 `[profiles.<name>.pricing]` 及 `max_run_cost_microunits` 对齐校验的币种 |
| `require_pricing_for_budget` | `false` | — | 开启预算上限时，拒绝未申报精确报价或币种不匹配的候选 |
| `circuit_breaker_failures` | `2` | `1..=16` | 连续暂时性失败达到次数后触发熔断 |
| `cooldown_seconds` | `60` | `1..=3600` | 触发熔断后的冷却窗口秒数 |
| `opaque_history` | `"reject"` | `reject` / `migrate_compatible` / `require_new_session` | 跨 profile/协议遇不透明 `ProviderItem` 时的显式处理策略 |

详细 TOML 示例见 [examples/config.example.toml](../examples/config.example.toml) 与 [配置组件](configuration.md)。

## 角色映射、故障切换与熔断器

1. **角色与模式解析**：`ModelRole::from_request(purpose, work_mode)` 将请求映射至 `Review`、`Compaction`、`Planning`（当 `purpose = Execution` 且 `work_mode = Plan`）或 `Execution`。各工作模式还可配置 `roles.plan`/`build`/`code`/`work` 覆盖列表；Goal 规划器亦支持路由到 `Planning` 角色。
2. **可解释淘汰理由**：`RoutingCoordinator` 对每个候选记录明确淘汰项（`ExcludedCandidate { profile, reason, detail }`），涵盖工具支持不足、缺多模态能力、缺少 `task_types`、上下文窗口不足、估算成本超单次/总预算、缺少报价、熔断冷却中或存在不兼容 `ProviderItem`。
3. **仅限预流（Pre-Stream）故障切换**：仅当本次请求**尚未收到任何流式事件或工具输出**（`!received && is_transient_model_error()`，如 429/5xx/连接超时等可恢复模型错误）时，协调器记录失败、按阈值打开原 profile 熔断器并尝试下一兼容候选。一旦流已经输出文本、推理片段或触发工具调用，绝不静默切换并在同一工具轮重放副作用。
4. **单 Run 与多 Profile 归因**：每次真实选路触发 `EventPayload::ModelRouted` 事件，并在 `RunMetrics` 持久化 `by_profile: BTreeMap<String, ProfileAttribution>`（分 profile 记录请求数、Token 与估算成本）及 `route_decisions: Vec<RouteDecisionRecord>`（至多 512 条，记录 `fallback_from`、`excluded` 与 `migration` 摘要）。

## 不透明协议状态与会话迁移（`OpaqueHistoryPolicy`）

当旧会话包含端点绑定或协议专属的不透明项（`HistoryItem::ProviderItem`）而需要切换到其他 profile/协议时，绝不直接替换 profile 名称了事：

- **`reject`（默认）**：检测到跨 profile/协议的不兼容 `ProviderItem` 时 fail-closed 拒绝继续或拒绝切流，不改动任何既有会话文件。
- **`migrate_compatible`**：在不丢失任何语义消息、已执行 `ToolCall` 与 `ToolResult` 记录的前提下，仅剥离不兼容的不透明 `ProviderItem` 并生成 `HistoryMigrationInfo`。
- **`require_new_session`**：要求用户显式分叉新建目标会话——通过 `session::fork_migrated`（或 CLI `soyo resume <session_id> --migrate-history`）在只读读取原会话后创建全新目标 `SessionId`，永不覆盖原会话历史。

## 观测、CLI 与桌面端入口

- **CLI 诊断与迁移**：
  - `soyo routing explain [--role execution|planning|review|compaction] [--tools] [--image] [--audio] [--video] [--tokens <N>] [--session <ID>]`：离线输出当前配置下的候选顺序、淘汰理由、健康状态与历史迁移判定（支持 `--output json|jsonl`）。
  - `soyo resume <session_id> --migrate-history`：对存在不透明历史或跨 profile/协议变化的会话显式分叉迁移为兼容新会话。
- **Goal 与指纹**：`[routing]` 配置（启用状态、角色表、fallback 与 `opaque_history` 策略）纳入 Goal 配置指纹，避免未确认状态下配置漂移。
- **桌面端实验开关**：`Settings › Experimental`（`ExperimentalPanel.tsx`）提供 `routing_enabled` 开关，经 `soyo host` 以 `toml_edit` 无损更新 `[routing].enabled`（要求全部活跃会话空闲），`useAppStore.ts` 支持 `model.routed` 事件处理。
