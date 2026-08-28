## Purpose

为多客户端和多供应商场景提供可预览、可回滚的命名配置档案，减少用户反复手工切换配置的风险，同时把官方 Codex 登录凭据与第三方路由配置彻底隔离。

## ADDED Requirements

### Requirement: Named configuration profiles
系统 SHALL 允许用户为 Codex 创建、重命名、复制、删除和选择命名配置档案，并为其他已支持客户端预留作用域字段。档案至少包含供应商、模型、上游协议、供应商代理、加速线路和生态开关；档案必须带版本号、更新时间和所属客户端作用域。

#### Scenario: Create and select a profile
- **WHEN** 用户保存一个包含供应商和模型的 Codex 档案并将其设为当前档案
- **THEN** 系统保存档案元数据并返回当前档案，且不改变官方 `auth.json` 的字节、权限或修改时间

#### Scenario: Profiles are scoped per client
- **WHEN** 用户切换 Codex 的档案
- **THEN** Claude、Gemini 或其他客户端的当前供应商和配置不得被隐式修改

### Requirement: Preview before applying a profile
系统 SHALL 在应用档案前生成逐字段差异、目标文件、当前哈希、备份目标和将保留的数据清单；用户未确认前不得写入活动配置。

#### Scenario: User cancels a preview
- **WHEN** 用户关闭预览或拒绝确认
- **THEN** 活动配置、供应商选择、会话数据和官方凭据均保持不变

#### Scenario: Concurrent change is detected
- **WHEN** 预览后目标配置的哈希已被其他进程改变
- **THEN** 系统拒绝应用并要求重新预览，不覆盖外部修改

### Requirement: Atomic apply and rollback
系统 SHALL 将档案应用作为可回滚事务执行；写入失败、验证失败或外部立即回写时，系统必须恢复应用前快照并报告失败阶段。

#### Scenario: Apply succeeds
- **WHEN** 用户确认且所有目标配置通过写入和重新读取校验
- **THEN** 档案被标记为当前，网关下一请求使用新供应商，且生成可恢复备份

#### Scenario: Apply fails midway
- **WHEN** 任一受控文件写入、权限设置或提交后校验失败
- **THEN** 系统回滚已写入文件，保留备份和错误摘要，不留下半套档案配置

### Requirement: Credential boundary
普通档案创建、复制、选择、应用、删除和回滚 MUST NOT 创建、覆盖、解析、恢复或删除 Codex 官方 `auth.json`，也不得保存 API Key、OAuth token、Cookie、Authorization、prompt 或响应正文。

#### Scenario: Mixed profile preserves official login
- **WHEN** 用户应用“官方登录保留 + 2xapi 网关”档案
- **THEN** 系统仅更新 2xapi 独占路由和网关状态，官方登录状态保持可用且不参与第三方故障转移

#### Scenario: Secret is excluded from profile export
- **WHEN** 用户导出档案
- **THEN** 导出包只包含脱敏连接元数据和受控字段；真实密钥只能在用户明确选择的本机安全存储中引用
