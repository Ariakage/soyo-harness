# 远程 worker 与内容寻址同步

F-13 的第三条远程增量（D-113 (c)，实现决策见 D-117）：`soyo remote-worker` 在另一台机器上执行任务，工作区输入与产物以内容寻址清单加 blob 往返，属主 helper 保有任务真相与显式合入权。

## 定位与信任模型

- **出站 only**：worker 作为客户端经既有 `[remote]` HTTP(S)+bearer token 传输连接属主 helper（见[使用指南](usage.md)）。远端机器不开放入站端口，不需要反向可达。
- **同一认证路径**：worker 使用 `run_kinds = ["remote"]` 的 bearer token，不引入第二套身份或密钥分发。token 撤销后 worker 的下一次请求即 401。
- **独立信任域**：worker 机器是另一个信任域。属主 helper 只下发任务输入与工作区文件的相对路径／内容，绝不下发模型密钥、凭据或其他宿主机密；worker 上报的摘要、改动清单与结果一律按外部输入处理——base 摘要对照持久化快照、blob 摘要重新计算、文本在入库前脱敏。
- **执行边界不迁移**：worker 用自己的配置、模型 profile、限额与原生 sandbox（macOS Seatbelt／Linux Bubblewrap+Seccomp）执行 Run；任务的工作模式与权限模式沿用任务记录。`Manual Approval` 任务不允许分配给远程 worker（`task_authorization`），因为远端没有交互式审批人。

## 快速开始

属主侧（helper 主机）：

```toml
[remote]
enabled = true
bind = "0.0.0.0:8443"
[remote.tls]
cert_path = "/etc/soyo/server.crt"
key_path  = "/etc/soyo/server.key"
```

```sh
soyo --config /path/to/config.toml credentials token issue --ttl-days 30
soyo --config /path/to/config.toml task submit '为仓库补齐 CHANGELOG'
soyo --config /path/to/config.toml remote workers
soyo --config /path/to/config.toml remote assign TASK_ID build-01
```

远端机器（自带模型配置与凭据）：

```sh
export SOYO_REMOTE_TOKEN=soyo_rt_...
export SOYO_REMOTE_CA=/etc/soyo/server.crt   # 自签或私有 CA 时
soyo --config /path/to/worker.toml remote-worker \
  --endpoint https://helper.example:8443 \
  --workspace ~/soyo-worker \
  --worker-id build-01
```

`--worker-id` 缺省取主机名（未显式设置时也读 `SOYO_REMOTE_WORKER_ID`）；同一 helper 上应保持唯一且稳定，任务绑定按这个 id 持久化。worker 每次连接都会重新注册，helper 重启后无需重新派发。

任务完成后结果只入库，需在属主侧显式合入：

```sh
soyo --config /path/to/config.toml remote show TASK_ID
soyo --config /path/to/config.toml remote apply TASK_ID
```

## 协议方法

全部为内部方法（不属于 D-106 冻结面），沿用同一 NDJSON 帧、`PROTOCOL_VERSION` 握手与「断线≠取消」语义：

| method | 方向 | 作用 |
| --- | --- | --- |
| `remote_worker_register` | worker → helper | 注册/刷新 worker id、arch、sandbox 能力与版本 |
| `remote_workers` | 客户端 → helper | 列出注册表（last_seen、当前进度） |
| `remote_task_assign` | 客户端 → helper | 把 queued 任务绑定到 worker id（`worker_id: null` 解除） |
| `remote_task_claim` | worker → helper | 取走最早绑定给该 worker 的 queued 任务，建立租约与 dispatch intent，返回工作区清单 |
| `remote_task_start` | worker → helper | 工作区物化完成后确认 Run 启动（副作用边界） |
| `remote_task_heartbeat` | worker → helper | 续租；返回 `cancel` 表示 pause/cancel 已请求 |
| `remote_task_progress` | worker → helper | 上报 `syncing`/`running`/`finishing` 等阶段 |
| `remote_blob_get` | worker → helper | 分块下载工作区 blob（`offset` + 1 MiB 块） |
| `remote_blob_begin` / `remote_blob_put` / `remote_blob_finish` | worker → helper | 分块上传产物 blob，finish 时校验摘要 |
| `remote_task_finish` | worker → helper | 上报 Run 结果与改动清单（只入库，不写工作区） |
| `remote_task_apply` | 客户端 → helper | 显式三方合入 |
| `remote_task_show` | 客户端 → helper | 任务视图＋暂存结果 |

`lease_ttl_ms` 可在 claim 时指定（1000..=600000，默认 60000）；worker 每 10 秒心跳续租。心跳失败或 `cancel` 会取消远端 Run。

## 任务与租约状态机

远程派发复用 `task_*` 三表，不复制任务状态：

1. `remote_task_assign`：仅 `queued` 任务可绑定；`Manual Approval` 拒绝。
2. `remote_task_claim`：取得既有租约（`owner = remote:<worker_id>`），记录 attempt 与 dispatch intent，任务进入 `running`，但 dispatch 仍为 `intent`。
3. `remote_task_start`：worker 物化工作区后把 dispatch 标为 `running`。这一步之前没有任何工具或模型副作用。
4. 心跳续租直到 Run 结束；`remote_task_finish` 走与本地 attempt 相同的状态机（`completed`/`failed`/`waiting_unknown` 等）。

过期恢复（显式 `soyo task recover`，以及每次 `remote_task_claim` 前对该 worker 名下任务的自动恢复）：

- **intent + 租约过期**：worker 从未确认启动，无副作用 → dispatch 记 `expired`，任务回 `queued`，可被再次 claim。
- **running + 租约过期**：Run 可能已产生副作用 → dispatch 记 `unknown`，任务进入 `waiting_unknown`；只有显式 `soyo task resolve TASK_ID --retry`（或 `--block`）才继续，绝不自动重放。

远端 worker 自身中止（物化失败、网络中断）不会伪造结果：租约自然过期后按上述规则处理。

## 工作区同步与边界

- **清单**：claim 时扫描任务会话工作区（`TaskRecord.workspace`），生成 `(path, sha256, size, executable)` 清单。遵循 `.gitignore`（仅 git 仓库内生效），永久排除 `.git`、符号链接与特殊文件；忽略工作区外的数据／状态／缓存目录。
- **上限**：10000 文件、64 MiB 总量、单文件 16 MiB；改动清单最多 256 个文件。清单序列化后还必须落在 4 MiB 派发帧预算内。超限 fail closed（`remote_sync_limit`），不会静默裁剪。
- **blob 存储**：`<data_dir>/remote/blobs/<sha 前两位>/<sha>`（0600），写入与整读都校验 SHA-256；分块上传按 offset 追加，finish 时才校验摘要并原子改名。`<data_dir>/remote/manifests/<task_id>.json` 保存派发快照，是 apply 的权威 base。
- **传输分块**：帧上限为 8 MiB，blob 以 1 MiB 原始字节分块（base64 后仍远低于上限）。
- **worker 侧目录**：`--workspace` 下每个 attempt 使用独立目录 `<task_id>-<attempt>`，不复用上一次尝试的现场；执行结束后保留供排查。

## 显式 apply 与三方检查

`remote_task_apply` 只在任务 `completed` 且存在暂存结果时工作：

- **base 取自已持久化的派发快照**，不信任 worker 自报的 `base_sha256`；不一致直接 `conflict`。
- 每个路径重新校验：工作区相对路径、无 `..`／绝对路径、路径链无符号链接、不命中受保护存储。
- 三方判定：本地摘要 == 新摘要 → `unchanged`；本地摘要 != base 且 != 新摘要 → 冲突；否则写入或删除。
- **全有或全无**：任一冲突则不写任何文件，返回冲突清单；成功时返回逐文件 receipt（`created`/`modified`/`deleted`/`unchanged`）。重复 apply 幂等。
- 可用 `--path` 只合入选定的文件；未出现在结果中的路径会报 `invalid_args`。
- 合入只保留可执行位：写入文件权限归一为 0755（可执行）或 0644，其他权限位不随远端传播。

## 已知限制

- blob 与 manifest 目录没有垃圾回收；清理需手动删除 `<data_dir>/remote/`（不影响任务记录，但会令已暂存结果的 apply 失败）。
- 一个 worker 进程串行执行任务；同一 worker id 不应并发运行多个进程。
- 远端 Run 的时长由 worker 运行时自身的限额与心跳存活共同约束，属主侧不额外设定远端执行超时。
- 工作区同步只覆盖普通文件；符号链接、`.git` 内部对象与 gitignore 排除的构建产物不会传输。worker 端不保留 git 元数据，因此远端 Run 内不能使用依赖 `.git` 的工具。
- 属主侧 `remote_workers` 注册表不持久化：helper 重启后列表为空，worker 重连即恢复。

## 验证

- `soyo-core`：清单扫描（gitignore／符号链接／.git 排除与上限）、blob 往返与分块上传、工作区 diff、三方 apply（写入、幂等、divergence 拒绝、base 不一致拒绝、路径穿越拒绝）、TaskStore 的绑定／远程结果入库／intent 回队／已启动不重放。
- `soyo-host`：自签证书 HTTPS 回环端到端——注册、绑任务、claim、下载输入 blob、start/heartbeat/progress、上传结果 blob、finish、show、apply、divergence 拒绝、未确认 intent 回 `queued`、已确认启动进入 `waiting_unknown` 且不自动重放、显式 resolve 后可再次 claim。
- `soyo-cli`：`cargo test -p soyo-cli --test remote_worker` 用真实 helper（HTTPS＋自签证书）、真实 `soyo remote-worker` 进程与固定模型替身（一次 `write_file` 工具调用）跑通提交→绑定→claim→物化→执行→上报→显式 apply 的完整链路；不依赖真实模型端点或网络。
