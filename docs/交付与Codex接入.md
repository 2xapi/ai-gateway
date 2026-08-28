# 交付与 Codex 接入手册

> 2xapi Codex Console —— 构建、运行、把 Codex 接到本地网关的完整步骤。

---

## 0. 前置确认
- 已修 `from_std` 运行期 panic（`main.rs` 绑定 8787 后 `set_nonblocking(true)`）。**旧 release 二进制必须重建**，否则启动即崩。
- 旧版本的 35 个单测与真实 DeepSeek 验证记录保留作历史证据；当前版本请以本页末尾的 P0 验证记录为准。

---

## 1. 构建

```bash
cd ~/Documents/Codex/2026-08-13/2xapi-tauri/src-tauri

# 方式 A：只编二进制（最快，跑起来就是一个带窗口的 app）
cargo build --release
# 产物：target/release/console-2xapi

# 方式 B：打 .app 包（需要 tauri-cli + icons/icon.png）
cargo tauri build
# 产物：src-tauri/target/release/bundle/{macos,exe,...}/
```

---

## 2. 运行（接你真实的 Codex）

app 启动后：固定监听 `127.0.0.1:8787`（本地网关），并弹出控制台窗口。
它读写的是 **Codex 主目录**（`CODEX_HOME` 环境变量，默认 `~/.codex`）。

```bash
# 正常用（操作你真实的 ~/.codex）：
cargo run --release
# 或直接跑产物：
./target/release/console-2xapi
```

> 排错：若启动报「无法绑定 127.0.0.1:8787」→ 有旧实例/别的程序占着，先 `lsof -ti:8787 | xargs kill`。

---

## 3. 接 Codex（三步）

1. **在控制台里建一个 provider**：
   - 点「+ 新建供应商」→ 选模式（见下）→ 填上游地址 / API Key / 默认模型 → 保存。
   - 例：DeepSeek → 模式 `Mixed`、`wire_api=ChatCompletions`、`base_url=https://api.deepseek.com`、`model=deepseek-chat`。
2. **激活**：在列表里点该 provider 的「激活」。控制台会自动写 `~/.codex/config.toml`（`custom.base_url` 指向 `127.0.0.1:8787` 网关），按模式处理 `auth.json`。
3. **（重启）Codex**：Codex 启动时读 `config.toml` → 把请求发到 `127.0.0.1:8787` → 网关注入 key、按 `wire_api` 转换 → 转发到上游。搞定。

> 热切换：在控制台激活**另一个** provider → **不用重启 Codex**，下一个请求就走新 provider（进行中的请求走完旧的不中断）。

---

## 4. 三种模式对 `~/.codex` 的改动（冻结契约，见 docs/01/02）

| 模式 | config.toml | auth.json | 说明 |
|---|---|---|---|
| **Official** | `model_provider="openai"`，删 custom 段 | 不动 | Codex 直连官方，不经网关 |
| **Mixed** | `model_provider="custom"`，base_url=`127.0.0.1:8787`，`requires_openai_auth=true` | 不动 | 保留官方登录，走网关用第三方模型 |
| **PureApi** | 同 Mixed | 覆盖 `OPENAI_API_KEY`（首次切换前自动备份 `auth.json.official.bak`） | 纯第三方；「切官方」可从备份恢复 |

config.toml 里 **不写** `experimental_bearer_token`（key 只由网关注入，单一真相源）。

---

## 5. 切回官方
顶栏「⇄ 切官方」→ 恢复 `auth.json`（若有 `.official.bak`）、config 改回 `model_provider="openai"`、清 active。重启 Codex 即回官方。

---

## 6. 验收对照（05 末 10 项 checklist）
- [x]（已自验）新建 Mixed provider → 网关转发 → 上游返回正确（真实 DeepSeek）
- [x]（已自验）ChatCompletions 协议转换：非流式 + 流式 SSE
- [x]（已自验）诊断三步、模型列表、config 预览==实际写入
- [ ]（需你）真实 Codex 对话、热切换不重启、activate-official 恢复、per-provider 代理/超时

## 7. P0 配置档案 / Doctor 2.0 / 会话任务控制（2026-08-28）

- [x] 项目书与 API 契约已写入 `docs/01_产品概述与需求冻结.md`、`docs/04_接口设计.md`。
- [x] Codex 配置档案：版本化 `2xapi-profiles.json`、原子写、预览令牌、配置与供应商 CAS、备份和失败回滚；档案不保存 key，应用不触碰 `auth.json`。
- [x] Doctor 2.0：固定 `checks[]`、错误分类与脱敏、健康注册表、连续失败熔断/冷却/半开探测；Official 状态 bypass，不自动故障转移到第三方。
- [x] 会话修复任务：预览、queued/running/cancelling/cancelled/completed/failed 生命周期、取消、检查点恢复、心跳/停滞提示、脱敏任务元数据持久化。
- [x] 真实执行的验证：针对性 profile/session/doctor/server 测试通过；`cargo clippy --all-targets -- -D warnings`、前端 `node --check`、release build、OpenSpec strict 均通过。
- [ ] 完整测试套件仍需在关闭本机 Cursor 后重跑；当前运行中的 Cursor 使既有 Cursor adapter 测试按设计返回 `E_CURSOR_RUNNING`，不是本次 P0 代码失败。
