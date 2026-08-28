//! 多平台 agent 注册表(多平台接入方案 §2.1,A 阶段地基)
//!
//! A 阶段:元数据 + 白名单 + 泛化路由分发骨架;外部行为零变化(codex/claude 照旧)。
//! B 阶段起:每平台一个 adapter 模块挂载于此(registry 登记即可接入),
//! workbuddy 为第一个新平台 adapter(2026-08-16,叠加写双 models.json,见 workbuddy.rs)。
//!
//! 注册表即产品事实源:前端导航(D3 决策「A 后一次全亮,未实现标即将上线」)与
//! providers.rs 的 agent 白名单都从本表派生;pi 已裁撤(2026-08-16),不在表内。

pub mod claude_code;
pub mod claude_desktop;
pub mod cursor;
pub mod eco;
pub mod gemini;
pub mod grok;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
pub mod workbuddy;

use serde_json::{json, Value};
use std::path::Path;

/// 单个 agent 平台的元数据。
#[derive(Debug, Clone)]
pub struct AgentMeta {
    /// 平台标识(codex / claude / gemini / grokbuild / opencode / openclaw / hermes / claude-desktop)
    pub id: &'static str,
    /// 显示名
    pub name: &'static str,
    /// 导航提示文案
    pub tip: &'static str,
    /// 后端已并入(网关可服务该平台流量,供应商白名单据此放行)
    pub available: bool,
    /// 前端世界已交付(导航可点亮、可进世界;false = 导航灰标「即将上线」)。
    /// 后端先行合并而前端未交付的平台:available=true + frontend_ready=false。
    pub frontend_ready: bool,
    /// 对网关的消费协议(responses|chat|anthropic|gemini);未实现平台为规划值
    pub egress: &'static str,
    /// 托管形态:"config"=写配置文件 / "inject"=注入式启动 / ""=未定
    pub hosting: &'static str,
}

/// 全平台注册表(顺序即前端导航顺序)。
static REGISTRY: &[AgentMeta] = &[
    AgentMeta {
        id: "codex",
        name: "Codex",
        tip: "Codex",
        available: true,
        frontend_ready: true,
        egress: "responses",
        hosting: "config",
    },
    AgentMeta {
        id: "claude",
        name: "Claude Code",
        tip: "Claude Code",
        available: true,
        frontend_ready: true,
        egress: "anthropic",
        hosting: "config",
    },
    AgentMeta {
        id: "gemini",
        name: "Gemini CLI",
        tip: "Gemini CLI(生成协议转换)",
        available: true,
        frontend_ready: true,
        egress: "gemini",
        hosting: "config",
    },
    AgentMeta {
        id: "grokbuild",
        name: "Grok Build",
        tip: "Grok Build(TOML 托管)",
        available: true,
        frontend_ready: true,
        egress: "chat",
        hosting: "config",
    },
    AgentMeta {
        id: "opencode",
        name: "OpenCode",
        tip: "OpenCode(叠加条目)",
        available: true,
        frontend_ready: true,
        egress: "chat",
        hosting: "config",
    },
    AgentMeta {
        id: "openclaw",
        name: "OpenClaw",
        tip: "OpenClaw(叠加条目)",
        available: true,
        frontend_ready: true,
        egress: "anthropic",
        hosting: "config",
    },
    AgentMeta {
        id: "hermes",
        name: "Hermes",
        tip: "Hermes",
        available: true,
        frontend_ready: true,
        egress: "chat",
        hosting: "config",
    },
    AgentMeta {
        id: "claude-desktop",
        name: "Claude 桌面版",
        tip: "Claude Desktop(3p 网关)",
        available: true,
        frontend_ready: true,
        egress: "anthropic",
        hosting: "config",
    },
    AgentMeta {
        id: "workbuddy",
        name: "WorkBuddy",
        tip: "WorkBuddy / CodeBuddy",
        available: true,
        frontend_ready: true,
        egress: "chat",
        hosting: "config",
    },
    AgentMeta {
        id: "cursor",
        name: "Cursor",
        tip: "Cursor(vscdb 托管)",
        available: true,
        frontend_ready: true,
        egress: "chat",
        hosting: "config",
    },
];

/// 平台注册表迭代(registry_json 与测试共用同源)。
pub fn registry() -> impl Iterator<Item = &'static AgentMeta> {
    REGISTRY.iter()
}

/// 按 id 查找(大小写不敏感,与 providers.rs 归一化口径一致)。
pub fn find(id: &str) -> Option<&'static AgentMeta> {
    let norm = id.trim().to_ascii_lowercase();
    REGISTRY.iter().find(|m| m.id == norm)
}

/// GET /api/desktop/agents 响应体。
pub fn registry_json() -> Value {
    json!({
        "agents": registry()
            .map(|m| {
                json!({
                    "id": m.id,
                    "name": m.name,
                    "tip": m.tip,
                    "available": m.available,
                    "frontend_ready": m.frontend_ready,
                    "egress": m.egress,
                    "hosting": m.hosting,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// 整平台托管的 CLI 型 agent(hermes/opencode/openclaw)通用启动响应:
/// 配置已写盘、直接运行 CLI 即走网关,命令本体无需 env 前缀(与 grok 同族)。
/// providerId 仅回显(平台级托管不绑死单个供应商,hosting.providerId 为常量/整体态);
/// 托管态由各模块先验(未托管 409)。
pub fn cli_start_response(
    providers_path: &Path,
    provider_id: &str,
    command: &str,
) -> Result<Value, (u16, String, String)> {
    let provider_name = if provider_id.trim().is_empty() {
        String::new()
    } else {
        crate::providers::load(providers_path)
            .providers
            .iter()
            .find(|p| p.id == provider_id)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    Ok(json!({
        "command": command,
        "providerId": provider_id,
        "providerName": provider_name,
        "note": "配置已写盘,直接运行 CLI 即可",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表完整性:9 平台(pi 已裁撤不在内)、id 唯一。
    #[test]
    fn registry_has_nine_unique_platforms() {
        let all: Vec<&str> = registry().map(|m| m.id).collect();
        assert_eq!(all.len(), 10, "平台数应为 10(pi 已裁撤): {all:?}");
        let mut uniq = all.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), all.len(), "id 不得重复");
        assert!(!all.contains(&"pi"), "pi 已裁撤,不得出现在注册表");
    }

    /// 可用平台白名单(available=true 的 id;仅测试断言使用)。
    fn supported_ids() -> Vec<&'static str> {
        REGISTRY
            .iter()
            .filter(|m| m.available)
            .map(|m| m.id)
            .collect()
    }

    /// 白名单(available 语义=后端已并入;gemini/workbuddy 后端在 main 而前端未交付→available=true
    /// +frontend_ready=false,其前端批次交付时翻 frontend_ready,本断言不动)。顺序 = REGISTRY 声明序。
    #[test]
    fn supported_ids_are_backend_merged() {
        assert_eq!(
            supported_ids(),
            vec![
                "codex",
                "claude",
                "gemini",
                "grokbuild",
                "opencode",
                "openclaw",
                "hermes",
                "claude-desktop",
                "workbuddy",
                "cursor"
            ]
        );
    }

    /// 前端世界满编:十平台全部可点亮。
    #[test]
    fn frontend_ready_platforms() {
        let ready: Vec<&str> = registry()
            .filter(|m| m.frontend_ready)
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ready,
            vec![
                "codex",
                "claude",
                "gemini",
                "grokbuild",
                "opencode",
                "openclaw",
                "hermes",
                "claude-desktop",
                "workbuddy",
                "cursor"
            ]
        );
    }

    /// find 大小写不敏感;未注册 id 返回 None(泛化路由据此 404)。
    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(find("Claude").unwrap().id, "claude");
        assert_eq!(find(" Claude-Desktop ").unwrap().id, "claude-desktop");
        assert_eq!(find("Cursor").unwrap().id, "cursor");
        assert!(find("vscode").is_none());
    }

    /// registry_json 结构:agents 数组 10 项,每项带完整字段。
    #[test]
    fn registry_json_shape() {
        let v = registry_json();
        let arr = v["agents"].as_array().unwrap();
        assert_eq!(arr.len(), 10);
        for m in arr {
            assert!(m["id"].is_string());
            assert!(m["name"].is_string());
            assert!(m["available"].is_boolean());
            assert!(m["egress"].is_string());
        }
    }
}
