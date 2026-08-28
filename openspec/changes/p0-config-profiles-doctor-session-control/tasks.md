## 1. 项目书与契约

- [x] 1.1 将竞品吸收结论、P0/P1/P2 路线和官方凭据不变量写入项目书及文档索引
- [x] 1.2 固化 profile、Doctor checks、健康状态和会话任务的 API/数据契约

## 2. 配置档案

- [x] 2.1 新增版本化 profile 数据模型、原子读写、权限设置和迁移默认值
- [x] 2.2 实现 profile CRUD、按客户端作用域选择和缺失供应商校验
- [x] 2.3 实现 profile 预览 token、逐字段 diff、CAS 检查和备份清单
- [x] 2.4 实现 profile 应用、验证、失败回滚和官方 auth.json 不变断言
- [x] 2.5 接入 profile API 与前端档案选择/保存/复制/删除界面

## 3. Provider Doctor 2.0

- [x] 3.1 为诊断阶段和错误分类补充固定结构及脱敏/限长测试
- [x] 3.2 扩展配置、代理、认证、模型、请求、流式和工具能力探测
- [x] 3.3 新增第三方 Provider 健康注册表、连续失败阈值、冷却和半开恢复
- [x] 3.4 确认 Official 永不进入第三方故障转移，保留旧诊断字段兼容
- [x] 3.5 接入 Doctor 结果、健康标签、建议和显式写回确认 UI

## 4. 会话修复任务控制

- [x] 4.1 为修复任务增加预览摘要、取消标记、检查点、跳过/失败计数和心跳时间
- [x] 4.2 实现有界读取、每安全边界取消检查、单项失败隔离和幂等去重
- [x] 4.3 实现取消、恢复 API 与持久化脱敏任务元数据
- [x] 4.4 更新前端轮询、取消/恢复按钮、停滞提示和最终统计
- [x] 4.5 增加修复期间 config/provider/auth/MCP/plugin 不变回归测试（`repair_preserves_config_provider_auth_mcp_plugin_invariants`：与 job 线程同链 create_history_backup+sync_provider，config.toml/providers.json/auth.json/插件文件哈希零改动、MCP 段保留、rollout 已同步）

## 5. 集成与验证

- [x] 5.1 在隔离 CODEX_HOME 完成 profile 新建/预览/应用及 auth.json 边界 E2E（CAS 冲突拒绝、预览令牌一次性、host 失败回滚已有专项单测覆盖：`apply_rejects_and_keeps_state_when_config_changed_after_preview`、`apply_rolls_back_profile_state_when_host_fails`）
- [x] 5.2 使用 mock 与至少一个真实第三方供应商完成 Doctor 协议/认证/流式验证（真实供应商实测：可用供应商 config/proxy/auth/models/request 五阶段通过、401 失效供应商错误分类为 auth、连续 3 次失败熔断 open、输出无 key 泄露）
- [x] 5.3 完成会话修复取消、重启恢复、重复执行和损坏文件 E2E（600 会话合成库：20% 处取消→进程重启磁盘补载→resume 续跑 600/600 completed，rollout 与 catalog 数据层全部同步）
- [x] 5.4 运行 cargo fmt、cargo test、clippy、前端语法检查和 release build
- [x] 5.5 检查 diff、敏感信息、OpenSpec 严格验证并更新交付清单（全套测试受正在运行的 Cursor 进程影响，详见交付手册）
