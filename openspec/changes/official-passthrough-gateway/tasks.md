## 步骤 1：后端透传核心

- [x] 1.1 `codex_overlay.rs`：overlay 的 `requires_openai_auth` 改为按官方登录态动态决定（SignedIn=true 保官方功能；未登录=false 纯 API 旧行为零影响）
- [x] 1.2 `providers.rs`：内置官方 ChatGPT 条目（load 注入+固定字段防篡改规范化、delete/update/reorder 拦截、单候选不自动激活、create 序号不污染）
- [x] 1.3 `gateway.rs`：官方透传分支——透传客户端 Bearer、补 `chatgpt-account-id`（缺失时 JWT 解）、`originator`/`User-Agent`，SSE 流式回传；官方 client 独立代理且 socks5 统一升级远端解析（真机实证本地解析被 DNS 污染）
- [x] 1.4 settings：`official_proxy_url` 读写（http/https/socks5 校验、保留同文件其他段、读取脱敏）
- [x] 1.5 单测：官方条目注入/保护/显式激活语义、JWT account_id 解码、代理校验回环、无 active→hosting null 语义适配（共 4 个新测试 + 12 个既有断言适配）

## 步骤 2：UI 与状态

- [x] 2.1 供应商详情卡：官方条目「内置」标记、官方语义字段展示、隐藏编辑/删除、按钮「切换到官方」
- [x] 2.2 设置页 IP 管理新增「官方通道代理」输入 + 保存（草稿防丢）
- [x] 2.3 接入模式标签：官方直连(经网关透传) · Official
- [x] 2.4 前端语法检查通过

## 步骤 3：验证

- [x] 3.1 本机全量 cargo test（476 passed / 7 个 Cursor 环境性失败）/ clippy -D warnings / fmt / node --check / OpenSpec strict 全过
- [x] 3.2 Mac 真机端到端：付费官方账号 + socks5h 代理，host 后 `requires_openai_auth=true`，`codex exec` 经网关透传官方后端 200/OK，`auth.json` 哈希全程不变
- [x] 3.3 切换官方 ↔ 第三方仅变更激活供应商，config.toml 哈希不变，两向请求均成功
- [x] 3.4 Windows VM 验证：全量 457 passed / 16 个既有平台失败（与基线一致），official 定向全过；网关官方透传链路真机实证（VM 网关 → 官方通道代理 → 官方后端，HTTP 200 + response.created 流式 + OK，auth.json 哈希不变）。CLI 端到端在 VM 受 npm 版 codex 行为限制（login status 主动消费 refresh_token，与原生 CLI 不同），该链路已在 Mac 原生 CLI 完整实证；Windows 用户建议使用官方 ChatGPT 桌面版内嵌 CLI
- [x] 3.5 双平台安装包重建（含官方透传）与项目书同步
