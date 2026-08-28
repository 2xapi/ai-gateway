> Apply baseline: `3b329a8bb0d77dbfd9d6539ee26f9414c3a815f4`
> Implementation branch: `protect-codex-official-login`

## 1. 实施基线与安全测试夹具

- [x] 1.1 记录 `git rev-parse HEAD` 作为 base-ref，确认工作树仅含本 change 规划文件，并在 apply 阶段创建 `protect-codex-official-login` 分支
- [x] 1.2 建立隔离 `CODEX_HOME`、带注释/多 provider/MCP 的 TOML 样例、file/keyring 状态替身和可编排输出/超时的 fake Codex CLI 测试夹具
- [x] 1.3 先添加会失败的凭据不变量测试：host、switch、unhost、official recovery、启动和状态刷新前后 `auth.json` 的存在性、字节、权限与修改时间不变
- [x] 1.4 先添加会失败的配置安全特征测试：无效 TOML 不被空对象覆盖、注释/排版/无关段保留、预览后并发改写被拒绝、事务失败完整回滚

## 2. 官方登录状态探针

- [ ] 2.1 为 ChatGPT、API key、未登录、未知成功文案、CLI 缺失、非零退出和超时编写 `codex login status` 解析与错误分类测试
- [ ] 2.2 实现跨平台 Codex CLI 定位、带超时的只读 login probe 和内存短缓存，确保输出及日志不包含凭据内容
- [ ] 2.3 更新桌面 state 与 `/health` 契约，用结构化 login state 取代 `auth.json.exists()`/token 字段扫描，并补充路由级回归测试
- [ ] 2.4 实现用户显式触发的 `codex login` 启动能力及状态轮询，测试取消、启动失败和完成后重新探测，不接收或存储 CLI 凭据

## 3. 保格式 Overlay 与所有权状态

- [x] 3.1 添加与现有 TOML 栈兼容的 `toml_edit` 直接依赖，创建只操作 Codex 受控路由键的保格式 editor
- [x] 3.2 实现配置 SHA-256/CAS、可校验 pre-write 备份、0600 同目录临时文件、原子替换和提交后重读校验，并让 1.4 测试通过
- [x] 3.3 为 ownership sidecar 编写 baseline/applied 序列化、无敏感字段、首次 host、重复 host、换供应商和损坏 sidecar 测试
- [x] 3.4 实现 `2xapi-codex-overlay-state.json` 的原子读写及 config/catalog/providers/sidecar 联合回滚，不把 upstream Key、bearer 或 token 写入 sidecar
- [x] 3.5 为三方合并编写 current=applied、current=baseline、外部冲突和无关字段并发修改测试，再实现逐受控路径恢复与冲突报告

## 4. 托管链路去凭据化与旧版迁移

- [x] 4.1 先添加 gateway host 测试：无论 ChatGPT、API key、未登录或 unknown，均生成独占 `2xapi_gateway`、`requires_openai_auth=false` 且不创建/改写 auth
- [x] 4.2 将 Codex 桌面 host/switch/unhost/activate-official 全部迁移到新 overlay editor，删除 `ensure_auth_key`、PureApi auth 写入、`.official.bak` 自动创建/回灌和凭据快照回滚
- [x] 4.3 为用户已有 `[model_providers.custom]`、地址碰撞和其他 provider 定义添加回归测试，确认新托管与恢复只操作 `2xapi_gateway`
- [ ] 4.4 为旧 2xapi gateway、旧 bearer direct、无法确认来源的 custom 三类配置先写迁移测试，再实现强签名识别、显式迁移和未知配置只报告
- [x] 4.5 退役桌面 direct 接口和 UI 入口，返回稳定 `E_DESKTOP_DIRECT_RETIRED`/gateway 迁移提示；保留并回归测试终端进程级 env direct
- [x] 4.6 审计并移除 Codex 运行时代码中所有直接 `auth.json` 读写/删除路径；仅为用户明确确认且 `codex logout` 成功后的 `reset-all` 保留不读内容的精确隔离移动，历史文件引用只能用于“不自动恢复”的提示或隔离测试断言

## 5. 官方恢复后端

- [ ] 5.1 为 recovery preview 编写只读测试，覆盖官方正常、断路 loopback、未登录+第三方路由、legacy 2xapi 和未知外部配置
- [ ] 5.2 实现带当前配置 hash、过期时间和目标路径绑定的 preview token，以及 `unhost`/`official-default` 两套逐字段 diff
- [ ] 5.3 为普通 unhost 编写 ownership 三方恢复、冲突保留、活动供应商清理和幂等测试并接入 apply API
- [ ] 5.4 为紧急 official-default 编写顶层路由键清理、默认 `openai` 解析、保留 provider/profile 文件/MCP/插件/权限设置和凭据字节不变测试并接入二次确认 API
- [ ] 5.5 实现 cc-switch 等已知外部管理器的只读进程预检、提交前 CAS 和提交后回写检测，覆盖 active、竞态启动和立即回写错误
- [ ] 5.6 实现显式 official smoke API，用 fake CLI 测试成功、401、未登录、超时和脱敏 stderr；真实请求前要求单独确认
- [x] 5.7 为 `reset-config`/`reset-all` 编写预览测试，列出精确文件、哈希、权限、keyring 风险、备份目标和将保留的 sessions/rollouts/MCP/插件数据
- [x] 5.8 实现 `reset-config` 的二次确认、提交前 CAS、同卷可恢复隔离移动、清单落盘、活动路径校验和重启后的默认 `openai` 验证；不得承诺物理生成 `config.toml`
- [x] 5.9 实现 `reset-all` 的官方 `codex logout` 阶段机、失败/取消保护、残留 `auth.json` 的无内容隔离移动和 `keyring|auto` 回归；系统钥匙串只由 Codex CLI 改变
- [ ] 5.10 覆盖 reset 期间外部管理器竞争、并发改写、移动失败、重启失败和幂等重试，确认不删除历史数据、不循环覆盖、不回滚到中转配置

## 6. 恢复界面与交互

- [x] 6.1 扩展 `frontend/api-client.js`，接入 recovery preview/apply、login start/status 和 official smoke，并保留稳定错误码/阶段信息
- [x] 6.2 在 Codex 连接（主通道）增加始终可见的官方状态卡，分别展示路由、登录方式、外部管理器、拟变更和备份信息；不要把连接恢复入口藏在高级设置
- [ ] 6.3 实现普通取消托管与紧急官方默认路由的不同文案、逐字段预览和二次确认，冲突或外部管理器 active 时禁止提交
- [ ] 6.4 实现“打开官方登录”后的轮询状态与“测试官方连接”的额度提示，只有配置、登录或 E2E 实际验证完成后才显示对应成功状态
- [x] 6.5 更新桌面托管文案为 gateway-only/零 Key，移除 direct 选择，并为 legacy direct 用户显示一次性安全迁移说明
- [x] 6.6 在 Codex 连接页提供“恢复 Codex 官方配置（保留登录）”，在会话管理页“Codex 环境恢复（高级）”卡片提供“初始化 Codex（退出登录并重新登录）”；两者均展示逐文件 diff、隔离备份位置、历史数据保留范围和不可恢复风险提示
- [x] 6.7 实现 reset 阶段机、`codex logout`/`codex login` 人工授权交接、keyring 提示和“未生成 config.toml 也属于成功”的状态文案
- [x] 6.8 固化三处入口的信息架构与退出纪律：连接页负责托管/停用/恢复官方配置/官方登录，会话页负责历史会话修复与高级初始化，高级设置只读诊断；应用退出、窗口关闭和网关停止不得自动改写 Codex 配置或调用 logout

## 7. 验证、审查与交付

- [x] 7.1 运行 `cargo fmt --check`、相关 Rust 单元/路由测试、完整 `cargo test`、`cargo check` 和项目既有构建命令，修复所有新增或回归失败
- [ ] 7.2 在隔离 `CODEX_HOME` 执行 host→switch→unhost、legacy migration、official-default、并发改写和失败回滚集成测试，对账 config/auth/sidecar/catalog/providers 的 hash 与权限
- [ ] 7.3 对真实 `~/.codex` 仅执行启动、状态、preview 的只读冒烟，验证 `config.toml`/`auth.json`/关键 sidecar 前后 hash 和修改时间不变
- [ ] 7.4 检查 `git diff`、依赖锁文件和敏感信息扫描，确认没有无关改动、真实 Key/token、凭据内容日志或自动删除历史备份
- [ ] 7.5 请求代码审查，重点复核凭据所有权边界、TOML 三方合并、TOCTOU、回滚与紧急恢复的用户授权语义，并修复适用意见
- [ ] 7.6 在用户明确确认后执行一次真实 ChatGPT `codex login status` 与最小 official `codex exec` E2E，随后重启 ChatGPT.app 验证桌面加载；记录脱敏结果和已知限制
- [ ] 7.7 在隔离 `CODEX_HOME` 完成两种 reset 的端到端回归，另对真实 `~/.codex` 只做 preview/status 只读验证，确认 reset 实现不会误触真实凭据和历史会话
