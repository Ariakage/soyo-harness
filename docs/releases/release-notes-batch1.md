# Soyo Harness Batch 1 扩展与基础设施说明（F-04 / F-02 / F-03 / F-15 / F-21 同步）

**开发线增量说明：当前保持 `version = 0.3.0`，未创建或推送版本标签。本批增量在本机（macOS 15 ARM / Rust 1.99.1）完成实现，并通过 `RUST_TEST_THREADS=1 just check`（94 组测试共 1,902 项通过、0 失败、27 项条件忽略＋release 构建 `target/release/soyo` SHA-256 `4d5d9b7d77c79c9222ca4d8374d627e4012087166d47f7f799ad687514908702`）、`just test-tooling`（13 组 177 项通过）、`just smoke target/release/soyo`（smoke／journey／5 项审查回归／28 组 TUI PTY）及 `just desktop-check`（`soyo-desktop` Clippy／`export_specta_bindings`、Vitest 24 项、`tsc --noEmit` 与 Vite 构建）。本机未安装 `cargo-audit`；Linux Bubblewrap 与远端四平台 CI 矩阵未在本轮单独触发；`windows` 原生沙箱／DACL 与非 `linux-arm64` 定制遮蔽 Chromium 分发继续作为显式未覆盖边界保留。**

## 中文

### F-04 扩展生命周期 Hooks 与默认关闭宿主 `[hooks]`（D-088）

- **全生命周期事件扩展**：`HookEvent` 与 `soyo-plugin-v1.schema.json` 新增 `model.before_request`、`model.finished`、`input.required` 与 `compaction.finished`（与原有 4 项组成 8 事件契约）。`check` 类型支持用于 `tool.before_dispatch` 与 `model.before_request` 阻断后续请求或派发，其余事件执行 `observer`。
- **宿主配置规则与零漂移**：新增 `[hooks]`（默认 `enabled = false`，关闭时配置与 Goal 指纹、提示词及运行时行为完全不变）与 `[[hooks.rules]]`（支持 `id`、`event`、`kind`、`argv`、`tools`、`modes`、`timeout_ms` 与经 `SecretRegistry` 校验的 `env` 映射）；所有宿主规则共用 `PreparedEnvironment` 原生沙箱、`HookJournal` 持久意图与 `Policy` 权限检查，绝不绕过 `PLAN` 只读边界。

### F-02 MCP Resources、Templates、Prompts、订阅与 Elicitation（D-089）

- **能力发现与按需只读工具**：扩展 `rmcp = 3.9.0` 客户端支持分页 `resources/list`、`resources/templates/list`、`resources/read`（文本有界、`Blob` 仅元数据）与 `prompts/list`、`prompts/get`。仅当已连接服务器通告 `resources` 或 `prompts` 时动态注册 `mcp_list_resources`、`mcp_read_resource`、`mcp_subscribe_resource`、`mcp_list_prompts`、`mcp_get_prompt`，未启用或无对应能力时模型前缀工具列表零变化。
- **双版本通知订阅与失败关闭 Elicitation**：按协议版本协商分别支持 `2026-07-28` `subscriptions/listen` 与 `2025-11-25` `resources/subscribe`，收到列表变更或资源更新通知时自动使目录缓存失效；`ClientHandler::create_elicitation` 在无交互处理器时默认返回 `Decline` 失败关闭。

### F-03 `soyo-plugin` 归档、验签与受控 HTTPS 拉取（D-090）

- **内存受限归档与校验**：`soyo plugin validate|install|update` 支持 `.tar`、`.tar.gz`、`.tgz` 本地归档及 `--url <HTTPS_URL>`，全部解包检查在内存中完成（不提前落盘），复用 `FileBudget` 上限，拒绝符号链接、硬链接、设备节点与路径穿越，并生成与本地目录逐字节一致的 `soyo-tree-sha256-v1` 树摘要（在 macOS 打包测试归档时可使用 `COPYFILE_DISABLE=1 tar` 避免附带 `._*` AppleDouble 元数据文件）。
- **强制校验／签名与 SSRF 防护**：支持 `--sha256` 校验与 `--pubkey`＋`--signature` Ed25519 验签（`ring 0.17.14`）；`--url` 强制要求至少提供校验或签名之一，复用 `crate::network::is_public_address` 固定公网 IP 解析地址、禁用系统代理并手工跟踪最多 3 次 HTTPS 重定向。

### F-15 离线宿主自检 `soyo doctor` 与文档同步（D-091 / F-21）

- **离线脱敏诊断**：新增 `soyo doctor [--strict]`（支持全局 `--output text|json|jsonl`），非破坏性检查 OS／架构、核心 `execution::probe_host_environment` 沙箱可用性（Seatbelt／Bubblewrap／socat）、配置与模型 profile、`.soyo`/`.saya` 及 XDG 私有目录权限／符号链接风险，以及 `git`、`js_repl`、启用状态 LSP（含 `rust-analyzer`）就绪状态，输出全程通过 `SecretRegistry` 脱敏。
- **文档事实同步（`F-21` 等）**：[后续能力路线图](../capability-roadmap.md) 与[后续开发计划](../post-mvp-plan.md) 中的 `F-21` 与 N6 章节同步更新为已实现状态（含 `D-083`/`D-084` QuickJS 与受控 Node 可选后端），并如实标注阻塞项。

### 兼容性与已知阻塞边界

- 未启用 `[hooks]`、未连接通告 `resources`/`prompts` 的 MCP 服务时，原有工具目录、提示词前缀、配置与 Goal 指纹保持完全向后兼容。
- **已知平台阻塞**：`windows` 仍为显式 `unsupported`（缺少 AppContainer／Job 原生沙箱与 POSIX `0700` 等效私有目录 DACL/ACL）；定制预遮蔽 `headless_shell` 产物仍仅验证 `linux-arm64`，未声称覆盖其他目标的定制遮蔽二进制。

---

## English

### F-04 Extended Lifecycle Hooks & Opt-In Host `[hooks]` (D-088)

- **Expanded Hook Events**: `HookEvent` and `soyo-plugin-v1.schema.json` now cover 8 lifecycle points (`run.started`, `run.finished`, `tool.before_dispatch`, `tool.finished`, `model.before_request`, `model.finished`, `input.required`, and `compaction.finished`). Blocking `check` hooks are permitted on `tool.before_dispatch` and `model.before_request`.
- **Zero-Drift Host Configuration**: Adds opt-in `[hooks]` (`enabled = false` by default, leaving existing runs, prompts, and Goal config fingerprints untouched) and `[[hooks.rules]]` (`id`, `event`, `kind`, `argv`, `tools`, `modes`, `timeout_ms`, and validated `env` mappings). All host hooks run inside `PreparedEnvironment`, record durable intent in `HookJournal`, and never escalate past `Policy` or `PLAN` read-only guarantees.

### F-02 MCP Resources, Templates, Prompts, Subscriptions & Safe Elicitation (D-089)

- **Capability Discovery & Conditional Read-Only Tools**: Extends the `rmcp = 3.9.0` client with paginated resource/template listing, bounded resource reading (metadata-only for blobs), and prompt listing/fetching. Dynamically exposes `mcp_list_resources`, `mcp_read_resource`, `mcp_subscribe_resource`, `mcp_list_prompts`, and `mcp_get_prompt` only when connected servers advertise those capabilities.
- **Version-Negotiated Subscriptions & Fail-Closed Elicitation**: Dispatches `subscriptions/listen` (`2026-07-28`) or `resources/subscribe` (`2025-11-25`) based on negotiated protocol, invalidating local caches on server notifications. `ClientHandler::create_elicitation` fails closed (`Decline`) when no interactive handler is bound.

### F-03 `soyo-plugin` Archives, Signatures & Controlled HTTPS Fetch (D-090)

- **Bounded In-Memory Archive Inspection**: Extends `soyo plugin validate|install|update` to accept `.tar`, `.tar.gz`, and `.tgz` archives without extracting to disk, enforcing `FileBudget`, rejecting symlinks/hardlinks/special files/path traversal, and preserving identical `soyo-tree-sha256-v1` digests.
- **Checksum/Signature Verification & SSRF Guard**: Supports `--sha256` and Ed25519 `--pubkey` + `--signature` (`ring 0.17.14`). `--url <HTTPS_URL>` requires verification flags, disables system proxies, resolves and pins only public IPs via `crate::network::is_public_address`, and manually follows up to 3 HTTPS redirects.

### F-15 `soyo doctor` Offline Diagnostics & Documentation Sync (D-091 / F-21)

- **Non-Mutating Host Diagnostics**: Adds `soyo doctor [--strict]` supporting `--output text|json|jsonl` to check OS/arch, `execution::probe_host_environment` sandbox readiness, config/profiles, `.soyo`/`.saya` & XDG private directory permissions/symlinks, optional `git`/`js_repl`/LSP binaries, and explicit platform support matrix output with `SecretRegistry` redaction.
- **Roadmap Sync**: Updates `F-21` / N6 status in the roadmap and post-MVP plan to match the implemented QuickJS (`D-083`) and opt-in Node (`D-084`) engines.

### Compatibility & Explicit Blockers

- Existing tool schemas, prompt prefixes, session schemas, and Goal fingerprints remain unchanged when `[hooks]` is disabled and no `resources`/`prompts`-capable MCP servers are connected.
- **Explicit Blockers**: `windows` stays explicitly `unsupported` (native AppContainer/Job sandbox and POSIX `0700`-equivalent DACL parity unimplemented); custom pre-masked `headless_shell` distribution remains validated for `linux-arm64` only.
