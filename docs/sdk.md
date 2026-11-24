# SDK 与稳定 wire 契约

> 本文档定义 Soyo 对外承诺的 **稳定面**：`soyo-sdk` crate 与 `soyo-ipc` wire
> 契约中对外公布的 method 子集。对应决策 D-105（socket 客户端形态）、D-106
> （冻结 wire 契约而非 Rust 类型签名）、D-107（断线≠取消）。未列出的 helper
> method 均属实现细节，可在不 bump 协议号的前提下调整。

## 定位

- `soyo-sdk` 是面向外部 Rust 消费者的客户端 SDK，收敛在
  `soyo_ipc::HelperClient`（`connect_or_spawn`＋helper 兼容探测＋NDJSON
  客户端）之上；不提供进程内嵌入，不冻结 `Config`/`Session`/运行时内部
  API。所有调用仍经 `soyo helper` 的权限、sandbox 与运行入口。
- 稳定边界是 **wire 契约**：`PROTOCOL_VERSION`、对外公布的 method 名字
  符串与相关 DTO 的 JSON schema。SDK 自身的 Rust 类型签名（函数名、参数
  结构、返回类型）在 0.x 内仍可演进。

## 冻结规则（D-106）

`crates/soyo-ipc/src/protocol.rs` 中的 `PUBLISHED_METHODS` 是唯一权威的
对外 method 清单。规则：

- 对表中任一 method 的**不兼容变更**（改名、删除、请求参数或结果 DTO
  schema 的破坏性修改、语义反转）必须先 bump `PROTOCOL_VERSION`。
- 兼容性扩展允许直接发生：新增可选请求字段、新增结果字段、新增 method
  （先作为内部方法，成熟后再纳入下表）。
- `soyo-host/src/server.rs` 的 `dispatch_tests` 保证表中每个 method 实际
  存在 dispatch 分支；改 method 名时测试会立即失败。

### 对外公布的 method（16 个）

| 分类 | Method | 说明 |
| --- | --- | --- |
| helper 握手 | `helper_info` | 版本、`protocol_version`、pid、活动 run/client 计数；客户端兼容探测依据 |
| helper 握手 | `get_bootstrap_state` | 配置就绪、工作区、profile、当前会话与 `active_run_id` 快照 |
| 会话 | `new_session` | 创建会话（可选 `cwd`/`profile` 覆盖），返回 `ActiveSessionDto` |
| 会话 | `resume_session` | 恢复既有会话（`ResumeSessionDto`），返回 bootstrap＋恢复计数 |
| 会话 | `release_session` | 释放本连接对会话的占用（不删除会话数据） |
| 会话 | `list_sessions_page` | 分页列出会话元数据（`SessionPageDto`） |
| 会话 | `get_session` | 读取单条会话持久记录（脱敏 JSON；不存在返回 `null`） |
| 会话 | `read_session_snapshot` | 只读快照（历史／事件／metrics），不锁定、不迁移源会话 |
| 运行 | `submit_prompt` | 交互式流式 run；`SubmitPromptParams`，`session_id` 钉住会话，不一致返回 `session_changed` |
| 运行 | `run_prompt` | 非交互流式 run（`RunPromptParams`），可推 `RawSchemaEvent` 直通 schema-v1 事件 |
| 运行 | `compact_session` | 压缩 run；`raw_events` 可选，`RunCompleted.compaction_pending` 报告排队状态 |
| 运行 | `cancel_run` | 取消 run——显式 `run_id` 定向取消，缺省取消本连接全部 run |
| 运行 | `get_run_state` | 会话级 mode/pending/compaction 状态（`RunStateDto`） |
| 任务 | `task_list` | 持久化任务队列投影（`Vec<TaskDto>`） |
| 任务 | `task_jobs` | 在跑的 helper 任务作业（`Vec<TaskJobDto>`） |
| 任务 | `task_status` | 单个任务投影（`task_id` → `TaskDto`） |

流式 method（`submit_prompt`/`run_prompt`/`compact_session`）在终态
`Response`/`Error` 之前按请求 id 推送 `ServerFrame::Event`，载荷为
`DesktopStreamEnvelope`；run 归属与应答路由规则不变——`reply_approval`、
`reply_input` 只接受发起连接，不属于本批公布面。

## 断线语义（D-107）

- **断线≠取消**：run 由 session 的 `active_run_id` 绑定，不归发起它的
  socket 连接所有。SDK `detach()`（或连接意外断开）**不取消**在跑 run；
  helper 仅清理该连接的 pending approval/input 与 `owner_conn_id` 标记。
- **attach/detach 是显式操作**：`attach(session_id)` 让连接重新绑定到
  会话并恢复其事件订阅；`detach()` 仅结束本连接侧订阅，run 继续执行、
  可经 `get_run_state`/`read_session_snapshot`/`run_events` 重查。
- 同一用户的另一客户端仍可通过带显式 `run_id` 的 `cancel_run` 取消任一
  run；对非自有 run 只读观察。
- 该语义取代「客户端断开即中止工作」的直觉默认；D-077 第 (4) 条描述的
  「所属连接关闭时取消其全部 Run」已按 D-107 修正为上述模型。

## 版本协商

客户端连接后先读 `helper_info`：仅当 `version` **或**
`protocol_version` 与本地 `PROTOCOL_VERSION` 不一致时判为 stale；
不一致且无活动 run/其他客户端时可回收 helper 重启，有 run 活动则
保持现状并提示。exe 路径永不参与比较（D-077 第 (9) 条）。
