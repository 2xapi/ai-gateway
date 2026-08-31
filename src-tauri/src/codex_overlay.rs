//! Codex `config.toml` 的保格式、零凭据 overlay 编辑器。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{table, value, DocumentMut};

pub const PROVIDER_ID: &str = "2xapi_gateway";
pub const PROVIDER_NAME: &str = "2xapi Gateway";
pub const OVERLAY_STATE_FILENAME: &str = "2xapi-codex-overlay-state.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileFingerprint {
    pub path: String,
    pub exists: bool,
    pub sha256: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OverlayField {
    pub present: bool,
    pub value: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OverlayState {
    pub version: u32,
    pub config_path: String,
    pub baseline_config_sha256: Option<String>,
    pub baseline: BTreeMap<String, OverlayField>,
    pub applied: BTreeMap<String, OverlayField>,
    pub applied_config_sha256: Option<String>,
    pub provider_id: String,
    pub catalog: Option<FileFingerprint>,
}

fn canonical_item(item: &toml_edit::Item) -> String {
    item.to_string().trim().to_string()
}

fn top_level_field(doc: &DocumentMut, key: &str) -> OverlayField {
    match doc.get(key) {
        Some(item) if !item.is_none() => OverlayField {
            present: true,
            value: Some(format!("{key} = {}", canonical_item(item))),
        },
        _ => OverlayField {
            present: false,
            value: None,
        },
    }
}

fn provider_field(doc: &DocumentMut) -> OverlayField {
    let item = doc
        .get("model_providers")
        .and_then(|value| value.as_table_like())
        .and_then(|table| table.get(PROVIDER_ID));
    match item {
        Some(item) if !item.is_none() => OverlayField {
            present: true,
            value: Some(canonical_item(item)),
        },
        _ => OverlayField {
            present: false,
            value: None,
        },
    }
}

pub fn overlay_state_path(backup_dir: &Path) -> PathBuf {
    backup_dir.join(OVERLAY_STATE_FILENAME)
}

pub fn capture_fields(path: &Path) -> Result<BTreeMap<String, OverlayField>, String> {
    let mut fields = BTreeMap::new();
    let doc = if path.exists() {
        read_doc(path)?
    } else {
        DocumentMut::new()
    };
    for key in ["model_provider", "model", "model_catalog_json", "openai_base_url"] {
        fields.insert(key.into(), top_level_field(&doc, key));
    }
    fields.insert(
        format!("model_providers.{PROVIDER_ID}"),
        provider_field(&doc),
    );
    Ok(fields)
}

pub fn read_overlay_state(path: &Path) -> Result<Option<OverlayState>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| format!("读取 Codex overlay sidecar 失败: {e}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取 Codex overlay sidecar 失败: {error}")),
    }
}

pub fn write_overlay_state(path: &Path, state: &OverlayState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "overlay sidecar 缺少父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("创建 overlay sidecar 目录失败: {e}"))?;
    // 使用同目录随机临时文件，避免并发请求或其他进程共用固定名称互相覆盖。
    let tmp = parent.join(format!(
        ".{}.2xapi-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("overlay-state"),
        uuid::Uuid::new_v4().simple()
    ));
    let raw = serde_json::to_vec_pretty(state)
        .map_err(|e| format!("序列化 overlay sidecar 失败: {e}"))?;
    fs::write(&tmp, raw).map_err(|e| format!("写入 overlay sidecar 临时文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&tmp);
            return Err(format!("设置 overlay sidecar 权限失败: {error}"));
        }
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Windows 不允许 rename 覆盖已有文件。先把旧 sidecar 移到随机备份，
            // 新文件安装失败时恢复，避免更新过程留下半写状态。
            let old = parent.join(format!(
                ".{}.2xapi-{}.old",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("overlay-state"),
                uuid::Uuid::new_v4().simple()
            ));
            fs::rename(path, &old).map_err(|e| format!("替换 overlay sidecar 失败: {e}"))?;
            match fs::rename(&tmp, path) {
                Ok(()) => {
                    let _ = fs::remove_file(old);
                    Ok(())
                }
                Err(replace_error) => {
                    let _ = fs::rename(&old, path);
                    let _ = fs::remove_file(&tmp);
                    Err(format!("替换 overlay sidecar 失败: {replace_error}"))
                }
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("原子替换 overlay sidecar 失败: {error}"))
        }
    }
}

pub fn record_applied_state(
    path: &Path,
    backup_dir: &Path,
    baseline: Option<OverlayState>,
    catalog_path: Option<&Path>,
) -> Result<OverlayState, String> {
    let before = fingerprint(path)?;
    let baseline_fields = baseline
        .as_ref()
        .map(|state| state.baseline.clone())
        .unwrap_or(capture_fields(path)?);
    let applied = capture_fields(path)?;
    let state = OverlayState {
        version: 1,
        config_path: path.to_string_lossy().into_owned(),
        baseline_config_sha256: baseline
            .as_ref()
            .and_then(|state| state.baseline_config_sha256.clone())
            .or(before.sha256.clone()),
        baseline: baseline_fields,
        applied,
        applied_config_sha256: before.sha256,
        provider_id: PROVIDER_ID.into(),
        catalog: catalog_path.map(fingerprint).transpose()?,
    };
    write_overlay_state(&overlay_state_path(backup_dir), &state)?;
    Ok(state)
}

pub fn new_baseline(path: &Path) -> Result<OverlayState, String> {
    let fp = fingerprint(path)?;
    let doc = if path.exists() {
        read_doc(path)?
    } else {
        DocumentMut::new()
    };
    let already_overlay =
        doc.get("model_provider").and_then(|item| item.as_str()) == Some(PROVIDER_ID);
    let mut baseline = capture_fields(path)?;
    if already_overlay {
        for field in baseline.values_mut() {
            *field = OverlayField {
                present: false,
                value: None,
            };
        }
    }
    Ok(OverlayState {
        version: 1,
        config_path: path.to_string_lossy().into_owned(),
        baseline_config_sha256: fp.sha256,
        baseline,
        applied: BTreeMap::new(),
        applied_config_sha256: None,
        provider_id: PROVIDER_ID.into(),
        catalog: None,
    })
}

pub fn set_item_from_text(doc: &mut DocumentMut, key: &str, raw: &str) -> Result<(), String> {
    let parsed = raw
        .parse::<DocumentMut>()
        .map_err(|e| format!("overlay sidecar 字段解析失败: {e}"))?;
    let item = parsed
        .get(key)
        .cloned()
        .ok_or_else(|| format!("overlay sidecar 缺少字段 {key}"))?;
    doc[key] = item;
    Ok(())
}

pub fn restore_owned_fields(
    path: &Path,
    sidecar_path: &Path,
    expected_sha256: Option<&str>,
) -> Result<Value, String> {
    let before = fingerprint(path)?;
    if expected_sha256 != before.sha256.as_deref() && expected_sha256.is_some() {
        return Err("E_CODEX_CONFIG_CHANGED: 预览后的 config.toml 已变化，请重新预览".into());
    }
    let Some(state) = read_overlay_state(sidecar_path)? else {
        return Err(
            "E_OVERLAY_STATE_MISSING: 没有可验证的 2xapi ownership sidecar，请使用官方默认恢复预览"
                .into(),
        );
    };
    if state.config_path != path.to_string_lossy() {
        return Err("E_OVERLAY_STATE_PATH_MISMATCH: sidecar 不属于当前 config.toml".into());
    }
    let mut doc = if before.exists {
        read_doc(path)?
    } else {
        DocumentMut::new()
    };
    let mut conflicts = Vec::new();
    for key in ["model_provider", "model", "model_catalog_json", "openai_base_url"] {
        let Some(applied) = state.applied.get(key) else {
            continue;
        };
        let current = top_level_field(&doc, key);
        let baseline = state.baseline.get(key).cloned().unwrap_or(OverlayField {
            present: false,
            value: None,
        });
        if current == *applied {
            if baseline.present {
                set_item_from_text(&mut doc, key, baseline.value.as_deref().unwrap_or(""))?;
            } else {
                doc.remove(key);
            }
        } else if current != baseline {
            conflicts.push(key.to_string());
        }
    }
    let key = format!("model_providers.{PROVIDER_ID}");
    let current_provider = provider_field(&doc);
    let applied_provider = state.applied.get(&key).cloned().unwrap_or(OverlayField {
        present: false,
        value: None,
    });
    let baseline_provider = state.baseline.get(&key).cloned().unwrap_or(OverlayField {
        present: false,
        value: None,
    });
    if current_provider == applied_provider {
        if baseline_provider.present {
            let raw = baseline_provider
                .value
                .as_deref()
                .unwrap_or("")
                .parse::<DocumentMut>()
                .map_err(|e| format!("overlay provider 恢复解析失败: {e}"))?;
            let item = raw
                .get("model_providers")
                .and_then(|value| value.as_table_like())
                .and_then(|table| table.get(PROVIDER_ID))
                .cloned()
                .ok_or_else(|| "overlay provider 基线缺失".to_string())?;
            doc["model_providers"][PROVIDER_ID] = item;
        } else if let Some(table) = doc
            .get_mut("model_providers")
            .and_then(|item| item.as_table_like_mut())
        {
            table.remove(PROVIDER_ID);
        }
    } else if current_provider != baseline_provider {
        conflicts.push(key);
    }
    if before.exists {
        let next = doc.to_string();
        let current =
            fs::read_to_string(path).map_err(|e| format!("读取 Codex config.toml 失败: {e}"))?;
        if next != current {
            write_atomic(path, next.as_bytes(), Some(&before))?;
        }
    }
    let after = fingerprint(path)?;
    Ok(
        json!({"changed": before.sha256 != after.sha256, "conflicts": conflicts, "fingerprint": after}),
    )
}

pub fn fingerprint(path: &Path) -> Result<FileFingerprint, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    let exists = path.exists();
    let sha256 = if exists {
        Some(hash_bytes(&bytes))
    } else {
        None
    };
    Ok(FileFingerprint {
        path: path.to_string_lossy().into_owned(),
        exists,
        sha256,
        size: bytes.len() as u64,
    })
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    hex::encode(digest.finalize())
}

fn read_doc(path: &Path) -> Result<DocumentMut, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("读取 Codex config.toml 失败: {e}"))?;
    raw.parse::<DocumentMut>()
        .map_err(|e| format!("E_CODEX_CONFIG_PARSE: {e}"))
}

fn ensure_provider(doc: &mut DocumentMut, base_url: &str, official_auth: bool) {
    let providers = doc["model_providers"].or_insert(table());
    let provider = providers[PROVIDER_ID].or_insert(table());
    provider["name"] = value(PROVIDER_NAME);
    provider["base_url"] = value(base_url);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(official_auth);
    // 每次 host 都移除旧 direct/认证字段，但不触碰其他 provider。
    if let Some(table) = provider.as_table_like_mut() {
        table.remove("env_key");
        table.remove("experimental_bearer_token");
        table.remove("auth");
    }
}

/// `official_auth`：官方登录在位（SignedIn）时为 true——CLI 携官方 Bearer 请求网关，
/// 激活官方条目即透传官方后端；未登录时 false，CLI 走纯 API 语义（零官方依赖）。
pub fn apply_gateway(
    path: &Path,
    base_url: &str,
    model: &str,
    catalog_path: &str,
    expected_sha256: Option<&str>,
    official_auth: bool,
) -> Result<FileFingerprint, String> {
    let before = fingerprint(path)?;
    if expected_sha256 != before.sha256.as_deref() && expected_sha256.is_some() {
        return Err("E_CODEX_CONFIG_CHANGED: 预览后的 config.toml 已变化，请重新预览".into());
    }
    let mut doc = if before.exists {
        read_doc(path)?
    } else {
        DocumentMut::new()
    };
    doc["model_provider"] = value(PROVIDER_ID);
    if !model.is_empty() {
        doc["model"] = value(model);
    }
    doc["model_catalog_json"] = value(catalog_path);
    // 官方 provider 的流量也引入网关(Codex++ 同款 openai_base_url 招法):
    // 旧会话按线程钉死 provider=openai,不指进来就会直连官方后端、撞官方套餐限额;
    // 进网关后由激活供应商转发,旧会话与官方登录并存。
    doc["openai_base_url"] = value(base_url);
    ensure_provider(&mut doc, base_url, official_auth);
    write_atomic(path, doc.to_string().as_bytes(), Some(&before))?;
    fingerprint(path)
}

fn write_atomic(
    path: &Path,
    data: &[u8],
    expected: Option<&FileFingerprint>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Codex config 缺少父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("创建 Codex config 目录失败: {e}"))?;
    if let Some(expected) = expected {
        ensure_unchanged(path, expected)?;
    }
    // 同目录随机临时文件，避免多请求/多进程共用固定名称互相覆盖。
    let tmp = parent.join(format!(
        ".{}.2xapi-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config"),
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&tmp, data).map_err(|e| format!("写入 Codex config 临时文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 Codex config 权限失败: {e}"))?;
    }
    // 写临时文件期间可能有外部管理器改写目标；提交前再次 CAS，拒绝覆盖新内容。
    if let Some(expected) = expected {
        if let Err(error) = ensure_unchanged(path, expected) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Windows 的 rename 不覆盖已有目标；先保留临时文件，完成一次受控替换。
            // 调用方已在此之前建立快照，失败仍可恢复原配置。
            fs::remove_file(path).map_err(|e| format!("替换 Codex config 目标失败: {e}"))?;
            fs::rename(&tmp, path).map_err(|e| format!("替换 Codex config 失败: {e}"))
        }
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("原子替换 Codex config 失败: {error}"))
        }
    }
}

fn ensure_unchanged(path: &Path, expected: &FileFingerprint) -> Result<(), String> {
    let current = fingerprint(path)?;
    if current.exists != expected.exists || current.sha256 != expected.sha256 {
        return Err(format!(
            "E_CODEX_CONFIG_CHANGED: {} 在写入期间发生变化，请重新预览",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preserves_comments_and_uncontrolled_provider() {
        let dir = std::env::temp_dir().join(format!("codex-overlay-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "# keep\nmodel_provider = \"openai\"\n\n[model_providers.custom]\nbase_url = \"https://user.example\"\n").unwrap();
        apply_gateway(
            &path,
            "http://127.0.0.1:8787",
            "gpt-test",
            "/tmp/catalog.json",
            None,
            false,
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# keep"));
        assert!(raw.contains("[model_providers.custom]"));
        assert!(raw.contains("2xapi_gateway"));
        assert!(!raw.contains("experimental_bearer_token"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn changed_hash_is_rejected() {
        let dir = std::env::temp_dir().join(format!("codex-overlay-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "model_provider = \"openai\"\n").unwrap();
        let hash = fingerprint(&path).unwrap().sha256.unwrap();
        fs::write(&path, "model_provider = \"custom\"\n").unwrap();
        assert!(apply_gateway(
            &path,
            "http://127.0.0.1:8787",
            "gpt-test",
            "catalog",
            Some(&hash),
            false
        )
        .is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ownership_sidecar_restores_only_owned_fields_and_preserves_user_changes() {
        let dir =
            std::env::temp_dir().join(format!("codex-overlay-sidecar-{}", uuid::Uuid::new_v4()));
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "# keep\nmodel_provider = \"openai\"\nmodel = \"user-model\"\n[model_providers.custom]\nbase_url = \"https://user.example\"\n").unwrap();
        let baseline = new_baseline(&path).unwrap();
        apply_gateway(
            &path,
            "http://127.0.0.1:8787",
            "gpt-test",
            "catalog",
            None,
            false,
        )
        .unwrap();
        record_applied_state(&path, &backups, Some(baseline), None).unwrap();
        let sidecar = overlay_state_path(&backups);
        let result = restore_owned_fields(&path, &sidecar, None).unwrap();
        assert!(result["changed"].as_bool().unwrap());
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("model_provider = \"openai\""));
        assert!(raw.contains("model = \"user-model\""));
        assert!(raw.contains("https://user.example"));
        assert!(!raw.contains(PROVIDER_ID));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ownership_sidecar_reports_concurrent_owned_path_conflict() {
        let dir =
            std::env::temp_dir().join(format!("codex-overlay-conflict-{}", uuid::Uuid::new_v4()));
        let backups = dir.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "model_provider = \"openai\"\n").unwrap();
        let baseline = new_baseline(&path).unwrap();
        apply_gateway(
            &path,
            "http://127.0.0.1:8787",
            "gpt-test",
            "catalog",
            None,
            false,
        )
        .unwrap();
        record_applied_state(&path, &backups, Some(baseline), None).unwrap();
        fs::write(&path, "model_provider = \"user-selected\"\n").unwrap();
        let result = restore_owned_fields(&path, &overlay_state_path(&backups), None).unwrap();
        assert!(result["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some("model_provider")));
        assert!(fs::read_to_string(&path).unwrap().contains("user-selected"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_toml_is_rejected_without_overwriting() {
        let dir =
            std::env::temp_dir().join(format!("codex-overlay-invalid-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "[broken\n";
        fs::write(&path, original).unwrap();
        assert!(apply_gateway(
            &path,
            "http://127.0.0.1:8787",
            "gpt-test",
            "catalog",
            None,
            false
        )
        .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let _ = fs::remove_dir_all(dir);
    }
}
