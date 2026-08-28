# 官方透传网关设计

## 目标

Mixed 托管下官方账号功能可用：`config.toml` 指向网关后，激活「官方 ChatGPT」供应商即用官方额度，激活第三方即走中转，切换不动 `config.toml`。

## 架构

```text
Codex CLI ──(Bearer 官方 token, requires_openai_auth=true)──▶ 127.0.0.1:8787
                                                                   │
                                   激活供应商 = 官方 ChatGPT ────────┤
                                   │                               │
                                   ▼                               ▼
                    POST chatgpt.com/backend-api/codex/responses   第三方中转(现状)
                    (透传 Bearer + 补 headers + 官方代理出口 + 流式)
```

## 关键决策

1. **`requires_openai_auth = true`**（overlay 字段）：CLI 复用 ChatGPT 登录请求网关；token 刷新仍由 CLI 对 auth.openai.com 自主完成，2xapi 不触碰 `auth.json`。
2. **透传认证**：网关转发客户端的 `Authorization: Bearer <JWT>`；`chatgpt-account-id` 优先透传客户端 header，缺失时从 JWT `https://api.openai.com/auth.chatgpt_account_id` 解出；固定补 `originator: codex_cli_rs`、`User-Agent: codex_cli_rs`（PoC 实证被接受）。
3. **流式**：官方端点强制 `stream: true`；网关原样流式回传（SSE 透传），不缓冲。
4. **官方代理出口**：新增 `official_proxy_url`（2xapi-settings.json），网关构建官方专用 reqwest client（http/https/socks5），与每供应商代理、加速线路互不干扰。
5. **内置官方条目**：providers store 内置 id=`official-chatgpt`、`access_mode=official`、`base_url=https://chatgpt.com/backend-api/codex` 的不可删除条目；`get_active_for_agent` 命中它时走透传分支。
6. **安全边界不变**：网关不保存、不解析落盘官方 token；官方通道不参与第三方故障转移；官方条目不进 KeyPool。

## 边界与已知限制

- 官方透传需要可达出口（直连或代理）；出口不可达时报上游网络错误，不回退第三方。
- 无 refresh_token 的注入式 auth.json（如外部构造的 free 账号）token 过期后 CLI 无法刷新，属凭据本身限制。
- CLI 对 custom provider 走 HTTPS responses（无 websocket），透传无需处理 WS 升级。
