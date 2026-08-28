//! Claude Code 配置托管。
//!
//! Claude Code 的网关字段通过 `~/.claude/settings.json` 的 `env` 段持久化，
//! 这是配置文件写入，不是 shell 环境注入；启用时不需要让用户复制命令或手动设置环境。
//! 该模块只托管网关模式，
//! 不把真实上游 API Key 写入本地配置。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use crate::desktop::OpError;
use crate::providers::Provider;

const GATEWAY_BASE_URL: &str = "http://127.0.0.1:8787/anthropic";
const GATEWAY_TOKEN: &str = "2xapi-gateway-managed";
const SNAPSHOT_FILE: &str = "claude-code-settings.snapshot.json";

const CONTROLLED_ENV_KEYS: [&str; 22] = [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
];

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    existed: bool,
    bytes_hex: Option<String>,
    #[serde(default)]
    mode: Option<u32>,
}

pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn snapshot_path(backup_dir: &Path) -> PathBuf {
    backup_dir.join(SNAPSHOT_FILE)
}

fn read_settings(path: &Path) -> Result<Value, OpError> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("读取 Claude Code 配置失败：{error}"),
        )
    })?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| {
        (
            422,
            "E_CLAUDE_CONFIG_PARSE".into(),
            format!("{} 不是合法 JSON，请先修复后再启用托管", path.display()),
        )
    })?;
    if !value.is_object() {
        return Err((
            422,
            "E_CLAUDE_CONFIG_PARSE".into(),
            format!("{} 必须是 JSON 对象，请先修复后再启用托管", path.display()),
        ));
    }
    Ok(value)
}

fn write_atomic(path: &Path, value: &Value, mode: Option<u32>) -> Result<(), OpError> {
    let parent = path.parent().ok_or((
        500,
        "E_CLAUDE_CONFIG_IO".into(),
        "Claude Code 配置目录无法确定".into(),
    ))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("创建 Claude Code 配置目录失败：{error}"),
        )
    })?;
    let temp = path.with_extension("2xapi.tmp");
    let content = serde_json::to_vec_pretty(value).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_SERIALIZE".into(),
            format!("生成 Claude Code 配置失败：{error}"),
        )
    })?;
    std::fs::write(&temp, [content.as_slice(), b"\n"].concat()).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("写入 Claude Code 临时配置失败：{error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let target_mode = mode.unwrap_or(0o600);
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(target_mode)).map_err(
            |error| {
                (
                    500,
                    "E_CLAUDE_CONFIG_IO".into(),
                    format!("设置 Claude Code 配置权限失败：{error}"),
                )
            },
        )?;
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("替换 Claude Code 配置失败：{error}"),
        )
    })
}

fn current_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn save_snapshot(path: &Path, backup_dir: &Path) -> Result<(), OpError> {
    let target = snapshot_path(backup_dir);
    if target.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(backup_dir).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("创建 Claude Code 备份目录失败：{error}"),
        )
    })?;
    let file_snapshot = crate::desktop::snapshot_file(path)?;
    let snapshot = Snapshot {
        existed: file_snapshot.bytes.is_some(),
        bytes_hex: file_snapshot.bytes.as_ref().map(hex::encode),
        mode: current_mode(path),
    };
    let bytes = serde_json::to_vec_pretty(&snapshot).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_SERIALIZE".into(),
            format!("生成 Claude Code 回滚快照失败：{error}"),
        )
    })?;
    let temp = target.with_extension("tmp");
    std::fs::write(&temp, [bytes.as_slice(), b"\n"].concat()).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("写入 Claude Code 回滚快照失败：{error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                (
                    500,
                    "E_CLAUDE_CONFIG_IO".into(),
                    format!("设置 Claude Code 回滚快照权限失败：{error}"),
                )
            },
        )?;
    }
    std::fs::rename(&temp, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("保存 Claude Code 回滚快照失败：{error}"),
        )
    })
}

fn restore_snapshot(path: &Path, backup_dir: &Path) -> Result<bool, OpError> {
    let snapshot_file = snapshot_path(backup_dir);
    if !snapshot_file.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(&snapshot_file).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("读取 Claude Code 回滚快照失败：{error}"),
        )
    })?;
    let snapshot: Snapshot = serde_json::from_str(&raw).map_err(|error| {
        (
            500,
            "E_CLAUDE_SNAPSHOT_PARSE".into(),
            format!("Claude Code 回滚快照损坏：{error}"),
        )
    })?;
    let bytes = if snapshot.existed {
        let encoded = snapshot.bytes_hex.ok_or((
            500,
            "E_CLAUDE_SNAPSHOT_PARSE".into(),
            "Claude Code 回滚快照缺少原始内容".into(),
        ))?;
        Some(hex::decode(encoded).map_err(|error| {
            (
                500,
                "E_CLAUDE_SNAPSHOT_PARSE".into(),
                format!("Claude Code 回滚快照内容损坏：{error}"),
            )
        })?)
    } else {
        None
    };
    #[cfg(unix)]
    let permissions = snapshot.mode.map(|mode| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    });
    #[cfg(not(unix))]
    let permissions = None;
    let file_snapshot = crate::desktop::FileSnapshot { bytes, permissions };
    crate::desktop::restore_file_snapshot(path, &file_snapshot).map_err(|message| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("恢复 Claude Code 配置失败：{message}"),
        )
    })?;
    std::fs::remove_file(snapshot_file).map_err(|error| {
        (
            500,
            "E_CLAUDE_CONFIG_IO".into(),
            format!("清理 Claude Code 回滚快照失败：{error}"),
        )
    })?;
    Ok(true)
}

fn model_candidates(provider: &Provider) -> Vec<(String, String)> {
    let mut values = Vec::new();
    let mut add = |model: &str, display: Option<&str>| {
        let model = model.trim();
        if model.is_empty() || values.iter().any(|(name, _)| name == model) {
            return;
        }
        values.push((
            model.to_string(),
            display
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(model)
                .to_string(),
        ));
    };
    add(&provider.model, None);
    for model in &provider.models {
        add(&model.name, model.display_name.as_deref());
    }
    values
}

fn managed_env(provider: &Provider) -> Map<String, Value> {
    let models = model_candidates(provider);
    let mut env = Map::new();
    env.insert("ANTHROPIC_BASE_URL".into(), json!(GATEWAY_BASE_URL));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(GATEWAY_TOKEN));
    env.insert("ANTHROPIC_MODEL".into(), json!(provider.model));
    env.insert(
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".into(),
        json!("1"),
    );
    env.insert(
        "CLAUDE_CODE_DISABLE_UNKNOWN_MODEL_WINDOW_ENFORCEMENT".into(),
        json!("1"),
    );
    env.insert(
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
        json!("0"),
    );
    let slots = [
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
        ),
        (
            "ANTHROPIC_DEFAULT_FABLE_MODEL",
            "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        ),
    ];
    for (index, (model_key, name_key)) in slots.into_iter().enumerate() {
        if let Some((model, display)) = models.get(index) {
            env.insert(model_key.into(), json!(model));
            env.insert(name_key.into(), json!(display));
        }
    }
    if let Some((model, display)) = models.get(4) {
        env.insert("ANTHROPIC_CUSTOM_MODEL_OPTION".into(), json!(model));
        env.insert("ANTHROPIC_CUSTOM_MODEL_OPTION_NAME".into(), json!(display));
        env.insert(
            "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION".into(),
            json!("2xapi 上游模型"),
        );
    }
    env
}

fn choose_provider(providers_path: &Path, provider_id: &str) -> Result<Provider, OpError> {
    let provider = if !provider_id.trim().is_empty() {
        crate::providers::load(providers_path)
            .providers
            .into_iter()
            .find(|provider| provider.id == provider_id)
            .ok_or((400, "E_NO_PROVIDER".into(), "供应商不存在".into()))?
    } else {
        crate::providers::get_provider_for_agent(providers_path, "claude").ok_or((
            503,
            "E_NO_CLAUDE_PROVIDER".into(),
            "请先选择 Claude 供应商".into(),
        ))?
    };
    if provider.agent != "claude" {
        return Err((
            400,
            "E_PROVIDER_AGENT_MISMATCH".into(),
            "所选供应商不属于 Claude Code 平台".into(),
        ));
    }
    if provider.api_key.trim().is_empty() {
        return Err((
            400,
            "E_NO_KEY".into(),
            "该 Claude 供应商缺少 api_key".into(),
        ));
    }
    Ok(provider)
}

fn apply_env(settings: &mut Value, provider: &Provider) -> Result<(), OpError> {
    let object = settings.as_object_mut().ok_or((
        422,
        "E_CLAUDE_CONFIG_PARSE".into(),
        "Claude Code 配置必须是 JSON 对象".into(),
    ))?;
    let env_value = object.entry("env").or_insert_with(|| json!({}));
    let env = env_value.as_object_mut().ok_or((
        422,
        "E_CLAUDE_CONFIG_PARSE".into(),
        "Claude Code settings.json 的 env 必须是 JSON 对象".into(),
    ))?;
    for key in CONTROLLED_ENV_KEYS {
        env.remove(key);
    }
    for (key, value) in managed_env(provider) {
        env.insert(key, value);
    }
    Ok(())
}

fn clear_managed_env(settings: &mut Value) -> Result<bool, OpError> {
    let object = settings.as_object_mut().ok_or((
        422,
        "E_CLAUDE_CONFIG_PARSE".into(),
        "Claude Code 配置必须是 JSON 对象".into(),
    ))?;
    let Some(env_value) = object.get_mut("env") else {
        return Ok(false);
    };
    let Some(env) = env_value.as_object_mut() else {
        return Err((
            422,
            "E_CLAUDE_CONFIG_PARSE".into(),
            "Claude Code settings.json 的 env 必须是 JSON 对象".into(),
        ));
    };
    let gateway_marked = env
        .get("ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .is_some_and(|value| value == GATEWAY_BASE_URL)
        && env
            .get("ANTHROPIC_AUTH_TOKEN")
            .and_then(Value::as_str)
            .is_some_and(|value| value == GATEWAY_TOKEN);
    if !gateway_marked {
        return Ok(false);
    }
    let before = env.len();
    for key in CONTROLLED_ENV_KEYS {
        env.remove(key);
    }
    Ok(env.len() != before)
}

pub fn host(
    home: &Path,
    backup_dir: &Path,
    providers_path: &Path,
    provider_id: &str,
    way: &str,
) -> Result<Value, OpError> {
    if !way.trim().is_empty() && way.trim() != "gateway" {
        return Err((
            400,
            "E_CLAUDE_DIRECT_UNSUPPORTED".into(),
            "Claude Code 配置托管目前仅支持网关方式，请切换到网关后再启用".into(),
        ));
    }
    let provider = choose_provider(providers_path, provider_id)?;
    let path = settings_path(home);
    let config_snapshot = crate::desktop::snapshot_file(&path)?;
    let providers_snapshot = crate::desktop::snapshot_file(providers_path)?;
    let had_persistent_snapshot = snapshot_path(backup_dir).exists();
    let mut settings = read_settings(&path)?;
    save_snapshot(&path, backup_dir)?;
    let result = apply_env(&mut settings, &provider)
        .and_then(|()| write_atomic(&path, &settings, current_mode(&path)))
        .and_then(|()| {
            crate::providers::set_active_for_agent(providers_path, &provider.id).map_err(
                |message| {
                    (
                        500,
                        "E_PROVIDER_ACTIVE_SAVE".into(),
                        format!("无法保存 Claude 当前供应商：{message}"),
                    )
                },
            )
        });
    match result {
        Ok(()) => Ok(json!({
            "hosted": true,
            "way": "gateway",
            "configPath": path.to_string_lossy(),
            "providerId": provider.id,
            "providerName": provider.name,
            "model": provider.model,
            "modelOptions": model_candidates(&provider).into_iter().take(5).map(|(model, display_name)| json!({"model": model, "displayName": display_name})).collect::<Vec<_>>(),
        })),
        Err(error) => {
            let error = crate::desktop::rollback_files(
                error,
                &[
                    (path.clone(), config_snapshot),
                    (providers_path.to_path_buf(), providers_snapshot),
                ],
            );
            if !had_persistent_snapshot {
                let _ = std::fs::remove_file(snapshot_path(backup_dir));
            }
            Err(error)
        }
    }
}

pub fn unhost(home: &Path, backup_dir: &Path, providers_path: &Path) -> Result<Value, OpError> {
    let path = settings_path(home);
    let restored = restore_snapshot(&path, backup_dir)?;
    let cleared = if restored {
        false
    } else if path.exists() {
        let mut settings = read_settings(&path)?;
        let changed = clear_managed_env(&mut settings)?;
        if changed {
            write_atomic(&path, &settings, current_mode(&path))?;
        }
        changed
    } else {
        false
    };
    if restored || cleared {
        crate::providers::clear_active_for_agent(providers_path, "claude");
    }
    Ok(json!({
        "restored": restored,
        "alreadyClean": !restored && !cleared,
        "configPath": path.to_string_lossy(),
    }))
}

pub fn state(home: &Path, backup_dir: &Path, providers_path: &Path) -> Value {
    let hosted = snapshot_path(backup_dir).exists();
    let provider = if hosted {
        crate::providers::get_provider_for_agent(providers_path, "claude")
    } else {
        None
    };
    json!({
        "hosted": hosted,
        "providerId": provider.as_ref().map(|item| item.id.clone()),
        "providerName": provider.as_ref().map(|item| item.name.clone()),
        "model": provider.as_ref().map(|item| item.model.clone()),
        "way": if hosted { "gateway" } else { "" },
        "configPath": settings_path(home).to_string_lossy(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{AccessMode, ProviderData};

    fn provider() -> Provider {
        Provider {
            id: "claude-1".into(),
            name: "Claude".into(),
            agent: "claude".into(),
            base_url: "https://upstream.invalid".into(),
            api_key: "sk-secret".into(),
            access_mode: AccessMode::PureApi,
            model: "gpt-5.6".into(),
            ..Default::default()
        }
    }

    fn write_provider(path: &Path) {
        std::fs::write(
            path,
            serde_json::to_vec(&ProviderData {
                schema_version: 1,
                active_provider_id: None,
                active_provider_ids: Default::default(),
                providers: vec![provider()],
            })
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn host_preserves_settings_and_unhost_restores_original() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);
        let original = json!({"theme":"dark","env":{"CUSTOM":"keep"}});
        std::fs::write(settings_path(&home), serde_json::to_vec(&original).unwrap()).unwrap();
        let hosted = host(&home, &backup, &providers, "claude-1", "gateway").unwrap();
        assert_eq!(hosted["hosted"], true);
        let current: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(current["theme"], "dark");
        assert_eq!(current["env"]["CUSTOM"], "keep");
        assert_eq!(current["env"]["ANTHROPIC_MODEL"], "gpt-5.6");
        assert!(!current.to_string().contains("sk-secret"));
        assert_eq!(
            host(&home, &backup, &providers, "claude-1", "gateway").unwrap()["hosted"],
            true
        );
        assert_eq!(
            unhost(&home, &backup, &providers).unwrap()["restored"],
            true
        );
        let restored: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(restored, original);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_settings_is_not_overwritten() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);
        std::fs::write(settings_path(&home), b"{broken").unwrap();
        let error = host(&home, &backup, &providers, "claude-1", "gateway").unwrap_err();
        assert_eq!(error.1, "E_CLAUDE_CONFIG_PARSE");
        assert_eq!(std::fs::read(settings_path(&home)).unwrap(), b"{broken");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_host_is_rejected_without_writes() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);

        let error = host(&home, &backup, &providers, "claude-1", "direct").unwrap_err();

        assert_eq!(error.1, "E_CLAUDE_DIRECT_UNSUPPORTED");
        assert!(!settings_path(&home).exists());
        assert!(!backup.exists());
        assert!(crate::providers::get_active_for_agent(&providers, "claude").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unhost_restores_exact_bytes_and_clears_active_provider() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);
        let original = b"{\n  \"theme\": \"dark\",\n  \"env\": {\"CUSTOM\": \"keep\"}\n}\n";
        std::fs::write(settings_path(&home), original).unwrap();

        host(&home, &backup, &providers, "claude-1", "gateway").unwrap();
        assert!(crate::providers::get_active_for_agent(&providers, "claude").is_some());
        let result = unhost(&home, &backup, &providers).unwrap();

        assert_eq!(result["restored"], true);
        assert_eq!(std::fs::read(settings_path(&home)).unwrap(), original);
        assert!(crate::providers::get_active_for_agent(&providers, "claude").is_none());
        assert!(!backup.join(SNAPSHOT_FILE).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unhost_clears_managed_env_without_snapshot() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);
        let settings = json!({
            "theme": "dark",
            "env": {
                "CUSTOM": "keep",
                "ANTHROPIC_BASE_URL": GATEWAY_BASE_URL,
                "ANTHROPIC_AUTH_TOKEN": GATEWAY_TOKEN,
                "ANTHROPIC_MODEL": "gpt-5.6"
            }
        });
        std::fs::write(
            settings_path(&home),
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();
        crate::providers::set_active_for_agent(&providers, "claude-1").unwrap();

        let result = unhost(&home, &backup, &providers).unwrap();
        assert_eq!(result["restored"], false);
        assert_eq!(result["alreadyClean"], false);
        let cleaned: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(cleaned["theme"], "dark");
        assert_eq!(cleaned["env"]["CUSTOM"], "keep");
        assert!(cleaned["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(crate::providers::get_active_for_agent(&providers, "claude").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unhost_removes_settings_created_by_host() {
        let root = std::env::temp_dir().join(format!("2xapi-claude-code-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        write_provider(&providers);

        host(&home, &backup, &providers, "claude-1", "gateway").unwrap();
        assert!(settings_path(&home).exists());
        unhost(&home, &backup, &providers).unwrap();

        assert!(!settings_path(&home).exists());
        assert!(crate::providers::get_active_for_agent(&providers, "claude").is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
