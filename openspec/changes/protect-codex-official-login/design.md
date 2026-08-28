## Context

参见 [proposal.md](./proposal.md) 的 Why。本设计同时受三个事实约束：

- OpenAI 官方文档说明 Codex 登录缓存可能位于 `CODEX_HOME/auth.json`，也可能位于操作系统凭据存储；ChatGPT 登录 token 由 Codex 在使用期间自动刷新。因此文件存在性和 token 字段扫描都不是可靠状态源，旧快照也可能已落后于当前 refresh-token generation。[Authentication](https://learn.chatgpt.com/docs/auth)
- Codex 内置 provider 默认值为 `openai`；自定义 provider 在 `requires_openai_auth=false` 且没有 `env_key` 时可以按“无需 Codex 认证”工作，适合由本地网关注入上游凭据。provider/auth 等键只能来自用户级配置。[Configuration Reference](https://learn.chatgpt.com/docs/config-file/config-reference)
- 当前实现的 `desktop.rs`/`config.rs` 会扫描、写入并从 `.bak` 恢复 `auth.json`；网关托管复用 `[model_providers.custom]`；TOML 经过 JSON Value 往返后整文件重写，注释和排版会丢失。已有 `config-backups`、原子写和活动供应商回滚原语可复用，但凭据备份/回灌必须退出主流程。

cc-switch 故障属于“独占式 provider 快照重放”问题：配置和 auth 被当成同一个供应商状态，在切换或启动重放当前供应商时，缺失 auth 被解释成删除 live `auth.json`；失效 key/token 随后产生 401，Codex 自身的失败/登出清理又放大症状。2xapi 不应尝试与这种写入器竞争，而应缩小自己的所有权并检测竞争。

## Goals / Non-Goals

**Goals:**

- 把 Codex credential store 设为 2xapi 的硬只读边界，同时支持 file/keyring 两种官方存储方式。
- 让桌面网关在已登录和未登录状态下都不依赖、不改写官方凭据。
- 让托管写入具有独占命名空间、保格式编辑、所有权记录、三方恢复和 TOCTOU 防护。
- 为未知外部残留提供一个显式、可预览、可回滚的官方默认路由恢复通道。
- 兼容清理旧 2xapi `custom`/direct 状态，但不自动采信或恢复旧 token 快照。

**Non-Goals:**

- 不修复 cc-switch 数据库、不替用户管理 cc-switch 供应商，也不自动杀死第三方进程。
- 不验证、续期、解密、复制或迁移 OAuth/access/refresh token。
- 不保证外部工具以后不会再次覆盖配置；本功能负责检测并给出明确阻塞。
- 不重置 Codex 的 MCP、插件、权限、通知、sandbox、telemetry 或其他非路由偏好。
- 不让恢复按钮自动产生模型额度消耗；真实请求验证保持显式操作。

## Decisions

### 1. 登录状态使用 Codex CLI 探针，凭据文件完全退出业务逻辑

新增只读 `CodexLoginProbe`，CLI 解析顺序复用启动器能力：显式 `CODEX_CLI_PATH`、PATH、平台安装默认位置（macOS 包含 `/Applications/ChatGPT.app/Contents/Resources/codex`）。运行 `codex login status` 时设置短超时，使用退出码作为认证有效性的主信号；成功输出仅用于分类 `chatgpt`、`api_key` 或 `authenticated_unknown`，失败分类 `signed_out`、`cli_missing`、`timeout`、`probe_error`。

状态结构建议：

```json
{
  "state": "signed_in|signed_out|unknown",
  "method": "chatgpt|api_key|unknown",
  "source": "codex_cli",
  "message": "脱敏的人话状态"
}
```

状态接口和 `/health` 不再返回 `auth.json.exists()` 推导出的 `officialAuthPresent`；短时间缓存只存在内存中，缓存失效不会触发文件写入。

选择 CLI 探针而不是解析 `auth.json`，因为它覆盖 keyring 并让 Codex 自己判断凭据是否可用。没有选择调用 `codex exec` 做状态探测，因为它会产生请求、会话和潜在 token 刷新，不属于无副作用健康检查。

### 2. 桌面版仅保留零 Key gateway overlay

桌面托管统一写入独占 provider：

```toml
model_provider = "2xapi_gateway"

[model_providers.2xapi_gateway]
name = "2xapi Gateway"
base_url = "http://127.0.0.1:8787"
wire_api = "responses"
requires_openai_auth = false
```

这里不配置 `env_key`、`experimental_bearer_token` 或 command auth；Codex 到本地网关这一跳无需凭据，上游 Key 继续由 app 私有供应商存储和网关内存路由注入。官方 ChatGPT/API-key 登录缓存可以继续存在，但不会被发送给网关，也不会被改写。

桌面 direct 因必须给无环境注入能力的 GUI 提供 bearer，与“Codex 配置零 Key”目标冲突，故退役：新请求返回 `E_DESKTOP_DIRECT_RETIRED` 并给出 gateway 迁移结果；旧 direct 只在用户确认迁移时清除 bearer。终端直连保留既有 `-c` + 进程环境变量方案，因为它不落盘且生命周期隔离。

没有选择把中转 Key 写进 OS keyring 再通过 `auth.command` 取出：这会引入三平台凭据桥接、生命周期和权限问题，而桌面 app 已常驻以提供网关，收益不足以抵消复杂度。

### 3. 用 `toml_edit` 代替 JSON/TOML 整文件重序列化

为 Codex overlay 单独实现保格式编辑器，直接依赖与现有 `toml 0.8` 兼容的 `toml_edit`。编辑器只暴露受控键操作，并把“读当前字节 → 解析 DocumentMut → 计算 diff → 建快照 → 同目录 0600 临时文件 → 原子 rename → 重新读取校验”封装成一次提交。

解析失败必须返回 `E_CODEX_CONFIG_PARSE`；不再沿用 `read_toml()` 把错误折叠为空对象的行为。每次提交都携带预览时的 SHA-256；提交前重新读文件，hash 不同返回 `E_CODEX_CONFIG_CHANGED`，要求重新预览。这样满足“每次 Edit 前重新 Read”，也避免和 cc-switch 相互覆盖。

通用 `config.rs` 可继续服务其他旧 API，但所有 Codex host/unhost/official recovery 路径必须切到新编辑器；完成迁移后删除其中的 auth 读写原语和 PureApi auth 分支。

### 4. Ownership sidecar 记录基线和最后应用值，不存凭据

新增私有 sidecar，建议放在现有 `config-backups` 下：`2xapi-codex-overlay-state.json`。其生命周期与配置事务绑定，结构包含：

```json
{
  "version": 1,
  "configPath": "规范化路径",
  "baselineConfigHash": "sha256",
  "baseline": { "controlled.path": { "present": true, "value": "..." } },
  "applied": { "controlled.path": { "present": true, "value": "..." } },
  "appliedConfigHash": "sha256",
  "providerId": "2xapi_gateway",
  "catalog": { "path": ".../2xapi-model-catalog.json", "sha256": "..." }
}
```

受控路径仅包括顶层 `model_provider`、由本次托管明确设置的 `model`/`model_catalog_json` 和 `model_providers.2xapi_gateway`。首次 host 捕获 baseline；重复 host 或换供应商只更新 applied，不改 baseline。unhost 对每个路径执行三方规则：

- `current == applied`：恢复 baseline；
- `current == baseline`：视为已由外部还原，无需写；
- 其他：报告 conflict 并保持 current。

sidecar、catalog、providers active 状态和 config 共同参与内存快照回滚。sidecar 不含上游 Key、token 或 bearer。

没有只依赖“最新 pre-host 备份”，因为时间排序无法证明所有权，且会覆盖托管期间的合法用户修改。完整备份仍保留为灾难恢复证据，但正常 unhost 使用三方合并。

### 5. 旧版状态采用强签名迁移，未知配置保持原样

legacy ownership 只在以下组合足以确证 2xapi 来源时成立：

- `custom.base_url` 是本产品固定 loopback gateway；且
- `model_catalog_json` 指向 2xapi catalog，或 `custom` 含旧版 `experimental_bearer_token` 并与本产品活动供应商一致。

确认后，迁移预览会列出：保存原 `custom` baseline、创建 `2xapi_gateway`、切换顶层 provider、移除旧 bearer、保留原本不是 2xapi 生成的 `custom` 内容。没有强签名时，`custom` 仅标记为 external/unknown；普通 unhost 不动它。

旧 `auth.json.official.bak` 不删除，避免擅自销毁用户取证资料；UI 提示其已停用且不应自动恢复。新版本永不再创建它。

### 6. 恢复 API 使用“预览 token + 两种模式”

后端增加以下概念接口（最终路由名可按现有风格调整）：

- `GET /api/desktop/recovery/preview`：只读诊断，返回 config hash、登录状态、外部管理器状态、普通 unhost diff、official-default diff 和短期 `previewToken`。
- `POST /api/desktop/recovery/apply`：请求包含 `mode=unhost|official-default|reset-config|reset-all`、`previewToken` 和显式确认。token 与当前 config hash/路径/过期时间绑定；reset 模式额外绑定隔离目录和待移动文件清单。
- `POST /api/desktop/login/start`：仅在用户点击后启动官方 `codex login`；前端轮询只读状态，不接收 token。
- `POST /api/desktop/official-smoke`：再次确认后运行最小官方 `codex exec` 并等待退出。

`unhost` 只按 sidecar 三方恢复。`official-default` 是紧急逃生舱：建立快照后清除会改变官方默认路由的顶层键（`model_provider`、`model`、`model_catalog_json`、`openai_base_url`、`chatgpt_base_url` 和活动 `profile` 选择），删除可确认属于 2xapi 的 provider/catalog，但保留所有 provider 定义和 profile 文件本身。删除顶层 `model_provider` 而不是强写 `openai`，因为官方默认即 `openai`，这样结果更接近默认配置；提交后解析值应等价于 `openai`。

紧急模式能处理没有 sidecar 的 cc-switch 残留，但因为它会重置用户选择的路由键，必须展示逐字段 diff 并二次确认。它仍不触碰任何 credential store。

### 7. “初始化 Codex”采用可恢复隔离 + 官方 logout，而不是直接 rm

初始化不实现一个未经官方定义的 `codex reset` 命令，也不承诺删除后一定重新生成 `config.toml`。后端在 recovery 预览中增加两个显式模式：

- `reset-config`：保留官方登录，仅把预览列出的活动 `config.toml`、受控 profile、2xapi provider/catalog 和 sidecar 移到 `CODEX_HOME/2xapi-reset/<timestamp>/`；恢复后不再有活动 2xapi 路由，解析结果等价于内置 `openai`。
- `reset-all`：先执行官方 `codex logout`，由 Codex 清理 file/keyring 凭据；只有命令成功后仍存在的精确 `auth.json` 才允许在二次确认下移动到同一隔离目录。应用永远不解析或改写 token，也不直接触碰系统钥匙串。随后由用户明确启动 `codex login`。

两种模式都必须满足以下事务顺序：关闭/检查外部写入器 → 只读预览 → 用户二次确认 → 记录路径、哈希和权限 → 同卷原子移动到 `0700` 隔离目录 → 重新读取活动路径校验 → 重启 Codex → `codex login status` 和配置解析验证。移动失败、CAS 冲突、外部回写或 logout 失败都停止流程并保留现场，不自动删除备份、不回滚到中转配置。sessions、rollouts、SQLite 历史、MCP、插件、权限和通知默认不在 reset 范围内；项目级 `.codex/config.toml` 也不被隐式修改。

建议 API 复用 recovery 资源并扩展模式枚举：`GET /api/desktop/recovery/preview` 返回 reset 两套 diff、外部管理器状态、是否存在 keyring 风险和备份目标；`POST /api/desktop/recovery/apply` 接受 `mode=reset-config|reset-all`、短期 `previewToken` 和显式确认。`reset-all` 返回 `logout_started|logout_failed|auth_quarantined|login_required` 等阶段，前端不得把“命令已启动”显示为“已登录”。

### 8. 外部管理器检测采用“提示性进程检查 + 强制 hash 校验”

平台层对已知 cc-switch 进程做只读检查；发现时 recovery apply 返回 `E_EXTERNAL_CONFIG_MANAGER_ACTIVE`，提示用户完整退出后重试，不自动 `pkill`。进程名可能变化，因此进程检查只提供早期提示，真正一致性由提交前 hash 和提交后重新读取校验保证。

若提交后路由立即变化，返回 `E_EXTERNAL_CONFIG_REWRITE`、备份路径和当前摘要，不循环争抢文件。

### 9. UI 按影响范围分布入口，并将“路由恢复”和“重新登录”做成可观察状态机

Codex 相关操作按影响范围分布在三个入口，避免把配置恢复、会话修复和登录清理混在一个按钮组中：

- **Codex 连接（主通道）**始终显示路由和登录状态卡。这里放置 `开启 2xapi 托管`、`停用 2xapi`、`恢复 Codex 官方配置（保留登录）`、`打开官方登录` 和用户确认后的 `测试官方连接`。前两个按钮处理 gateway overlay/unhost，恢复官方配置使用 official-default 预览/apply；打开官方登录只启动官方 CLI，不接收凭据。
- **会话管理**保留 `历史会话修复`，只读/写会话载体（state、catalog、session index、rollout），不得修改 `config.toml`、provider 或凭据。页面另设折叠的 **Codex 环境恢复（高级）** 卡片，里面放 `初始化 Codex（退出登录并重新登录）`，仅此入口可启动 reset-all。
- **高级设置**只展示 CLI 路径、运行日志、诊断、备份位置和安全说明，不放置重复的恢复或初始化按钮。这样用户在连接页能找到连接问题的修复，在会话页能找到环境级重置，高级设置不会成为高风险操作的默认入口。

按钮文案必须区分三种语义：`停用 2xapi` 是普通 unhost（按 ownership 三方规则恢复，保留登录）；`恢复 Codex 官方配置（保留登录）` 是清除活动路由覆盖、使有效 provider 等价官方 `openai`（保留登录）；`初始化 Codex（退出登录并重新登录）` 是 reset-all（官方 logout 后再引导 login）。初始化不能作为历史会话修复的快捷别名。

应用退出、窗口关闭和网关停止不自动恢复 Codex 配置，也不调用 `codex logout`。窗口关闭默认只是隐藏，托盘退出只结束本地进程；如果仍处于 2xapi 托管，最多显示非写入性提醒，由用户下一次在 Codex 连接页显式点击恢复。禁止在退出钩子中静默改写 `config.toml`、隔离文件或凭据：崩溃/强制结束可能跳过钩子，外部配置管理器也可能与退出事务竞争。

三个入口共用同一状态机，阶段为：

1. 诊断：官方路由、登录方式、网关/外部管理器、可恢复性；
2. 预览并恢复路由：普通 unhost 或紧急 official-default；
3. 登录：只有非 ChatGPT 状态才显示“打开官方登录”；
4. 自动验证：配置解析 + provider 默认 + `login status`；
5. 可选 E2E：用户确认后执行最小 `codex exec`。

错误按阶段展示，任何一步失败都不得把“按钮已点击”描述成“恢复成功”。日志和 API 仅返回错误码、阶段、脱敏 stderr 摘要与备份位置。

## Risks / Trade-offs

- [旧版 2xapi bearer 可能已存在于配置/备份] → 迁移时从 live config 清除；备份不自动删除，UI 提醒按用户授权另行清理，测试/日志禁止输出其值。
- [CLI `login status` 文案未来变化] → 退出码决定 authenticated，method 解析失败降级为 `unknown` 而不是 signed-out；用 fake CLI 覆盖输出变体。
- [外部管理器在进程检查后启动] → preview hash、提交前 CAS 和提交后校验三层防护；不循环覆盖。
- [退役桌面 direct 改变少量用户路径] → 自动迁移至功能等价的 gateway，终端 direct 保留；发布说明标记行为变更。
- [保格式编辑增加依赖] → 只在 Codex overlay 模块使用 `toml_edit`，锁定与当前 TOML 栈兼容版本并加入注释/数组/嵌套表回归样例。
- [官方默认恢复清除用户主动选择的 model/profile] → 仅紧急模式执行，必须逐字段预览、二次确认和快照；普通 unhost 不做此事。
- [启动 `codex login` 涉及浏览器与人工操作] → API 只报告 started，最终成功以再次执行 `login status` 为准；取消登录不回滚已修复路由。
- [用户把“删除文件”误解为“官方一定重新生成文件”] → 文案以有效 provider 和 CLI 状态为验收，不承诺物理生成 `config.toml`；如需可见配置文件，另设独立的用户确认操作。
- [凭据位于 keyring 而不在 `auth.json`] → reset-all 强制走官方 `codex logout`，文件隔离只处理 logout 后仍存在的精确残留，不提供 2xapi 的钥匙串清理逻辑。

## Migration Plan

1. 先加入 login probe、保格式 editor、sidecar 数据模型和全套隔离单测，不接 UI。
2. 将所有 Codex host/unhost/activate-official 路径切到新 overlay；删除 `ensure_auth_key`、`write_auth_json`、`.official.bak` 自动创建/恢复和 `auth_exists` 登录判据。
3. 把 gateway provider 从 `custom` 迁移为 `2xapi_gateway`，拒绝新的桌面 direct；加入 legacy 识别和显式迁移。
4. 增加 recovery preview/apply、login start、smoke API，再接入前端状态机和确认文案。
5. 增加 `reset-config`/`reset-all` 两种官方初始化模式：前者隔离活动配置并保留登录，后者先调用官方 `codex logout` 再处理残留文件，最后交给用户 `codex login`。
6. 在隔离 `CODEX_HOME` 上验证 file/keyring 探针替身、legacy 配置、并发改写、失败回滚、reset 阶段机和字节级凭据不变量；随后对真实 `~/.codex` 仅做 hash 前后只读冒烟。
7. 发布前在用户明确确认下执行一次 ChatGPT 登录 + official `codex exec` 真机 E2E，验证后重启 ChatGPT.app 检查桌面端加载结果。

回滚时先用新版本执行普通 unhost 或 official-default，使 live 配置不依赖 `2xapi_gateway`，再回滚二进制；sidecar 和新 provider 表可保留为惰性数据。严禁通过旧 `.official.bak` 回滚凭据。若回滚到仍会写 auth 的旧版本，必须同时禁用其桌面 host/activate 入口。
