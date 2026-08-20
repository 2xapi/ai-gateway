# 2xapi Codex Console

让桌面版 Codex(ChatGPT.app)一键走 API 中转站。面向小白用户:装好 → 选供应商 → 点一下,桌面版 Codex 立即开始走你的中转;官方登录、插件、订阅全部照常保留。

> 本项目采用 [MIT License](./LICENSE) 开源授权。第三方名称、Logo 和服务仍归各自权利人所有。

---

## 它解决什么问题

很多 Codex 用户想用 API 中转站(更便宜、模型更多),但官方 Codex 桌面版只认官方登录,配置中转需要手改 `~/.codex/config.toml` 和 `auth.json`——对普通用户几乎是劝退门槛。

2xapi Codex Console 把这个过程收敛成**一次点击**:

| 能力 | 说明 |
|---|---|
| 桌面版一键走中转 | 托管开启后,桌面版 Codex 的模型请求经本地网关转发到所选供应商,官方登录/插件/订阅原样保留 |
| 账号自动识别 | 有官方账号 → 混入模式(登录保留);没账号 → 纯 API 模式,自动备份、可一键还原 |
| 协议转换 | 只支持 Chat Completions 的中转站,经网关自动转成 Codex 的 Responses 协议,照样能用 |
| 会话统一 | 对话记录保存在 `~/.codex`,官方与中转聊的都在同一个历史列表,随时继续 |
| 加速线路(规划中) | 自有加速节点,2xapi 中转站流量自动走专线,可选开关 |
| 极简 UI | 没有"供应商""模式"这类黑话,只有选谁、点一下 |

## 快速开始

1. 下载安装包(见 [GitHub Releases](https://github.com/2xapi/ai-gateway/releases/latest))。
2. 打开软件 → 登录 2xapi 账号(或手动填一个中转站地址 + Key)。
3. 选好供应商 → 点「开启:桌面版走中转」。
4. 打开桌面版 Codex,开始使用。

不需要修改任何配置文件;随时可一键还原官方配置。

## 开发

```bash
cd src-tauri
cargo test        # 单元测试
cargo build --release
```

技术栈:Tauri 2 + Rust(axum 本地网关)+ 原生 JS 前端。

## 在线更新

应用从本仓库的 GitHub Releases 读取签名的 latest.json，检查、下载和安装由 Tauri updater 完成；更新包使用 minisign 签名校验，安装后自动重启。1.0.11 是启用在线更新的引导版本，旧版本需要先手动安装一次。

发布版本时保持 src-tauri/Cargo.toml、src-tauri/tauri.conf.json 与 Git tag 版本一致，并配置以下 GitHub Actions Secrets：

- TAURI_SIGNING_PRIVATE_KEY
- TAURI_SIGNING_PRIVATE_KEY_PASSWORD

更新签名私钥不得提交到仓库。普通安装包和 updater 专用包由 [.github/workflows/build-all.yml](./.github/workflows/build-all.yml) 统一发布。

## 许可证

本项目采用 MIT License，版权归 2xapi 所有。完整条款见 [LICENSE](./LICENSE)。

商业合作与授权请联系 2xapi。
