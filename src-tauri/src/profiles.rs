//! 版本化配置档案（P0）。档案只保存路由元数据，不保存任何凭据。

use crate::providers::{self, WireApi};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

const SCHEMA_VERSION: i64 = 1;
const PROFILE_FILE: &str = "2xapi-profiles.json";
const PREVIEW_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub agent: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_api: Option<WireApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accel_mode: Option<String>,
    #[serde(default)]
    pub eco_ids: Vec<String>,
    pub version: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileData {
    #[serde(default)]
    pub schema_version: i64,
    #[serde(default)]
    pub active_profiles: HashMap<String, String>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, Clone)]
struct Preview {
    profile_id: String,
    agent: String,
    config_hash: Option<String>,
    providers_hash: Option<String>,
    profile_version: u32,
    expires_at: i64,
}

static PROFILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static PREVIEWS: LazyLock<Mutex<HashMap<String, Preview>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn path(codex_home: &Path) -> PathBuf {
    codex_home.join(PROFILE_FILE)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn normalize_agent(raw: &str) -> Option<String> {
    let agent = raw.trim().to_ascii_lowercase();
    crate::agents::find(&agent)
        .filter(|meta| meta.available)
        .map(|_| agent)
}

fn sanitize_proxy(raw: Option<&str>) -> Option<String> {
    let value = raw.map(str::trim).filter(|value| !value.is_empty())?;
    let mut url = reqwest::Url::parse(value).ok()?;
    // profile 只保存路由地址，永远不落盘代理用户名/密码。
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url.to_string())
}

pub fn load(path: &Path) -> ProfileData {
    let mut data: ProfileData = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if data.schema_version == 0 {
        data.schema_version = SCHEMA_VERSION;
    }
    data.profiles
        .retain(|p| !p.id.trim().is_empty() && !p.name.trim().is_empty());
    let valid: std::collections::HashSet<String> =
        data.profiles.iter().map(|p| p.id.clone()).collect();
    data.active_profiles.retain(|_, id| valid.contains(id));
    data
}

fn save_atomic(path: &Path, data: &ProfileData) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "profile 文件缺少父目录".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建 profile 目录失败: {e}"))?;
    let raw = serde_json::to_vec_pretty(data).map_err(|e| format!("序列化 profile 失败: {e}"))?;
    let tmp = parent.join(format!(
        ".{}.2xapi-{}.tmp",
        PROFILE_FILE,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&tmp, raw).map_err(|e| format!("写入 profile 临时文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 profile 权限失败: {e}"))?;
    }
    #[cfg(windows)]
    if path.exists() {
        let old = parent.join(format!(
            ".{}.2xapi-old-{}.bak",
            PROFILE_FILE,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(path, &old).map_err(|e| format!("替换旧 profile 失败: {e}"))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::rename(&old, path);
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("替换 profile 失败: {e}"));
        }
        let _ = std::fs::remove_file(old);
        return Ok(());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("原子替换 profile 失败: {e}")
    })
}

fn valid_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=40).contains(&count) && !name.chars().any(|ch| ch == '\n' || ch == '\r')
}

fn profile_from_value(value: &Value, existing: Option<&Profile>) -> Result<Profile, String> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| existing.map(|p| p.name.as_str()))
        .unwrap_or("")
        .trim();
    if !valid_name(name) {
        return Err("profile 名称须为 1–40 个字符".into());
    }
    let agent_raw = value
        .get("agent")
        .and_then(Value::as_str)
        .or_else(|| existing.map(|p| p.agent.as_str()))
        .unwrap_or("codex");
    let agent = normalize_agent(agent_raw).ok_or_else(|| "不支持的客户端作用域".to_string())?;
    let provider_id = value
        .get("providerId")
        .or_else(|| value.get("provider_id"))
        .and_then(Value::as_str)
        .or_else(|| existing.map(|p| p.provider_id.as_str()))
        .unwrap_or("")
        .trim();
    if provider_id.is_empty() {
        return Err("profile 必须选择供应商".into());
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| existing.and_then(|p| p.model.clone()));
    let wire_value = value.get("wireApi").or_else(|| value.get("wire_api"));
    let wire_api = if let Some(raw) = wire_value {
        let raw = raw
            .as_str()
            .ok_or_else(|| "wireApi 必须是字符串".to_string())?;
        Some(WireApi::parse(raw).ok_or_else(|| "不支持的 wireApi".to_string())?)
    } else {
        existing.and_then(|p| p.wire_api)
    };
    let proxy_value = value.get("proxyUrl").or_else(|| value.get("proxy_url"));
    let proxy_url = if let Some(raw) = proxy_value {
        let raw = raw
            .as_str()
            .ok_or_else(|| "proxyUrl 必须是字符串".to_string())?;
        if raw.trim().is_empty() {
            None
        } else {
            Some(
                sanitize_proxy(Some(raw))
                    .ok_or_else(|| "proxyUrl 必须是合法 http(s)/socks5 地址".to_string())?,
            )
        }
    } else {
        existing.and_then(|p| p.proxy_url.clone())
    };
    let accel_value = value.get("accelMode").or_else(|| value.get("accel_mode"));
    let accel_mode = if let Some(raw) = accel_value {
        let mode = raw
            .as_str()
            .ok_or_else(|| "accelMode 必须是字符串".to_string())?
            .trim()
            .to_string();
        if matches!(mode.as_str(), "off" | "official" | "custom") {
            Some(mode)
        } else {
            return Err("不支持的 accelMode".into());
        }
    } else {
        existing.and_then(|p| p.accel_mode.clone())
    };
    let eco_ids = value
        .get("ecoIds")
        .or_else(|| value.get("eco_ids"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .take(100)
                .collect()
        })
        .or_else(|| existing.map(|p| p.eco_ids.clone()))
        .unwrap_or_default();
    let ts = now();
    Ok(Profile {
        id: existing
            .map(|p| p.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: name.to_string(),
        agent,
        provider_id: provider_id.to_string(),
        model,
        wire_api,
        proxy_url,
        accel_mode,
        eco_ids,
        version: existing.map(|p| p.version + 1).unwrap_or(1),
        created_at: existing.map(|p| p.created_at).unwrap_or(ts),
        updated_at: ts,
    })
}

pub fn list(path: &Path, agent: Option<&str>) -> ProfileData {
    let mut data = load(path);
    if let Some(agent) = agent.and_then(normalize_agent) {
        data.profiles.retain(|p| p.agent == agent);
    }
    data
}

pub fn create(path: &Path, value: &Value) -> Result<Profile, String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "profile 锁已损坏".to_string())?;
    let mut data = load(path);
    let profile = profile_from_value(value, None)?;
    if data
        .profiles
        .iter()
        .any(|p| p.agent == profile.agent && p.name == profile.name)
    {
        return Err("同一客户端下 profile 名称已存在".into());
    }
    data.profiles.push(profile.clone());
    save_atomic(path, &data)?;
    Ok(profile)
}

pub fn update(path: &Path, id: &str, value: &Value) -> Result<Profile, String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "profile 锁已损坏".to_string())?;
    let mut data = load(path);
    let idx = data
        .profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| "profile 不存在".to_string())?;
    let profile = profile_from_value(value, Some(&data.profiles[idx]))?;
    if data
        .profiles
        .iter()
        .enumerate()
        .any(|(i, p)| i != idx && p.agent == profile.agent && p.name == profile.name)
    {
        return Err("同一客户端下 profile 名称已存在".into());
    }
    data.profiles[idx] = profile.clone();
    save_atomic(path, &data)?;
    Ok(profile)
}

pub fn delete(path: &Path, id: &str) -> Result<(), String> {
    let _guard = PROFILE_LOCK
        .lock()
        .map_err(|_| "profile 锁已损坏".to_string())?;
    let mut data = load(path);
    let before = data.profiles.len();
    data.profiles.retain(|p| p.id != id);
    if before == data.profiles.len() {
        return Err("profile 不存在".into());
    }
    data.active_profiles.retain(|_, active| active != id);
    save_atomic(path, &data)
}

fn profile_json(profile: &Profile) -> Value {
    serde_json::to_value(profile).unwrap_or_else(|_| json!({}))
}

pub fn preview(
    codex_home: &Path,
    config_path: &Path,
    providers_path: &Path,
    value: &Value,
) -> Result<Value, String> {
    let data = load(&path(codex_home));
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let profile = if id.is_empty() {
        profile_from_value(value, None)?
    } else {
        data.profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| "profile 不存在".to_string())?
    };
    if profile.agent != "codex" {
        return Err("P0 profile 首期只支持 Codex 作用域".into());
    }
    let providers = providers::load(providers_path);
    let provider = providers
        .providers
        .iter()
        .find(|p| p.id == profile.provider_id)
        .ok_or_else(|| "profile 引用的供应商不存在".to_string())?;
    if provider.access_mode == providers::AccessMode::Official {
        return Err("官方供应商请使用官方连接入口，不创建第三方 profile".into());
    }
    if let Some(model) = profile.model.as_deref() {
        if model != provider.model {
            return Err("profile 模型与供应商默认模型不一致，请先更新供应商或 profile".into());
        }
    }
    let fp = crate::codex_overlay::fingerprint(config_path)?;
    let providers_fp = crate::codex_overlay::fingerprint(providers_path)?;
    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = now() + PREVIEW_TTL.as_secs() as i64;
    PREVIEWS
        .lock()
        .map_err(|_| "profile 预览锁已损坏".to_string())?
        .insert(
            token.clone(),
            Preview {
                profile_id: profile.id.clone(),
                agent: profile.agent.clone(),
                config_hash: fp.sha256.clone(),
                providers_hash: providers_fp.sha256.clone(),
                profile_version: profile.version,
                expires_at,
            },
        );
    let current = providers::get_active_for_agent(providers_path, "codex").map(|p| p.id);
    Ok(json!({
        "previewToken": token,
        "expiresAt": expires_at,
        "profile": profile_json(&profile),
        "diff": [{"field":"activeProviderId","from":current,"to":profile.provider_id}],
        "files": [
            {"path": config_path, "sha256": fp.sha256, "size": fp.size},
            {"path": providers_path, "sha256": providers_fp.sha256, "size": providers_fp.size}
        ],
        "backup": {"willCreate": true, "scope": "config + catalog + provider state"},
        "preserved": ["auth.json", "sessions", "rollouts", "MCP", "plugins", "permissions"],
        "officialAuth": "preserved"
    }))
}

pub fn apply(
    codex_home: &Path,
    config_path: &Path,
    backup_dir: &Path,
    providers_path: &Path,
    id: &str,
    token: &str,
    confirmed: bool,
) -> Result<Value, (u16, String, String)> {
    if !confirmed {
        return Err((
            400,
            "E_CONFIRM_REQUIRED".into(),
            "应用 profile 前需要确认预览".into(),
        ));
    }
    let preview = PREVIEWS
        .lock()
        .map_err(|_| {
            (
                500,
                "E_PROFILE_PREVIEW".into(),
                "profile 预览锁已损坏".into(),
            )
        })?
        .remove(token)
        .ok_or_else(|| {
            (
                409,
                "E_PROFILE_PREVIEW_EXPIRED".into(),
                "profile 预览不存在或已过期".into(),
            )
        })?;
    if preview.profile_id != id || preview.agent != "codex" || preview.expires_at < now() {
        return Err((
            409,
            "E_PROFILE_PREVIEW_EXPIRED".into(),
            "profile 预览已过期，请重新预览".into(),
        ));
    }
    let current = crate::codex_overlay::fingerprint(config_path)
        .map_err(|e| (400, "E_CODEX_CONFIG_PARSE".into(), e))?;
    if current.sha256 != preview.config_hash {
        return Err((
            409,
            "E_PROFILE_CONFIG_CHANGED".into(),
            "Codex 配置在预览后发生变化，请重新预览".into(),
        ));
    }
    let providers_current = crate::codex_overlay::fingerprint(providers_path)
        .map_err(|e| (400, "E_PROVIDERS_CONFIG_PARSE".into(), e))?;
    if providers_current.sha256 != preview.providers_hash {
        return Err((
            409,
            "E_PROFILE_PROVIDERS_CHANGED".into(),
            "供应商配置在预览后发生变化，请重新预览".into(),
        ));
    }
    let profile = load(&path(codex_home))
        .profiles
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| (404, "E_PROFILE_NOT_FOUND".into(), "profile 不存在".into()))?;
    if profile.version != preview.profile_version {
        return Err((
            409,
            "E_PROFILE_CHANGED".into(),
            "profile 在预览后发生变化，请重新预览".into(),
        ));
    }
    let before = load(&path(codex_home));
    let mut next = before.clone();
    next.active_profiles
        .insert("codex".into(), profile.id.clone());
    save_atomic(&path(codex_home), &next).map_err(|e| (500, "E_PROFILE_WRITE".into(), e))?;
    match crate::desktop::host(
        config_path,
        backup_dir,
        codex_home,
        providers_path,
        &profile.provider_id,
        "gateway",
    ) {
        Ok(result) => Ok(
            json!({"applied": true, "activeProfileId": profile.id, "hosting": result.get("hosting").cloned().unwrap_or(Value::Null), "rollback": {"available": true}}),
        ),
        Err((status, code, message)) => {
            let _ = save_atomic(&path(codex_home), &before);
            Err((status, code, message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn profile_round_trip_does_not_store_secrets() {
        let dir = tempdir().unwrap();
        let p = create(&path(dir.path()), &json!({"name":"开发","agent":"codex","providerId":"p1","proxyUrl":"http://user:pass@example.com:8080"})).unwrap();
        assert_eq!(p.proxy_url.as_deref(), Some("http://example.com:8080/"));
        let raw = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(!raw.contains("pass"));
        assert!(!raw.contains("api_key"));
    }

    #[test]
    fn duplicate_names_are_rejected_per_agent() {
        let dir = tempdir().unwrap();
        let file = path(dir.path());
        create(
            &file,
            &json!({"name":"开发","agent":"codex","providerId":"p1"}),
        )
        .unwrap();
        assert!(create(
            &file,
            &json!({"name":"开发","agent":"codex","providerId":"p2"})
        )
        .is_err());
        assert!(create(
            &file,
            &json!({"name":"开发","agent":"claude","providerId":"p2"})
        )
        .is_ok());
    }

    fn write_min_fixture(codex_home: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let config_path = codex_home.join("config.toml");
        let providers_path = codex_home.join("providers.json");
        std::fs::write(&config_path, "model = \"gpt-5\"\n").unwrap();
        std::fs::write(
            &providers_path,
            json!({
                "schema_version": 1,
                "providers": [{
                    "id": "p1", "name": "中转A", "agent": "codex",
                    "base_url": "https://example.com/v1", "api_key": "sk-test",
                    "access_mode": "mixed"
                }]
            })
            .to_string(),
        )
        .unwrap();
        (config_path, providers_path)
    }

    #[test]
    fn apply_rejects_and_keeps_state_when_config_changed_after_preview() {
        let dir = tempdir().unwrap();
        let codex_home = dir.path();
        let (config_path, providers_path) = write_min_fixture(codex_home);
        let profile = create(
            &path(codex_home),
            &json!({"name":"开发","agent":"codex","providerId":"p1"}),
        )
        .unwrap();
        let token = preview(
            codex_home,
            &config_path,
            &providers_path,
            &json!({"id": profile.id}),
        )
        .unwrap()["previewToken"]
            .as_str()
            .unwrap()
            .to_string();

        // 模拟预览后外部工具（如 cc-switch）或用户改动了 config.toml
        std::fs::write(
            &config_path,
            "model = \"gpt-5\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();

        let backup_dir = codex_home.join("config-backups");
        let err = apply(
            codex_home,
            &config_path,
            &backup_dir,
            &providers_path,
            &profile.id,
            &token,
            true,
        )
        .unwrap_err();
        assert_eq!(err.0, 409);
        assert_eq!(err.1, "E_PROFILE_CONFIG_CHANGED");
        let data = load(&path(codex_home));
        assert!(
            !data.active_profiles.contains_key("codex"),
            "CAS 冲突时不得改动 active_profiles"
        );

        // 预览令牌一次性：同一 token 二次使用必须失效
        let err2 = apply(
            codex_home,
            &config_path,
            &backup_dir,
            &providers_path,
            &profile.id,
            &token,
            true,
        )
        .unwrap_err();
        assert_eq!(err2.1, "E_PROFILE_PREVIEW_EXPIRED");
    }

    #[test]
    fn apply_rolls_back_profile_state_when_host_fails() {
        let dir = tempdir().unwrap();
        let codex_home = dir.path();
        let (config_path, providers_path) = write_min_fixture(codex_home);
        let profile = create(
            &path(codex_home),
            &json!({"name":"开发","agent":"codex","providerId":"p1"}),
        )
        .unwrap();
        let token = preview(
            codex_home,
            &config_path,
            &providers_path,
            &json!({"id": profile.id}),
        )
        .unwrap()["previewToken"]
            .as_str()
            .unwrap()
            .to_string();

        // backup_dir 指向普通文件 → host 写备份时确定性失败
        let blocker = codex_home.join("backup-blocker");
        std::fs::write(&blocker, "not a dir").unwrap();

        let err = apply(
            codex_home,
            &config_path,
            &blocker,
            &providers_path,
            &profile.id,
            &token,
            true,
        )
        .unwrap_err();
        assert_ne!(err.1, "E_PROFILE_CONFIG_CHANGED");
        assert_ne!(err.1, "E_PROFILE_PREVIEW_EXPIRED");
        let data = load(&path(codex_home));
        assert!(
            !data.active_profiles.contains_key("codex"),
            "host 失败后 active_profiles 必须回滚, 实际: {:?}",
            data.active_profiles
        );
    }
}
