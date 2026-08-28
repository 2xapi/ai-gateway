# Proposal: official-passthrough-gateway

## Why

当前 Mixed 模式的 `requires_openai_auth = false` 使官方登录 token 完全闲置：`auth.json` 文件虽保留，但托管后没有任何流量走官方账号额度，"官方登录保留"名存实亡。单一 `model_provider` 指向网关后，用户失去官方账号的使用能力。

PoC 已实证官方后端接受网关形态透传（Bearer 官方 token + `chatgpt-account-id` header + 流式 responses，HTTP 200）。

## What Changes

- 网关 overlay 改为 `requires_openai_auth = true`：Codex CLI 始终以官方登录凭据请求本地网关，`auth.json` 由 CLI 自主使用与刷新，2xapi 仍不代管。
- 网关新增官方透传分支：当前激活供应商为「官方 ChatGPT」（内置条目，`AccessMode::Official`）时，请求透传至 `https://chatgpt.com/backend-api/codex/responses`，携带客户端官方 Bearer、补齐 `chatgpt-account-id`/`originator` headers，流式回传。
- 新增「官方通道代理」设置（`official_proxy_url`，http/socks5）：官方透传出专用代理出口；留空直连。
- UI：供应商列表内置「官方 ChatGPT」条目（不可删除），设置页新增官方通道代理输入；官方模式下主通道状态显示官方直连。
- 切换官方/第三方仅改激活供应商，`config.toml` 不再变动；配置档案天然支持官方档案与中转档案并存秒切。

## Capabilities

### Capability: official-passthrough
网关在激活官方供应商时透传请求至官方后端，官方账号额度在托管状态下可用。

### Capability: official-proxy-egress
官方透传通道可配置独立代理出口，覆盖直连不可达环境。

## Impact

- `codex_overlay.rs`：overlay 字段值变更（`requires_openai_auth = true`）。
- `gateway.rs`：新增官方透传路由分支与官方客户端构建（带代理）。
- `providers.rs`：内置官方条目与不可删除保护。
- `settings`：`official_proxy_url` 持久化。
- 前端：官方条目渲染、代理设置输入、状态显示。
- 兼容：既有第三方托管行为不变；官方条目未激活时不产生任何官方流量。
