//! Codex 官方恢复/初始化：只做可预览、可恢复、可审计的精确路径操作。

use crate::{codex_overlay, codex_security};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct PreviewRecord {
    token: String,
    mode: String,
    config_path: PathBuf,
    config_sha256: Option<String>,
    // reset 不只移动 config.toml；catalog/sidecar 也必须绑定到预览时的
    // 指纹，避免外部管理器在预览后改写它们而被静默带走。
    artifacts: Vec<(PathBuf, Option<String>)>,
    expires_at: Instant,
}

static PREVIEW: OnceLock<Mutex<HashMap<String, PreviewRecord>>> = OnceLock::new();
// 同一进程内串行化 reset，避免两个请求同时通过同一个 preview token 后互相移动文件。
static APPLY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn preview_state() -> &'static Mutex<HashMap<String, PreviewRecord>> {
    PREVIEW.get_or_init(|| Mutex::new(HashMap::new()))
}

fn apply_lock() -> &'static Mutex<()> {
    APPLY_LOCK.get_or_init(|| Mutex::new(()))
}

fn external_manager_running() -> bool {
    let output = if cfg!(target_os = "windows") {
        Command::new("tasklist").output()
    } else {
        // 固定参数直接调用 pgrep；这里没有任何用户输入，不需要 shell。
        Command::new("pgrep")
            .args(["-af", "cc-switch|CC Switch"])
            .output()
    };
    let Ok(output) = output else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.contains("cc-switch") || text.contains("cc switch")
}

fn file_meta(path: &Path, include_hash: bool) -> Result<Value, String> {
    let metadata = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"path": path.to_string_lossy(), "exists": false}))
        }
        Err(e) => return Err(format!("读取 {} 元数据失败: {e}", path.display())),
    };
    let mut item = json!({"path": path.to_string_lossy(), "exists": true, "size": metadata.len()});
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        item["mode"] = json!(format!("{:04o}", metadata.permissions().mode() & 0o7777));
    }
    if include_hash {
        let fp = codex_overlay::fingerprint(path)?;
        item["sha256"] = json!(fp.sha256);
    }
    Ok(item)
}

pub fn preview(codex_home: &Path, config_path: &Path, mode: &str) -> Result<Value, String> {
    if !matches!(mode, "reset-config" | "reset-all") {
        return Err("E_BAD_RESET_MODE: 仅支持 reset-config 或 reset-all".into());
    }
    let config = codex_overlay::fingerprint(config_path)?;
    let catalog_path = codex_home.join("2xapi-model-catalog.json");
    let overlay_path = codex_home.join("config-backups/2xapi-codex-overlay-state.json");
    let catalog = codex_overlay::fingerprint(&catalog_path)?;
    let overlay = codex_overlay::fingerprint(&overlay_path)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    let record = PreviewRecord {
        token: token.clone(),
        mode: mode.into(),
        config_path: config_path.to_path_buf(),
        config_sha256: config.sha256.clone(),
        artifacts: vec![
            (catalog_path.clone(), catalog.sha256.clone()),
            (overlay_path.clone(), overlay.sha256.clone()),
        ],
        expires_at: Instant::now() + Duration::from_secs(600),
    };
    let mut previews = preview_state()
        .lock()
        .map_err(|_| "reset 预览锁已损坏".to_string())?;
    let now = Instant::now();
    previews.retain(|_, item| item.expires_at > now);
    previews.insert(token.clone(), record);
    let config_meta = file_meta(config_path, true)?;
    let catalog_meta = file_meta(&catalog_path, true)?;
    let overlay_meta = file_meta(&overlay_path, true)?;
    let auth_meta = file_meta(&codex_home.join("auth.json"), false)?;
    let mut planned = vec![
        config_meta.clone(),
        catalog_meta.clone(),
        overlay_meta.clone(),
    ];
    if mode == "reset-all" {
        planned.push(auth_meta.clone());
    }
    Ok(json!({
        "mode": mode,
        "codexHome": codex_home.to_string_lossy(),
        "previewToken": token,
        "expiresInSeconds": 600,
        "externalManagerActive": external_manager_running(),
        "config": config_meta,
        "catalog": catalog_meta,
        "overlay": overlay_meta,
        "auth": auth_meta,
        "plannedFiles": planned,
        "keyringRisk": mode == "reset-all",
        "preserved": ["sessions", "archived_sessions", "sqlite", "MCP", "plugins", "permissions"],
        "warning": if mode == "reset-all" { "将先调用官方 codex logout；keyring 只由官方 CLI 管理" } else { "保留官方登录；不保证物理生成新的 config.toml" },
    }))
}

fn quarantine_root(codex_home: &Path) -> Result<PathBuf, String> {
    let root = codex_home.join("2xapi-reset");
    if root.exists() {
        let metadata =
            fs::symlink_metadata(&root).map_err(|e| format!("读取 reset 隔离目录失败: {e}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("E_PATH_BOUNDARY: CODEX_HOME/2xapi-reset 必须是普通目录".into());
        }
        let canonical_home =
            fs::canonicalize(codex_home).map_err(|e| format!("解析 CODEX_HOME 失败: {e}"))?;
        let canonical_root =
            fs::canonicalize(&root).map_err(|e| format!("解析 reset 隔离目录失败: {e}"))?;
        if !canonical_root.starts_with(&canonical_home) {
            return Err("E_PATH_BOUNDARY: reset 隔离目录不在 CODEX_HOME 内".into());
        }
    }
    fs::create_dir_all(&root).map_err(|e| format!("创建 reset 隔离目录失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置 reset 目录权限失败: {e}"))?;
    }
    let dir = root.join(format!(
        "{}-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir(&dir).map_err(|e| format!("创建 reset 隔离批次失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置 reset 批次权限失败: {e}"))?;
    }
    Ok(dir)
}

fn move_exact(
    source: &Path,
    codex_home: &Path,
    quarantine: &Path,
    manifest: &mut Vec<Value>,
    moved: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    let canonical_home =
        fs::canonicalize(codex_home).map_err(|e| format!("解析 CODEX_HOME 失败: {e}"))?;
    let canonical_source =
        fs::canonicalize(source).map_err(|e| format!("解析 reset 源失败: {e}"))?;
    if !canonical_source.starts_with(&canonical_home) {
        return Err("E_PATH_BOUNDARY: reset 路径不在 CODEX_HOME 内".into());
    }
    let name = source
        .file_name()
        .ok_or_else(|| "reset 源缺少文件名".to_string())?;
    let target = quarantine.join(name);
    if target.exists() {
        return Err(format!("reset 目标已存在，拒绝覆盖 {}", target.display()));
    }
    let before = fs::metadata(source).map_err(|e| format!("读取 reset 源元数据失败: {e}"))?;
    let hash = if source.file_name().and_then(|v| v.to_str()) == Some("auth.json") {
        None
    } else {
        codex_overlay::fingerprint(source)?.sha256
    };
    fs::rename(source, &target).map_err(|e| format!("隔离移动 {} 失败: {e}", source.display()))?;
    moved.push((source.to_path_buf(), target.clone()));
    manifest.push(json!({ "source": source.to_string_lossy(), "target": target.to_string_lossy(), "size": before.len(), "sha256": hash }));
    Ok(())
}

fn rollback_moves(moved: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut failures = Vec::new();
    for (source, target) in moved.iter().rev() {
        if !target.exists() {
            continue;
        }
        if source.exists() {
            failures.push(format!("{} 已被外部重新创建", source.display()));
            continue;
        }
        if let Err(error) = fs::rename(target, source) {
            failures.push(format!("恢复 {} 失败: {error}", source.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

fn with_move_rollback(error: String, moved: &[(PathBuf, PathBuf)]) -> String {
    match rollback_moves(moved) {
        Ok(()) => error,
        Err(rollback) => format!("{error}；回滚失败: {rollback}"),
    }
}

fn verify_preview(token: &str, mode: &str, config_path: &Path) -> Result<(), String> {
    let guard = preview_state()
        .lock()
        .map_err(|_| "reset 预览锁已损坏".to_string())?;
    let record = guard
        .get(token)
        .ok_or_else(|| "E_RESET_PREVIEW_EXPIRED: 请重新预览".to_string())?;
    if record.token != token
        || record.mode != mode
        || record.config_path != config_path
        || record.expires_at <= Instant::now()
    {
        return Err("E_RESET_PREVIEW_EXPIRED: reset 预览已过期或目标已变化".into());
    }
    let current = codex_overlay::fingerprint(config_path)?;
    if current.sha256 != record.config_sha256 {
        return Err("E_CODEX_CONFIG_CHANGED: 预览后 config.toml 已变化，请重新预览".into());
    }
    for (path, expected_sha256) in &record.artifacts {
        let current = codex_overlay::fingerprint(path)?;
        if current.sha256 != *expected_sha256 {
            return Err(format!(
                "E_CODEX_RESET_ARTIFACT_CHANGED: 预览后 {} 已变化，请重新预览",
                path.display()
            ));
        }
    }
    Ok(())
}

pub fn apply(
    codex_home: &Path,
    config_path: &Path,
    mode: &str,
    token: &str,
    confirmed: bool,
) -> Result<Value, String> {
    if !confirmed {
        return Err("E_CONFIRM_REQUIRED: reset 必须二次确认".into());
    }
    let _apply_guard = apply_lock()
        .lock()
        .map_err(|_| "E_RESET_LOCK: reset 操作锁已损坏".to_string())?;
    verify_preview(token, mode, config_path)?;
    if external_manager_running() {
        return Err("E_EXTERNAL_CONFIG_MANAGER_ACTIVE: 请先退出 cc-switch 等外部配置管理器".into());
    }
    let quarantine = quarantine_root(codex_home)?;
    let mut manifest = Vec::new();
    let mut moved = Vec::new();
    let catalog = codex_home.join("2xapi-model-catalog.json");
    let overlay = codex_home.join("config-backups/2xapi-codex-overlay-state.json");
    let manifest_path = quarantine.join("manifest.json");
    // 先创建并设置清单权限，尽量把不可恢复的失败挡在 logout 之前。
    fs::write(&manifest_path, b"{\"version\":1,\"files\":[]}")
        .map_err(|e| format!("写入 reset 清单失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置 reset 清单权限失败: {e}"))?;
    }
    // reset-all 先移动配置文件，再执行官方 logout。这样配置移动失败时不会
    // 意外把用户踢出官方账号；logout 成功后仅隔离 auth.json，不读取其内容。
    let move_result = (|| {
        move_exact(
            config_path,
            codex_home,
            &quarantine,
            &mut manifest,
            &mut moved,
        )?;
        move_exact(&catalog, codex_home, &quarantine, &mut manifest, &mut moved)?;
        move_exact(&overlay, codex_home, &quarantine, &mut manifest, &mut moved)?;
        if mode == "reset-all" {
            codex_security::logout(codex_home)?;
            move_exact(
                &codex_home.join("auth.json"),
                codex_home,
                &quarantine,
                &mut manifest,
                &mut moved,
            )?;
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = move_result {
        return Err(with_move_rollback(error, &moved));
    }
    let raw = serde_json::to_vec_pretty(&json!({"version": 1, "mode": mode, "createdAt": chrono::Utc::now().to_rfc3339(), "files": manifest})).map_err(|e| e.to_string())?;
    if let Err(error) =
        fs::write(&manifest_path, raw).map_err(|e| format!("写入 reset 清单失败: {e}"))
    {
        return Err(with_move_rollback(error, &moved));
    }
    // reset 后 config.toml 被有意隔离，不能把读取缺失文件得到的默认值冒充为
    // 已验证的官方配置；真正的默认路由由 Codex 下次启动时解析。
    let effective_provider = if config_path.exists() {
        crate::config::read_toml(config_path)
            .get("model_provider")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    } else {
        None
    };
    // 二次确认 token 是一次性提交凭证；成功后立即作废，防止同一预览被重放。
    if let Ok(mut previews) = preview_state().lock() {
        previews.remove(token);
    }
    Ok(json!({
        "mode": mode,
        "stage": if mode == "reset-all" { "login_required" } else { "config_reset" },
        "quarantine": quarantine,
        "effectiveProvider": effective_provider,
        "defaultVerified": false,
        "verification": "deferred_to_codex_start",
        "login": codex_security::probe_login_cached(codex_home),
        "preserved": ["sessions", "archived_sessions", "sqlite", "MCP", "plugins", "permissions"]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preview_does_not_read_auth_contents() {
        let dir = std::env::temp_dir().join(format!("codex-reset-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let home = dir.join("codex");
        fs::create_dir_all(&home).unwrap();
        let config = home.join("config.toml");
        fs::write(&config, "model_provider = \"custom\"\n").unwrap();
        fs::write(home.join("auth.json"), "{\"tokens\":\"secret\"}").unwrap();
        let result = preview(&home, &config, "reset-config").unwrap();
        assert_eq!(result["auth"]["exists"], true);
        assert!(result.to_string().contains("reset-config"));
        assert!(!result.to_string().contains("secret"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_config_moves_only_config_artifacts_and_preserves_auth_and_history() {
        let dir = std::env::temp_dir().join(format!("codex-reset-apply-{}", uuid::Uuid::new_v4()));
        let home = dir.join("codex");
        fs::create_dir_all(home.join("sessions")).unwrap();
        let config = home.join("config.toml");
        fs::write(&config, "model_provider = \"2xapi_gateway\"\n").unwrap();
        let auth = home.join("auth.json");
        fs::write(&auth, "{\"tokens\":\"opaque\"}").unwrap();
        let preview_data = preview(&home, &config, "reset-config").unwrap();
        let preview_token = preview_data["previewToken"].as_str().unwrap().to_string();
        let result = apply(&home, &config, "reset-config", &preview_token, true).unwrap();
        assert_eq!(result["stage"].as_str(), Some("config_reset"));
        assert!(!config.exists());
        assert!(auth.exists());
        assert!(home.join("sessions").exists());
        assert!(result["quarantine"]
            .as_str()
            .unwrap()
            .contains("2xapi-reset"));
        let replay = apply(&home, &config, "reset-config", &preview_token, true).unwrap_err();
        assert!(replay.starts_with("E_RESET_PREVIEW_EXPIRED"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reset_rejects_catalog_changes_after_preview() {
        let dir = std::env::temp_dir().join(format!("codex-reset-cas-{}", uuid::Uuid::new_v4()));
        let home = dir.join("codex");
        fs::create_dir_all(home.join("config-backups")).unwrap();
        let config = home.join("config.toml");
        let catalog = home.join("2xapi-model-catalog.json");
        fs::write(&config, "model_provider = \"2xapi_gateway\"\n").unwrap();
        fs::write(&catalog, "{\"models\":[]}").unwrap();
        let preview_data = preview(&home, &config, "reset-config").unwrap();
        fs::write(&catalog, "{\"models\":[\"changed\"]}").unwrap();
        let error = apply(
            &home,
            &config,
            "reset-config",
            preview_data["previewToken"].as_str().unwrap(),
            true,
        )
        .unwrap_err();
        assert!(error.starts_with("E_CODEX_RESET_ARTIFACT_CHANGED"));
        assert!(config.exists());
        assert!(catalog.exists());
        let _ = fs::remove_dir_all(dir);
    }
}
