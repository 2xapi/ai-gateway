## Context

当前项目已有 `Provider` 数据层、每供应商代理、基础 Provider Doctor、网关健康状态、备份/原子写入和会话修复任务。会话修复仍由单个后台线程执行，任务状态是进程内易失数据；供应商诊断只覆盖配置、`/models` 和单次请求；配置快照也主要面向文件恢复，没有“按客户端保存一整套工作状态”的概念。

本设计遵循 `protect-codex-official-login` 已确定的约束：官方 `auth.json` 和系统 keyring 不属于普通 2xapi 配置所有权；Codex 桌面托管只使用独占 gateway overlay；所有配置写入必须支持哈希校验、原子替换和回滚。

## Goals / Non-Goals

**Goals:**

- 用版本化本地档案统一编排供应商、模型、协议、代理、加速和生态开关。
- 将诊断结果标准化为可供 UI 和健康路由消费的结构化信号。
- 让会话修复支持可观察、可取消、可恢复和幂等执行。
- 不改变现有客户端路由语义，不把官方账号加入第三方自动故障转移。

**Non-Goals:**

- 本变更不实现云同步、Deep Link、CDP 页面注入、Prompt/Skill 市场或官方账号代理接管。
- 不迁移或解析现有 OAuth/API 凭据，不重写用户未被 2xapi 明确拥有的配置。
- 不以自动重试掩盖 401/403/区域限制，也不把诊断请求正文持久化。

## Decisions

### 1. 档案采用独立版本化文件，Provider Store 仍是供应商真相源

新增 `{CODEX_HOME}/2xapi-profiles.json`，顶层包含 `schema_version`、`active_profiles` 和 `profiles`。档案只保存供应商 ID、客户端作用域、模型/协议、代理引用、加速模式、生态 ID 集合和显示信息；真实密钥仍由现有 Provider Store/私有存储提供，官方认证只记录 `official_auth: preserved` 等状态，不复制 `auth.json`。

选择独立文件而不是把 profile 嵌入 `providers.json`，因为 Provider CRUD 与“整套环境切换”生命周期不同，且可在旧版本无感回退。写入使用当前设置文件相同的锁、临时文件和权限策略，并保留未知字段以便未来迁移。

### 2. 档案应用走 Preview → CAS → Apply → Verify

新增 profile preview/apply API。预览返回作用域、配置字段 diff、现有文件 SHA-256、备份 ID、官方凭据不变承诺以及会话/MCP/插件保留清单。apply 携带短期 token；token 绑定 profile、路径、基线哈希和过期时间。提交前重新计算哈希，冲突则返回 409；成功后重新读取并验证激活供应商、网关状态和 sidecar，失败时按快照回滚。

档案切换只调用现有 provider activation/overlay primitive，不直接重写 Codex 整个 TOML。这样可复用已有三方合并和 `2xapi_gateway` ownership 逻辑。

### 3. Doctor 结果采用固定阶段和错误分类

扩展现有 `diagnose` 结果为 `checks[]`，每项包含 `stage`、`status`、`latency_ms`、`http_status`、`error_class`、`message` 和有限 `details`。阶段固定为 config、proxy、auth、models、request、stream、tools；旧字段 `configValid/reachable/...` 继续返回以兼容前端。

协议探测遵循“读取优先、写回需确认”：先根据供应商声明和无副作用端点判断 Responses/Chat/Anthropic/Gemini，最小请求只在用户点击 Doctor 时执行；模型自动发现失败不得清空当前列表。响应正文只读取受限字节并通过现有脱敏函数处理。

健康状态放在内存 `HealthRegistry`，按 provider ID 维护连续失败、成功率、延迟、冷却截止时间和最近错误。状态可持久化为不含凭据的摘要，避免应用重启后立即误判。熔断仅针对第三方 Provider；Official 永不进入 failover 候选。

### 4. 会话修复使用可取消 token + 检查点

扩展现有 `RepairJob` 增加 `cancel_requested`、`checkpoint`、`skipped`、`failed`、`started_at`、`updated_at` 和预览摘要。取消状态放在线程安全的 job store 中；处理 rollout 列表、state DB、catalog DB、session index 时，在每个安全边界检查取消标记并落盘检查点。

任务状态保存在 `{CODEX_HOME}/2xapi-session-jobs/<job-id>.json` 的脱敏元数据中，重启后只恢复未完成任务的检查点和备份 ID，不自动继续写入；用户点击“恢复”才会继续。每个会话以 ID + 内容哈希去重，数据库写入使用事务和 CAS；单项错误写入结果列表并继续。

为防止之前的“卡在 49%”假象，前端轮询以 `updated_at` 和状态变化为依据，失败/取消任务不再保留旧百分比；后端在所有阶段都更新心跳时间，超过阈值时返回 `stalled` 提示而不是无限等待。

### 5. API 和 UI 兼容策略

- `/api/profiles`、`/api/profiles/preview`、`/api/profiles/apply` 提供档案 CRUD 和切换。
- `/api/providers/diagnose` 保留旧信封，新增 `checks`、`health` 和 `suggestions` 字段。
- `/api/sessions/repair` 保留启动入口；新增 `/api/sessions/jobs/:id/cancel`、`/resume` 和可选的 `/preview`，旧客户端只读 job 仍可工作。
- 连接页显示模式/健康/当前档案；会话页显示修复任务控制。高风险操作仍要求预览和确认。

## Risks / Trade-offs

- [档案引用已删除的 Provider] → 预览阶段标记缺失并拒绝应用，不删除其他配置。
- [多个进程同时修改 profile 或 Codex 配置] → 文件哈希 CAS、进程内锁和提交后验证；冲突只报告，不循环覆盖。
- [Doctor 最小请求产生少量上游费用] → 只有用户显式点击才执行，并显示请求模型和费用风险提示；模型列表/配置检查保持无请求。
- [健康状态误摘除短暂抖动线路] → 使用连续失败阈值、指数冷却和半开探测；所有状态可手动恢复。
- [任务元数据损坏] → 备份 ID 和实际文件快照仍是恢复真相源；损坏 job 只显示不可恢复并要求重新预览。
- [旧前端不认识新字段] → 保留旧响应字段和原路由；新增接口采用可选字段，迁移不要求一次升级所有客户端。

## Migration Plan

1. 先加入 profile、Doctor checks、job checkpoint 的数据模型和隔离单测，不改变现有写入路径。
2. 接入 profile preview/apply，复用现有 Provider activation/overlay；默认只生成空档案，不自动迁移活动配置。
3. 扩展 Doctor 和健康注册表，旧诊断响应字段保持不变；增加真实第三方和 mock 上游回归测试。
4. 为会话修复加入预览、取消、恢复和持久化检查点；旧任务状态可只读兼容，未完成旧任务需重新预览。
5. 接入前端入口和文案，运行完整 Rust/前端检查及隔离 CODEX_HOME E2E。
6. 若需回滚，先停用当前档案/恢复原 provider，再回滚二进制；profile/job 元数据可保留，不删除历史备份或凭据。

## Open Questions

- 是否在后续 P1 中把档案存储迁移到 SQLite；本 P0 先采用独立版本化 JSON，避免与现有 Provider Store schema 迁移耦合。
