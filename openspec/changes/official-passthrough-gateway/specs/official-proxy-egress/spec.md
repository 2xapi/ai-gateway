# official-proxy-egress 规格

## ADDED Requirements

### Requirement: 官方通道代理
系统 SHALL 提供「官方通道代理」设置（`official_proxy_url`），网关官方透传请求 SHALL 经该代理发出。

#### Scenario: 代理生效
- **WHEN** 设置了 `official_proxy_url` 且激活官方供应商
- **THEN** 官方透传连接经该代理建立

#### Scenario: 留空直连
- **WHEN** `official_proxy_url` 为空
- **THEN** 官方透传直连官方端点

#### Scenario: 非法值拒绝
- **WHEN** 输入非 http/https/socks5(s):// 代理地址
- **THEN** 保存被拒绝并提示

#### Scenario: 与第三方代理隔离
- **WHEN** 官方通道代理与某供应商独立代理同时设置
- **THEN** 官方透传仅用官方通道代理，第三方请求仅用该供应商代理，互不影响
