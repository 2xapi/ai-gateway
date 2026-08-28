## Purpose

为每个供应商提供可重复、可解释且不泄露凭据的连接能力诊断，让用户能够在切换前识别协议、认证、模型、代理、流式和工具调用问题，并为健康路由提供可靠信号。

## ADDED Requirements

### Requirement: Multi-stage provider diagnosis
系统 SHALL 按配置校验、网络/代理可达性、认证、模型列表、最小真实请求和流式/工具能力顺序执行诊断，并返回每一步的状态、耗时、错误类别和脱敏说明。

#### Scenario: Healthy provider
- **WHEN** 供应商地址、代理、凭据、模型列表和最小请求均成功
- **THEN** 诊断返回全绿结果、实际协议、延迟和可用模型摘要

#### Scenario: Authentication failure
- **WHEN** 上游返回 401 或 403
- **THEN** 诊断将结果分类为认证/区域限制，不把它显示为网络超时，也不自动更换账号或修改凭据

#### Scenario: Protocol mismatch
- **WHEN** `/models` 可用但 Responses 请求失败且 Chat Completions 探测成功
- **THEN** 诊断明确提示协议不匹配，并给出可选转换方案，不静默改写供应商配置

### Requirement: Safe diagnostic output
诊断结果、日志和前端展示 MUST 不包含 API Key、OAuth token、Cookie、Authorization、请求正文或响应正文；上游错误正文必须限长并脱敏。

#### Scenario: Sensitive upstream error
- **WHEN** 上游错误正文包含 bearer 或 key 字样
- **THEN** 返回结果只保留 HTTP 状态、错误类别和脱敏摘要

### Requirement: Health state and circuit policy
系统 SHALL 为第三方供应商维护成功率、连续失败数、最近成功时间、延迟和熔断状态；熔断与恢复必须有明确阈值、冷却时间和用户可见原因。

#### Scenario: Repeated upstream failure
- **WHEN** 同一第三方供应商连续达到失败阈值
- **THEN** 系统将其暂时摘除并显示冷却倒计时，后续请求按用户配置转移到其他第三方供应商或返回明确错误

#### Scenario: Official account is never failed over
- **WHEN** 官方 Codex 请求返回 401、403、区域限制或登录失效
- **THEN** 系统不得将请求转移到第三方供应商，不得修改官方登录凭据

### Requirement: Explicit protocol and model confirmation
系统 SHALL 在自动发现模型或协议后要求用户确认写回；自动发现失败不得清空现有模型、协议或供应商配置。

#### Scenario: Model discovery fails
- **WHEN** `/models` 超时、返回非 JSON 或权限不足
- **THEN** 系统保留原配置并返回具体失败原因，用户仍可手工选择已保存模型
