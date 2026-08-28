## Purpose

将历史会话修复从一次性阻塞操作改为可观察、可取消、可恢复的任务，使大批量会话在单项损坏、进程重启或外部文件变化时仍能安全完成，并且不触碰 Codex 路由和官方凭据。

## ADDED Requirements

### Requirement: Repair preview and bounded work
系统 SHALL 在修复前返回目标数据库/索引、预计条目数、会修改的字段、备份位置和预计跳过条件；单个文件、响应正文和任务日志必须有大小上限。

#### Scenario: Preview before repair
- **WHEN** 用户点击历史会话修复
- **THEN** 页面先展示只读预览，用户确认后才创建修复任务

#### Scenario: Oversized or malformed item
- **WHEN** 单个会话文件超过上限或无法解析
- **THEN** 系统跳过该项并记录脱敏错误，继续处理其他项目，不清空数据库或索引

### Requirement: Durable repair job lifecycle
系统 SHALL 为每个修复任务提供 queued、running、cancelling、cancelled、completed、failed 状态，保存已处理、成功、跳过、失败计数和最近检查点。

#### Scenario: User cancels a running job
- **WHEN** 用户点击取消
- **THEN** 任务在当前安全边界结束并标记 cancelled，已完成的安全修改保留，未处理条目不再继续写入

#### Scenario: Resume after interruption
- **WHEN** 应用重启或任务进程中断后用户点击恢复
- **THEN** 系统从最近检查点继续，已成功处理的条目不会重复计数或重复破坏

### Requirement: Idempotent and isolated repair
修复操作 SHALL 以稳定会话 ID 和内容签名去重，重复执行同一任务不得重复写入或重复统计；单个会话失败不得阻塞其他会话。

#### Scenario: Retry the same job
- **WHEN** 用户对已部分完成的任务点击恢复
- **THEN** 系统跳过已验证完成的条目，只处理未完成或先前失败的条目

#### Scenario: External change during repair
- **WHEN** 外部进程在任务运行期间修改数据库或索引
- **THEN** 系统停止受影响批次、保留安全备份并报告冲突，不覆盖外部修改

### Requirement: Repair scope isolation
会话修复 MUST 只操作会话数据库、索引和 rollout 元数据，不得写入 `config.toml`、provider、MCP、插件、权限或 `auth.json`。

#### Scenario: Repair completes while third-party hosting is active
- **WHEN** 2xapi 网关正在托管且用户执行会话修复
- **THEN** 网关路由、供应商选择、官方登录凭据和生态配置保持不变
