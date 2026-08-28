## Purpose

将 2xapi 对 Codex 路由的修改限制为可识别、可恢复且不含凭据的最小 overlay，避免覆盖用户配置、其他供应商管理器的条目以及操作期间发生的并发修改。

## ADDED Requirements

### Requirement: 2xapi 使用独占 provider 命名空间
系统 SHALL 使用不会覆盖通用 `custom` 条目的独占 provider id `2xapi_gateway`，并 SHALL 只在活动路由明确指向该条目时判定为 2xapi 托管。

#### Scenario: 用户已有 custom provider
- **WHEN** 用户的配置已包含 `[model_providers.custom]` 且启用 2xapi 托管
- **THEN** 原 `custom` 条目字节语义保持不变，系统新增并激活独立的 `2xapi_gateway` 条目

#### Scenario: 地址碰巧与供应商相同
- **WHEN** 非 2xapi provider 的地址与某个已保存供应商地址相同
- **THEN** 系统不因地址相同将其认作 2xapi 托管，也不在取消托管时删除它

### Requirement: TOML 修改保留未受控内容
系统 SHALL 对 `config.toml` 进行保留注释、键顺序和未受控段的最小编辑，SHALL 在写入前创建带目标路径与哈希的快照，并 SHALL 通过同目录临时文件、私有权限和原子替换完成提交。

#### Scenario: 配置含注释和 MCP 条目
- **WHEN** 配置包含注释、自定义排版、`mcp_servers`、插件、通知、权限和其他 provider 条目
- **THEN** 托管与恢复只改变已预览的路由键，其他内容及其注释和排版保持不变

#### Scenario: 写入中途失败
- **WHEN** 快照、临时文件写入、权限设置或原子替换任一步失败
- **THEN** 操作返回失败且配置、ownership 状态和活动供应商回滚到操作前的一致状态

#### Scenario: 配置无法解析
- **WHEN** 当前 `config.toml` 不是有效 TOML
- **THEN** 系统拒绝托管或恢复并报告解析错误，不用空配置覆盖原文件

### Requirement: Overlay 所有权支持三方合并
系统 SHALL 在不含凭据的 ownership sidecar 中记录基线受控值、最后应用值、目标配置身份和版本；恢复时 SHALL 对“基线、最后应用、当前值”做三方比较，仅恢复仍等于最后应用值的字段。

#### Scenario: 托管期间用户修改无关字段
- **WHEN** 用户在托管期间修改不受控字段后执行恢复
- **THEN** 系统保留该修改并恢复 2xapi 仍拥有的路由字段

#### Scenario: 托管期间外部工具修改受控字段
- **WHEN** 当前受控字段已不等于 2xapi 最后应用值
- **THEN** 系统将该字段列为冲突并保持当前值，不静默覆盖外部工具或用户的修改

#### Scenario: 重复启用同一托管
- **WHEN** 相同供应商和网关配置已生效且没有外部修改
- **THEN** 操作幂等完成，不重写配置、不重置基线且不产生无意义快照

### Requirement: 写入前重新读取并检测竞争
每个配置变更操作 SHALL 在提交前重新读取目标文件并验证预览所基于的版本；若文件在预览和提交之间发生变化，系统 SHALL 重新计算安全合并或返回冲突，不以旧内存快照覆盖新内容。

#### Scenario: 外部配置管理器并发写入
- **WHEN** cc-switch 或其他进程在 2xapi 的预览与提交之间改写 `config.toml`
- **THEN** 2xapi 检测到内容版本变化并停止或重新预览，不覆盖对方刚写入的配置

### Requirement: 旧版 2xapi overlay 安全迁移
系统 SHALL 仅在旧 `[model_providers.custom]` 同时满足 2xapi 网关地址、2xapi catalog 路径或旧 bearer 等强所有权特征时迁移或清理该条目；不满足强特征时 SHALL 只报告为未知外部配置。

#### Scenario: 可确认的旧网关配置
- **WHEN** 旧 `custom` 条目指向 2xapi 本地网关且 catalog 指向 2xapi 生成文件
- **THEN** 系统可在用户确认后把它迁移到 `2xapi_gateway`，并把迁移前值纳入可恢复基线

#### Scenario: 无法确认来源的 custom 配置
- **WHEN** `custom` 条目没有足够的 2xapi 所有权特征
- **THEN** 系统不删除、不改名且不声称能够自动恢复该条目

