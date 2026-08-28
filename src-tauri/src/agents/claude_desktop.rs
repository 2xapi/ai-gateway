//! Claude Desktop adapter(阶段 D;调研报告 §七定案:第三方推理为官方原生功能)。
//!
//! 写入手法(cc-switch claude_desktop_config.rs 实证 + 本机 v1.30096.1 调研):
//! ① `…/Claude/claude_desktop_config.json` 顶层 deploymentMode="3p"(保留 mcpServers 等其余字段)
//! ② `…/Claude-3p/claude_desktop_config.json` 同
//! ③ `…/Claude-3p/configLibrary/<PROFILE_ID>.json` profile(bearer/gateway/base URL/Key)
//! ④ `…/Claude-3p/configLibrary/_meta.json` 登记 entries[].id + appliedId
//! 簿记:③④ 旁写私有 `2xapi-state.json` 记 host 前两处 deploymentMode 原值,unhost 按它
//! 恢复(原值本就是 3p 的用户——调研实证本机现状——保持不动)。
//!
//! 协议:Anthropic messages,走网关专属入口 `/{claude-desktop 的 anthropic 路径}`,per-agent
//! 取供应商,与 Claude Code 的 /anthropic/* 不串台。改配置后需重启 Claude Desktop 生效
//! (host 响应带 note)。Key 语义同先例:gateway=占位(真 Key 只在网关),direct=落盘 profile。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub type OpError = (u16, String, String);

const GATEWAY_BASE: &str = "http://127.0.0.1:8787";
const PLACEHOLDER_KEY: &str = "2xapi-gateway-managed";
/// 本产品固定 profile id(合法 hex UUID;与 cc-switch 的不同,避免互踩)。
pub const PROFILE_ID: &str = "2a0f1e5d-0000-4000-8000-0000000c0d3a";
const STATE_FILE: &str = "2xapi-state.json";
const PROFILE_NAME: &str = "2xapi";

/// Claude 主目录(`…/Claude`)与 3p 目录(`…/Claude-3p`)的公共父(Application Support 根;
/// 测试传 tempdir)。注:Windows 的 Claude Desktop 路径(APPDATA)未实证,首版 macOS 为主。
pub fn main_dir(cd_home: &Path) -> PathBuf {
    cd_home.join("Claude")
}
pub fn p3_dir(cd_home: &Path) -> PathBuf {
    cd_home.join("Claude-3p")
}
fn config_json(dir: &Path) -> PathBuf {
    dir.join("claude_desktop_config.json")
}
fn profile_path(cd_home: &Path) -> PathBuf {
    p3_dir(cd_home)
        .join("configLibrary")
        .join(format!("{PROFILE_ID}.json"))
}
fn meta_path(cd_home: &Path) -> PathBuf {
    p3_dir(cd_home).join("configLibrary").join("_meta.json")
}
fn state_path(cd_home: &Path) -> PathBuf {
    p3_dir(cd_home).join("configLibrary").join(STATE_FILE)
}

fn read_json_obj(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn read_json_obj_checked(path: &Path) -> Result<Map<String, Value>, OpError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| {
        (
            500,
            "E_IO".into(),
            format!("读取 {} 失败: {e}", path.display()),
        )
    })?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| {
        (
            422,
            "E_BAD_CONFIG".into(),
            format!("{} 不是合法 JSON: {e}", path.display()),
        )
    })?;
    value.as_object().cloned().ok_or_else(|| {
        (
            422,
            "E_BAD_CONFIG".into(),
            format!("{} 顶层必须是 JSON 对象", path.display()),
        )
    })
}

fn write_json(path: &Path, obj: &Map<String, Value>) -> Result<(), OpError> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(obj.clone()))
        .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    let tmp = path.with_extension("json.2xapi-tmp");
    std::fs::write(&tmp, text).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| (500, "E_IO".into(), e.to_string()))
}

fn find_provider(
    providers_path: &Path,
    provider_id: &str,
) -> Result<crate::providers::Provider, OpError> {
    let provider = crate::providers::load(providers_path)
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
    crate::desktop::validate_provider_agent(&provider, "claude-desktop")?;
    Ok(provider)
}

fn fallback_providers_path(cd_home: &Path) -> PathBuf {
    let home = if cfg!(target_os = "macos") || cfg!(windows) {
        cd_home.parent().and_then(Path::parent).unwrap_or(cd_home)
    } else {
        cd_home.parent().unwrap_or(cd_home)
    };
    home.join(".codex").join("providers.json")
}

fn providers_path_from_state(cd_home: &Path, state: &Map<String, Value>) -> PathBuf {
    state
        .get("providersPath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| fallback_providers_path(cd_home))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelRoute {
    pub role: &'static str,
    pub route_id: &'static str,
    pub upstream_model: String,
    pub label_override: String,
    pub supports_1m: bool,
}

const MODEL_ROLES: [(&str, &str); 4] = [
    ("sonnet", "claude-sonnet-5"),
    ("opus", "claude-opus-5"),
    ("fable", "claude-fable-5"),
    ("haiku", "claude-haiku-4-5"),
];

fn configured_route<'a>(
    provider: &'a crate::providers::Provider,
    role: &str,
) -> Option<&'a crate::providers::ClaudeDesktopModelRoute> {
    provider
        .claude_desktop_model_routes
        .iter()
        .find(|route| route.role.trim().eq_ignore_ascii_case(role))
}

pub fn resolved_model_routes(provider: &crate::providers::Provider) -> Vec<ResolvedModelRoute> {
    let fallback_model = configured_route(provider, "sonnet")
        .map(|route| route.model.trim())
        .filter(|model| !model.is_empty())
        .or_else(|| {
            MODEL_ROLES.iter().find_map(|(role, _)| {
                configured_route(provider, role)
                    .map(|route| route.model.trim())
                    .filter(|model| !model.is_empty())
            })
        })
        .or_else(|| {
            let model = provider.model.trim();
            (!model.is_empty()).then_some(model)
        })
        .or_else(|| {
            provider
                .models
                .iter()
                .map(|model| model.name.trim())
                .find(|model| !model.is_empty())
        })
        .unwrap_or_default()
        .to_string();

    MODEL_ROLES
        .iter()
        .map(|(role, route_id)| {
            let configured = configured_route(provider, role);
            let upstream_model = configured
                .map(|route| route.model.trim())
                .filter(|model| !model.is_empty())
                .unwrap_or(&fallback_model)
                .to_string();
            let label_override = configured
                .and_then(|route| route.label_override.as_deref())
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .unwrap_or(&upstream_model)
                .to_string();
            ResolvedModelRoute {
                role,
                route_id,
                upstream_model,
                label_override,
                supports_1m: configured.map(|route| route.supports_1m).unwrap_or(true),
            }
        })
        .collect()
}

pub fn map_request_model(
    provider: &crate::providers::Provider,
    requested_model: &str,
) -> Option<String> {
    let normalized = requested_model
        .trim()
        .to_ascii_lowercase()
        .strip_prefix("anthropic/")
        .unwrap_or(requested_model.trim())
        .to_ascii_lowercase();
    let role = MODEL_ROLES.iter().find_map(|(role, route_id)| {
        (normalized == *route_id || normalized.starts_with(&format!("claude-{role}-")))
            .then_some(*role)
    })?;
    resolved_model_routes(provider)
        .into_iter()
        .find(|route| route.role == role)
        .map(|route| route.upstream_model)
}

fn inference_models(provider: &crate::providers::Provider) -> Vec<Value> {
    resolved_model_routes(provider)
        .into_iter()
        .map(|route| {
            let mut model = json!({
                "name": route.route_id,
                "labelOverride": route.label_override,
            });
            if route.supports_1m {
                model["supports1m"] = json!(true);
            }
            model
        })
        .collect()
}

/// 托管态:profile 文件存在且 _meta.appliedId 指向我们。
pub fn state(cd_home: &Path) -> Value {
    let applied = read_json_obj(&meta_path(cd_home))
        .get("appliedId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let hosted = profile_path(cd_home).exists() && applied == PROFILE_ID;
    json!({
        "hosting": if hosted { json!({ "providerId": PROFILE_ID, "mode": "3p" }) } else { Value::Null },
        "deploymentMode": read_json_obj(&config_json(&main_dir(cd_home)))
            .get("deploymentMode")
            .cloned()
            .unwrap_or(Value::Null),
    })
}

pub fn host(
    cd_home: &Path,
    providers_path: &Path,
    provider_id: &str,
    way: &str,
) -> Result<Value, OpError> {
    if way != "gateway" && way != "direct" {
        return Err((
            400,
            "E_BAD_WAY".into(),
            "未知托管方式,仅支持 gateway / direct".into(),
        ));
    }
    let provider = find_provider(providers_path, provider_id)?;
    if provider.model.trim().is_empty() {
        return Err((
            422,
            "E_NO_MODEL".into(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".into(),
        ));
    }
    if way == "direct" && provider.base_url.trim().is_empty() {
        return Err((
            422,
            "E_NO_BASE_URL".into(),
            "该供应商未配置 API 地址".into(),
        ));
    }

    let main_cfg = config_json(&main_dir(cd_home));
    let p3_cfg = config_json(&p3_dir(cd_home));
    let mut main_obj = read_json_obj_checked(&main_cfg)?;
    let mut p3_obj = read_json_obj_checked(&p3_cfg)?;
    let mut meta = read_json_obj_checked(&meta_path(cd_home))?;
    let existing_state = if state_path(cd_home).exists() {
        Some(read_json_obj_checked(&state_path(cd_home))?)
    } else {
        None
    };
    let prev_main = main_obj
        .get("deploymentMode")
        .and_then(|v| v.as_str())
        .map(String::from);
    let prev_p3 = p3_obj
        .get("deploymentMode")
        .and_then(|v| v.as_str())
        .map(String::from);

    let (base_url, api_key, key_note) = if way == "gateway" {
        (
            format!("{GATEWAY_BASE}/claude-desktop"),
            PLACEHOLDER_KEY.to_string(),
            "占位(真实 Key 只在网关)",
        )
    } else {
        (
            provider.base_url.trim().trim_end_matches('/').to_string(),
            provider.api_key.clone(),
            "直连:真实 Key 写入 profile",
        )
    };
    let profile = json!({
        "coworkEgressAllowedHosts": ["*"],
        "disableDeploymentModeChooser": true,
        "inferenceGatewayApiKey": api_key,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceGatewayBaseUrl": base_url,
        "inferenceProvider": "gateway",
    });

    main_obj.insert("deploymentMode".into(), json!("3p"));
    p3_obj.insert("deploymentMode".into(), json!("3p"));

    let mut prof_obj = profile.as_object().cloned().unwrap_or_default();
    prof_obj.insert("inferenceModels".into(), json!(inference_models(&provider)));
    let entries = meta
        .entry("entries".to_string())
        .or_insert_with(|| json!([]));
    if !entries.is_array() {
        *entries = json!([]);
    }
    if let Some(arr) = entries.as_array_mut() {
        if let Some(entry) = arr
            .iter_mut()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(PROFILE_ID))
        {
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("name".into(), json!(PROFILE_NAME));
            }
        } else {
            arr.push(json!({ "id": PROFILE_ID, "name": PROFILE_NAME }));
        }
    }
    meta.insert("appliedId".into(), json!(PROFILE_ID));
    // 私有簿记只在首次 host 创建，重复 host/热切换不得覆盖首次还原基线。
    let state_obj = existing_state.unwrap_or_else(|| {
        let mut m = Map::new();
        if let Some(p) = &prev_main {
            m.insert("prevMain".into(), json!(p));
        }
        if let Some(p) = &prev_p3 {
            m.insert("prevP3".into(), json!(p));
        }
        m
    });
    let mut state_obj = state_obj;
    state_obj.insert(
        "providersPath".into(),
        json!(providers_path.to_string_lossy()),
    );

    let paths = [
        main_cfg.clone(),
        p3_cfg.clone(),
        profile_path(cd_home),
        meta_path(cd_home),
        state_path(cd_home),
        providers_path.to_path_buf(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let write_result = (|| {
        write_json(&main_cfg, &main_obj)?;
        write_json(&p3_cfg, &p3_obj)?;
        write_json(&profile_path(cd_home), &prof_obj)?;
        write_json(&meta_path(cd_home), &meta)?;
        write_json(&state_path(cd_home), &state_obj)?;
        crate::desktop::set_active_checked(providers_path, &provider, "claude-desktop")?;
        Ok::<(), OpError>(())
    })();
    if let Err(error) = write_result {
        return Err(crate::desktop::rollback_files(error, &snapshots));
    }

    Ok(json!({
        "hosted": true, "way": way,
        "profileId": PROFILE_ID,
        "keyNote": key_note,
        "note": "配置已写入;重启 Claude Desktop 后生效",
        "changed": { "mainConfig": true, "p3Config": true, "profile": true, "meta": true },
    }))
}

pub fn unhost(cd_home: &Path) -> Result<Value, OpError> {
    let bk = read_json_obj_checked(&state_path(cd_home))?;
    let providers_path = providers_path_from_state(cd_home, &bk);
    if !profile_path(cd_home).exists() {
        crate::desktop::clear_active_checked(&providers_path, "claude-desktop")?;
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }
    // 簿记恢复 deploymentMode(无簿记/原值缺失 → "1p",官方模式)
    let restore = |cfg: &Path, key: &str| -> Result<Map<String, Value>, OpError> {
        let mode = bk
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("1p")
            .to_string();
        let mut object = read_json_obj_checked(cfg)?;
        object.insert("deploymentMode".into(), json!(mode));
        Ok(object)
    };
    let main_cfg = config_json(&main_dir(cd_home));
    let p3_cfg = config_json(&p3_dir(cd_home));
    let main_obj = restore(&main_cfg, "prevMain")?;
    let p3_obj = restore(&p3_cfg, "prevP3")?;
    let mut meta = read_json_obj_checked(&meta_path(cd_home))?;
    if let Some(arr) = meta.get_mut("entries").and_then(|v| v.as_array_mut()) {
        arr.retain(|e| e.get("id").and_then(|v| v.as_str()) != Some(PROFILE_ID));
    }
    if meta.get("appliedId").and_then(|v| v.as_str()) == Some(PROFILE_ID) {
        meta.remove("appliedId");
    }
    let paths = [
        main_cfg.clone(),
        p3_cfg.clone(),
        profile_path(cd_home),
        meta_path(cd_home),
        state_path(cd_home),
        providers_path.clone(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let result = (|| {
        write_json(&main_cfg, &main_obj)?;
        write_json(&p3_cfg, &p3_obj)?;
        std::fs::remove_file(profile_path(cd_home))
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        write_json(&meta_path(cd_home), &meta)?;
        if state_path(cd_home).exists() {
            std::fs::remove_file(state_path(cd_home))
                .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        }
        crate::desktop::clear_active_checked(&providers_path, "claude-desktop")?;
        Ok::<(), OpError>(())
    })();
    if let Err(error) = result {
        return Err(crate::desktop::rollback_files(error, &snapshots));
    }

    Ok(json!({ "restored": true, "note": "已还原;重启 Claude Desktop 后生效" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(label: &str) -> (PathBuf, PathBuf, PathBuf, String) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "2xapi-claude-desktop-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cd_home = root.join("application-support");
        let providers_path = root.join("providers.json");
        let provider = crate::providers::create(
            &providers_path,
            crate::providers::ProviderInput {
                name: "Claude Desktop Test".into(),
                agent: "claude-desktop".into(),
                base_url: "https://upstream.example.com".into(),
                api_key: "sk-test".into(),
                model: "claude-test".into(),
                sub2api_multiplier: 1.0,
                ..Default::default()
            },
        )
        .unwrap();
        (root, cd_home, providers_path, provider.id)
    }

    fn write_raw(path: &Path, raw: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, raw).unwrap();
    }

    #[test]
    fn host_rejects_bad_json_without_overwriting_user_config() {
        let (root, cd_home, providers_path, provider_id) = fixture("bad-json");
        let main_cfg = config_json(&main_dir(&cd_home));
        let original = br#"{"deploymentMode":"1p""#;
        std::fs::create_dir_all(main_cfg.parent().unwrap()).unwrap();
        std::fs::write(&main_cfg, original).unwrap();

        let error = host(&cd_home, &providers_path, &provider_id, "gateway").unwrap_err();

        assert_eq!(error.1, "E_BAD_CONFIG");
        assert_eq!(std::fs::read(&main_cfg).unwrap(), original);
        assert!(!config_json(&p3_dir(&cd_home)).exists());
        assert!(!profile_path(&cd_home).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_writes_only_safe_role_models_to_profile() {
        let (root, cd_home, providers_path, provider_id) = fixture("all-models");
        let mut data = crate::providers::load(&providers_path);
        // 按目标 id 定位（load 内置官方 ChatGPT 条目，禁止依赖下标）
        let idx = data
            .providers
            .iter()
            .position(|p| p.id == provider_id)
            .unwrap();
        data.providers[idx].models = vec![
            crate::providers::ModelConfig {
                name: "claude-test".into(),
                ..Default::default()
            },
            crate::providers::ModelConfig {
                name: "claude-test-fast".into(),
                ..Default::default()
            },
            crate::providers::ModelConfig {
                name: "claude-test".into(),
                ..Default::default()
            },
        ];
        data.providers[idx].claude_desktop_model_routes = vec![
            crate::providers::ClaudeDesktopModelRoute {
                role: "sonnet".into(),
                model: "gpt-5.6".into(),
                label_override: Some("GPT 5.6".into()),
                supports_1m: true,
            },
            crate::providers::ClaudeDesktopModelRoute {
                role: "opus".into(),
                model: "gpt-5.6-sol".into(),
                label_override: None,
                supports_1m: false,
            },
        ];
        crate::providers::store(&providers_path, &data).unwrap();

        host(&cd_home, &providers_path, &provider_id, "gateway").unwrap();

        let profile: Value =
            serde_json::from_str(&std::fs::read_to_string(profile_path(&cd_home)).unwrap())
                .unwrap();
        let models = profile["inferenceModels"].as_array().unwrap();
        let names: Vec<&str> = models
            .iter()
            .map(|model| model["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5",
                "claude-haiku-4-5"
            ]
        );
        assert_eq!(models[0]["labelOverride"], "GPT 5.6");
        assert_eq!(models[1]["labelOverride"], "gpt-5.6-sol");
        assert!(models[1].get("supports1m").is_none());
        assert_eq!(models[2]["labelOverride"], "gpt-5.6");
        assert_eq!(models[3]["labelOverride"], "gpt-5.6");
        assert_eq!(models[3]["supports1m"], true);

        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(meta_path(&cd_home)).unwrap()).unwrap();
        let entry = meta["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == PROFILE_ID)
            .unwrap();
        assert_eq!(entry["name"], PROFILE_NAME);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_roles_fall_back_to_sonnet_or_first_configured_model() {
        let mut provider = crate::providers::Provider {
            model: "provider-default".into(),
            claude_desktop_model_routes: vec![crate::providers::ClaudeDesktopModelRoute {
                role: "fable".into(),
                model: "kimi-k2".into(),
                label_override: Some("Kimi".into()),
                supports_1m: true,
            }],
            ..Default::default()
        };

        let routes = resolved_model_routes(&provider);
        assert!(routes.iter().all(|route| route.upstream_model == "kimi-k2"));
        assert_eq!(routes[2].label_override, "Kimi");

        provider.claude_desktop_model_routes.insert(
            0,
            crate::providers::ClaudeDesktopModelRoute {
                role: "sonnet".into(),
                model: "gpt-5.6".into(),
                label_override: None,
                supports_1m: true,
            },
        );
        let routes = resolved_model_routes(&provider);
        assert_eq!(routes[0].upstream_model, "gpt-5.6");
        assert_eq!(routes[1].upstream_model, "gpt-5.6");
        assert_eq!(routes[2].upstream_model, "kimi-k2");
        assert_eq!(routes[3].upstream_model, "gpt-5.6");
    }

    #[test]
    fn request_model_mapping_accepts_role_aliases_and_ignores_actual_models() {
        let provider = crate::providers::Provider {
            model: "gpt-5.6".into(),
            ..Default::default()
        };

        assert_eq!(
            map_request_model(&provider, "claude-haiku-4-5"),
            Some("gpt-5.6".into())
        );
        assert_eq!(
            map_request_model(&provider, "anthropic/claude-opus-5-20260801"),
            Some("gpt-5.6".into())
        );
        assert_eq!(map_request_model(&provider, "gpt-5.6"), None);
    }

    #[test]
    fn repeated_host_preserves_first_restore_baseline() {
        let (root, cd_home, providers_path, provider_id) = fixture("repeat-host");
        let main_cfg = config_json(&main_dir(&cd_home));
        let p3_cfg = config_json(&p3_dir(&cd_home));
        write_raw(&main_cfg, r#"{"deploymentMode":"1p","keep":"main"}"#);
        write_raw(&p3_cfg, r#"{"deploymentMode":"custom","keep":"p3"}"#);

        host(&cd_home, &providers_path, &provider_id, "gateway").unwrap();
        let first_state = std::fs::read(state_path(&cd_home)).unwrap();
        host(&cd_home, &providers_path, &provider_id, "gateway").unwrap();
        assert_eq!(
            std::fs::read(state_path(&cd_home)).unwrap(),
            first_state,
            "重复 host 不得把已托管的 3p 覆盖为恢复基线"
        );

        unhost(&cd_home).unwrap();
        let main: Value =
            serde_json::from_str(&std::fs::read_to_string(main_cfg).unwrap()).unwrap();
        let p3: Value = serde_json::from_str(&std::fs::read_to_string(p3_cfg).unwrap()).unwrap();
        assert_eq!(main["deploymentMode"], "1p");
        assert_eq!(main["keep"], "main");
        assert_eq!(p3["deploymentMode"], "custom");
        assert_eq!(p3["keep"], "p3");
        assert!(!profile_path(&cd_home).exists());
        assert!(!state_path(&cd_home).exists());
        assert!(
            crate::providers::get_active_for_agent(&providers_path, "claude-desktop").is_none(),
            "unhost 必须清理 Claude Desktop active"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let (root, cd_home, providers_path, provider_id) = fixture("foreign-agent");
        let mut data: Value =
            serde_json::from_str(&std::fs::read_to_string(&providers_path).unwrap()).unwrap();
        // 按目标 id 定位（load 会内置官方 ChatGPT 条目，禁止依赖数组下标）
        let idx = data["providers"]
            .as_array()
            .unwrap()
            .iter()
            .position(|p| p["id"] == json!(provider_id))
            .unwrap();
        data["providers"][idx]["agent"] = json!("cursor");
        std::fs::write(&providers_path, data.to_string()).unwrap();

        let error = host(&cd_home, &providers_path, &provider_id, "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!config_json(&main_dir(&cd_home)).exists());
        assert!(!profile_path(&cd_home).exists());
        assert!(!state_path(&cd_home).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_rolls_back_earlier_writes_when_later_write_fails() {
        let (root, cd_home, providers_path, provider_id) = fixture("rollback");
        let main_cfg = config_json(&main_dir(&cd_home));
        let p3_cfg = config_json(&p3_dir(&cd_home));
        let original_main = r#"{"deploymentMode":"1p","keep":"main"}"#;
        let original_p3 = r#"{"deploymentMode":"custom","keep":"p3"}"#;
        write_raw(&main_cfg, original_main);
        write_raw(&p3_cfg, original_p3);
        let library_path = p3_dir(&cd_home).join("configLibrary");
        std::fs::write(&library_path, "block directory creation").unwrap();

        let error = host(&cd_home, &providers_path, &provider_id, "gateway").unwrap_err();

        assert_eq!(error.1, "E_IO");
        assert_eq!(std::fs::read_to_string(&main_cfg).unwrap(), original_main);
        assert_eq!(std::fs::read_to_string(&p3_cfg).unwrap(), original_p3);
        assert_eq!(
            std::fs::read_to_string(&library_path).unwrap(),
            "block directory creation"
        );
        assert!(
            crate::providers::get_active_for_agent(&providers_path, "claude-desktop").is_none(),
            "失败写入不得激活供应商"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
