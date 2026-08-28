## Purpose

保证 2xapi 的线路托管、状态检测和恢复功能不会取得 Codex 官方凭据的所有权，并在文件凭据、系统钥匙串及 OAuth token 自动轮换等不同环境中保持登录态安全。

## ADDED Requirements

### Requirement: Codex 凭据归 Codex 独占管理
系统 SHALL 在供应商激活、桌面托管、取消托管、普通恢复、应用启动、状态查询和失败回滚中，不直接创建、覆盖、合并、恢复或删除 `CODEX_HOME/auth.json`，也 SHALL 不读写 Codex 使用的系统凭据存储。仅当用户明确选择“完整官方初始化”且官方 `codex logout` 已完成后，系统 MAY 对精确的残留 `auth.json` 做一次可恢复的隔离移动；该例外不得读取、解析、复制或改写凭据内容。

#### Scenario: 托管与普通恢复前后凭据不变
- **WHEN** 用户依次执行桌面托管、切换供应商和普通恢复
- **THEN** `auth.json` 的存在性、字节内容和权限保持不变，系统凭据存储不发生由 2xapi 发起的变更

#### Scenario: 完整初始化先调用官方 logout
- **WHEN** 用户明确确认完整官方初始化
- **THEN** 系统先调用官方 `codex logout`，由 Codex CLI 清理 file/keyring 凭据；只有 CLI 完成后仍存在的精确 `auth.json` 才可被移动到 `0700` 隔离目录，移动前后均不读取其内容

#### Scenario: 完整初始化取消或 logout 失败
- **WHEN** 用户取消初始化，或 `codex logout` 退出失败/超时
- **THEN** 系统不得移动或删除 `auth.json`，报告失败阶段并保留可恢复备份

#### Scenario: 用户明确发起官方登录
- **WHEN** 用户点击“打开官方登录”并确认启动官方 Codex 登录流程
- **THEN** 系统仅把控制权交给 `codex login`，凭据变更由 Codex CLI 完成且 2xapi 不解析或复制返回的 token

#### Scenario: 历史凭据备份存在
- **WHEN** `auth.json.official.bak` 或其他历史 token 快照存在
- **THEN** 系统只把它们标记为取证资料，不自动回灌、不删除且不据此宣称登录有效

#### Scenario: 凭据存储位于系统钥匙串
- **WHEN** `cli_auth_credentials_store` 为 `keyring` 或 `auto`
- **THEN** 系统不得把删除/移动 `auth.json` 当成退出登录；完整初始化必须以官方 `codex logout` 的结果为准

### Requirement: 登录状态由官方 CLI 只读探测
系统 SHALL 通过 `codex login status` 的进程结果判断本机 Codex 登录状态，SHALL 支持区分 ChatGPT 登录、API key 登录、未登录和未知/探测失败，且 SHALL 不以 `auth.json` 是否存在或是否包含 token 字段作为回退判据。

#### Scenario: 凭据位于系统钥匙串
- **WHEN** `auth.json` 不存在但 `codex login status` 成功并报告 ChatGPT 登录
- **THEN** 系统返回已使用 ChatGPT 登录，不把用户误判为未登录

#### Scenario: auth 文件存在但官方会话无效
- **WHEN** `auth.json` 存在但 `codex login status` 返回未登录
- **THEN** 系统返回未登录并建议重新登录，不把文件存在视为有效会话

#### Scenario: CLI 不可用或探测超时
- **WHEN** 无法定位 Codex CLI、命令超时或输出无法识别
- **THEN** 系统返回 `unknown` 和可操作错误，不读取凭据文件尝试猜测

### Requirement: 桌面托管不得向 Codex 配置泄露中转 Key
系统 SHALL 让桌面版 Codex 统一通过本地网关注入中转凭据，Codex 的 `config.toml`、`auth.json`、命令行参数、API 响应、日志和 ownership 元数据中 SHALL 不出现上游 Key。桌面版 direct 托管 SHALL 被拒绝或迁移为 gateway；终端直连 SHALL 仅使用现有进程级环境变量注入。

#### Scenario: 未登录用户使用网关
- **WHEN** 未登录用户启用桌面版网关托管
- **THEN** 自定义 provider 以无需 Codex 认证的方式连接本地网关，功能可用且系统不创建 `auth.json`

#### Scenario: ChatGPT 登录用户使用网关
- **WHEN** 已使用 ChatGPT 登录的用户启用桌面版网关托管
- **THEN** 官方登录缓存保持原样，中转请求由网关注入供应商 Key，恢复后官方登录仍可被 CLI 识别

#### Scenario: 旧 direct 托管被发现
- **WHEN** 系统检测到由旧版 2xapi 写入 bearer 的桌面 direct 配置
- **THEN** 系统在获得用户确认后迁移到 gateway 并从 Codex 配置移除该 bearer，不把它复制到任何新文件或响应

### Requirement: 只读生命周期不改写 Codex 状态
系统 SHALL 保证应用启动、健康检查、桌面状态刷新和诊断预览不会改写 `config.toml`、`auth.json` 或应用 ownership 状态；供应商配置只能由明确的用户操作触发。

#### Scenario: 应用重启
- **WHEN** 用户仅启动或重启 2xapi 且未确认任何托管/恢复操作
- **THEN** Codex 配置与凭据文件的内容哈希、权限和修改时间保持不变

#### Scenario: 反复刷新状态
- **WHEN** 前端反复调用健康检查和 Codex 状态接口
- **THEN** 系统只返回诊断数据，不自动应用当前供应商或创建备份文件
