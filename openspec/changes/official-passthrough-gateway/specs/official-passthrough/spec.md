# official-passthrough 规格

## ADDED Requirements

### Requirement: 官方供应商透传
网关 SHALL 在当前激活供应商为内置「官方 ChatGPT」条目时，把 Codex CLI 发往 `/v1/responses` 的请求透传至 `https://chatgpt.com/backend-api/codex/responses`。

#### Scenario: 官方账号在托管状态可用
- **WHEN** 托管 overlay 已写入且激活供应商为官方 ChatGPT
- **THEN** `codex exec` 请求经网关透传官方后端并成功返回
- **AND** 全程 `auth.json` 哈希不变

#### Scenario: 官方请求头补齐
- **WHEN** 客户端请求缺少 `chatgpt-account-id`
- **THEN** 网关从 Bearer JWT 的 `https://api.openai.com/auth.chatgpt_account_id` 解出并注入
- **AND** 透传请求带 `originator: codex_cli_rs` 与 `User-Agent: codex_cli_rs`

#### Scenario: 流式透传
- **WHEN** 官方端点返回 SSE 流
- **THEN** 网关以流式回传客户端，不整包缓冲

#### Scenario: 第三方行为不变
- **WHEN** 激活供应商为第三方
- **THEN** 网关注入供应商 key 转发中转，与既有行为一致

#### Scenario: 官方条目受保护
- **WHEN** 尝试删除或修改内置官方条目
- **THEN** 操作被拒绝且条目保持原样

#### Scenario: 官方不参与故障转移
- **WHEN** 官方透传失败（网络或 4xx/5xx）
- **THEN** 网关返回上游错误，不回退第三方供应商
