# Goal 目标编排设计

**状态：DEV-069 至 DEV-074 已实现并通过本机受控验收；`DEV-091`（S3/D-080 步骤验证器模型、v2 存储、`set-verifier` 与沙箱化 `run_step_verifier`）与 `DEV-092`（`run_step` 验证门禁、`start --auto` 仅在验证通过后前进、失败有界重规划）已实现并通过本机验证；`D-111`（`auto_chain` 持久化自动推进链，`goal start --auto --chain` 与 `[goal] auto_chain`）与 `D-112`（委派步骤以持久子 Agent 执行、宿主 verifier 闭环，v3 存储）已实现并通过本机验证。能力编号 F-20；跨平台固定源码证据和发行版本仍按验收记录单独维护。**

## 当前实现

Goal 使用独立、版本化的 `data_dir/goals/goals.sqlite3`。数据库采用 WAL、私有目录／文件权限、显式 application ID／schema version、版本 CAS、单 Goal worker 租约和 dispatch intent。Goal 绑定一个专属 Session 与工作区，步骤按依赖串行复用现有 AgentRuntime、工具派发、权限、隔离、Compact、检查点及 Session 锁。

CLI 与 TUI 共用 `soyo-core::goal` 数据／存储和 `soyo-app::goal` 计划／运行编排。模型计划与人工证据核验使用无工具、纯英文请求；模型输出只是候选数据，必须经过严格 schema 校验。步骤完成、Run 完成及 Goal 验收分别记录，模型自述或计划文字不能单独令 Goal 完成。

当前入口：

```sh
soyo goal create "OBJECTIVE" [--accept TEXT]...
soyo goal plan GOAL_ID [--plan-file PLAN.json]
soyo goal set-verifier GOAL_ID STEP_ID [--cwd REL] [--timeout-seconds S] [--max-output-bytes B] [--clear] [-- ARGV...]
soyo goal confirm GOAL_ID
soyo goal list
soyo goal status GOAL_ID
soyo goal continue GOAL_ID [--approval-file PATH]
soyo goal start GOAL_ID --auto [--chain]
soyo goal pause|resume|cancel GOAL_ID
soyo goal verify GOAL_ID CRITERION [--summary TEXT]
soyo goal resolve GOAL_ID STEP_ID --retry
soyo goal replan GOAL_ID
soyo goal report GOAL_ID [--format json|markdown] [--file PATH]
soyo goal worker [--once]
```

TUI 提供 `/goals` 选择器与 `/goal create|plan|confirm|status|continue|start|pause|resume|cancel|verify|report|replan ID`。TUI 的交互式 Goal Run 当前要求 `auto` 或 `bypass` 权限；`Manual Approval` 使用 CLI 的终端审批路径。自动启动必须显式执行 `start --auto`，不会由普通发送或后台恢复隐式产生。

核验只在全部 Step 完成后开放。人工条件要求 `--summary`（TUI 为条件 ID 后的文字）并经过独立无工具核验；文件条件不接受人工摘要，`goal verify GOAL_ID CRITERION` 直接由主机重新读取文件。暂停会保留当前等待原因，恢复等待计划确认或验收的 Goal 时仍回到对应 Waiting，不会绕过确认。运行中的暂停／取消会持久化控制意图并传播到当前 Runtime；取消为终态并撤销自动授权。

实现和验收细节见[N4验收](n4-validation.md)。

## 产品语义

Goal 保存目标、验收条件、步骤、预算和证据，关联一个工作区及多个 Run。PLAN、BUILD、CODE、WORK 继续表达某次执行的工作方式，界面可同时显示 Goal 状态和当前工作模式。

当前方案将目标生命周期放在应用／编排层，底层复用 AgentRuntime、工具派发、权限、取消和存储。首次交付以单目标串行步骤和手动继续为基础，随后增加显式授权的自动续跑。用户需求见[决策记录](decisions.md)，具体拆分是本设计的规划方案。

规划与核验提示词按 D-030 使用英文，模型建议通过结构化校验后交给编排服务。当前重规划采用有界、保守的失败修正：只对已持久化且非 unknown／取消的失败步骤追加“先检查现状、不得重放未确认副作用”的修正指令，并受 `max_replans` 限制；unknown 副作用进入等待，不自动重试。Compact 后从 Goal 持久记录和当前配置恢复目标、验收项、权限与剩余预算。

## 三层交付

| 层次 | 交付范围 | 主要基础 |
| --- | --- | --- |
| G1 手动 Goal | 目标定义、计划、步骤、证据、手动执行与报告 | 共享应用服务、一致性快照、持久化和现有有界 Run |
| G2 有限自动 Goal | 自动推进、有限重规划、跨 Run 调度及显式恢复 | G1、Sandbox、检查点、Compact、持久 worker、总预算 |
| G3 多 Agent Goal | 子任务分工、依赖、隔离、汇总与独立验证 | G2 及 F-09 多 Agent 能力 |

G1 可在共享基础稳定后单独开发，G2 的执行依赖按[后续开发计划](post-mvp-plan.md)落实。TUI 的目标面板随可用服务开放。

## 数据与关联

| 对象 | 主要字段 |
| --- | --- |
| Goal | UUID、专属 Session、objective、acceptance criteria、工作区、profile、模式、状态／阶段、预算／usage、当前 Step、wait／failure、auto authorization、confirmed_verifiers_digest、revision、创建／更新时间 |
| Step | UUID、ordinal、依赖、标题／指令、可选结构化 `verifier`、可选 `delegate`、`subagent_id`、运行模式、状态、尝试次数、关联 Run、最近结构化 Run 结果与最近 `last_verification` 宿主验证证据 |
| Evidence | UUID、验收项 ID、来源 Step／Run、manual／file-exists／file-digest／artifact 类型、摘要、相对路径、SHA-256、核验时间（v2 `step_verifications` 表另行审计保存步骤验证器退出码与有界摘录） |

### 步骤宿主验证器（D-080 / DEV-091）

- **结构与确认锁定**：`PlannedStep.verifier` 与 `GoalStep.verifier` 仅接受结构化 `argv`（禁止隐式 shell 字符串拼接）、可选工作区相对 `cwd`（禁止绝对路径、`..`、控制字符与 `.git` 控制路径）、有界超时（`1–600s`，默认 `120s`）与流摘录上限（`256B–32KiB`，默认 `8KiB`）。`goal plan` 草案与 `--plan-file` 可提议 verifier，亦可经 `soyo goal set-verifier`（或 `--clear`）调整；任何更改均清除 `confirmed_verifiers_digest` 并使 Goal 回到 `Waiting(PlanConfirmation)`，必须经 `goal confirm` 人工核对锁定后方可进入可执行状态。
- **沙箱执行与不变量**：宿主侧 `run_step_verifier` / `execute_host_verifier` 校验 `verifiers_confirmed()`、剩余活跃时间预算、`PLAN` 跳过、`[permissions]` `deny`/`ask`/`Manual` 确认、`ExecutionBackend`（原生沙箱＋受保护宿主路径）与 `SecretRegistry` 脱敏。`goals.sqlite3`（schema v2 引入 `step_verifications`，v3 增加 `dispatches.subagent_id`；含原子迁移链）持久化宿主验证审计记录，`GoalRecord::validate` 强制约束带 verifier 的步骤不得在缺少退出码 `0` 且状态 `passed` 的宿主验证证据时标记为 `Completed`。

### 自动推进门禁与有界重规划（D-080 / DEV-092）

- **验证门禁**：`soyo-app::goal::run_step` 在步骤的 Run 结束后、同一 Goal 租约与心跳内执行已确认的 verifier，并经 `finish_dispatch_with_verification` 在同一事务写入 `StepVerificationResult`。仅当宿主退出码为 `0` 且状态为 `passed` 时步骤才为 `Completed`；模型文字声称完成、Run 成功均不足以关闭步骤或 Goal。`start --auto`、`continue` 与 TUI `/goal start` 在 `!verifiers_confirmed()`（verifier 被修改而未重新 `goal confirm`）时拒绝推进。
- **失败重规划**：verifier 退出非 `0` 时，`replan_failed_step` 在 `max_replans` 内把经 `SecretRegistry` 脱敏、控制字符替换为空格的单行宿主输出摘录写入该步骤的修正指令，保留已锁定的 verifier 并清除旧 `last_verification`，随后 `--auto` 继续重试；`max_replans` 耗尽后 Goal 进入 `limit_reached`（退出码 5），绝不 `completed`。
- **保守停止**：`deny` 规则或权限拒绝进入 `Blocked(Authorization)` 且不重规划；超时、取消与未知基础设施错误进入 `Waiting(UnknownSideEffect)`，不自动重放；`PLAN` 跳过执行，步骤不会被视为通过。
- **验收证据**：`crates/soyo-cli/tests/goal.rs` 以原生 Seatbelt（不设 `backend = "host"`）端到端覆盖：模型第一次谎称完成 → verifier 退出 1 → 重规划 → 第二次通过 → Goal `completed`；`max_replans = 1` 且持续谎报 → `limit_reached`；`deny`／超时／未确认分别 fail-closed。

### 自动推进链（D-111）

- **opt-in 持久化**：`GoalRecord.auto_chain` 默认 `false`，经 `soyo goal start GOAL_ID --auto --chain` 或 `[goal] auto_chain = true` 显式开启；`start --auto` 的既有配置指纹与授权记录原样复用——授权覆盖整条链，配置变化（含权限、profile、沙箱等指纹项）仍让链停在既有 `blocked / authorization` 状态，需人工恢复并重新显式授权。关闭时保留“一次 `start --auto` 在本进程内尽量推进”的语义，且 `soyo goal worker` 不会自动恢复普通 `--auto` Goal。
- **推进条件**：当前步骤宿主 verifier 落库 `passed` 且退出码 `0`、`confirmed_verifiers_digest` 未变、剩余预算 > 0、无 `waiting_unknown` 副作用、Goal 未取消／暂停时，worker 在同一 `start --auto` 序列或后续 `goal worker` 轮次里调度下一 pending 步骤的 Run + verifier；每一步仍各有独立 `Run`、租约心跳与 dispatch intent 记录。
- **保守停止绝对**：`unknown` 副作用、取消、超时、`!verifiers_confirmed()`（verifier 被修改而未重新 `goal confirm`）、`deny`／`PLAN` 跳过各自进入既有 `waiting`／`blocked`／`limit_reached`，绝不自动重放未确认副作用；verifier 失败仍在 `max_replans` 内 `replan_failed_step` 后继续。模型自述“完成”不关闭步骤或 Goal 的不变式保持。
- **可见性**：`goal status` 在文本输出中为链式 Goal 打印 `Chain: on · N/M steps done · next step K`；`goal report` 的 JSON 记录含 `auto_chain`，Markdown 报告为链式 Goal 增加 `Auto-chain` 行。桌面端经 helper `goal_start` 启动时使用 `[goal] auto_chain` 配置值。
- **验收证据**：`crates/soyo-cli/tests/goal.rs` 覆盖三步链全部通过 → `completed`、verifier 失败在 `max_replans` 内重规划后继续、持续失败落到 `limit_reached`、verifier 未重新确认时 `start --auto --chain` 被拒绝、`max_runs` 预算在下一步 dispatch 前耗尽转为 `limit_reached`、`goal worker` 恢复链式 Goal 但不恢复普通 `--auto` Goal。

### 步骤委派持久子 Agent（D-112）

- **模型与确认**：`GoalStep.delegate` 为可选 `{"subagent": true}`；`PlannedStep` 同样接受该字段，`goal plan` 草案与 `--plan-file` 均可提议。委派步骤**必须**同时具有宿主 `verifier`，否则 `GoalPlan::parse`／`apply_plan`／`GoalRecord::validate` 以 `invalid_goal` 拒绝；`goal set-verifier --clear` 也不能清除委派步骤的 verifier。计划替换或 verifier 修改仍清除 `confirmed_verifiers_digest` 并要求重新 `goal confirm`，与 D-080 的锁定语义一致。
- **执行边界**：执行前要求 `[subagents] enabled = true` 与 `[tasks] enabled = true`，且 Goal／步骤模式不是 `PLAN`（`plan_write_denied`）；配置缺失在 `begin_dispatch` 之前失败，不会留下悬挂 dispatch。Goal worker 以 Goal 的会话、工作区、profile 与模式为父，经 `spawn_persistent_job` 创建 `run_kind = "subagent"` 的持久任务（D-110），子策略由 `Policy::derive_for_subagent` 从 Goal 当前策略逐级收紧，使用隔离工作区；宿主数据／状态／凭据目录等受保护路径不会进入子工作区拷贝。子 Agent 的 exec 仍受 Goal 继承的 `deny`／`ask`、权限模式与原生沙箱约束。
- **等待与租约**：Goal worker 在自身 lease 心跳与暂停／取消轮询下，用与 D-110 相同的 `run_subagent_once` 驱动该任务。步骤的 Run 记录保留，`GoalStep.subagent_id` 与 `goals.sqlite3` schema v3 的 `dispatches.subagent_id` 共同记录关联；子 Agent 的模型请求、工具调用与活跃时间计入 Goal 预算。
- **宿主 verifier 闭环**：子 Agent 到达 `Completed` 后，宿主以三方 SHA-256 检测把隔离工作区变更合入 Goal 工作区；合并成功后仍由 Goal 侧 `run_step_verifier` 在原生沙箱与现行权限规则下执行已确认的 verifier。只有子 Run 完成、合并成功且 verifier 退出码 `0`／`passed` 时步骤才 `Completed`；子 Agent 自述完成不关闭步骤或 Goal。verifier 失败进入既有 `max_replans` 重规划；合并冲突或子任务已知失败记为已知失败并允许有界重规划。
- **保守停止**：子任务 `waiting_unknown`、运行被取消、超时或结果无法确认时，步骤记为 `Failed`，Goal 进入既有 `Waiting(unknown_side_effect)`，`auto_chain` 停止推进且绝不自动重放；`goal resolve STEP --retry` 清除旧 `subagent_id` 并显式重新排队（重试会 spawn 新子 Agent），`goal replan` 仍受 `max_replans` 限制。旧子 Agent 任务仍可经 `soyo subagent status|resolve|cancel` 独立检查。
- **自动推进链**：结合 D-111，委派步骤 verifier 通过后与直接步骤完全一致地调度下一步；unknown／取消／超时／预算／未确认的保守停止保持不变。
- **可见性**：`goal status` 文本为委派步骤打印 `Delegated subagent: <task-id> · <state>`；`goal report` JSON 含 `delegate`／`subagent_id`，Markdown 增加 `Delegated subagent` 行。TUI 与桌面端 Goal 面板暂未展示委派字段，作为后续增量。
- **存储迁移**：`goals.sqlite3` 由 v2 原子迁移到 v3（`dispatches` 增加 `subagent_id TEXT`），v1 数据库按 v1→v2→v3 链式升级；旧记录在字段缺省时继续可读。
- **验收证据**：`crates/soyo-cli/tests/goal.rs` 覆盖委派子 Agent 写入 → 宿主合并 → verifier 通过；子 Agent 谎报完成 → verifier 失败 → 有界重规划；运行中暂停 → 子任务 `waiting_unknown` → Goal `waiting` 且 `goal worker` 不重放；委派步骤缺少 verifier 被计划校验拒绝；`auto_chain` 在委派步骤通过后继续调度下一步。`crates/soyo-core/src/goal/tests.rs` 覆盖 v2→v3 迁移、委派链接与 unknown 收尾的 store 不变量。

| Continuation | 显式 auto 标记、模式、权限模式、配置指纹、授权时间、worker owner／过期租约及持久 dispatch 状态 |

当前实现将一个 Goal 绑定到一个专属 Session，步骤关联多个 Run；跨 Session 关联及子 Goal 按后续需求扩展。工具调用继续使用 `(run_id, call_id)` 关联。

证据中的文件包含生成时摘要，核验时重新检查版本。中间 Step 只在文件条件已满足时记录证据，暂未满足不会阻断后续 Step；全部 Step 完成后会重新核验所有文件条件，文件变化或缺失会使旧证据失效。计划完成、模型自述、命令退出码和目标验收分别记录，目标完成要求每条验收条件具有与核验类型、路径及摘要匹配的证据。

## 生命周期草案

| 状态 | 含义与后续动作 |
| --- | --- |
| draft | 目标已保存，等待计划及验收条件收敛 |
| ready | 当前步骤及授权范围已明确，等待启动 |
| running | 正在规划、执行、核验或有限重规划，具体阶段单独记录 |
| waiting | 等待确认、输入或外部条件，记录 wait reason 与对应请求 |
| paused | 用户暂停或当前 Run 被停止，等待显式继续 |
| blocked | 前置条件无法推进，记录原因及可执行的恢复入口 |
| completed | 验收条件及证据全部满足 |
| failed | 遇到终止性错误，保留已完成步骤与产物 |
| cancelled | 用户取消，完成受管理资源清理并保存实际结果 |
| limit_reached | 预算或尝试上限到达，等待用户处理目标 |

```mermaid
flowchart TD
    D[draft] --> R[ready]
    R --> A[running]
    A --> W[waiting]
    W --> R
    A --> P[paused]
    P --> R
    A --> B[blocked]
    B --> R
    A --> V[核验阶段]
    V --> C[completed]
    V --> R
    A --> F[failed]
    A --> L[limit_reached]
    A --> X[cancelled]
```

暂停、取消和预算停止首先关闭新调度，再传播到当前 Run 及后台任务，清理结束后保存状态。进程结果无法确认时记录 unknown，并保留实际清理证据。

## 用户入口草案

交互入口与 CLI 子命令使用同一应用服务。

```text
/goals
/goal create Deliver a Rust API with tests and documentation
/goal plan GOAL_ID
/goal confirm GOAL_ID
/goal status GOAL_ID
/goal continue GOAL_ID
/goal start GOAL_ID
/goal pause|resume|cancel GOAL_ID
/goal verify GOAL_ID criterion-1 Reviewed evidence
/goal report GOAL_ID
```

```sh
soyo goal create "Deliver a Rust API with tests and documentation"
soyo goal plan GOAL_ID
soyo goal continue GOAL_ID
soyo goal report GOAL_ID
soyo goal start GOAL_ID --auto --mode build
soyo goal worker
```

创建先保存目标。计划生成和步骤执行各自由明确操作启动，G1 的 continue 执行一个步骤后返回。恢复加载状态、当前配置和证据，随后等待继续指令。`soyo resume SESSION_ID` 使用会话恢复语义。

## 权限、模式与预算

- 计划、模型输出和验收结果作为数据进入编排服务，工具权限仍由当前运行策略决定。Manual Approval 逐工具确认，PLAN、deny 以及选定隔离后端的边界在实际派发处验证。
- 模式来自用户选择。自动阶段的首版建议使用用户选定的固定执行模式，模式调整由 Agent 提议并等待用户确认，在调用边界生效。
- 自动继续需要独立、有限的 `start --auto` 启动授权，记录模式、权限、profile／权限规则／沙箱／运行上限构成的配置指纹和预算。worker 或手工恢复时重新判定当前权限；配置变化会令自动 Goal 进入 `blocked / authorization`，需人工恢复并重新显式授权。
- 自动启动前核验所选后端的文件、网络、资源和清理能力，硬要求缺失时给出可处理原因。best_effort 的资源与清理能力按显式 profile 展示，具体矩阵见[原生后端设计](sandbox.md)。
- 每个 Run 保有模型、工具、活跃时间和清理上限。Goal 总预算累计 Run 数、模型请求（含 review／plan／compaction）、工具、活跃时间、输入／输出 token、规划／核验尝试及重规划；达到任一硬上限后，在下一个 Step dispatch 前停止新调度。
- 成功规划、执行或人工核验缺少完整 input／output token usage 时，记录 `token_usage_unknown` 并保守停止后续 Step 调度，不按零用量处理。已知失败 Run 保留已观察用量，是否重试仍由 unknown 边界和 `max_replans` 单独约束。
- `continue --approval-file` 的预授权只适用于该次 Run；`start --auto --approval-file` 只装入自动序列的第一个 Run，不会在后续 Step 重新加载或恢复已消费授权。
- 重试和重规划具有次数上限、失败分类及停止原因。结果不明的副作用进入等待处理，重试前先检查已有意图、结果和幂等记录。

## 持久化与恢复

Goal、Step、Run 关联和调度意图位于独立版本化数据库，按事务和配置化 redaction 保存。目标状态改变与步骤启动 intent 在事务内提交，之后才调用运行服务；写入失败停止后续调度。状态更新使用 revision CAS，验收证据与 completion 在同一事务内发布。

每个 Goal 同时运行一个受管理 Run，持有 Goal lease 和现有 Session 写锁。worker 心跳续租；续期失败会立即请求取消当前 Run，避免失去排他所有权后继续执行。只恢复租约已过期的 dispatch。intent／running 未得到终态的 dispatch 标为 unknown，并等待 `goal resolve --retry`，不会自动重放。`goal continue` 与 TUI 前台执行使用同一租约和 Session 锁边界。

G2 引入持久队列后，再根据明确的自动授权恢复调度。0.1.0 运行内后台任务的结果作为原有 Run 数据记录，其现有结束清理语义在适用运行中继续执行。

## 验收

DEV-069／070 验证创建、计划、逐步执行、模式选择、证据、单目标锁和手动恢复。DEV-071／072 注入 worker 崩溃、调用已开始但结果未保存、未知副作用、预算耗尽、过期授权和核验失败。DEV-073／074 验证 TUI／CLI 的一致视图、报告导出和全生命周期。

完成判断包括需求对应、真实产物、指定验证及证据版本。模型声称完成但证据缺失的案例应保持在待核验或待处理阶段。所有测试默认采用可控模型、时钟、临时工作区和受控执行后端。
