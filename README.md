# 2xapi Codex Console

让主流 AI 客户端（OpenAI Codex 桌面版/CLI、Claude Code、Hermes、Gemini CLI、Grok Build、OpenCode、OpenClaw、WorkBuddy/CodeBuddy、Cursor 等）一键走 API 中转站。面向小白用户：装好 → 登录 2xapi 账号 → 选供应商 → 点一下，各客户端立即开始走你的中转；官方登录、插件、订阅全部照常保留。

> 本项目采用 [MIT License](./LICENSE) 开源授权。第三方名称、Logo 和服务仍归各自权利人所有。

---

## 它解决什么问题

很多用户想用 API 中转站（更便宜、模型更多），但各 AI 客户端默认只认官方登录或官方配置，改用中转需要手改 `~/.codex/config.toml`、`~/.claude/settings.json` 等配置文件——对普通用户几乎是劝退门槛。

2xapi Codex Console 把这个过程收敛成**一次点击**：

| 能力 | 说明 |
|---|---|
| 一键托管 9 类 AI 客户端 | OpenAI Codex（桌面版/CLI）、Claude Code、Hermes、Gemini CLI、Grok Build、OpenCode、OpenClaw、WorkBuddy/CodeBuddy、Cursor，经本地网关统一走所选供应商；另有 Claude Desktop 3p 网关接入。官方配置自动备份、可一键还原 |
| 2xapi 账号一键接入 | 邮箱 + 滑块验证码登录，导入 Key 自动生成供应商；也可手动填写任意 OpenAI 兼容中转地址 |
| 供应商管理 | 手动添加、自动测试连接、自动拉取模型、多供应商一键切换 |
| 多协议网关 | 本地网关 `127.0.0.1:8787` 统一入口，支持 OpenAI Responses、Chat Completions、Anthropic Messages、Gemini、图片生成（`/images/generations`），按需自动转换协议 |
| 用量统计 | 第三方中转 Token 统计：今日摘要、热力图、每日趋势（按模型拆分）、模型排行、缓存命中率与预计节省、请求性能 P50/P90；透明悬浮窗（可拖动/置顶/鼠标穿透/透明度可调） |
| 插件与能力市场 | http 型插件与媒体服务（A/B/C 段）即装即用；生态中心统一管理各客户端的 MCP 服务器 |
| 历史会话 | Codex 会话列表/恢复/修复，Claude 会话历史只读浏览 |
| 加速线路 | 2xapi 加速节点与自定义 IP 管理，中转流量可选走专线 |
| 在线更新 | GitHub Releases + minisign 签名校验（v1.0.11 起），自动下载安装 |

**平台支持**：macOS（arm64 / x86_64）、Windows（x86_64）。macOS 发布若尚未配置 Apple 开发者证书（未签名），首次打开被系统拦截时请右键（或 Control + 点击）应用图标选择「打开」；配置证书后安装包为签名+公证版本。

## 快速开始

1. 下载安装包（见 [GitHub Releases](https://github.com/2xapi/ai-gateway/releases/latest)）。
2. 打开软件 → 登录 2xapi 账号（或手动填一个中转站地址 + Key）。
3. 选好供应商 → 点「开启托管」。
4. 打开对应的 AI 客户端（Codex 桌面版/CLI、Claude Code 等），开始使用。

不需要修改任何配置文件；随时可一键还原官方配置。

## 开发

```bash
cd src-tauri
cargo test        # 单元测试
cargo build --release
```

技术栈：Tauri 2 + Rust（axum 本地网关）+ 原生 JS 前端。

## 在线更新

应用从本仓库的 GitHub Releases 读取签名的 latest.json，检查、下载和安装由 Tauri updater 完成；更新包使用 minisign 签名校验，安装后自动重启。1.0.11 是启用在线更新的引导版本，旧版本需要先手动安装一次（当前版本 1.0.13）。

发布版本时保持 src-tauri/Cargo.toml、src-tauri/tauri.conf.json 与 Git tag 版本一致，并配置以下 GitHub Actions Secrets：

- TAURI_SIGNING_PRIVATE_KEY
- TAURI_SIGNING_PRIVATE_KEY_PASSWORD

更新签名私钥不得提交到仓库。普通安装包和 updater 专用包由 [.github/workflows/build-all.yml](./.github/workflows/build-all.yml) 统一发布。

## 许可证

本项目采用 MIT License，版权归 2xapi 所有。完整条款见 [LICENSE](./LICENSE)。

商业合作与授权请联系 2xapi。
