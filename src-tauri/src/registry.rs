//! 能力注册表骨架(超融合 A 线一期 §3,方案 v1.0):
//! 「一切能力=注册表条目」,挂载点四个(媒体解析/工具执行/协议转换/调度策略)永久冻结。
//! 一期=骨架 + kind=model 条目(探测标签的注册表视角);二期媒体关卡/工具执行开始消费。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const SECRET_MASK: &str = "••••••••";

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Model,
    Plugin,
    Tool,
}

impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::Model => "model",
            Kind::Plugin => "plugin",
            Kind::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    /// model 条目:归属供应商+模型(标签粒度对齐);plugin/tool 条目:二期契约
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub enabled: bool,
    pub meta: Map<String, Value>,
    /// v3 用户配置(配置页保存):{models:[{id,api,note}](优先级), failover:bool, values:{k:v}}
    pub config: Map<String, Value>,
    /// v3 来源:local|paste|remote|official(旧档读取按 builtin 推导)
    pub source: String,
    /// v3 最近变更(安装/配置/启停/更新),unix 秒
    pub updated_at: String,
}

fn registry_path(codex_home: &Path) -> PathBuf {
    codex_home.join("fusion-registry.json")
}

fn secrets_path(codex_home: &Path) -> PathBuf {
    codex_home.join("fusion-secrets.json")
}

fn write_private_json(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // 私密配置可能被多个 UI 请求同时更新，不能共享固定 tmp 路径。
    let tmp = parent.join(format!(
        ".{}.2xapi-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("secrets.json"),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    }
    #[cfg(windows)]
    if path.exists() {
        let old = parent.join(format!(
            ".{}.2xapi-{}.old",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("secrets.json"),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(path, &old)?;
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::rename(&old, path);
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        let _ = std::fs::remove_file(old);
    }
    #[cfg(windows)]
    if !path.exists() {
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    }
    #[cfg(not(windows))]
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_secrets(codex_home: &Path) -> Value {
    std::fs::read_to_string(secrets_path(codex_home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "version": 1, "plugins": {} }))
}

fn secret_plugins_mut(root: &mut Value) -> &mut Map<String, Value> {
    if !root.is_object() {
        *root = json!({ "version": 1, "plugins": {} });
    }
    let object = root.as_object_mut().expect("secret root normalized");
    object.entry("version").or_insert_with(|| json!(1));
    if !object.get("plugins").is_some_and(Value::is_object) {
        object.insert("plugins".into(), json!({}));
    }
    object
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .expect("secret plugins normalized")
}

pub fn get_plugin_secret(codex_home: &Path, plugin_id: &str, key: &str) -> Option<String> {
    load_secrets(codex_home)
        .get("plugins")
        .and_then(Value::as_object)
        .and_then(|plugins| plugins.get(plugin_id))
        .and_then(Value::as_object)
        .and_then(|entry| entry.get(key))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(String::from)
}

pub fn set_plugin_secret(
    codex_home: &Path,
    plugin_id: &str,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let mut root = load_secrets(codex_home);
    let plugins = secret_plugins_mut(&mut root);
    if value.is_empty() {
        if let Some(entry) = plugins.get_mut(plugin_id).and_then(|v| v.as_object_mut()) {
            entry.remove(key);
            if entry.is_empty() {
                plugins.remove(plugin_id);
            }
        }
    } else {
        plugins
            .entry(plugin_id.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(|entry| entry.insert(key.to_string(), json!(value)));
    }
    write_private_json(&secrets_path(codex_home), &root)
        .map_err(|e| format!("保存插件秘密失败: {e}"))
}

pub fn clear_plugin_secrets(codex_home: &Path, plugin_id: &str) {
    let mut root = load_secrets(codex_home);
    secret_plugins_mut(&mut root).remove(plugin_id);
    let _ = write_private_json(&secrets_path(codex_home), &root);
}

pub fn is_secret_config_key(entry: &Entry, key: &str) -> bool {
    if secret_key_name(key) {
        return true;
    }
    entry
        .meta
        .get("config")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| {
            item.get("k").and_then(Value::as_str) == Some(key)
                && item.get("type").and_then(Value::as_str) == Some("password")
        })
}

fn secret_key_name(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("key")
        || key.contains("token")
        || key.contains("password")
        || key.contains("secret")
}

fn secret_in_root<'a>(root: &'a Value, plugin_id: &str, key: &str) -> Option<&'a str> {
    root.get("plugins")?
        .as_object()?
        .get(plugin_id)?
        .as_object()?
        .get(key)?
        .as_str()
        .filter(|value| !value.is_empty())
}

fn update_secret_in_root(root: &mut Value, plugin_id: &str, key: &str, value: &str) {
    let plugins = secret_plugins_mut(root);
    if value.is_empty() {
        if let Some(entry) = plugins.get_mut(plugin_id).and_then(Value::as_object_mut) {
            entry.remove(key);
            if entry.is_empty() {
                plugins.remove(plugin_id);
            }
        }
    } else {
        plugins
            .entry(plugin_id.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(|entry| entry.insert(key.to_string(), json!(value)));
    }
}

fn sanitize_config_value(
    entry: &Entry,
    value: &mut Value,
    path: &str,
    secrets: &mut Value,
    found_secret: &mut bool,
) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            for key in object.keys().cloned().collect::<Vec<_>>() {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let is_values_field = path == "values" && is_secret_config_key(entry, &key);
                if secret_key_name(&key) || is_values_field {
                    *found_secret = true;
                    let storage_key = if path == "values" {
                        key.clone()
                    } else {
                        child_path.clone()
                    };
                    let submitted = object
                        .get(&key)
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("秘密配置 {child_path} 必须为字符串"))?;
                    let has_secret = if submitted == SECRET_MASK {
                        secret_in_root(secrets, &entry.id, &storage_key).is_some()
                    } else {
                        update_secret_in_root(secrets, &entry.id, &storage_key, submitted);
                        !submitted.is_empty()
                    };
                    object.insert(key, json!(if has_secret { SECRET_MASK } else { "" }));
                } else if let Some(child) = object.get_mut(&key) {
                    sanitize_config_value(entry, child, &child_path, secrets, found_secret)?;
                }
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter_mut().enumerate() {
                sanitize_config_value(
                    entry,
                    child,
                    &format!("{path}[{index}]"),
                    secrets,
                    found_secret,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn migrate_legacy_plugin_secrets(codex_home: &Path, entries: &mut [Entry]) -> bool {
    let mut secrets = load_secrets(codex_home);
    let mut pending = Vec::new();
    let mut registry_changed = false;

    for (entry_index, entry) in entries.iter_mut().enumerate() {
        if !matches!(entry.kind, Kind::Plugin | Kind::Tool) {
            continue;
        }
        let keys = entry
            .config
            .get("values")
            .and_then(Value::as_object)
            .map(|values| values.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for key in keys {
            if !is_secret_config_key(entry, &key) {
                continue;
            }
            let value = entry
                .config
                .get("values")
                .and_then(Value::as_object)
                .and_then(|values| values.get(&key))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let existing = secrets
                .get("plugins")
                .and_then(Value::as_object)
                .and_then(|plugins| plugins.get(&entry.id))
                .and_then(Value::as_object)
                .and_then(|plugin| plugin.get(&key))
                .and_then(Value::as_str)
                .filter(|secret| !secret.is_empty());
            if value == SECRET_MASK || value.is_empty() {
                continue;
            }
            if existing.is_some() {
                if let Some(values) = entry
                    .config
                    .get_mut("values")
                    .and_then(Value::as_object_mut)
                {
                    values.insert(key, json!(SECRET_MASK));
                    registry_changed = true;
                }
            } else {
                pending.push((entry_index, entry.id.clone(), key, value));
            }
        }
    }

    if !pending.is_empty() {
        {
            let plugins = secret_plugins_mut(&mut secrets);
            for (_, plugin_id, key, value) in &pending {
                plugins
                    .entry(plugin_id.clone())
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .map(|plugin| plugin.insert(key.clone(), json!(value)));
            }
        }
        if write_private_json(&secrets_path(codex_home), &secrets).is_ok() {
            for (entry_index, _, key, _) in pending {
                if let Some(values) = entries[entry_index]
                    .config
                    .get_mut("values")
                    .and_then(Value::as_object_mut)
                {
                    values.insert(key, json!(SECRET_MASK));
                    registry_changed = true;
                }
            }
        }
    }
    registry_changed
}

fn load(codex_home: &Path) -> Vec<Entry> {
    let raw = std::fs::read_to_string(registry_path(codex_home)).unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    let mut entries = v
        .get("entries")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(Entry {
                        id: e.get("id")?.as_str()?.to_string(),
                        kind: match e.get("kind")?.as_str()? {
                            "plugin" => Kind::Plugin,
                            "tool" => Kind::Tool,
                            _ => Kind::Model,
                        },
                        provider_id: e
                            .get("provider_id")
                            .and_then(|x| x.as_str())
                            .map(String::from),
                        model: e.get("model").and_then(|x| x.as_str()).map(String::from),
                        enabled: e.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
                        meta: e
                            .get("meta")
                            .and_then(|m| m.as_object())
                            .cloned()
                            .unwrap_or_default(),
                        config: e
                            .get("config")
                            .and_then(|c| c.as_object())
                            .cloned()
                            .unwrap_or_default(),
                        source: e
                            .get("source")
                            .and_then(|s| s.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| {
                                let builtin = e["meta"]
                                    .get("builtin")
                                    .and_then(|b| b.as_bool())
                                    .unwrap_or(false);
                                if builtin {
                                    "official".into()
                                } else {
                                    "remote".into()
                                }
                            }),
                        updated_at: e
                            .get("updated_at")
                            .and_then(|u| u.as_str())
                            .map(String::from)
                            .unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if migrate_legacy_plugin_secrets(codex_home, &mut entries) {
        save(codex_home, &entries);
    }
    entries
}

fn save(codex_home: &Path, entries: &[Entry]) -> bool {
    let path = registry_path(codex_home);
    let body = json!({
        "version": 1,
        "entries": entries.iter().map(|e| json!({
            "id": e.id, "kind": e.kind.as_str(),
            "provider_id": e.provider_id, "model": e.model,
            "enabled": e.enabled, "meta": e.meta,
            "config": e.config, "source": e.source, "updated_at": e.updated_at,
        })).collect::<Vec<_>>(),
    });
    write_private_json(&path, &body).is_ok()
}

/// upsert(plugin 条目):meta=manifest 全量;同 id 覆盖(重装/更新,保留用户 config)。
/// 新条目带默认配置(config def 种子化 + manifest models + failover 默认开)与 source。
pub fn upsert_plugin(codex_home: &Path, manifest: &Map<String, Value>) {
    let id = manifest
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return;
    }
    let mut entries = load(codex_home);
    let global = manifest
        .get("source_id")
        .and_then(|v| v.as_str())
        .map(|s| format!("{s}.{id}"))
        .unwrap_or(id); // 前缀命名(OpenWrt 吸收,市场源用;直接登记=无前缀
                        // 内置能力=tool 条目(本机实现);http 型=plugin 条目
    let kind = if manifest
        .get("builtin")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        Kind::Tool
    } else {
        Kind::Plugin
    };
    let source = manifest
        .get("source")
        .and_then(|v| v.as_str())
        .filter(|s| matches!(*s, "local" | "paste" | "remote" | "official"))
        .map(String::from)
        .unwrap_or_else(|| {
            if manifest
                .get("builtin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                "official".into()
            } else if manifest.get("source_id").and_then(|v| v.as_str()) == Some("local") {
                "local".into()
            } else {
                "remote".into()
            }
        });
    if let Some(e) = entries
        .iter_mut()
        .find(|e| e.id == global && e.kind == kind)
    {
        e.meta = manifest.clone();
        e.updated_at = now();
    } else {
        // 默认配置:manifest config 的 def 种子化 + models 声明 + failover 默认开
        let mut values = Map::new();
        if let Some(arr) = manifest.get("config").and_then(|c| c.as_array()) {
            for c in arr {
                if let (Some(k), Some(def)) = (c.get("k").and_then(|v| v.as_str()), c.get("def")) {
                    values.insert(k.to_string(), def.clone());
                }
            }
        }
        let mut config = Map::new();
        config.insert(
            "models".into(),
            manifest.get("models").cloned().unwrap_or_else(|| json!([])),
        );
        config.insert("failover".into(), json!(true));
        config.insert("values".into(), json!(values));
        entries.push(Entry {
            id: global,
            kind,
            provider_id: None,
            model: None,
            enabled: true,
            meta: manifest.clone(),
            config,
            source,
            updated_at: now(),
        });
    }
    save(codex_home, &entries);
}

pub fn get_plugin(codex_home: &Path, id: &str) -> Option<Entry> {
    load(codex_home)
        .into_iter()
        .find(|e| e.id == id && (e.kind == Kind::Plugin || e.kind == Kind::Tool))
}

pub fn remove(codex_home: &Path, id: &str) {
    let mut entries = load(codex_home);
    entries.retain(|e| e.id != id);
    save(codex_home, &entries);
    clear_plugin_secrets(codex_home, id);
}

pub fn set_enabled(codex_home: &Path, id: &str, enabled: bool) {
    let mut entries = load(codex_home);
    if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
        e.enabled = enabled;
        e.updated_at = now();
    }
    save(codex_home, &entries);
}

/// v3:保存插件用户配置(models 优先级/故障转移开关/配置项值);id 不存在返回 false。
pub fn set_config(codex_home: &Path, id: &str, config: Map<String, Value>) -> bool {
    let mut entries = load(codex_home);
    let Some(index) = entries.iter().position(|entry| entry.id == id) else {
        return false;
    };
    let entry = entries[index].clone();
    let mut config = Value::Object(config);
    let mut secrets = load_secrets(codex_home);
    let mut found_secret = false;
    if sanitize_config_value(&entry, &mut config, "", &mut secrets, &mut found_secret).is_err() {
        return false;
    }
    if found_secret && write_private_json(&secrets_path(codex_home), &secrets).is_err() {
        return false;
    }
    entries[index].config = config.as_object().cloned().unwrap_or_default();
    entries[index].updated_at = now();
    save(codex_home, &entries)
}

/// unix 秒时间戳(updated_at 用)。
pub fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

pub fn list_json(codex_home: &Path) -> Value {
    let entries = load(codex_home);
    json!({
        "entries": entries.iter().map(|e| json!({
            "id": e.id, "kind": e.kind.as_str(),
            "provider_id": e.provider_id, "model": e.model,
            "enabled": e.enabled, "meta": e.meta,
            "config": public_config(codex_home, e), "source": e.source, "updated_at": e.updated_at,
        })).collect::<Vec<_>>(),
    })
}

fn public_config(codex_home: &Path, entry: &Entry) -> Value {
    let mut config = entry.config.clone();
    if let Some(values) = config.get_mut("values").and_then(Value::as_object_mut) {
        for key in values.keys().cloned().collect::<Vec<_>>() {
            if is_secret_config_key(entry, &key) {
                let has_secret = get_plugin_secret(codex_home, &entry.id, &key).is_some()
                    || values
                        .get(&key)
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty());
                values.insert(key, json!(if has_secret { SECRET_MASK } else { "" }));
            }
        }
    }
    Value::Object(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_plugin_secrets_are_migrated_and_never_listed() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("2xapi-reg-secret-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let legacy = json!({
            "version": 1,
            "entries": [{
                "id": "local.secure-plugin",
                "kind": "plugin",
                "enabled": true,
                "meta": {
                    "config": [
                        {"k": "api_key", "type": "text"},
                        {"k": "region", "type": "text"}
                    ]
                },
                "config": {
                    "models": [],
                    "failover": true,
                    "values": {"api_key": "sk-legacy-secret", "region": "cn"}
                },
                "source": "local",
                "updated_at": ""
            }]
        });
        std::fs::write(
            registry_path(&root),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let listed = list_json(&root);
        assert_eq!(
            listed["entries"][0]["config"]["values"]["api_key"],
            SECRET_MASK
        );
        assert_eq!(
            get_plugin_secret(&root, "local.secure-plugin", "api_key").as_deref(),
            Some("sk-legacy-secret")
        );
        let registry_raw = std::fs::read_to_string(registry_path(&root)).unwrap();
        assert!(!registry_raw.contains("sk-legacy-secret"));
        assert!(registry_raw.contains(SECRET_MASK));
        let secrets_raw = std::fs::read_to_string(secrets_path(&root)).unwrap();
        assert!(secrets_raw.contains("sk-legacy-secret"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_config_immediately_moves_all_secret_fields_out_of_registry() {
        let root = std::env::temp_dir().join(format!(
            "2xapi-reg-set-secret-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let manifest: Map<String, Value> = serde_json::from_value(json!({
            "id": "secure-plugin",
            "name": "Secure",
            "version": "1.0.0",
            "mount": "tool_exec",
            "input": {},
            "output": {},
            "endpoint": "https://plugin.example",
            "config": [
                {"k": "apiKey", "type": "password"},
                {"k": "password", "type": "password"},
                {"k": "accessToken", "type": "password"},
                {"k": "clientSecret", "type": "password"},
                {"k": "region", "type": "text"}
            ]
        }))
        .unwrap();
        upsert_plugin(&root, &manifest);

        let config: Map<String, Value> = serde_json::from_value(json!({
            "models": [],
            "failover": true,
            "values": {
                "apiKey": "sk-direct-secret",
                "password": "plain-password",
                "accessToken": "plain-token",
                "clientSecret": "plain-client-secret",
                "region": "cn"
            },
            "nested": {"refreshToken": "nested-token"}
        }))
        .unwrap();
        assert!(set_config(&root, "secure-plugin", config));

        let registry_raw = std::fs::read_to_string(registry_path(&root)).unwrap();
        for secret in [
            "sk-direct-secret",
            "plain-password",
            "plain-token",
            "plain-client-secret",
            "nested-token",
        ] {
            assert!(
                !registry_raw.contains(secret),
                "fusion-registry.json 不得含明文秘密: {secret}"
            );
        }
        assert!(registry_raw.contains(SECRET_MASK));
        assert!(registry_raw.contains("\"region\": \"cn\""));
        assert_eq!(
            get_plugin_secret(&root, "secure-plugin", "apiKey").as_deref(),
            Some("sk-direct-secret")
        );
        assert_eq!(
            get_plugin_secret(&root, "secure-plugin", "nested.refreshToken").as_deref(),
            Some("nested-token")
        );
        let secrets_raw = std::fs::read_to_string(secrets_path(&root)).unwrap();
        assert!(secrets_raw.contains("plain-password"));
        assert!(secrets_raw.contains("plain-token"));
        assert!(secrets_raw.contains("plain-client-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(secrets_path(&root))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
