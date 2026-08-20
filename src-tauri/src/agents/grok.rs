//! Grok Build adapter(接线层):泛化路由分支 ↔ grok_config 配置引擎。
//! 引擎(grok_config.rs)已含语法校验/官方态识别/凭据解析/托管写入与受控还原;
//! 本层只做 provider 解析(引擎吃 &Provider)与泛化路由形态对接。
//! 前端世界(「全部做好」批次通用世界)已点亮;state 契约对齐通用世界 UI 形态(hosting 键,同 opencode/openclaw/workbuddy)。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub type OpError = (u16, String, String);

/// live 配置路径:`<grok_home>/config.toml`(grok_home 由 AppState 注入;生产=默认 ~/.grok,测试=tempdir)。
pub fn config_path(grok_home: &Path) -> PathBuf {
    grok_home.join("config.toml")
}

/// 托管态(通用世界 UI 契约:hosting 键=null 或 {way, profile};前端 gwHosted 读 s.hosting)。
pub fn state(grok_home: &Path) -> Value {
    json!({ "hosting": crate::grok_config::detect_hosting(&config_path(grok_home)) })
}

fn find_provider(
    providers_path: &Path,
    provider_id: &str,
) -> Result<crate::providers::Provider, OpError> {
    let data = crate::providers::load(providers_path);
    let provider = data
        .providers
        .into_iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| {
            (
                404,
                "E_NO_PROVIDER".into(),
                format!("供应商不存在: {provider_id}"),
            )
        })?;
    crate::desktop::validate_provider_agent(&provider, "grokbuild")?;
    Ok(provider)
}

pub fn host(
    grok_home: &Path,
    backup_dir: &Path,
    providers_path: &Path,
    provider_id: &str,
    way: &str,
) -> Result<Value, OpError> {
    let provider = find_provider(providers_path, provider_id)?;
    let config = config_path(grok_home);
    let paths = [config.clone(), providers_path.to_path_buf()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let result = crate::grok_config::host(&config, backup_dir, &provider, way)?;
        crate::desktop::set_active_checked(providers_path, &provider, "grokbuild")?;
        Ok(result)
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

pub fn unhost(grok_home: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let config = config_path(grok_home);
    let providers_path = crate::desktop::providers_path_from_backup_dir(backup_dir);
    let paths = [config.clone(), providers_path.clone()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let result = crate::grok_config::unhost(&config, backup_dir)?;
        crate::desktop::clear_active_checked(&providers_path, "grokbuild")?;
        Ok(result)
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// 生成启动命令(对齐 gemini/workbuddy 的 start 形态;前端「⌘ 生成启动命令」按钮)。
/// Grok 与 gemini(env 注入式)不同:CLI 唯一配置入口即 ~/.grok/config.toml,托管态
/// 下直接运行 `grok` 即走网关——命令本体无需 env 前缀,真实 Key 只在网关(零 Key 契约)。
/// 非交互单发可用 `grok -p "<提示词>"`(真机 e2e 实证走 /grokbuild/responses 通路)。
pub fn start(
    providers_path: &Path,
    way: &str,
    provider_id: &str,
    grok_home: &Path,
) -> Result<Value, OpError> {
    if state(grok_home)["hosting"].is_null() {
        return Err((409, "E_NOT_HOSTED".into(), "请先托管,再启动".into()));
    }
    let p = if !provider_id.trim().is_empty() {
        find_provider(providers_path, provider_id)?
    } else {
        crate::providers::get_provider_for_agent(providers_path, "grokbuild").ok_or((
            503u16,
            "E_NO_GROK_PROVIDER".to_string(),
            "请先选择 Grok Build 供应商".to_string(),
        ))?
    };
    Ok(json!({
        "command": "grok -p \"你的问题\"",
        "way": way,
        "providerId": p.id,
        "providerName": p.name,
        "model": p.model,
        "hint": "托管配置已生效,直接运行 grok 即走中转;非交互单发可用:grok -p \"你的问题\"",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{AccessMode, ProviderInput, WireApi};

    fn sandbox(label: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("2xapi-grok-adapter-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let grok_home = root.join("grok");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&grok_home).unwrap();
        std::fs::create_dir_all(&backup_dir).unwrap();
        (grok_home, backup_dir, root.join("providers.json"))
    }

    fn create_provider(providers_path: &std::path::Path, name: &str) -> String {
        let p = crate::providers::create(
            providers_path,
            ProviderInput {
                name: name.into(),
                agent: "grokbuild".into(),
                base_url: "https://up.example.com".into(),
                api_key: "sk-test".into(),
                access_mode: AccessMode::PureApi,
                wire_api: WireApi::Responses,
                model: "grok-4.5".into(),
                sub2api_multiplier: 1.0,
                ..Default::default()
            },
        )
        .unwrap();
        p.id
    }

    #[test]
    fn start_requires_hosted_state() {
        let (grok_home, _bk, providers_path) = sandbox("start-409");
        let id = create_provider(&providers_path, "T");
        let err = start(&providers_path, "gateway", &id, &grok_home).unwrap_err();
        assert_eq!(err.0, 409);
        assert_eq!(err.1, "E_NOT_HOSTED");
    }

    #[test]
    fn start_gateway_returns_command_and_provider() {
        let (grok_home, bk, providers_path) = sandbox("start-gw");
        let id = create_provider(&providers_path, "T");
        host(&grok_home, &bk, &providers_path, &id, "gateway").unwrap();
        let out = start(&providers_path, "gateway", &id, &grok_home).unwrap();
        assert_eq!(out["command"], serde_json::json!("grok -p \"你的问题\""));
        assert_eq!(out["way"], serde_json::json!("gateway"));
        assert_eq!(out["providerId"], serde_json::json!(id));
        assert_eq!(out["model"], serde_json::json!("grok-4.5"));
        assert!(
            out["hint"].as_str().unwrap().contains("grok -p"),
            "hint 应带非交互单发提示: {out}"
        );
    }

    #[test]
    fn start_without_provider_id_resolves_by_agent() {
        let (grok_home, bk, providers_path) = sandbox("start-resolve");
        let id = create_provider(&providers_path, "T");
        host(&grok_home, &bk, &providers_path, &id, "gateway").unwrap();
        // 空 providerId → get_provider_for_agent(grokbuild) 兜底(唯一条目即选中)
        let out = start(&providers_path, "gateway", "", &grok_home).unwrap();
        assert_eq!(out["providerId"], serde_json::json!(id));
        // 无 grokbuild 供应商且空 id → 503 人话
        let (home2, _bk2, pp2) = sandbox("start-503");
        let err = start(&pp2, "gateway", "", &home2).unwrap_err();
        // home2 未托管,先撞 409;托管后再验 503 分支
        assert_eq!(err.1, "E_NOT_HOSTED");
        let id2 = create_provider(&pp2, "T2");
        host(&home2, &_bk2, &pp2, &id2, "gateway").unwrap();
        // 清空供应商库但保持托管态(配置已写盘),空 id 解析无门 → 503
        std::fs::remove_file(&pp2).unwrap();
        let err2 = start(&pp2, "gateway", "", &home2).unwrap_err();
        assert_eq!(err2.0, 503);
        assert_eq!(err2.1, "E_NO_GROK_PROVIDER");
    }

    #[test]
    fn start_unknown_provider_returns_404() {
        let (grok_home, bk, providers_path) = sandbox("start-404");
        let id = create_provider(&providers_path, "T");
        host(&grok_home, &bk, &providers_path, &id, "gateway").unwrap();
        let err = start(&providers_path, "gateway", "no-such-id", &grok_home).unwrap_err();
        assert_eq!(err.0, 404);
        assert_eq!(err.1, "E_NO_PROVIDER");
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let (grok_home, bk, providers_path) = sandbox("foreign-agent");
        let id = create_provider(&providers_path, "T");
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&providers_path).unwrap()).unwrap();
        data["providers"][0]["agent"] = json!("codex");
        std::fs::write(&providers_path, data.to_string()).unwrap();

        let error = host(&grok_home, &bk, &providers_path, &id, "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!config_path(&grok_home).exists());
    }

    #[test]
    fn unhost_clears_grok_active() {
        let (grok_home, bk, providers_path) = sandbox("unhost-active");
        let id = create_provider(&providers_path, "T");
        host(&grok_home, &bk, &providers_path, &id, "gateway").unwrap();
        assert!(crate::providers::get_active_for_agent(&providers_path, "grokbuild").is_some());

        unhost(&grok_home, &bk).unwrap();

        assert!(crate::providers::get_active_for_agent(&providers_path, "grokbuild").is_none());
    }
}
