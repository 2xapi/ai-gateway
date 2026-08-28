//! 桌面版托管开关(阶段 1,开发任务书 §1.1)。
//!
//! 桌面版 ChatGPT.app 无法注入 env/参数，唯一配置入口是 `~/.codex/config.toml`。
//! 「托管」= 保格式合并写入独占的 `[model_providers.2xapi_gateway]`，指向本机网关 8787。
//! 官方 CLI 的 file/keyring 凭据由本模块完全只读；网关托管不读取、不创建、不改写 `auth.json`。
//!
//! 与 config.rs(M2)的关系:复用其 toml 读写/备份/catalog 原语,但合并逻辑独立——
//! M2 的 Mixed/旧 direct 仅作为 legacy 状态报告，不会被新版本自动采信或恢复。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::{io::Write, process::Stdio};

use crate::config::{
    backup_file, build_model_catalog, read_toml, GATEWAY_BASE_URL, MODEL_CATALOG_FILENAME,
};
use crate::providers::Provider;

pub const GATEWAY_ADDR: &str = "127.0.0.1:8787";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// host/unhost 的错误:(HTTP 状态码, 错误码, 人话信息)。handler 层转 {"error": code, "message": msg}。
pub type OpError = (u16, String, String);

#[derive(Clone)]
pub(crate) struct FileSnapshot {
    pub(crate) bytes: Option<Vec<u8>>,
    pub(crate) permissions: Option<std::fs::Permissions>,
}

pub(crate) fn snapshot_file(path: &Path) -> Result<FileSnapshot, OpError> {
    let (bytes, permissions) = if path.exists() {
        let bytes = std::fs::read(path).map_err(|e| {
            (
                500,
                "E_IO".into(),
                format!("读取回滚快照 {} 失败: {e}", path.display()),
            )
        })?;
        let permissions = std::fs::metadata(path)
            .map_err(|e| {
                (
                    500,
                    "E_IO".into(),
                    format!("读取回滚快照权限 {} 失败: {e}", path.display()),
                )
            })?
            .permissions();
        (Some(bytes), Some(permissions))
    } else {
        (None, None)
    };
    Ok(FileSnapshot { bytes, permissions })
}

pub(crate) fn restore_file_snapshot(path: &Path, snapshot: &FileSnapshot) -> Result<(), String> {
    match &snapshot.bytes {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("重建 {} 的父目录失败: {e}", path.display()))?;
            }
            let tmp = path.with_extension("2xapi-rollback.tmp");
            std::fs::write(&tmp, bytes)
                .map_err(|e| format!("回写临时文件 {} 失败: {e}", tmp.display()))?;
            if let Some(permissions) = &snapshot.permissions {
                std::fs::set_permissions(&tmp, permissions.clone())
                    .map_err(|e| format!("恢复 {} 权限失败: {e}", path.display()))?;
            }
            std::fs::rename(&tmp, path).map_err(|e| format!("恢复 {} 失败: {e}", path.display()))
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删除事务中新建文件 {} 失败: {e}", path.display())),
        },
    }
}

pub(crate) fn rollback_files(error: OpError, snapshots: &[(PathBuf, FileSnapshot)]) -> OpError {
    let failures: Vec<String> = snapshots
        .iter()
        .filter_map(|(path, snapshot)| restore_file_snapshot(path, snapshot).err())
        .collect();
    if failures.is_empty() {
        error
    } else {
        (
            error.0,
            error.1,
            format!("{}；回滚失败: {}", error.2, failures.join("；")),
        )
    }
}

pub(crate) fn validate_provider_agent(
    provider: &Provider,
    expected_agent: &str,
) -> Result<(), OpError> {
    if provider.agent == expected_agent {
        Ok(())
    } else {
        Err((
            409,
            "E_PROVIDER_AGENT_MISMATCH".into(),
            format!(
                "供应商 {} 属于平台 {},不能用于 {}",
                provider.id, provider.agent, expected_agent
            ),
        ))
    }
}

pub(crate) fn set_active_checked(
    providers_path: &Path,
    provider: &Provider,
    expected_agent: &str,
) -> Result<(), OpError> {
    validate_provider_agent(provider, expected_agent)?;
    crate::providers::set_active_checked(providers_path, &provider.id).map_err(|error| {
        (
            500,
            "E_ACTIVE_WRITE".into(),
            format!("保存 {expected_agent} 当前供应商失败: {error}"),
        )
    })?;
    if crate::providers::get_active_for_agent(providers_path, expected_agent)
        .is_some_and(|active| active.id == provider.id)
    {
        Ok(())
    } else {
        Err((
            500,
            "E_ACTIVE_WRITE".into(),
            format!("保存 {expected_agent} 当前供应商失败"),
        ))
    }
}

pub(crate) fn clear_active_checked(
    providers_path: &Path,
    expected_agent: &str,
) -> Result<(), OpError> {
    if !providers_path.exists() {
        return Ok(());
    }
    crate::providers::clear_active_for_agent_checked(providers_path, expected_agent).map_err(
        |error| {
            (
                500,
                "E_ACTIVE_WRITE".into(),
                format!("清理 {expected_agent} 当前供应商失败: {error}"),
            )
        },
    )?;
    if crate::providers::get_active_for_agent(providers_path, expected_agent).is_none() {
        Ok(())
    } else {
        Err((
            500,
            "E_ACTIVE_WRITE".into(),
            format!("清理 {expected_agent} 当前供应商失败"),
        ))
    }
}

pub(crate) fn providers_path_from_backup_dir(backup_dir: &Path) -> PathBuf {
    backup_dir
        .parent()
        .unwrap_or(backup_dir)
        .join("providers.json")
}

fn live_snapshots(paths: &[PathBuf]) -> Result<Vec<(PathBuf, FileSnapshot)>, OpError> {
    paths
        .iter()
        .map(|path| snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect()
}

// ── hosting 判定 ─────────────────────────────────────────────

/// 当前 config.toml 是否处于本软件托管态。
/// - custom.base_url 指向网关 → gateway(providerId 用 providers.json active 交叉印证)
/// - custom 段存在 `experimental_bearer_token` 键 → direct(受控标记,见下)
/// - 其余(无 custom 段 / 用户手写的第三方 custom)→ null
///
/// direct 判定依据(UI2 已定「detect_hosting 禁止地址匹配」——手写 custom 地址撞上某 active
/// 供应商时,地址匹配会把用户手写配置误判为托管、unhost 再误删,真机暴露过):
/// 该键**仅本软件 host direct 会写**(gateway 托管零 Key 不写,M2 Mixed 虽写但 base_url
/// 恒指网关、先走 gateway 分支),手写用户几乎不会带此实验性键,故以其存在性为受控标记。
/// 阶段 1 备注的更完备方案(旁写 2xapi 标记键或独立 state 文件)留待后续批次。
pub fn detect_hosting(config_path: &Path, providers_path: &Path) -> Value {
    let cfg = read_toml(config_path);
    let Some(active_provider) = cfg.get("model_provider").and_then(|value| value.as_str()) else {
        // 没有活动 provider 指针时，备用 custom 段不具备 ownership 证据。
        return Value::Null;
    };
    let provider_id = match active_provider {
        crate::codex_overlay::PROVIDER_ID => crate::codex_overlay::PROVIDER_ID,
        "custom" => "custom",
        // 非活动 provider 段即使含有旧 bearer/网关字段，也不属于当前托管，
        // 避免 unhost 误把用户手写的备用配置当成 2xapi 所有内容。
        _ => return Value::Null,
    };
    let custom = cfg.get("model_providers").and_then(|m| m.get(provider_id));
    let Some(custom) = custom else {
        return Value::Null;
    };
    let base_url = custom
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 独占 provider 是新托管标记；custom + loopback 仅兼容旧版状态。
    let legacy_owned = custom.get("experimental_bearer_token").is_some()
        || cfg
            .get("model_catalog_json")
            .and_then(|value| value.as_str())
            .is_some_and(|path| path.contains(MODEL_CATALOG_FILENAME));
    let way = if (provider_id == crate::codex_overlay::PROVIDER_ID
        && base_url.contains(GATEWAY_ADDR))
        || (provider_id == "custom" && base_url.contains(GATEWAY_ADDR) && legacy_owned)
    {
        "gateway"
    } else if custom.get("experimental_bearer_token").is_some() {
        "direct"
    } else {
        return Value::Null; // 第三方手写 custom(地址匹配禁止)→ 未托管
    };
    let data = crate::providers::load(providers_path);
    // 无任何供应商 → 未托管:config 残留托管 custom 段也不表达托管(空状态必须 hosting=null)
    if data.providers.is_empty() {
        return Value::Null;
    }
    let active = crate::providers::get_active_for_agent(providers_path, "codex");
    let (id, name) = match active {
        Some(provider) => (json!(provider.id), json!(provider.name)),
        None => (Value::Null, Value::Null),
    };
    json!({ "providerId": id, "providerName": name, "way": way })
}

pub fn gateway_alive() -> bool {
    let addr: std::net::SocketAddr = GATEWAY_ADDR.parse().unwrap();
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// GET /api/desktop/state
pub fn state(config_path: &Path, providers_path: &Path, codex_home: &Path) -> Value {
    let login = crate::codex_security::probe_login_cached(codex_home);
    let signed_in = login.state == crate::codex_security::LoginState::SignedIn;
    json!({
        "hasOfficial": signed_in,
        "login": login,
        "hosting": detect_hosting(config_path, providers_path),
        "gateway": { "addr": GATEWAY_ADDR, "alive": gateway_alive() },
        "codexHome": codex_home.to_string_lossy(),
    })
}

// ── host ─────────────────────────────────────────────────────

/// POST /api/desktop/host {providerId, way}
pub fn host(
    config_path: &Path,
    backup_dir: &Path,
    codex_home: &Path,
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
    // 桌面版 direct 会把 bearer 写入 Codex 配置，违反零凭据边界，永久退役。
    if way == "direct" {
        return Err((
            410,
            "E_DESKTOP_DIRECT_RETIRED".into(),
            "桌面版直连已退役，请使用零 Key 本地网关；终端直连仍可通过进程环境变量使用".into(),
        ));
    }
    let data = crate::providers::load(providers_path);
    let provider = data
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| {
            (
                404,
                "E_PROVIDER_NOT_FOUND".to_string(),
                "找不到该供应商".to_string(),
            )
        })?;
    validate_provider_agent(&provider, "codex")?;
    // catalog 最小目录以默认模型生成:无默认模型则无从生成(见 build_hosted_config 注释)
    if provider.model.is_empty() {
        return Err((
            422,
            "E_NO_MODEL".to_string(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".to_string(),
        ));
    }

    let io = |e: String| -> OpError { (500, "E_IO".to_string(), e) };

    // 已处于 gateway 托管(含换供应商):独占 provider 段保持稳定,set_active;
    // 真机故障补充(2026-08-15,交接日志):同步 model 字段与 catalog——不同步会让新供应商
    // 收到旧模型名/读到旧 catalog,桌面版与 CLI 均实测故障。决策本意(custom 稳定+热切换)保留。
    let already = detect_hosting(config_path, providers_path);
    if already.get("way").and_then(|v| v.as_str()) == Some("gateway") {
        let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
        let overlay_path = crate::codex_overlay::overlay_state_path(backup_dir);
        let snapshots = live_snapshots(&[
            config_path.to_path_buf(),
            catalog_path.clone(),
            providers_path.to_path_buf(),
            overlay_path.clone(),
        ])?;
        let outcome = (|| {
            let previous_overlay =
                crate::codex_overlay::read_overlay_state(&overlay_path).map_err(io)?;
            let baseline_overlay = match previous_overlay {
                Some(state) => Some(state),
                None => Some(crate::codex_overlay::new_baseline(config_path).map_err(io)?),
            };
            let mut config_written = false;
            let current = read_toml(config_path);
            let model_differs =
                current.get("model").and_then(|v| v.as_str()) != Some(provider.model.as_str());
            let catalog_missing = !catalog_path.exists();
            let provider_differs = current.get("model_provider").and_then(|v| v.as_str())
                != Some(crate::codex_overlay::PROVIDER_ID);
            if (model_differs || catalog_missing || provider_differs) && !provider.model.is_empty()
            {
                backup_file(config_path, backup_dir, "config-apply", "pre-switch").map_err(io)?;
                let before = crate::codex_overlay::fingerprint(config_path).map_err(io)?;
                let after = crate::codex_overlay::apply_gateway(
                    config_path,
                    GATEWAY_BASE_URL,
                    &provider.model,
                    &catalog_path.to_string_lossy(),
                    before.sha256.as_deref(),
                )
                .map_err(io)?;
                config_written = before.sha256 != after.sha256;
                let catalog_models: Vec<crate::providers::ModelConfig> =
                    if provider.models.is_empty() {
                        vec![crate::providers::ModelConfig {
                            name: provider.model.clone(),
                            display_name: None,
                            context_window: None,
                            is_multimodal: false,
                            send_as_is: false,
                        }]
                    } else {
                        provider.models.clone()
                    };
                let catalog = build_model_catalog(
                    &catalog_models,
                    provider.reasoning_levels.as_deref().unwrap_or(&[]),
                );
                let raw = serde_json::to_string_pretty(&catalog).map_err(|e| io(e.to_string()))?;
                std::fs::write(&catalog_path, format!("{raw}\n")).map_err(|e| io(e.to_string()))?;
            }

            crate::codex_overlay::record_applied_state(
                config_path,
                backup_dir,
                baseline_overlay,
                Some(&catalog_path),
            )
            .map_err(io)?;

            set_active_checked(providers_path, &provider, "codex")?;
            let login = crate::codex_security::probe_login_cached(codex_home);
            Ok(json!({
                "hosted": true, "switched": true,
                "hasOfficial": login.state == crate::codex_security::LoginState::SignedIn,
                "login": login,
                "hosting": detect_hosting(config_path, providers_path),
                "changed": { "config": config_written, "auth": false },
            }))
        })();
        return outcome.map_err(|error| rollback_files(error, &snapshots));
    }

    // 全量托管写(字段级合并 + 备份)
    let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
    let overlay_path = crate::codex_overlay::overlay_state_path(backup_dir);
    let current = read_toml(config_path);
    let baseline_overlay =
        match crate::codex_overlay::read_overlay_state(&overlay_path).map_err(io)? {
            Some(state) => Some(state),
            None => Some(crate::codex_overlay::new_baseline(config_path).map_err(io)?),
        };
    let provider_ready = current.get("model_provider").and_then(|v| v.as_str())
        == Some(crate::codex_overlay::PROVIDER_ID)
        && current.get("model").and_then(|v| v.as_str()) == Some(provider.model.as_str())
        && catalog_path.exists();
    let snapshots = live_snapshots(&[
        config_path.to_path_buf(),
        catalog_path.clone(),
        providers_path.to_path_buf(),
        overlay_path,
    ])?;
    let outcome = (|| {
        let config_written = if !provider_ready {
            let purpose = if already.is_null() {
                "pre-host"
            } else {
                "pre-switch"
            };
            backup_file(config_path, backup_dir, "config-apply", purpose).map_err(io)?;
            let before = crate::codex_overlay::fingerprint(config_path).map_err(io)?;
            let after = crate::codex_overlay::apply_gateway(
                config_path,
                GATEWAY_BASE_URL,
                &provider.model,
                &catalog_path.to_string_lossy(),
                before.sha256.as_deref(),
            )
            .map_err(io)?;
            before.sha256 != after.sha256
        } else {
            false
        };

        let catalog_models: Vec<crate::providers::ModelConfig> = if provider.models.is_empty() {
            vec![crate::providers::ModelConfig {
                name: provider.model.clone(),
                display_name: None,
                context_window: None,
                is_multimodal: false,
                send_as_is: false,
            }]
        } else {
            provider.models.clone()
        };
        let catalog = build_model_catalog(
            &catalog_models,
            provider.reasoning_levels.as_deref().unwrap_or(&[]),
        );
        let raw = serde_json::to_string_pretty(&catalog)
            .map_err(|e| e.to_string())
            .map_err(io)?;
        std::fs::write(&catalog_path, format!("{raw}\n"))
            .map_err(|e| e.to_string())
            .map_err(io)?;

        crate::codex_overlay::record_applied_state(
            config_path,
            backup_dir,
            baseline_overlay,
            Some(&catalog_path),
        )
        .map_err(io)?;

        set_active_checked(providers_path, &provider, "codex")?;
        let login = crate::codex_security::probe_login_cached(codex_home);

        Ok(json!({
            "hosted": true, "switched": false,
            "hasOfficial": login.state == crate::codex_security::LoginState::SignedIn,
            "login": login,
            "hosting": detect_hosting(config_path, providers_path),
            "changed": { "config": config_written, "auth": false, "authBackup": false },
        }))
    })();
    outcome.map_err(|error| rollback_files(error, &snapshots))
}

// ── unhost ───────────────────────────────────────────────────

/// POST /api/desktop/unhost
pub fn unhost(
    config_path: &Path,
    backup_dir: &Path,
    codex_home: &Path,
    providers_path: &Path,
) -> Result<Value, OpError> {
    let io = |e: String| -> OpError { (500, "E_IO".to_string(), e) };

    let hosting = detect_hosting(config_path, providers_path);
    if hosting.is_null() {
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }
    let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
    let overlay_path = crate::codex_overlay::overlay_state_path(backup_dir);
    let snapshots = live_snapshots(&[
        config_path.to_path_buf(),
        catalog_path.clone(),
        providers_path.to_path_buf(),
        overlay_path.clone(),
    ])?;
    let outcome = (|| {
        backup_file(config_path, backup_dir, "config-apply", "pre-unhost").map_err(io)?;
        let new_provider = read_toml(config_path)
            .get("model_provider")
            .and_then(|value| value.as_str())
            == Some(crate::codex_overlay::PROVIDER_ID);
        let (config_written, conflicts) = if new_provider {
            let result =
                crate::codex_overlay::restore_owned_fields(config_path, &overlay_path, None)
                    .map_err(io)?;
            let conflicts: Vec<String> = result["conflicts"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            (result["changed"].as_bool().unwrap_or(false), conflicts)
        } else {
            return Err((
                409,
                "E_OVERLAY_STATE_MISSING".into(),
                "检测到旧版或外部 Codex 路由，但没有 2xapi ownership sidecar；请先使用官方默认恢复预览，不会自动删除 custom 配置".into(),
            ));
        };
        let catalog_owned = crate::codex_overlay::read_overlay_state(&overlay_path)
            .map_err(io)?
            .and_then(|state| state.catalog)
            .and_then(|expected| {
                crate::codex_overlay::fingerprint(&catalog_path)
                    .ok()
                    .map(|actual| expected.sha256 == actual.sha256)
            })
            .unwrap_or(false);
        if catalog_owned && catalog_path.exists() {
            std::fs::remove_file(&catalog_path).map_err(|e| io(e.to_string()))?;
        }
        if overlay_path.exists() {
            std::fs::remove_file(&overlay_path).map_err(|e| io(e.to_string()))?;
        }
        clear_active_checked(providers_path, "codex")?;

        Ok(json!({
            "restored": true, "way": "clean",
            "conflicts": conflicts,
            "changed": { "config": config_written, "catalog": catalog_owned, "auth": false },
        }))
    })();
    outcome.map_err(|error| rollback_files(error, &snapshots))
}

// ── Claude Code 配置托管与启动 ───────────────────────────────

/// 兼容旧接口：把 Claude Code 网关设置写入 `~/.claude/settings.json`。
/// 返回托管元数据，不返回环境变量、启动命令或真实上游 Key。
pub fn claude_start(providers_path: &Path, way: &str, provider_id: &str) -> Result<Value, OpError> {
    let parent = providers_path.parent().unwrap_or_else(|| Path::new("."));
    let home = if parent.file_name().and_then(|name| name.to_str()) == Some(".codex") {
        parent.parent().unwrap_or(parent).to_path_buf()
    } else {
        parent.to_path_buf()
    };
    let backup_dir = parent.join("config-backups");
    crate::agents::claude_code::host(&home, &backup_dir, providers_path, provider_id, way)
}

fn claude_launch_with<LocateCli, LaunchTerminal>(
    providers_path: &Path,
    way: &str,
    provider_id: &str,
    macos_supported: bool,
    locate_cli: LocateCli,
    launch_terminal: LaunchTerminal,
) -> Result<Value, OpError>
where
    LocateCli: FnOnce() -> Result<PathBuf, String>,
    LaunchTerminal: FnOnce(&str) -> Result<(), String>,
{
    let start = claude_start(providers_path, way, provider_id)?;
    if !macos_supported {
        return Err((
            501,
            "E_PLATFORM_UNSUPPORTED".into(),
            "Claude Code 一键启动目前仅支持 macOS Terminal".into(),
        ));
    }

    let cli_path = locate_cli().map_err(|_| {
        (
            400,
            "E_CLAUDE_CLI_NOT_FOUND".into(),
            "未找到 Claude Code CLI。请先安装 Claude Code，然后重新打开 2xapi 再试。".into(),
        )
    })?;
    let cli = cli_path.to_str().filter(|value| !value.is_empty()).ok_or((
        400,
        "E_CLAUDE_CLI_NOT_FOUND".into(),
        "Claude Code CLI 路径无法识别，请重新安装 Claude Code 后再试。".into(),
    ))?;
    let command = shell_quote(cli);
    launch_terminal(&command).map_err(|reason| {
        (
            500,
            "E_CLAUDE_LAUNCH_FAILED".into(),
            format!("无法打开 macOS Terminal 启动 Claude Code：{reason}"),
        )
    })?;

    Ok(json!({
        "launched": true,
        "terminal": "Terminal",
        "way": start["way"].clone(),
        "providerId": start["providerId"].clone(),
        "providerName": start["providerName"].clone(),
        "model": start["model"].clone(),
        "modelOptions": start["modelOptions"].clone(),
    }))
}

#[cfg(target_os = "macos")]
fn locate_claude_cli_macos() -> Result<PathBuf, String> {
    let output = std::process::Command::new("/bin/zsh")
        .args(["-lic", "whence -p claude"])
        .env("TERM", "dumb")
        .output()
        .map_err(|error| format!("无法检查 Claude Code CLI：{error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('/') && Path::new(line).is_file())
        .map(PathBuf::from)
        .ok_or_else(|| "PATH 中没有可执行的 claude".into())
}

#[cfg(target_os = "macos")]
fn applescript_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(target_os = "macos")]
fn launch_claude_in_terminal_macos(command: &str) -> Result<(), String> {
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script {}\nend tell\n",
        applescript_string(command)
    );
    let mut child = std::process::Command::new("/usr/bin/osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法调用系统自动化工具：{error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "无法写入 Terminal 启动指令".to_string())?
        .write_all(script.as_bytes())
        .map_err(|error| format!("写入 Terminal 启动指令失败：{error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("等待 Terminal 响应失败：{error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if detail.is_empty() {
            "系统拒绝打开 Terminal，请检查自动化权限".into()
        } else {
            detail
        })
    }
}

/// 在 macOS Terminal 中一键启动 Claude Code。响应仅返回启动结果与供应商元数据，
/// 不返回 command/env；网关模式始终使用本机占位 token，不暴露真实上游 Key。
pub fn claude_launch(
    providers_path: &Path,
    way: &str,
    provider_id: &str,
) -> Result<Value, OpError> {
    #[cfg(target_os = "macos")]
    {
        claude_launch_with(
            providers_path,
            way,
            provider_id,
            true,
            locate_claude_cli_macos,
            launch_claude_in_terminal_macos,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        claude_launch_with(
            providers_path,
            way,
            provider_id,
            false,
            || unreachable!(),
            |_| unreachable!(),
        )
    }
}

// ── 单测(任务书 §1.3)────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{AccessMode, ProviderData};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn sandbox(
        label: &str,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("2xapi-stage1-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let codex_home = root.join("codex");
        let backup_dir = root.join("backups");
        let config_path = codex_home.join("config.toml");
        let providers_path = root.join("providers.json");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::create_dir_all(&backup_dir).unwrap();
        (root, config_path, backup_dir, codex_home, providers_path)
    }

    fn provider(id: &str, name: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            base_url: "https://up.example.com".into(),
            api_key: "sk-test-secret".into(),
            access_mode: AccessMode::PureApi,
            model: "gpt-demo".into(),
            ..Default::default()
        }
    }

    fn write_providers(path: &Path, providers: Vec<Provider>) {
        std::fs::write(
            path,
            serde_json::to_string(&ProviderData {
                schema_version: 1,
                active_provider_id: None,
                active_provider_ids: Default::default(),
                providers,
            })
            .unwrap(),
        )
        .unwrap();
    }

    // ── host(gateway)写入快照 ──

    #[test]
    fn host_gateway_writes_expected_config() {
        let (root, cfg, bk, home, prov) = sandbox("host-gw");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        // host 前已有 auth.json(别家 key)→ 必须完全不触碰
        let auth_before = r#"{"OPENAI_API_KEY":"sk-old"}"#;
        std::fs::write(home.join("auth.json"), auth_before).unwrap();
        let mut p = provider("p1", "2xapi");
        p.models = vec![crate::providers::ModelConfig {
            name: "gpt-demo".into(),
            display_name: None,
            context_window: Some(400000),
            is_multimodal: false,
            send_as_is: false,
        }];
        write_providers(&prov, vec![p]);

        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("model_provider = \"2xapi_gateway\""));
        assert!(
            written.contains("base_url = \"http://127.0.0.1:8787\""),
            "custom 段应指向网关:\n{written}"
        );
        assert!(written.contains("wire_api = \"responses\""));
        // 无官方账号 → requires_openai_auth=false
        assert!(
            written.contains("requires_openai_auth = false"),
            "无账号应为 false:\n{written}"
        );
        // 零 Key 契约:不写 bearer token,上游地址与 key 都不进 config
        assert!(
            !written.contains("experimental_bearer_token"),
            "不应写 bearer:\n{written}"
        );
        assert!(!written.contains("up.example.com"));
        assert!(!written.contains("sk-test-secret"));
        // 用户字段保留 + catalog 指向
        assert!(written.contains("my_custom_setting"));
        assert!(written.contains("model_catalog_json"));
        // catalog 文件与 active
        assert!(home.join(MODEL_CATALOG_FILENAME).exists());
        assert_eq!(
            crate::providers::load(&prov).active_provider_id,
            Some("p1".into())
        );
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            auth_before
        );
        assert!(!home.join("auth.json.official.bak").exists());
        assert!(bk.join("2xapi-codex-overlay-state.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_gateway_with_official_keeps_auth_untouched() {
        let (root, cfg, bk, home, prov) = sandbox("host-official");
        let official_auth = r#"{"tokens":{"id_token":"official-state"}}"#;
        std::fs::write(home.join("auth.json"), official_auth).unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);

        let _out = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("requires_openai_auth = false"),
            "gateway 不依赖官方登录:\n{written}"
        );
        // auth.json 原样、无备份
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            official_auth
        );
        assert!(!home.join("auth.json.official.bak").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_switch_unhost_preserves_auth_bytes_and_metadata() {
        let (root, cfg, bk, home, prov) = sandbox("auth-invariant");
        let auth_path = home.join("auth.json");
        std::fs::write(&auth_path, r#"{"tokens":{"access_token":"opaque-test"}}"#).unwrap();
        let before_bytes = std::fs::read(&auth_path).unwrap();
        let before_meta = std::fs::metadata(&auth_path).unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        unhost(&cfg, &bk, &home, &prov).unwrap();
        let after_meta = std::fs::metadata(&auth_path).unwrap();
        assert_eq!(std::fs::read(&auth_path).unwrap(), before_bytes);
        assert_eq!(before_meta.len(), after_meta.len());
        assert_eq!(
            before_meta.permissions().readonly(),
            after_meta.permissions().readonly()
        );
        assert_eq!(
            before_meta.modified().unwrap(),
            after_meta.modified().unwrap()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// UI 对齐批:direct 已放开(无账号),此测试只保留两类 4xx——未知 way 与未知 provider;
    /// 有账号 direct 拒绝见 host_direct_rejected_with_official。
    #[test]
    fn host_rejects_unknown_way_and_provider() {
        let (root, cfg, bk, home, prov) = sandbox("host-err");
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        let err = host(&cfg, &bk, &home, &prov, "p1", "nonsense").unwrap_err();
        assert_eq!(err.0, 400);
        assert_eq!(err.1, "E_BAD_WAY");
        assert!(!err.2.is_empty(), "4xx 消息须为人话,不可为空");
        let err2 = host(&cfg, &bk, &home, &prov, "nope", "gateway").unwrap_err();
        assert_eq!(err2.1, "E_PROVIDER_NOT_FOUND");
        assert_eq!(
            err2.2, "找不到该供应商",
            "providerId 不存在的 4xx 须为人话(UI2 空状态兜底)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let (root, cfg, bk, home, prov) = sandbox("host-foreign-agent");
        let original = "user_setting = \"keep\"\n";
        std::fs::write(&cfg, original).unwrap();
        let mut foreign = provider("p1", "Gemini");
        foreign.agent = "gemini".into();
        write_providers(&prov, vec![foreign]);

        let error = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        assert!(!home.join(MODEL_CATALOG_FILENAME).exists());
        assert!(crate::providers::get_active_for_agent(&prov, "codex").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_rolls_back_live_files_when_active_write_fails() {
        let (root, cfg, bk, home, prov) = sandbox("host-active-rollback");
        let original = "user_setting = \"keep\"\n";
        std::fs::write(&cfg, original).unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        std::fs::create_dir(prov.with_extension("json.tmp")).unwrap();

        let error = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_ACTIVE_WRITE");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        assert!(!home.join(MODEL_CATALOG_FILENAME).exists());
        assert!(!home.join("auth.json").exists());
        assert!(crate::providers::get_active_for_agent(&prov, "codex").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_idempotent_same_provider() {
        let (root, cfg, bk, home, prov) = sandbox("host-idem");
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        let r1 = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert!(r1["changed"]["config"].as_bool().unwrap());
        let before = std::fs::read_to_string(&cfg).unwrap();
        let r2 = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert!(
            r2["switched"].as_bool().unwrap(),
            "重复 host 同供应商走切换分支"
        );
        assert!(!r2["changed"]["config"].as_bool().unwrap());
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(before, after, "config 不应变化");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_switch_provider_keeps_custom_and_syncs_model() {
        let (root, cfg, bk, home, prov) = sandbox("host-switch");
        let mut p1 = provider("p1", "A");
        p1.model = "model-a".into();
        let mut p2 = provider("p2", "B");
        p2.model = "model-b".into();
        write_providers(&prov, vec![p1, p2]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();
        let gateway_before: String = before
            .split("[model_providers.2xapi_gateway]")
            .nth(1)
            .unwrap()
            .to_string();
        let r = host(&cfg, &bk, &home, &prov, "p2", "gateway").unwrap();
        assert!(r["switched"].as_bool().unwrap());
        let after = std::fs::read_to_string(&cfg).unwrap();
        let gateway_after: String = after
            .split("[model_providers.2xapi_gateway]")
            .nth(1)
            .unwrap()
            .lines()
            .take(5)
            .collect::<String>();
        assert!(
            after.contains("[model_providers.2xapi_gateway]")
                && after.contains("base_url = \"http://127.0.0.1:8787\""),
            "gateway 段应保留(网关指向不变):\n{after}"
        );
        assert!(
            after.contains("model = \"model-b\""),
            "换供应商应同步 model(真机故障:旧模型名发给新上游):\n{after}"
        );
        assert_eq!(
            gateway_before.lines().take(5).collect::<String>(),
            gateway_after,
            "gateway 段内容不应变化"
        );
        assert_eq!(
            crate::providers::load(&prov).active_provider_id,
            Some("p2".into())
        );
        // catalog 同步为新供应商
        let catalog = std::fs::read_to_string(home.join(MODEL_CATALOG_FILENAME)).unwrap();
        assert!(catalog.contains("model-b"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 真机故障回归(2026-08-15):供应商无 models 时 host 也必须写 catalog 文件——
    /// config 指向不存在的文件,codex(桌面版新建聊天/CLI)直接报
    /// "No such file or directory / failed to resolve feature override precedence"。
    #[test]
    fn host_without_models_still_writes_minimal_catalog() {
        let (root, cfg, bk, home, prov) = sandbox("host-mincat");
        let p = provider("p1", "NoModels"); // models 为空
        write_providers(&prov, vec![p]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();

        let catalog_path = home.join(MODEL_CATALOG_FILENAME);
        assert!(
            catalog_path.exists(),
            "无 models 也必须生成最小 catalog(config 恒指向它)"
        );
        let catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        let slugs: Vec<&str> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["slug"].as_str().unwrap())
            .collect();
        assert_eq!(
            slugs,
            vec!["gpt-demo"],
            "最小目录应含默认模型(模型名对客户端可解析):\n{slugs:?}"
        );
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("model_catalog_json"),
            "config 应指向 catalog:\n{written}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_rejects_provider_without_model() {
        let (root, cfg, bk, home, prov) = sandbox("host-nomodel");
        let mut p = provider("p1", "NoDefault");
        p.model = String::new();
        write_providers(&prov, vec![p]);
        let err = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap_err();
        assert_eq!(err.1, "E_NO_MODEL");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── hosting 判定变体 ──

    #[test]
    fn detect_hosting_variants() {
        let (root, cfg, _bk, home, prov) = sandbox("detect");
        // 无 custom → null
        std::fs::write(&cfg, "model_provider = \"openai\"\n").unwrap();
        assert!(detect_hosting(&cfg, &prov).is_null());
        // 第三方 custom(opencode 形态)→ null
        std::fs::write(
            &cfg,
            "[model_providers.custom]\nbase_url = \"https://opencode.ai/zen/go/v1\"\n",
        )
        .unwrap();
        assert!(
            detect_hosting(&cfg, &prov).is_null(),
            "第三方 custom 不应误判为托管"
        );
        // 非活动 provider 段即使带旧标记，也不能被当成当前托管。
        std::fs::write(
            &cfg,
            "model_provider = \"openai\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:8787\"\nexperimental_bearer_token = \"opaque\"\n",
        )
        .unwrap();
        assert!(detect_hosting(&cfg, &prov).is_null());
        std::fs::write(
            &cfg,
            "[model_providers.custom]\nbase_url = \"http://127.0.0.1:8787\"\nexperimental_bearer_token = \"opaque\"\n",
        )
        .unwrap();
        assert!(detect_hosting(&cfg, &prov).is_null());
        // 真机暴露场景:用户手写 custom 地址恰与 active 供应商地址相同但无 bearer 标记键
        // → 仍应 null(UI2 已定:detect 禁止地址匹配,仅有我们写入的
        // experimental_bearer_token 键才算 direct 托管)
        write_providers(&prov, vec![provider("p1", "A")]);
        std::fs::write(&cfg, "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"https://up.example.com\"\n").unwrap();
        crate::providers::set_active(&prov, "p1");
        assert!(
            detect_hosting(&cfg, &prov).is_null(),
            "地址撞 active 供应商也不应判 direct"
        );
        // M2 Mixed 形态(网关地址 + experimental_bearer_token)→ 归 gateway(网关判定优先)
        std::fs::write(
            &cfg,
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:8787\"\nexperimental_bearer_token = \"sk-m2\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_hosting(&cfg, &prov)["way"],
            "gateway",
            "网关地址优先于 bearer 标记"
        );
        // 新版 gateway 托管 + active：独占 provider 才能被识别
        std::fs::write(&cfg, "model_provider = \"2xapi_gateway\"\n[model_providers.2xapi_gateway]\nbase_url = \"http://127.0.0.1:8787\"\n").unwrap();
        crate::providers::set_active(&prov, "p1");
        let h = detect_hosting(&cfg, &prov);
        assert_eq!(h["way"], "gateway");
        assert_eq!(h["providerId"], "p1");
        assert_eq!(h["providerName"], "A");
        // gateway 但无 active(状态破坏)→ way=gateway, providerId=null
        crate::providers::clear_active(&prov);
        let h2 = detect_hosting(&cfg, &prov);
        assert_eq!(h2["way"], "gateway");
        assert!(h2["providerId"].is_null());
        let _ = std::fs::remove_dir_all(&root);
        let _ = home;
    }

    // ── UI2 空状态:无任何供应商时 hosting 必须为 null ──

    #[test]
    fn state_hosting_null_when_no_providers() {
        let (root, cfg, _bk, home, prov) = sandbox("state-empty");
        // 空 providers.json(无任何供应商)
        std::fs::write(
            &prov,
            r#"{"schema_version":1,"active_provider_id":null,"providers":[]}"#,
        )
        .unwrap();
        // 未托管(config 无 custom 段)→ null
        std::fs::write(&cfg, "model_provider = \"openai\"\n").unwrap();
        let s = state(&cfg, &prov, &home);
        assert!(
            s["hosting"].is_null(),
            "无供应商且未托管 → hosting null:\n{s}"
        );
        assert!(
            !s["hasOfficial"].as_bool().unwrap(),
            "无 auth.json → hasOfficial false"
        );
        assert_eq!(s["gateway"]["addr"], GATEWAY_ADDR);
        // 此前托管过、后来清空供应商 → config 残留网关段,仍必须未托管(null)
        std::fs::write(
            &cfg,
            "model_provider = \"2xapi_gateway\"\n[model_providers.2xapi_gateway]\nbase_url = \"http://127.0.0.1:8787\"\n",
        )
        .unwrap();
        let s2 = state(&cfg, &prov, &home);
        assert!(
            s2["hosting"].is_null(),
            "无供应商但 config 残留网关段 → 仍应未托管:\n{s2}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = home;
    }

    // ── unhost ──

    #[test]
    fn unhost_no_official_cleans_everything() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-clean");
        // host 前用户已有别家 key(本机真实场景:opencode)
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-other-vendor"}"#,
        )
        .unwrap();
        write_providers(&prov, vec![provider("p1", "A")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();

        let out = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out["restored"].as_bool().unwrap());
        assert_eq!(out["way"], "clean");
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(!written.contains("[model_providers.2xapi_gateway]"));
        assert!(!written.contains("model_provider ="));
        assert!(!written.contains("model_catalog_json"));
        assert!(!written.contains("model ="));
        assert!(!home.join(MODEL_CATALOG_FILENAME).exists());
        // auth 必须回到 host 前的别家 key（且从未被 2xapi 写入）
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            r#"{"OPENAI_API_KEY":"sk-other-vendor"}"#
        );
        assert!(crate::providers::load(&prov).active_provider_id.is_none());
        // 幂等
        let out2 = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out2["alreadyClean"].as_bool().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_with_official_restores_official() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-official");
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"id_token":"OFFICIAL"}}"#,
        )
        .unwrap();
        write_providers(&prov, vec![provider("p1", "A")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();

        let out = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert_eq!(out["way"], "clean");
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(!written.contains("model_provider ="));
        assert!(!written.contains("[model_providers.2xapi_gateway]"));
        assert!(std::fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("OFFICIAL"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_without_auth_does_not_create_or_remove_auth() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-nobak");
        // host 前无 auth.json，网关托管和恢复都不得创建 auth.json
        write_providers(&prov, vec![provider("p1", "A")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(!home.join("auth.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_ignores_third_party_custom() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-third");
        std::fs::write(
            &cfg,
            "[model_providers.custom]\nbase_url = \"https://opencode.ai/zen/go/v1\"\n",
        )
        .unwrap();
        let out = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out["alreadyClean"].as_bool().unwrap());
        // 第三方段原样保留
        assert!(std::fs::read_to_string(&cfg)
            .unwrap()
            .contains("opencode.ai"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §1.5-1:存在 pre-host 快照时,unhost 应把受控字段恢复为快照值(opencode 等手写配置得以还原),
    /// 而非清除。host 前 config 有 opencode custom 段 → host → unhost → 应回到 opencode 配置。
    #[test]
    fn unhost_restores_pre_host_snapshot_controlled_fields() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-restore");
        // host 前:opencode 手写 custom 段(真实场景)
        std::fs::write(
            &cfg,
            "model_provider = \"custom\"\nmodel = \"deepseek-v4-flash\"\n[model_providers.custom]\nbase_url = \"https://opencode.ai/zen/go/v1\"\nwire_api = \"responses\"\n",
        )
        .unwrap();
        write_providers(&prov, vec![provider("p1", "A")]);

        // host 产生 ownership sidecar；custom 作为用户配置保持不动
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();

        // unhost → 受控字段恢复为快照(opencode 配置回来)
        let out = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert_eq!(out["way"], "clean");
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("opencode.ai"),
            "custom 段应恢复为快照值(opencode):\n{written}"
        );
        assert!(written.contains("model_provider = \"custom\""));
        assert!(
            written.contains("deepseek-v4-flash"),
            "model 应恢复为快照值:\n{written}"
        );
        // host 期间的其他改动(若有)应保留——这里没加,只验受控字段回弹
        assert!(crate::providers::load(&prov).active_provider_id.is_none());

        // 幂等:二次 unhost → alreadyClean
        let out2 = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out2["alreadyClean"].as_bool().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Claude Code 配置托管 ─────────────────────────────────

    fn claude_provider(id: &str, name: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            agent: "claude".into(),
            base_url: "https://up.claude.example.com".into(),
            api_key: "sk-claude-test-secret".into(),
            access_mode: AccessMode::PureApi,
            model: "claude-sonnet".into(),
            ..Default::default()
        }
    }

    #[test]
    fn claude_start_writes_gateway_settings_without_returning_key() {
        let (root, _c, _b, _h, prov) = sandbox("claude-start-config");
        write_providers(&prov, vec![claude_provider("p1", "ClaudeT")]);
        let out = claude_start(&prov, "", "").unwrap();
        assert_eq!(out["way"], "gateway");
        assert_eq!(out["providerId"], "p1");
        assert!(out.get("env").is_none());
        assert!(out.get("command").is_none());
        let settings = root.join(".claude/settings.json");
        let written = std::fs::read_to_string(settings).unwrap();
        assert!(written.contains("http://127.0.0.1:8787/anthropic"));
        assert!(written.contains("ANTHROPIC_MODEL"));
        assert!(!written.contains("sk-claude-test-secret"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_start_uses_selected_provider_id_and_model_options() {
        let (root, _c, _b, _h, prov) = sandbox("claude-start-selected");
        let mut first = claude_provider("c1", "First");
        first.model = "first-model".into();
        let mut second = claude_provider("c2", "Second");
        second.model = "second-model".into();
        write_providers(&prov, vec![first, second]);
        let out = claude_start(&prov, "gateway", "c2").unwrap();
        assert_eq!(out["providerId"], "c2");
        assert_eq!(out["providerName"], "Second");
        let settings: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "second-model");
        assert!(claude_start(&prov, "gateway", "no-such-id").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_start_rejects_direct_and_foreign_provider() {
        let (root, _c, _b, _h, prov) = sandbox("claude-direct");
        write_providers(&prov, vec![claude_provider("p1", "ClaudeT")]);
        let direct = claude_start(&prov, "direct", "").unwrap_err();
        assert_eq!(direct.1, "E_CLAUDE_DIRECT_UNSUPPORTED");
        assert!(!root.join(".claude/settings.json").exists());

        let mut foreign = claude_provider("p2", "Foreign");
        foreign.agent = "gemini".into();
        write_providers(&prov, vec![foreign]);
        let err = claude_start(&prov, "gateway", "p2").unwrap_err();
        assert_eq!(err.1, "E_PROVIDER_AGENT_MISMATCH");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_start_no_claude_provider_errs() {
        let (root, _c, _b, _h, prov) = sandbox("claude-noprov");
        write_providers(&prov, vec![provider("p1", "Cx")]);
        let err = claude_start(&prov, "", "").unwrap_err();
        assert_eq!(err.0, 503);
        assert_eq!(err.1, "E_NO_CLAUDE_PROVIDER");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn claude_launch_applescript_string_escapes_script_boundaries() {
        let encoded = applescript_string("echo \"safe\"\nend tell\nsay \"unsafe\"\\tail");
        assert!(encoded.starts_with('"') && encoded.ends_with('"'));
        assert!(!encoded[1..encoded.len() - 1].contains('\n'));
        assert!(encoded.contains("\\\"safe\\\""));
        assert!(encoded.contains("\\nend tell\\n"));
        assert!(encoded.contains("\\\"unsafe\\\"\\\\tail"));
    }

    #[test]
    fn claude_launch_reports_missing_cli_in_plain_language() {
        let (root, _c, _b, _h, prov) = sandbox("claude-launch-no-cli");
        write_providers(&prov, vec![claude_provider("p1", "ClaudeT")]);
        let err = claude_launch_with(
            &prov,
            "gateway",
            "p1",
            true,
            || Err("not found".into()),
            |_| panic!("CLI 不存在时不应尝试打开 Terminal"),
        )
        .unwrap_err();
        assert_eq!(err.0, 400);
        assert_eq!(err.1, "E_CLAUDE_CLI_NOT_FOUND");
        assert!(err.2.contains("未找到 Claude Code CLI"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_launch_reports_terminal_failure_in_plain_language() {
        let (root, _c, _b, _h, prov) = sandbox("claude-launch-terminal-fail");
        write_providers(&prov, vec![claude_provider("p1", "ClaudeT")]);
        let err = claude_launch_with(
            &prov,
            "gateway",
            "p1",
            true,
            || Ok(PathBuf::from("/usr/local/bin/claude")),
            |_| Err("Terminal automation denied".into()),
        )
        .unwrap_err();
        assert_eq!(err.0, 500);
        assert_eq!(err.1, "E_CLAUDE_LAUNCH_FAILED");
        assert!(err.2.contains("无法打开 macOS Terminal"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_launch_rejects_non_macos_explicitly() {
        let (root, _c, _b, _h, prov) = sandbox("claude-launch-platform");
        write_providers(&prov, vec![claude_provider("p1", "ClaudeT")]);
        let err = claude_launch_with(
            &prov,
            "gateway",
            "p1",
            false,
            || panic!("非 macOS 不应查找 CLI"),
            |_| panic!("非 macOS 不应打开 Terminal"),
        )
        .unwrap_err();
        assert_eq!(err.0, 501);
        assert_eq!(err.1, "E_PLATFORM_UNSUPPORTED");
        assert!(err.2.contains("仅支持 macOS"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
