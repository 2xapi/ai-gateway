## Purpose

为用户提供一个可预览、可回滚的 Codex 官方登录恢复入口，在配置被中转工具或旧版 2xapi 改写后恢复官方路由，并由 Codex 自身完成登录和有效性验证。

## ADDED Requirements

### Requirement: 恢复前提供无副作用诊断与预览
系统 SHALL 在恢复前展示 CLI 登录状态、当前 model provider、2xapi 托管状态、可确认的旧版残留、外部配置冲突和拟变更字段；诊断与预览 SHALL 不修改任何文件。

#### Scenario: 当前已是官方可用状态
- **WHEN** provider 为官方默认且 `codex login status` 报告 ChatGPT 登录
- **THEN** 系统显示“官方登录正常”并将恢复操作标记为无需变更

#### Scenario: 配置指向未监听的本地端口
- **WHEN** 当前 provider 指向本地地址且对应服务不可达
- **THEN** 预览明确标出断路路由和拟恢复的官方路由，不把该症状误报为凭据失效

#### Scenario: 登录与路由同时异常
- **WHEN** CLI 报告未登录且配置指向第三方 provider
- **THEN** 预览把“路由恢复”和“官方重新登录”列为两个独立阶段，不尝试用一项覆盖另一项

### Requirement: 提供安全取消托管与紧急官方默认恢复
系统 SHALL 提供两种明确语义：普通“取消 2xapi 托管”仅三方合并还原 2xapi 拥有的字段；用户单独确认的“恢复官方默认路由” SHALL 备份当前配置后移除顶层 provider/model/catalog/官方 base URL 等路由覆盖并回到内置 `openai` 默认，同时保留其他 provider 定义、MCP、插件、权限、通知和非路由设置。两种操作均 SHALL 不触碰 Codex 凭据。

#### Scenario: 普通取消托管
- **WHEN** ownership sidecar 有效且用户选择取消 2xapi 托管
- **THEN** 系统按三方合并恢复 2xapi 字段、清除活动供应商标记并保留所有外部修改

#### Scenario: cc-switch 配置残留且无 2xapi ownership
- **WHEN** 用户在预览后明确确认恢复官方默认路由
- **THEN** 系统仅重置已列出的顶层路由选择，保留 cc-switch 的非活动 provider 数据以便人工恢复，并不读取或删除其数据库和凭据备份

#### Scenario: 恢复出现冲突
- **WHEN** 配置在确认后、提交前再次变化
- **THEN** 系统取消提交并要求用户重新预览，不报告恢复成功

### Requirement: 外部配置管理器竞争必须可见
系统 SHALL 在发现已知外部配置管理器仍在运行、或恢复后配置立即被再次改写时，停止自动重试并提示用户先退出或关闭其自动应用功能；系统 SHALL 不自动终止第三方进程。

#### Scenario: cc-switch 仍在自动应用
- **WHEN** 恢复预检确认 cc-switch 正在管理 Codex 配置
- **THEN** 系统阻止写入并提示用户退出 cc-switch 后重新检查

#### Scenario: 恢复后配置被回写
- **WHEN** 恢复提交后校验发现目标路由不再是刚提交的值
- **THEN** 系统报告外部回写冲突并提供快照位置，不进入循环覆盖

### Requirement: 官方登录必须通过 Codex 完成
当恢复后 CLI 状态不是 ChatGPT 登录时，系统 SHALL 提供由用户明确触发的 `codex login` 官方浏览器流程；登录窗口完成后 SHALL 重新执行只读状态探测。系统 SHALL 不请求用户把 token、refresh token 或 `auth.json` 粘贴到 2xapi。

#### Scenario: 用户完成浏览器登录
- **WHEN** 用户从恢复界面启动官方登录并完成浏览器回调
- **THEN** 系统重新检查并显示 ChatGPT 登录成功，且不在应用数据或日志中保存返回凭据

#### Scenario: 用户取消或登录失败
- **WHEN** 登录命令退出但状态仍不是 ChatGPT 登录
- **THEN** 系统保留已恢复的官方路由，显示 Codex 登录日志位置和重试入口，不回滚到中转配置

### Requirement: 验证分为无成本状态验证与显式端到端验证
系统 SHALL 在恢复后自动完成配置解析、官方 provider 选择和 `codex login status` 验证；会产生请求或额度消耗的 `codex exec` 冒烟 SHALL 由用户明确确认后运行，并 SHALL 报告命令退出状态而不是仅报告进程已启动。

#### Scenario: 自动验证通过
- **WHEN** 官方路由提交成功且 CLI 报告 ChatGPT 登录
- **THEN** 系统显示配置与登录验证通过，但不自动消耗用户额度

#### Scenario: 用户确认端到端测试
- **WHEN** 用户明确点击测试官方连接
- **THEN** 系统用官方 provider 发起最小 `codex exec` 请求，等待完成并展示成功或经过脱敏的错误类别

#### Scenario: 验证失败
- **WHEN** 配置解析、登录状态或端到端请求任一失败
- **THEN** 系统准确标记失败阶段、保留恢复前快照且不宣称已完成

### Requirement: 提供官方 Codex 重置与初始化双模式
系统 SHALL 将“恢复官方路由（保留登录）”与“完整官方初始化（清除登录后重新登录）”作为两个独立、可预览、需二次确认的操作。前者 SHALL 只隔离活动配置和 2xapi 拥有的 provider/catalog，保留 `auth.json`、系统钥匙串、sessions、rollouts、SQLite 历史、MCP、插件和权限；后者 SHALL 先调用官方 `codex logout`，再处理残留凭据文件，并引导用户执行官方 `codex login`。两种模式均 SHALL 使用精确路径、备份清单、提交前 CAS 和提交后校验。

#### Scenario: 配置重置保留官方登录
- **WHEN** 用户确认“重置官方配置（保留登录）”
- **THEN** 系统将预览列出的 `config.toml`/profile/2xapi overlay 可恢复地移动到私有时间戳目录，保持 `auth.json` 与 keyring 不变，重启后有效 provider 等价于官方默认 `openai`

#### Scenario: 完整官方初始化
- **WHEN** 用户确认“完整初始化官方 Codex”
- **THEN** 系统先执行 `codex logout`，成功后才允许把仍存在的精确 `auth.json` 移入隔离备份；随后保持无活动用户路由配置并启动 `codex login` 官方流程，不自动回灌任何旧 token

#### Scenario: keyring 模式下的初始化
- **WHEN** `auth.json` 不存在或 `cli_auth_credentials_store` 为 `keyring|auto`
- **THEN** 系统仍必须执行并验证 `codex logout`，不得把文件不存在解释成已经退出登录，也不得直接操作系统钥匙串

#### Scenario: 配置文件不自动重生成
- **WHEN** reset 后 Codex 使用内置默认配置且没有立即创建新的 `config.toml`
- **THEN** 系统按“有效 provider 为 `openai`、登录状态可验证、用户数据保留”判定成功，不把物理文件重新生成作为必要条件

#### Scenario: 外部管理器或并发修改
- **WHEN** cc-switch 等外部管理器仍在运行，或预览后配置哈希发生变化
- **THEN** 系统阻止 reset，不自动终止外部进程、不循环覆盖，并要求用户退出外部管理器后重新预览
