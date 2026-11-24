# MCP 与凭据管理

当前 N2 开发线支持显式配置的 stdio、HTTP MCP 和宿主凭据引用，已通过完整四平台整体验收。0.2.0 的发行范围与限制继续按其历史记录维护。[N2 验收记录](n2-validation.md)、[0.2.0 发行说明](releases/release-notes-0.2.0.md)

接入使用固定 `rmcp = 3.9.0`，协商首选协议 `2026-07-28`，兼容 `2025-11-25`。当前工具发现、目录变化与工具调用复用宿主注册、权限和持久化边界。[MCP 实现](../crates/soyo-core/src/mcp.rs)

## 配置与工具发现

MCP 服务通过显式 TOML 配置选择，最多 16 个。运行创建连接并发现工具目录，结束时清理所拥有的进程和连接。恢复会话后等待新输入，下一次 Run 按本次配置重新建立服务。

服务的原始工具名、描述、输入／输出 schema 和 annotations 来自外部服务器。宿主注册表生成带 `mcp__` 前缀、服务器片段、工具片段及稳定摘要的模型工具名。发现内容经过 schema、分页、总量与目录代次检查，实际调用继续检查当前工作模式、权限、授权绑定和取消。目录变化使旧目录授权失效，模型下一次看到的是新注册表快照，目录刷新（`replace_external`）后已移除的展开工具也会从当前暴露集合中剔除。

### 按需发现与延迟加载（D-079）

当配置启用了较多外部 MCP 工具时，可在 `[mcp]` 下通过 `defer_loading`（`"auto"`／`"always"`／`"never"`，默认 `"auto"`）与 `defer_threshold`（默认 `8`，范围 `1..=256`）控制向模型暴露完整 schema 的策略，单个服务还可通过 `eager_tools = ["..."]` 列出即使延迟生效也保持常驻的远端工具名：

```toml
[mcp]
defer_loading = "auto"
defer_threshold = 8

[mcp.servers.billing]
enabled = true
read_only_tools = ["invoice_lookup"]
eager_tools = []
```

- **阈值触发**：`"auto"` 模式下，仅当当前运行模式允许的已发现 MCP 工具总数超过 `defer_threshold`（默认 `> 8`）时自动启用延迟加载；小目录（`≤ 8`）维持既有全量直接注入，不引入额外搜索轮次。`"always"` 只要存在可用 MCP 工具即启用，`"never"` 强制全量注入。
- **稳定前缀与按需展开**：触发延迟加载后，系统提示中的核心内置工具、`tool_search`、`tool_describe` 与 `eager_tools` 按固定顺序构成稳定的前缀列表，其余 MCP schema 暂不注入；模型调用 `tool_search` 按关键词或 `server` 检索脱敏摘要后，调用 `tool_describe`（单次最多 8 个精确模型工具名）查看详情并将命中的工具定义追加到前缀末尾，保持首部系统指令与核心工具缓存前缀逐字节稳定。
- **安全边界**：`tool_search` 与 `tool_describe` 同样受工作模式（`PLAN` 仅暴露 `read_only_tools` 明确允许的只读工具）、`[permissions]` `deny`／`ask`／`Manual` 规则、目录代次绑定与 `SecretRegistry` 脱敏约束；`tool_search` 不直接展开 schema，必须通过 `tool_describe` 后才会附着到后续请求。

### 调用样例（D-081）

每个服务可通过 `tool_examples` 为远端工具配置宿主编写的真实调用样例（按原始远端工具名索引，每个工具 1–5 条、服务最多 256 项）：

```toml
[mcp.servers.billing.tool_examples]
invoice_export = [
  { month = "2026-11", format = "csv" },
]
```

样例在配置加载时做结构与占位符校验，注册时再通过该工具已发现的 input schema 离线校验与 `SecretRegistry` 脱敏检查；延迟加载的工具经 `tool_describe` 返回样例，常驻工具把样例附加到模型侧描述文本。样例只来自宿主配置或插件 manifest，不接受服务器上报内容，具体约束见[工具注册](tool-registry.md#调用样例d-081)。

工具结果按内容类型投影并限制大小。文本与结构化结果可进入模型上下文，不受支持的内容以有界元数据表示。工具失败保留明确错误，连接中断、执行已开始但完成无法确认时按 `unknown` 处理，运行时保存记录并阻止历史重放。

## Resources、Prompts、订阅与 Elicitation（D-089）

基于固定 `rmcp = 3.9.0`，`McpClient` 扩展支持服务器声明的 Resources、Resource Templates、Prompts、通知订阅与安全 Elicitation：

- **只读能力按需注册（零漂移）**：仅当至少一个已连接服务器在能力协商中通告 `resources` 时注册 `mcp_list_resources`、`mcp_read_resource` 与 `mcp_subscribe_resource`；仅当至少一个已连接服务器通告 `prompts` 时注册 `mcp_list_prompts` 与 `mcp_get_prompt`。未连接或未通告对应能力时，工具目录与模型前缀完全保持不变。
- **有界分页与内容投影**：列表与读取复用单页／总页／总字节上限，`mcp_read_resource` 返回有界文本内容或仅保留媒体类型与长度的 `Blob` 元数据（不把原始二进制直接塞入模型正文），所有输出经 `SecretRegistry` 脱敏并将服务器文本标为低信任数据；`PLAN` 模式仅开放只读查询／读取，`mcp_subscribe_resource` 继续遵守模式与目录代次约束。
- **双协议版本订阅**：针对 `2026-07-28` 协议使用 `subscriptions/listen`、针对 `2025-11-25` 使用 `resources/subscribe` 建立并管理订阅；收到 `notifications/resources/list_changed`、`notifications/resources/updated` 或 `notifications/prompts/list_changed` 时使本地缓存目录失效并触发后续读取刷新。
- **Elicitation 失败关闭**：`ClientHandler::create_elicitation` 在无宿主交互处理器（或非交互运行）时默认返回 `Decline`（`needs_input` 兼容，不自动放行或提升权限）。

## SEP-2663 长异步任务（D-104）

基于同一 `rmcp = 3.9.0`，`McpClient` 接入 `io.modelcontextprotocol/tasks` 扩展的服务器端长异步任务（`tasks/get`／`tasks/cancel`／`tasks/update`）：

- **opt-in 门控**：`[mcp.tasks] enabled = false` 为默认。仅在开启后客户端于 `initialize` 的 `capabilities.extensions` 中通告该扩展；关闭时即使 server 通告支持，`tools/call` 返回的 `CreateTaskResult` 仍按 `outcome_unknown` 失败关闭，工具目录不变。
- **宿主工具零漂移**：`mcp_get_task`、`mcp_cancel_task`、`mcp_respond_task` 仅当 opt-in 开启**且**至少一个已连接 server 通告 `io.modelcontextprotocol/tasks` 时才注册；`get` 为 `ExternalReadOnly`（PLAN 只读可见），`cancel`／`respond` 会改变 server 端状态、为 `ExternalUnknown`（PLAN 不可用）。
- **状态投影**：`tasks/get` 投影为 `run_state`（`working→running`、`input_required→needs_input`、`completed→done`、`failed→failed`、`cancelled→cancelled`，未知 terminal variant 归 `unknown`）；`input_requests` 封顶 64 条并仅接受 server 列出的 outstanding keys。
- **边界沿用**：`mcp_respond_task` 拒绝空集合；`mcp_cancel_task` ack 不代表已终止（须再 `mcp_get_task` 观察终态）；所有 `tasks/*` 调用复用 `McpLimits.request_timeout` 与取消语义；任务内部任何 `tools/call` 继续走既有权限与 sandbox，宿主不自动续跑。

```toml
[mcp.tasks]
enabled = false   # SEP-2663 tasks/*，默认关闭
```

## stdio 服务

```toml
[mcp.servers.local]
enabled = true
read_only_tools = ["lookup"]

[mcp.servers.local.transport]
type = "stdio"
argv = ["/absolute/path/to/mcp-server", "--stdio"]
# cwd = "/absolute/path/to/project"
# env = { SERVICE_TOKEN = "SOYO_MCP_TOKEN" }
```

启动参数采用结构化 argv。`cwd` 省略时使用工作区，`env` 的值是启动环境变量名。宿主显式选择服务可接收的来源引用，并绑定目标变量与实际环境摘要；常规模型命令执行采用自身的敏感环境过滤。解释器或工具链位于原生视图之外时，需添加实际 `sandbox.read_roots`。[配置说明](configuration.md#n2-原生隔离配置)

stdio 启动按通用命令执行策略完成规则判断、模型核查或人工确认，执行前后持久化启动记录。Manual Approval 会确认服务启动及之后每次模型工具调用。服务器进程在与本地命令相同的 ExecutionBackend 中启动，stdout 用于有界 MCP 协议，stderr 并发排空。

PLAN 保持 stdio 服务停止。切换请求按当前前台调用边界协调，待审批或握手阶段按切换取消，随后关闭本地 MCP 进程，清理确认后应用 PLAN。服务重启安排在下一次用户输入的新 Run，并重新走启动授权。运行结束同样等待清理。握手前尚未派发进程的预算耗尽保存为已知失败，进程启动后握手中断保存为 `unknown`，随后继续完成进程收尾。

## HTTP 服务

```toml
[mcp.servers.remote]
enabled = true
read_only_tools = ["lookup"]

[mcp.servers.remote.transport]
type = "http"
endpoint = "https://mcp.example.com/mcp"
scopes = ["files:read"]
```

完整 endpoint 由宿主配置。HTTP MCP 默认使用 HTTPS，明文 endpoint 需要显式 `allow_insecure_http = true`。该连接由宿主管理，和原生工具进程的 `sandbox.network` 代理策略分别处理。[原生网络边界](sandbox.md#网络代理桥)

可用 `bearer_env` 引用进程提供的 bearer token，或通过 `headers_env` 为额外请求头指定环境变量名。启用服务时校验这些引用，实际值加入共享已知凭据集合。`credential_ref` 选择下节的宿主私有存储，与 `bearer_env` 互斥。请求目标绑定完整配置 endpoint，HTTP 传输关闭自动重定向与请求重试。

运行期仅开放宿主 `read_only_tools` 明确分类的 PLAN HTTP 工具。其他工具继续按副作用未知处理，服务器自报只读 annotations 不能授予权限。401／403 返回鉴权或权限错误，宿主等待新的显式授权流程。401 后原 MCP 工具请求保持单次发送。

## 导入、登录与引用

管理命令使用显式 HTTP 服务名，凭据值通过已提供的环境引用或授权码交换取得。以下命令中的 `SOYO_MCP_ACCESS_TOKEN` 是环境变量名，值由使用者的凭据管理方式在启动前提供。

```sh
soyo --config /path/to/config.toml mcp credentials import remote \
  --from-env SOYO_MCP_ACCESS_TOKEN --scope files:read --approve-scopes
soyo --config /path/to/config.toml mcp credentials list
```

导入和登录的授权范围由配置 `scopes` 与重复 `--scope` 参数的并集组成。默认在控制终端展示服务器、endpoint 和完整 scope 集并询问确认，`--approve-scopes` 显式批准本次选择。成功返回新的不透明 UUID reference，列表仅返回元数据。

管理入口在审批前检查服务器、原始 endpoint、scope 和 client ID 中的已知凭据，导入来源变量的实际值也计入检查。实际授权 URL 在显示或交给系统 URL handler 前再次检查，包括有界 URL 编码视图；命中时返回 `sensitive_data`。正常授权流程中的 state 与 PKCE challenge 按授权 UI 字段展示。[管理入口](../crates/soyo-cli/src/mcp.rs)

交互 OAuth 使用授权码、PKCE S256、单次 state、精确 loopback 回调地址、issuer 与有效期检查。可指定已有 public client ID，省略时尝试动态注册 public client。授权 URL 写入 stderr，`--open-browser` 使用系统 URL handler 打开该地址。登录要求 stdin 与 stderr 均连接终端，即使已经提供 `--approve-scopes` 也会检查交互条件。

```sh
soyo --config /path/to/config.toml mcp login remote \
  --scope files:read --client-id YOUR_PUBLIC_CLIENT_ID --open-browser
```

默认授权等待 300 秒，`--timeout-seconds` 支持 1 至 600 秒，`--callback-port` 可显式选择 IPv4 loopback 端口。发现、回调等待和令牌交换共享本次登录的有效期，并响应取消。进入本地私有凭据提交阶段后，宿主等待原子写入的实际结果并返回真实 reference。

将返回的 reference 写入服务的 transport 配置。下面 UUID 是格式示例，需替换成实际返回值。

```toml
[mcp.servers.remote.transport]
type = "http"
endpoint = "https://mcp.example.com/mcp"
credential_ref = "00000000-0000-0000-0000-000000000000"
scopes = ["files:read"]
```

服务器名称、精确 endpoint 与批准的 scope 集共同绑定该引用。OAuth 还记录 issuer 与发现的 canonical resource。canonical resource 可以是同 origin 下覆盖 endpoint 的祖先路径，仍与精确的 MCP 请求 endpoint 分别保存和检查。恢复时验证 issuer 与 canonical resource，变化后需要重新登录。Resource Indicator 用于授权和令牌请求，issuer 回调按 RFC 9207 比较。[MCP 授权规范](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)、[RFC 9207](https://www.rfc-editor.org/rfc/rfc9207.html)

运行中的 MCP 客户端在每次新 HTTP 请求前重新解析引用。OAuth 刷新采用跨进程刷新锁，并重新核对 scope、client 与 issuer，令牌轮换持续加入共享脱敏集合。撤销使引用永久失效，旧刷新结果不能恢复该引用。需要新授权时创建新的 reference。

`soyo credentials` 是统一管理入口，除 `list`/`import`/`revoke` 外新增 `token issue`（为远程 helper 签发一次性显示的 bearer token，只持久化 SHA-256 摘要与 `run_kinds = ["remote"]` grant，撤销走同一 `revoke` 路径）、`rotate`（secret 类原地换值、`revision` 自增、引用不变）、`grant`/`grant-revoke`/`grant-list`（scoped 授予管理）与 `login`（不要求启用 server，但仍需一个已配置的 HTTP server 名绑定 scope）；`soyo mcp credentials` 委托同一实现。`import`/`revoke`/`rotate`/`grant`/`login`/`token issue` 各写一行脱敏 `credential.event` 审计到 `<credentials>/audit.log`（0600 append-only、写前校验不含已知 secret）。

```sh
soyo --config /path/to/config.toml credentials list
soyo --config /path/to/config.toml credentials revoke RETURNED_REFERENCE
soyo --config /path/to/config.toml credentials rotate RETURNED_REFERENCE --from-env SOYO_NEW_TOKEN
soyo --config /path/to/config.toml credentials grant RETURNED_REFERENCE \
  --run-kind tool --server-id remote --tool fetch --expires-at 2030-01-01T00:00:00Z
soyo --config /path/to/config.toml credentials grant-list
```

管理命令的 stdout 返回 `schema_version = 1` 的单个 `data`／`error` JSON 对象。JSONL 模式增加 `type = "mcp.result"`。成功退出 0，需要交互或重新鉴权退出 4，需要确认退出 3，取消退出 130，配置／引用／scope 错误退出 2，普通操作失败退出 1，达到限制退出 5。人工批准提示与授权 URL 使用 stderr 或控制终端。

## 私有存储与执行边界

凭据后端由 `[credentials] backend` 选择。默认 `file` 使用 state 目录下的 `credentials` 私有文件（`$XDG_STATE_HOME/soyo/credentials` 或 `~/.local/state/soyo/credentials`）：目录 0700、记录与锁文件 0600、独占文件＋原子替换，已有目录或文件的所有者、权限、符号链接及文件身份按私有存储约束校验。`system` 为 opt-in 后端（macOS Keychain，需启用 `security-framework` feature），secret material 由系统钥匙串托管、元数据索引仍走同一文件索引，scope／grant／撤销语义跨后端一致；平台不支持时在配置加载与打开两处都返回 `credential_backend_unavailable`，绝不静默回退到明文。

stdio MCP 可用 `env_credential_refs`（目标环境名→凭据引用）在启动计划后把单个 secret 注入子进程环境；注入必须命中匹配的 scoped grant（`run_kinds` 含 `tool`、server_id／工具名／工作区身份／过期时间全部满足），无适用 grant 时启动返回 `credential_grant_denied`、绝不注入整套宿主凭据。

宿主默认把启动时已存在的配置、data、state、cache 与工作区 `.soyo/local` 加入原生保护集合。为活动凭据引用新建存储后，实际凭据目录也会强制加入保护集合。`host` 兼容模式在启用凭据引用或已有凭据存储目录时拒绝启动。模型使用注册后的 MCP 工具，凭据解析和最终请求注入由宿主处理。

共享凭据集合覆盖刷新前后的已知值，并用于历史保存、工具结果、事件和显示边界。启用 MCP 时，模型正文与可读 Thinking 先收集到本次请求完成，再使用最新凭据快照脱敏展示。Thinking 活跃状态仍可见。凭据集合或脱敏边界失败时相关内容会扣留或返回明确错误。

## 状态与限制

全屏 TUI 侧栏显示宿主生命周期事件中的 Configured、Connecting、Connected、Stopped、Needs auth 和 Failed，以及实际工具数。配置加载阶段显示 Configured，Connected 由连接与目录发现完成后的事件更新。列表最多接受 16 个配置服务，高屏最多显示 5 行，其余显示省略数。矮屏展示首项状态及省略数。

JSONL 运行事件使用 `mcp.status`，其服务器条目仅包含 `server`、`transport`、`state` 与 `tools`。已建立服务在清理确认后显示 Stopped，随后才产生运行终态。新 Run 先将当前配置服务初始化为 Configured，恢复会话后的连接在新输入启动。

每个服务的连接、鉴权、握手和目录发现均受到配置超时及运行共享活跃预算约束。stdio 启动同时消耗工具预算，模型核查消耗共享模型请求预算。人工确认暂停活跃计时，进程与连接清理在活跃预算结束后继续完成。

MCP服务要求额外输入时，当前Run以 `needs_input` 结束并保存实际调用状态。用户可在后续输入中补充信息，新的执行继续经过正常授权。`ClientHandler::create_elicitation` 在无显式交互处理器时默认返回 `Decline` 失败关闭，不替用户自动填写或放行确认；原调用续接与通用 MCP Tasks 长任务仍属后续扩展范围。

## Soyo 作为 MCP 服务器（D-108）

除作为 MCP 客户端外，Soyo 也可经 `soyo mcp-server` 以 stdio 方式对外充当 MCP 服务器，让外部 MCP 客户端驱动一个最小任务面。实现是 `soyo-ipc` 之上的薄适配层：进程内不构造运行时、不直接 spawn run，所有工具调用逐一映射到对外发布的 helper IPC 方法。

```sh
soyo --config /path/to/config.toml mcp-server
```

进程启动后经 `soyo_ipc::HelperClient` 连接（必要时拉起）匹配当前 `--config`／`--data-dir` 的 `soyo helper` 守护进程并完成版本兼容检查，随后在 stdin/stdout 上以 `rmcp` server 侧协议服务，直到客户端断开。

**传输与范围**：本批只做 stdio 单连接。HTTP 传输、鉴权模型与远程 worker＋工作区/产物同步按 D-108 明确留作 F-13 后续增量，当前不提供网络监听。

**工具面**（三个，命名与 JSON schema 属对外契约）：

| 工具 | 映射的 helper 方法 | 说明 |
| --- | --- | --- |
| `run` | `run_prompt`（流式） | 参数 `prompt` 必填，`session_id`／`cwd`／`profile`／`mode`／`permissions`／`accept_config_change` 可选。`session_id` 缺省时新建会话。返回 `{action: "started", session_id, run_id}`：id 取自第一条携带 run 标识的 schema 事件；其后事件流在后台排空，客户端断开不取消 run（断线语义见 D-107）。annotations：`readOnlyHint=false`、`openWorldHint=true`。 |
| `status` | `get_run_state`＋`read_session_snapshot` | 参数 `session_id` 必填、`run_id` 可选。合并实时运行状态与持久化快照中该 run 的终态（`result.status`／`error`），`run_id` 缺省时取最近一条 run 记录；未结束的 run 报 `running`。annotations：`readOnlyHint=true`、`idempotentHint=true`。 |
| `cancel` | `cancel_run` | 参数 `run_id` 可选；缺省时取消该调用方连接上的活动 run。返回 `{cancelled: bool}`，取消为协作语义，终态需再查 `status` 确认。annotations：`readOnlyHint=false`、`destructiveHint=true`、`idempotentHint=true`。 |

工具执行失败（参数非法、run 未找到、helper 错误码）以 `isError=true` 的工具级错误返回，调用方能看到具体 `code: message`；未知工具名走协议层 `invalid_params`。

**权限边界**：`run` 透传的 `permissions` 仅是该次 run 的权限模式取值，与 CLI `--permissions` 同源——Manual Approval 下审批请求会挂起等待（非交互调用以 `needs_confirmation` 收尾），绝不绕过 `HostService` 的权限与 sandbox 路径。`soyo mcp-server` 自身没有独立的鉴权模型：调用方身份就是本机同 UID 用户，等价于直接连 helper socket，与桌面端复用 `HelperClient` 的边界一致。

macOS 后代进程清理仍按能力等级报告为 `best_effort`。Linux 使用私有 PID namespace，系统依赖、有效委派资源控制及原生执行证据按[N2 验收记录](n2-validation.md)维护。可执行插件和自动／手动上下文压缩已完成N3固定源码验收；Browser及其他后续能力按[后续计划](post-mvp-plan.md)推进。
