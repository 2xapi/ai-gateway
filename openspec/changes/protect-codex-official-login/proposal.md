## Why

2xapi 当前会通过解析和改写 `~/.codex/auth.json` 来判断及切换 Codex 登录态，但官方凭据也可能位于系统钥匙串，且 OAuth token 会由 Codex 自行轮换；第三方工具一旦用缺失、失效或过期的凭据覆盖/删除该文件，就会把可用的官方登录变成 401 或未认证状态。当前配置写入还复用通用 `[model_providers.custom]` 并整文件重序列化，可能覆盖用户或其他工具的 provider、注释及并发修改，因此需要把“线路配置”和“Codex 凭据”彻底解耦。

## What Changes

- 建立 Codex 凭据零写入边界：2xapi 默认不创建、覆盖、恢复或删除 `auth.json`，也不以该文件是否存在判断登录状态；改用 Codex CLI 的 `login status` 只读结果。仅在用户明确选择“完整官方初始化”、且官方 `codex logout` 已执行后，允许把精确的残留 `auth.json` 可恢复地隔离移动，不读取或改写凭据内容。
- 网关托管在无官方登录时仍由本地网关注入上游 Key，不再把中转 Key 写进 Codex 凭据；应用启动和状态查询保持只读，不自动重放供应商配置。
- **BREAKING**：桌面版 Codex 退役会把中转 Key 写入 `config.toml` 的 direct 托管；桌面版统一走零 Key 网关，终端直连继续使用现有进程级环境变量注入。
- 使用 2xapi 独占的 model provider id 和无密钥 ownership sidecar，避免与用户的 `custom` provider 冲突，并支持受控字段的三方合并恢复。
- 将 Codex TOML 写入改为保留注释/顺序/未受控字段的最小编辑和原子替换；所有变更前创建可校验快照，失败时事务回滚。
- 新增“恢复 Codex 官方登录”诊断与恢复流程：移除或还原仅由 2xapi 拥有的路由覆盖，清理可确证的旧版 2xapi 残留，保留 MCP、插件、权限、通知及其他 provider 配置；未登录时引导用户执行官方 `codex login`，登录后提供端到端验证。
- 新增“Codex 官方重置/初始化”双模式：配置重置保留登录并将活动配置可恢复地隔离；完整初始化通过官方 `codex logout` 清理 file/keyring 凭据，再由用户执行 `codex login`。不承诺删除 `config.toml` 后一定物理生成新文件，只验收有效 provider 回到官方默认 `openai`。
- 迁移现有 legacy `custom` 托管状态和 `auth.json.official.bak` 机制；历史凭据备份只作为人工取证资料，不再自动回灌。

## Capabilities

### New Capabilities

- `codex-credential-safety`: 定义凭据零写入、CLI 登录状态探测、启动只读和中转 Key 隔离要求。
- `codex-config-overlay-safety`: 定义独占 provider、所有权记录、保格式最小编辑、并发冲突保护及旧版托管迁移要求。
- `codex-official-recovery`: 定义官方配置诊断、受控恢复、登录引导、验证和用户可见结果。

### Modified Capabilities

无；项目此前没有 OpenSpec 主规格。

## Impact

- 后端：`src-tauri/src/desktop.rs`、`config.rs`、`server.rs`，以及 Codex CLI 定位/启动的共用代码。
- 前端：`frontend/api-client.js`、`frontend/app.js` 和相关样式，新增恢复入口、诊断结果和冲突提示。
- 数据：新增不含凭据的 Codex overlay sidecar；保留现有配置备份格式，并停止生成/消费 `auth.json.official.bak`。
- 依赖：建议引入 `toml_edit` 进行保格式 TOML 编辑；不新增凭据存储依赖。
- 兼容：读取并安全迁移现有 `[model_providers.custom]` + 2xapi 网关签名；无法确认所有权的配置只报告、不覆盖。现有桌面版 direct 用户下次操作时迁移到 gateway。
