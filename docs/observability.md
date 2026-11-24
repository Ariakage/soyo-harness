# 日志与追踪设计

**DEV-040 至 DEV-042 已实现五级类型化日志、受控 tracing 接入与 CLI 查询。实际验证记录见[开发进度](progress.md)。**

## 已有实现

[logging.rs](../crates/soyo-core/src/logging.rs) 提供 `JsonLog`，接受 `trace`、`debug`、`info`、`warn`、`error`，默认严重程度阈值为 `warn`，在已选 state 目录写 JSON 行，单文件 10 MiB、最多五份备份，写入前进行已知凭据脱敏，并用跨进程锁保护轮转。CLI 的 `log_run_result` 记录未完成运行的摘要，`log_warning` 处理写入失败。

[events.rs](../crates/soyo-cli/src/events.rs) 的事件输出与 SQLite 持久事件负责运行展示及恢复。新增诊断日志与这些数据分别维护，并通过 ID 关联。

## 类型化记录

`DiagnosticRecord` 使用 `LogContext` 关联 session／run／request／call／job／span，`LogFields` 白名单接受状态、错误码、profile、协议、目的、耗时、字节数、数量及丢弃数。新记录保存 `schema_version = 1`、UTC 时间、进程号和本 logger 的单调序号。旧 `record` API 与旧行解码继续可用。

每条记录最多 64 KiB，写入前使用统一脱敏处理已知凭据与嵌套 JSON。默认阈值在打开目录前过滤，配置为 trace 后可记录全部五级。日志文件为 0600，新建目录为 0700，拒绝符号链接、硬链接、其他用户拥有的文件及过宽文件权限。写锁最多等待一秒，失败通过 `log_error` 返回。轮转维持 10 MiB 与五份备份。

## tracing 写入与关联

`logging::diagnostics::Diagnostics` 管理本地 writer 线程和 subscriber，应用入口安装 dispatch 并保留 guard 至退出。默认普通队列容量 256，warn／error 优先队列容量 32，普通事件队列满后计数丢弃，高优先事件最多等待 20 毫秒。`flush` 返回写入失败或超时，退出最多等待两秒，`diagnostics.dropped` 记录累计变化。

subscriber 只接收 `soyo`／`soyo::` target，按类型化白名单采集字段，未知字段和 message 被过滤。超长单字段以 withheld 标记处理后再进入队列。span 保存 session／run／request／call／job 归属，后台任务显式传播 subscriber 与 span。运行、模型执行、独立审核、工具派发和后台状态通过同一诊断层关联，模型完成记录包含单调时钟耗时。

## 五级日志

| 级别 | 记录内容 |
| --- | --- |
| trace | 状态边界、调度阶段、队列与 span 元数据 |
| debug | 参数校验结果、连接阶段、限额判断及耗时 |
| info | 会话／运行／工具生命周期摘要 |
| warn | 可恢复错误、重试、权限拒绝及能力降级原因 |
| error | 运行失败、存储失败及需要处理的系统错误 |

默认级别为 `warn`，通过 `--log-level` 或 `SOYO_LOG_LEVEL` 显式选择其他级别，优先级为 CLI 高于环境变量、高于默认值。首批保留现有配置文件加载方式。

```sh
soyo --log-level debug chat
soyo --log-level trace run "Investigate the failure"
soyo logs show --session SESSION_ID --level warn
soyo --output jsonl logs tail --session SESSION_ID
soyo --output json logs show --level info --limit 100 --offset 0
soyo logs show --since 2026-11-08T08:00:00+08:00
```

级别过滤按严重程度阈值处理，`warn` 同时包含 `error`。时间范围使用带时区输入，保存的时间为 UTC。耗时使用单调时钟，日志排序同时参考进程、关联 ID 和本地序号。

## 日志查询

`logs show` 支持 `--session`、`--run`、`--level`、`--since`、`--until`、`--limit` 与 `--offset`，查询阈值默认 trace，包含所有已保存级别。每页默认 100 行、最多 1,000 行，offset 最大 1,000,000。时间使用显式时区的 RFC 3339，起止边界均包含。查询返回 `records`、`next_offset` 与 `malformed_lines`，兼容旧记录中 data 的 session／run 关联。

查询只读当前与五份轮转文件，缺失日志返回空页。每文件最多读取 10 MiB、每行最多 64 KiB，超长、损坏及末尾部分行有明确省略计数，并按当前配置重新脱敏。JSON 使用版本信封，JSONL 逐行输出 `log.entry` 和 `logs.page`。text 输出 JSON 转义文本，额外转换 DEL／C1 等控制字符。

`logs tail` 使用同一筛选与每批 limit，按 inode、持有的文件描述符及完整行 offset 跟随轮转，重复观察保持游标，部分行等待写完。扫描由 blocking worker 执行并检查共享取消，管道输出使用非阻塞小块写入，慢消费者时仍可响应 SIGINT／SIGTERM，退出码 130。tail 提供 text／JSONL 表达，选择 JSON 返回用法错误。

## 接入与记录结构

使用 `tracing` 的 span／event 作为进程内诊断入口，在可执行入口安装 subscriber，安全记录层负责字段选择、脱敏、大小检查和轮转。库层发出结构化诊断，应用入口管理订阅生命周期。异步任务通过显式 span 传播归属。设计参考官方 [tracing](https://docs.rs/tracing/latest/tracing/) 和 [tracing-subscriber](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/) 文档。

记录保留 `timestamp`、`level`、`event`、`data`，在兼容读取旧行的基础上扩展格式版本、target、trace／span、session／run／request／call／job ID、状态、耗时和字节计数。新增字段由 DEV-040 的类型化记录固定，旧日志按缺失元数据读取。

记录路径采用以下顺序。

1. 采集类型化字段，文本参数使用字段白名单。
2. 对结构化值执行已知凭据遮蔽，处理嵌套／编码内容及控制字符。
3. 执行单行与队列上限检查，附加关联元数据。
4. 通过现有私有目录、文件权限、跨进程锁和轮转机制保存。

Prompt、环境变量、HTTP 认证头、命令完整环境和工具正文具有独立的数据访问范围。五级日志默认记录其 ID、大小、耗时和状态。第三方诊断按 target 白名单接入，过滤发生在落盘与显示之前。

## 并发、失败与输出

- 使用有界后台写队列承接阻塞文件操作。队列满时记录诊断丢弃计数，保留 warn／error 的优先通道，并设置有界等待和退出排空策略。
- 关键业务事件继续通过持久化入口保存。日志写入失败显示经过脱敏的降级状态，涉及执行意图／结果的存储失败按现有运行终止契约处理。
- CLI 诊断与运行 JSON／JSONL 的 stdout 分离。TUI 中由界面展示诊断状态，终端绘制保持单一输出所有者。
- `logs show` 分页读取轮转文件，`tail` 跟随文件更换。二者处理损坏尾行、取消、符号链接和私有文件权限，输出经过终端安全转换。
- 日志保持本地保存，远端追踪接收端属于后续独立集成。

## 只读投影与回放

F-12 的投影层在已持久化采集底座（sessions／runs／events／change_log／run_metrics）之上提供四条只读 CLI 命令，实现位于 `soyo-app::observe`（纯函数投影）与 `soyo-cli::observe`（命令装配与输出）。所有命令经 `SqliteStore::read_snapshot` 读取不可变 WAL 快照——不加会话锁、不触发迁移、不写 WAL；唯一例外是无 `--session` 的 `soyo usage`，它经活动存储执行只读 SELECT 聚合全部会话。四者均不构造运行时、不派发模型请求、不执行工具、不访问网络，会话被其他进程持锁时仍可查询。

- `soyo usage [--session ID] [--since DAYS] [--by run|profile|day]`：聚合 run_metrics 的 execution_requests、token 与按币种 microunits 累加的 cost，保留 by_profile 归因行；token 仅在已上报值上累加，`estimated` 沿记录向上传播。`--session` 路径走快照，`--since`（默认30天，上限365）仅对跨会话路径按 `runs.started_at` 下限过滤；day 分组按 UTC 日界，仅在跨会话路径可用。
- `soyo replay SESSION_ID [--run ID] [--step N]`：把 events（按 seq）与 history（按 ordinal）投影为有序时间线，tool 行经持久 `tool_calls` 补齐 run 归属；`ProviderItem` 不透出，输入／输出字段分别按 4 KiB／64 KiB 截断。`--step` 输出单步完整 input／output。
- `soyo diagnose SESSION_ID [--run ID]`：在同一时间线上定位第一个 `failed`／`denied`／`cancelled` 步骤（覆盖 tool.finished、model.finished、permission.resolved denied、run.finished 非 completed），输出错误码、最近健康步骤序号与 run 终态。
- `soyo session report SESSION_ID [--run ID] [--format markdown|json] --file PATH`：把指定 run 的元数据、RunResult、RunMetrics、时间线与诊断经 `publish_session` 发布——与 `session export` 相同的独占创建、0600 权限、PLAN 工作区拒绝约定；前者面向 run 级报告，后者面向整会话转录。未指定 `--run` 时选最近 finished run，否则最后一个持久化 run，均无则报 `run_not_found`。

输出与其他查询一致：text 为终端安全文本，JSON 使用 `{schema_version:1,data}` 信封，JSONL 附加 `type`；取消退出130，其余错误退出2。

## TUI 与桌面端观测面板

F-12 把同一投影层接到两端，全部只读，不派发模型请求、不执行工具、不访问网络、不写入文件。

- **TUI**：`/usage`、`/replay [RUN]`、`/diagnose [RUN]`、`/session-report [RUN]` 打开观测模态（复用 Memory／Checkpoint 弹窗外壳）；模态内另有 `step N` 单步钻取与 `help`／`close`。本地后端经 `read_snapshot`，helper 后端经 `read_session_snapshot`，投影调用 `soyo_app::observe`。详见 [TUI](tui.md#f-12-观测模态)。
- **桌面端**：`soyo-host` 新增 `observe_usage`／`observe_timeline`／`observe_diagnose`／`observe_report` 四个内部 IPC method（`session_id` 缺省时复用检查点的会话解析：绑定会话→工作区最新会话），返回 bounded 且脱敏的 JSON，report 另附渲染后的 Markdown。桌面端 Settings 新增「Observe」设置页与侧栏入口：用量表（run／profile 分组）、可展开的时间线（单步 input／output）、诊断摘要与 Markdown 运行报告。详见[桌面端](desktop.md)。

`soyo_app::observe::metrics_records` 统一把快照 `run_metrics` 行解码为 `RunMetricsRecord`，CLI、TUI、helper 与评估共用同一解码。

## 固定任务集回归评估

`soyo eval FIXTURE_DIR` 对固定、已录制的会话快照集合运行与 `session report` 相同的投影，并按记录逐项比对期望结果，用于两个版本之间的回归比较。全程离线：fixture 是合成的持久化快照，没有真实模型请求、网络或付费额度依赖。

fixture 为目录下的 `*.json` 文件，按文件名排序读取，每个文件一个用例：

```json
{
  "name": "failed-tool",
  "description": "optional one-line context",
  "run": "optional run id; defaults to the latest finished run",
  "expect": {
    "steps": 5,
    "failed_step": 3,
    "error_code": "tool_error",
    "run_status": "failed"
  },
  "snapshot": { "created_at": "...", "session": { "...": "..." }, "runs": [], "events": [] }
}
```

`expect` 四项为必填的完整期望：`steps` 为所选 run 的投影步骤总数；`failed_step`／`error_code` 为 `null` 表示期望无失败；`run_status` 为 `null` 表示 fixture 中 run 未结束。`evaluate_case` 复用 `run_report`，输出与 `session report` 相同的 `run`／`result`／`metrics`／`timeline`／`diagnosis` 字段，另附 `expected`、逐项 `checks`（expected／actual／passed）与 `passed`。`eval_report` 汇总通过／失败数，`eval_markdown` 输出确定性 Markdown。

任一 fixture 不匹配时退出码为 2（投影错误同样为 2，取消为 130），便于 CI 以非零退出捕获版本回归；`--output json` 的信封可直接 diff。样例 fixture 位于 `fixtures/observe-eval/`：失败工具、干净完成与权限拒绝三个用例。

## 验收

DEV-040 验证全部级别、旧行读取、字段白名单、深层编码秘密、权限、符号链接和跨进程轮转。DEV-041 验证并发模型／核查／工具／后台任务的 span 归属，以及队列过载、慢磁盘与退出排空。DEV-042 验证筛选、跟随、取消和 JSON／JSONL stdout。

诊断完整性、业务事件完整性和副作用保存分别给出结果，测试使用本地模型、合成凭据及故障注入。任务依赖见[后续开发计划](post-mvp-plan.md)。
