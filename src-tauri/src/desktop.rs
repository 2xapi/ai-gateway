//! 桌面版托管开关(阶段 1,开发任务书 §1.1)。
//!
//! 桌面版 ChatGPT.app 无法注入 env/参数,唯一配置入口是 `~/.codex/config.toml`。
//! 「托管」= 字段级合并写一处 `[model_providers.custom]` 指向本机网关 8787:
//! - 有官方登录 → `requires_openai_auth=true`(混入:官方 token 发网关,网关丢弃并注入中转 Key)
//! - 无官方登录 → `requires_openai_auth=false` + auth.json 写 OPENAI_API_KEY(纯 API 形态,先备份)
//!
//! 配置文件零 Key(Key 由网关注入);「还原」= unhost。
//!
//! 与 config.rs(M2)的关系:复用其 toml 读写/备份/catalog 原语,但合并逻辑独立——
//! M2 的 Mixed 会写 experimental_bearer_token 且 requires_openai_auth 恒 true,
//! 与任务书 §1.1(b) 契约(零 bearer、按账号态取值)不同,M2 行为保持不动。
//!
//! direct 通路(UI 对齐批放开,仅限无官方账号):custom 段直指供应商,供应商 key 以
//! `experimental_bearer_token` 写入 config.toml——阶段 1 已实测该字段即 direct 的 provider
//! 段 Bearer 字段(codex 二进制 18 字段清单 + 隔离环境 `codex exec` 上游收到
//! `Authorization: Bearer <该字段值>`)。与网关的「零 Key」卖点相反,key 落盘是阶段 1
//! 定稿的差异,UI 文案由前端区分,后端只管写入。有官方账号时维持 4xx 拒绝
//! (token/bearer 优先级待阶段 5 实测后再放开)。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::{io::Write, process::Stdio};

use crate::config::{
    backup_file, build_model_catalog, config_to_toml_string, read_auth_json, read_toml,
    write_auth_json, write_toml, AUTH_OFFICIAL_BAK, GATEWAY_BASE_URL, MODEL_CATALOG_FILENAME,
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
    crate::providers::set_active(providers_path, &provider.id);
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
    crate::providers::clear_active_for_agent(providers_path, expected_agent);
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

// ── hasOfficial 判定 ─────────────────────────────────────────

/// auth.json 是否含官方登录态(ChatGPT 账号 token)。
/// ⚠️探索点(任务书 §1.1(a)):官方登录的实际字段名以真机为准,本机当前无官方登录、无法观察,
/// 故写成通用检查——存在键名含 "token" 且值非空的字段(官方 OAuth 形态为 `tokens` 对象)即算;
/// 仅 OPENAI_API_KEY 或文件不存在/解析失败 → false。
pub fn has_official(codex_home: &Path) -> bool {
    let raw = match std::fs::read_to_string(codex_home.join("auth.json")) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let obj = match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(o)) => o,
        _ => return false,
    };
    for (k, v) in &obj {
        if k == "OPENAI_API_KEY" {
            continue;
        }
        if !k.to_lowercase().contains("token") {
            continue;
        }
        let nonempty = match v {
            Value::String(s) => !s.is_empty(),
            Value::Object(o) => !o.is_empty(),
            _ => false,
        };
        if nonempty {
            return true;
        }
    }
    false
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
    let custom = cfg.get("model_providers").and_then(|m| m.get("custom"));
    let Some(custom) = custom else {
        return Value::Null;
    };
    let base_url = custom
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // 网关判定优先:M2 Mixed 形态(网关地址 + bearer)也归 gateway(流量实际走网关)
    let way = if base_url.contains(GATEWAY_ADDR) {
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
    json!({
        "hasOfficial": has_official(codex_home),
        "hosting": detect_hosting(config_path, providers_path),
        "gateway": { "addr": GATEWAY_ADDR, "alive": gateway_alive() },
        "codexHome": codex_home.to_string_lossy(),
    })
}

// ── host ─────────────────────────────────────────────────────

/// 合并出 gateway 托管态 config(零 Key:不写 experimental_bearer_token,任务书 §1.1(b) 契约)。
/// model_catalog_json 恒插入:catalog 文件由 host 保证写入(无模型时生成最小目录),
/// 真机教训——指向不存在的文件会让 codex(桌面版新建聊天/CLI)直接报
/// "No such file or directory / failed to resolve feature override precedence"。
fn build_hosted_config(
    current: &Value,
    provider: &Provider,
    catalog_path: &str,
    requires_openai_auth: bool,
) -> Value {
    let mut cfg = current.clone();
    let obj = cfg.as_object_mut().expect("config 不是 object");
    obj.insert("model_provider".into(), json!("custom"));
    if !provider.model.is_empty() {
        obj.insert("model".into(), json!(provider.model));
    }
    obj.insert("model_catalog_json".into(), json!(catalog_path));
    let mut custom = serde_json::Map::new();
    custom.insert("name".into(), json!("custom"));
    custom.insert("base_url".into(), json!(GATEWAY_BASE_URL));
    custom.insert("wire_api".into(), json!("responses"));
    custom.insert("requires_openai_auth".into(), json!(requires_openai_auth));
    let mp = obj.entry("model_providers").or_insert(json!({}));
    if let Some(m) = mp.as_object_mut() {
        m.insert("custom".into(), Value::Object(custom));
    }
    cfg
}

/// 合并出 direct 托管态 config(仅无官方账号,门控在 host() 开头)。
/// custom 段直指供应商,key 以 `experimental_bearer_token` 落盘——阶段 1 实测该字段即
/// direct 的 provider 段 Bearer 字段(codex 18 字段清单 + 隔离环境 `codex exec` 上游收到
/// `Authorization: Bearer <该字段值>`)。与 gateway 零 Key 契约相反,UI 文案由前端区分。
/// 不写 model_catalog_json / auth.json:direct 不经网关,bearer 已在 config,auth 无需动。
fn build_direct_hosted_config(current: &Value, provider: &Provider) -> Value {
    let mut cfg = current.clone();
    let obj = cfg.as_object_mut().expect("config 不是 object");
    obj.insert("model_provider".into(), json!("custom"));
    if !provider.model.is_empty() {
        obj.insert("model".into(), json!(provider.model));
    }
    let mut custom = serde_json::Map::new();
    custom.insert("name".into(), json!("custom"));
    custom.insert("base_url".into(), json!(provider.base_url));
    custom.insert(
        "wire_api".into(),
        serde_json::to_value(provider.wire_api).unwrap_or(json!("responses")),
    );
    custom.insert("requires_openai_auth".into(), json!(false));
    custom.insert("experimental_bearer_token".into(), json!(provider.api_key));
    let mp = obj.entry("model_providers").or_insert(json!({}));
    if let Some(m) = mp.as_object_mut() {
        m.insert("custom".into(), Value::Object(custom));
    }
    cfg
}

/// 无官方账号时:auth.json 设 OPENAI_API_KEY;首次(.bak 不存在)先备份 host 前状态(01-D4 同语义)。
/// 返回 (changed, backup_created)。
fn ensure_auth_key(codex_home: &Path, api_key: &str) -> Result<(bool, bool), String> {
    let auth_p = codex_home.join("auth.json");
    let bak_p = codex_home.join(AUTH_OFFICIAL_BAK);
    let mut created = false;
    if !bak_p.exists() {
        if let Ok(data) = std::fs::read(&auth_p) {
            std::fs::write(&bak_p, &data).map_err(|e| format!("备份 auth 失败: {e}"))?;
            created = true;
        }
    }
    let mut existing = read_auth_json(&auth_p);
    if existing.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(api_key) {
        return Ok((false, created));
    }
    if existing.is_object() {
        if let Some(o) = existing.as_object_mut() {
            o.insert("OPENAI_API_KEY".into(), json!(api_key));
        }
        write_auth_json(&auth_p, &existing)?;
    }
    Ok((true, created))
}

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
    // direct 门控 hasOfficial(UI 对齐批):无官方账号放开;有官方 → 维持 4xx 拒绝
    // (官方 token 与 experimental_bearer_token 的优先级待阶段 5 实测后再放开)
    if way == "direct" && has_official(codex_home) {
        return Err((
            400,
            "E_DIRECT_UNAVAILABLE".into(),
            "官方登录下直连暂不支持".into(),
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

    // direct 托管(仅无官方账号,门控在函数开头):字段级合并 + 备份,写完即托管态。
    // 幂等:已 direct 托管同供应商时合并结果与现值相同 → 不写盘 no-op 200;
    // 换路(gateway↔direct)/换供应商 → 重写 custom 段;备份 purpose 用 pre-switch
    // (不新增 pre-host 快照,保住最初 pre-host 供 unhost 还原到首次托管前)。
    if way == "direct" {
        let snapshots = live_snapshots(&[config_path.to_path_buf(), providers_path.to_path_buf()])?;
        let outcome = (|| {
            let already = detect_hosting(config_path, providers_path);
            let current = read_toml(config_path);
            let merged = build_direct_hosted_config(&current, &provider);
            let new_toml = config_to_toml_string(&merged).map_err(io)?;
            let current_toml = config_to_toml_string(&current).unwrap_or_default();
            let config_written = if new_toml != current_toml {
                let purpose = if already.is_null() {
                    "pre-host"
                } else {
                    "pre-switch"
                };
                backup_file(config_path, backup_dir, "config-apply", purpose).map_err(io)?;
                write_toml(config_path, &merged).map_err(io)?;
                true
            } else {
                false
            };
            set_active_checked(providers_path, &provider, "codex")?;
            Ok(json!({
                "hosted": true, "switched": !already.is_null(),
                "hasOfficial": false,
                "hosting": detect_hosting(config_path, providers_path),
                "changed": { "config": config_written, "auth": false },
            }))
        })();
        return outcome.map_err(|error| rollback_files(error, &snapshots));
    }

    // 已处于 gateway 托管(含换供应商):custom 段不动(网关热切换),set_active;
    // 真机故障补充(2026-08-15,交接日志):同步 model 字段与 catalog——不同步会让新供应商
    // 收到旧模型名/读到旧 catalog,桌面版与 CLI 均实测故障。决策本意(custom 稳定+热切换)保留。
    let already = detect_hosting(config_path, providers_path);
    if already.get("way").and_then(|v| v.as_str()) == Some("gateway") {
        let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
        let snapshots = live_snapshots(&[
            config_path.to_path_buf(),
            catalog_path.clone(),
            codex_home.join("auth.json"),
            codex_home.join(AUTH_OFFICIAL_BAK),
            providers_path.to_path_buf(),
        ])?;
        let outcome = (|| {
            let mut config_written = false;
            let current = read_toml(config_path);
            let model_differs =
                current.get("model").and_then(|v| v.as_str()) != Some(provider.model.as_str());
            let catalog_missing = !catalog_path.exists();
            if (model_differs || catalog_missing) && !provider.model.is_empty() {
                let mut merged = current.clone();
                if let Some(obj) = merged.as_object_mut() {
                    obj.insert("model".into(), json!(provider.model));
                }
                let new_toml = config_to_toml_string(&merged).map_err(io)?;
                let current_toml = config_to_toml_string(&current).unwrap_or_default();
                if new_toml != current_toml {
                    backup_file(config_path, backup_dir, "config-apply", "pre-switch")
                        .map_err(io)?;
                    write_toml(config_path, &merged).map_err(io)?;
                    config_written = true;
                }
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

            let mut auth_changed = false;
            if !has_official(codex_home) {
                auth_changed = ensure_auth_key(codex_home, &provider.api_key)
                    .map_err(io)?
                    .0;
            }
            set_active_checked(providers_path, &provider, "codex")?;
            Ok(json!({
                "hosted": true, "switched": true,
                "hasOfficial": has_official(codex_home),
                "hosting": detect_hosting(config_path, providers_path),
                "changed": { "config": config_written, "auth": auth_changed },
            }))
        })();
        return outcome.map_err(|error| rollback_files(error, &snapshots));
    }

    // 全量托管写(字段级合并 + 备份)
    let has_off = has_official(codex_home);
    let current = read_toml(config_path);
    let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
    let merged = build_hosted_config(
        &current,
        &provider,
        &catalog_path.to_string_lossy(),
        has_off,
    );
    let new_toml = config_to_toml_string(&merged).map_err(io)?;
    let current_toml = config_to_toml_string(&current).unwrap_or_default();
    let snapshots = live_snapshots(&[
        config_path.to_path_buf(),
        catalog_path.clone(),
        codex_home.join("auth.json"),
        codex_home.join(AUTH_OFFICIAL_BAK),
        providers_path.to_path_buf(),
    ])?;
    let outcome = (|| {
        let config_written = if new_toml != current_toml {
            let purpose = if already.is_null() {
                "pre-host"
            } else {
                "pre-switch"
            };
            backup_file(config_path, backup_dir, "config-apply", purpose).map_err(io)?;
            write_toml(config_path, &merged).map_err(io)?;
            true
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

        let (auth_changed, backup_created) = if has_off {
            (false, false)
        } else {
            ensure_auth_key(codex_home, &provider.api_key).map_err(io)?
        };

        set_active_checked(providers_path, &provider, "codex")?;

        Ok(json!({
            "hosted": true, "switched": false,
            "hasOfficial": has_off,
            "hosting": detect_hosting(config_path, providers_path),
            "changed": { "config": config_written, "auth": auth_changed, "authBackup": backup_created },
        }))
    })();
    outcome.map_err(|error| rollback_files(error, &snapshots))
}

// ── unhost ───────────────────────────────────────────────────

/// 移除 custom 段及关联字段(model_provider/model/model_catalog_json),其余保留。
fn build_unhosted_config(current: &Value) -> Value {
    let mut cfg = current.clone();
    let obj = cfg.as_object_mut().expect("config 不是 object");
    obj.remove("model_provider");
    obj.remove("model");
    obj.remove("model_catalog_json");
    if let Some(mp) = obj
        .get_mut("model_providers")
        .and_then(|v| v.as_object_mut())
    {
        mp.remove("custom");
        if mp.is_empty() {
            obj.remove("model_providers");
        }
    }
    cfg
}

/// 从 pre-host 快照恢复受控字段(§1.5-1):model_provider/model/model_catalog_json/custom 段
/// 取快照值,其余字段保留当前。快照无某字段则移除之。
fn restore_controlled_from_snapshot(current: &Value, snapshot: &Value) -> Value {
    let mut cfg = current.clone();
    let obj = cfg.as_object_mut().expect("config 不是 object");
    for k in ["model_provider", "model", "model_catalog_json"] {
        match snapshot.get(k) {
            Some(v) if !v.is_null() => {
                obj.insert(k.into(), v.clone());
            }
            _ => {
                obj.remove(k);
            }
        }
    }
    let mut mp = current.get("model_providers").cloned().unwrap_or(json!({}));
    if let Some(m) = mp.as_object_mut() {
        m.remove("custom");
    }
    if let Some(sp) = snapshot
        .get("model_providers")
        .and_then(|x| x.get("custom"))
    {
        if let Some(m) = mp.as_object_mut() {
            m.insert("custom".into(), sp.clone());
        }
    }
    let mp_empty = mp.as_object().map(|m| m.is_empty()).unwrap_or(true);
    if mp_empty {
        obj.remove("model_providers");
    } else {
        obj.insert("model_providers".into(), mp);
    }
    cfg
}

/// 找 backup_dir 里用途为 pre-host 的最新快照(host 前 config)。无则 None。
fn find_pre_host_snapshot(backup_dir: &Path, config_path: &Path) -> Option<Value> {
    let mut candidates: Vec<(Option<std::time::SystemTime>, Value)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(backup_dir) {
        for e in rd.flatten() {
            let manifest = e.path();
            let name = manifest.file_name()?.to_string_lossy();
            if !name.ends_with(".manifest.json") {
                continue;
            }
            let meta: Value =
                serde_json::from_str(&std::fs::read_to_string(&manifest).ok()?).ok()?;
            if meta.get("purpose").and_then(|v| v.as_str()) != Some("pre-host") {
                continue;
            }
            if !crate::config::backup_matches_target(&manifest, config_path) {
                continue;
            }
            let toml_path = manifest.with_file_name(name.trim_end_matches(".manifest.json"));
            let v = match crate::config::read_verified_backup(&toml_path, config_path) {
                Ok(Some(data)) => toml::from_str::<toml::Value>(&String::from_utf8_lossy(&data))
                    .map(|value| serde_json::to_value(value).unwrap_or_else(|_| json!({})))
                    .unwrap_or_else(|_| json!({})),
                Ok(None) => json!({}),
                Err(_) => continue,
            };
            candidates.push((e.metadata().and_then(|m| m.modified()).ok(), v));
        }
    }
    candidates.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts)); // 最新在前
    candidates.into_iter().next().map(|(_, v)| v)
}

/// auth.json 里移除我们写入的 OPENAI_API_KEY(仅当值匹配最后一次托管供应商的 key;无 .bak 时的兜底)。
fn remove_auth_key_if_ours(codex_home: &Path, api_key: &str) -> Result<bool, String> {
    let auth_p = codex_home.join("auth.json");
    let mut existing = read_auth_json(&auth_p);
    let matches = existing.get("OPENAI_API_KEY").and_then(|v| v.as_str()) == Some(api_key);
    if matches {
        if let Some(o) = existing.as_object_mut() {
            o.remove("OPENAI_API_KEY");
            write_auth_json(&auth_p, &existing)?;
            return Ok(true);
        }
    }
    Ok(false)
}

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
        // 未托管(或第三方手写 custom 段,不属于我们)→ 幂等 no-op
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }

    if has_official(codex_home) {
        let snapshots = live_snapshots(&[
            config_path.to_path_buf(),
            codex_home.join("auth.json"),
            codex_home.join(MODEL_CATALOG_FILENAME),
            providers_path.to_path_buf(),
        ])?;
        let outcome = (|| {
            backup_file(
                config_path,
                backup_dir,
                "config-apply",
                "pre-activate-official",
            )
            .map_err(|e| (500, "E_IO".to_string(), e))?;
            crate::config::activate_official(config_path, backup_dir, providers_path, codex_home)
                .map_err(|e| (500, "E_IO".to_string(), e))?;
            let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
            if catalog_path.exists() {
                std::fs::remove_file(&catalog_path)
                    .map_err(|e| (500, "E_IO".to_string(), e.to_string()))?;
            }
            clear_active_checked(providers_path, "codex")?;
            Ok(json!({ "restored": true, "way": "official" }))
        })();
        return outcome.map_err(|error| rollback_files(error, &snapshots));
    }

    // 无官方账号:移除托管痕迹。优先从 pre-host 快照恢复受控字段(§1.5-1,opencode 等手写
    // 用户的配置得以还原);无快照才清除。
    let active = crate::providers::get_active_for_agent(providers_path, "codex");
    let current = read_toml(config_path);
    let merged = match find_pre_host_snapshot(backup_dir, config_path) {
        Some(snapshot) => restore_controlled_from_snapshot(&current, &snapshot),
        None => build_unhosted_config(&current),
    };
    let new_toml = config_to_toml_string(&merged).map_err(io)?;
    let current_toml = config_to_toml_string(&current).unwrap_or_default();
    let catalog_path = codex_home.join(MODEL_CATALOG_FILENAME);
    let snapshots = live_snapshots(&[
        config_path.to_path_buf(),
        catalog_path.clone(),
        codex_home.join("auth.json"),
        providers_path.to_path_buf(),
    ])?;
    let outcome = (|| {
        let config_written = if new_toml != current_toml {
            backup_file(config_path, backup_dir, "config-apply", "pre-unhost").map_err(io)?;
            write_toml(config_path, &merged).map_err(io)?;
            true
        } else {
            false
        };
        if catalog_path.exists() {
            std::fs::remove_file(&catalog_path).map_err(|e| io(e.to_string()))?;
        }

        let auth_restored = if codex_home.join(AUTH_OFFICIAL_BAK).exists() {
            let data =
                std::fs::read(codex_home.join(AUTH_OFFICIAL_BAK)).map_err(|e| io(e.to_string()))?;
            std::fs::write(codex_home.join("auth.json"), &data).map_err(|e| io(e.to_string()))?;
            true
        } else if let Some(p) = &active {
            remove_auth_key_if_ours(codex_home, &p.api_key).map_err(io)?
        } else {
            false
        };

        clear_active_checked(providers_path, "codex")?;

        Ok(json!({
            "restored": true, "way": "clean",
            "changed": { "config": config_written, "auth": auth_restored },
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

    // ── hasOfficial 三态 ──

    #[test]
    fn has_official_three_states() {
        let (_r, _c, _b, home, _p) = sandbox("auth3");
        // 不存在 → false
        assert!(!has_official(&home));
        // 仅 OPENAI_API_KEY → false
        std::fs::write(home.join("auth.json"), r#"{"OPENAI_API_KEY":"sk-x"}"#).unwrap();
        assert!(!has_official(&home));
        // 官方 OAuth(tokens 对象)→ true
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"id_token":"a","access_token":"b"}}"#,
        )
        .unwrap();
        assert!(has_official(&home));
        // 两者并存(混入后常见)→ true
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-x","tokens":{"access_token":"b"}}"#,
        )
        .unwrap();
        assert!(has_official(&home));
        // 坏 JSON → false
        std::fs::write(home.join("auth.json"), "not json").unwrap();
        assert!(!has_official(&home));
    }

    // ── host(gateway)写入快照 ──

    #[test]
    fn host_gateway_writes_expected_config() {
        let (root, cfg, bk, home, prov) = sandbox("host-gw");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        // host 前已有 auth.json(别家 key)→ 应被备份
        std::fs::write(home.join("auth.json"), r#"{"OPENAI_API_KEY":"sk-old"}"#).unwrap();
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
        assert!(written.contains("model_provider = \"custom\""));
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
        // auth:无账号 → key 写入 + host 前状态备份
        let auth = std::fs::read_to_string(home.join("auth.json")).unwrap();
        assert!(auth.contains("sk-test-secret"));
        assert!(
            home.join(AUTH_OFFICIAL_BAK).exists(),
            "应备份 host 前的 auth.json"
        );
        assert_eq!(
            std::fs::read_to_string(home.join(AUTH_OFFICIAL_BAK)).unwrap(),
            r#"{"OPENAI_API_KEY":"sk-old"}"#
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn host_gateway_with_official_keeps_auth_untouched() {
        let (root, cfg, bk, home, prov) = sandbox("host-official");
        let official_auth = r#"{"tokens":{"id_token":"official-state"}}"#;
        std::fs::write(home.join("auth.json"), official_auth).unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);

        let out = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert!(out["hasOfficial"].as_bool().unwrap());
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("requires_openai_auth = true"),
            "有账号应混入:\n{written}"
        );
        // auth.json 原样、无备份
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            official_auth
        );
        assert!(!home.join(AUTH_OFFICIAL_BAK).exists());
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

    // ── host(direct,UI 对齐批放开:仅无官方账号)──

    /// ① 无账号 host direct:custom 直指供应商,key 以 experimental_bearer_token 落盘
    /// (阶段 1 实测 Bearer 字段),requires_openai_auth=false,active 生效,不动 auth.json。
    #[test]
    fn host_direct_writes_expected_config() {
        let (root, cfg, bk, home, prov) = sandbox("host-direct");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);

        let out = host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        assert!(out["hosted"].as_bool().unwrap());
        assert!(!out["hasOfficial"].as_bool().unwrap());
        assert_eq!(out["hosting"]["way"], "direct");
        assert_eq!(out["hosting"]["providerId"], "p1");

        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("model_provider = \"custom\""));
        assert!(
            written.contains("model = \"gpt-demo\""),
            "默认模型应写入:\n{written}"
        );
        assert!(
            written.contains("base_url = \"https://up.example.com\""),
            "custom 应直指供应商:\n{written}"
        );
        assert!(written.contains("wire_api = \"responses\""));
        assert!(written.contains("requires_openai_auth = false"));
        assert!(
            written.contains("experimental_bearer_token = \"sk-test-secret\""),
            "direct=Key 落盘(阶段 1 定稿差异,与网关零 Key 相反):\n{written}"
        );
        assert!(
            !written.contains("127.0.0.1:8787"),
            "direct 不经网关:\n{written}"
        );
        assert!(
            written.contains("my_custom_setting"),
            "用户字段应保留:\n{written}"
        );
        assert_eq!(
            crate::providers::load(&prov).active_provider_id,
            Some("p1".into())
        );
        // direct 不动 auth(bearer 已在 config):不创建 auth.json、不留备份
        assert!(!home.join("auth.json").exists());
        assert!(!home.join(AUTH_OFFICIAL_BAK).exists());
        // 首次 direct 托管留 pre-host 快照(unhost 据此还原)
        assert!(find_pre_host_snapshot(&bk, &cfg).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ② 幂等:重复 host 同供应商 + direct → no-op 200,config 不变。
    #[test]
    fn host_direct_idempotent_same_provider() {
        let (root, cfg, bk, home, prov) = sandbox("host-direct-idem");
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        let r1 = host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        assert!(r1["changed"]["config"].as_bool().unwrap());
        let before = std::fs::read_to_string(&cfg).unwrap();
        let r2 = host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        assert!(r2["hosted"].as_bool().unwrap(), "重复 host 应 200 no-op");
        assert!(
            !r2["changed"]["config"].as_bool().unwrap(),
            "config 不应重写"
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            before,
            "config 不应变化"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ③ 有官方账号 host direct → 4xx E_DIRECT_UNAVAILABLE,config 一字不落(阶段 5 实测后再放开)。
    #[test]
    fn host_direct_rejected_with_official() {
        let (root, cfg, bk, home, prov) = sandbox("host-direct-official");
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"id_token":"official"}}"#,
        )
        .unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        let err = host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap_err();
        assert_eq!(err.0, 400);
        assert_eq!(err.1, "E_DIRECT_UNAVAILABLE");
        assert!(!err.2.is_empty(), "4xx 消息须为人话,不可为空");
        assert!(!cfg.exists(), "被拒的 host 不应写 config");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 反向换路:direct 托管 → host gateway → unhost 仍还原到最初 pre-host
    /// (gateway 全量写在已托管态用 pre-switch 备份,不把 direct 态快照成 pre-host,
    /// 否则 unhost 会"还原"回带 key 的 direct 配置、托管态解不开)。
    #[test]
    fn host_gateway_from_direct_keeps_pre_host_snapshot() {
        let (root, cfg, bk, home, prov) = sandbox("direct-back");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        let _r = host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert_eq!(detect_hosting(&cfg, &prov)["way"], "gateway");

        unhost(&cfg, &bk, &home, &prov).unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            after.contains("my_custom_setting"),
            "应还原到最初 pre-host 而非 direct 态:\n{after}"
        );
        assert!(!after.contains("experimental_bearer_token"));
        assert!(!after.contains("[model_providers.custom]"));
        assert!(
            detect_hosting(&cfg, &prov).is_null(),
            "unhost 后不应残留托管态:\n{}",
            detect_hosting(&cfg, &prov)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ④ state 检测:direct 托管后 hosting={way:"direct", providerId, providerName}。
    #[test]
    fn state_reports_direct_hosting() {
        let (root, cfg, bk, home, prov) = sandbox("state-direct");
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        let s = state(&cfg, &prov, &home);
        assert!(!s["hasOfficial"].as_bool().unwrap());
        assert_eq!(s["hosting"]["way"], "direct", "state 应报 direct:\n{s}");
        assert_eq!(s["hosting"]["providerId"], "p1");
        assert_eq!(s["hosting"]["providerName"], "2xapi");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⑤ unhost 清理 direct 托管:experimental_bearer_token 随 custom 段一并清除,幂等。
    #[test]
    fn unhost_cleans_direct_hosting() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-direct");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();

        let out = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out["restored"].as_bool().unwrap());
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            !written.contains("experimental_bearer_token"),
            "bearer 应随 custom 段清理:\n{written}"
        );
        assert!(!written.contains("[model_providers.custom]"));
        assert!(!written.contains("model_provider ="));
        assert!(
            !written.contains("sk-test-secret"),
            "key 不应残留:\n{written}"
        );
        assert!(
            written.contains("my_custom_setting"),
            "用户字段应保留:\n{written}"
        );
        assert!(crate::providers::load(&prov).active_provider_id.is_none());
        // 幂等
        let out2 = unhost(&cfg, &bk, &home, &prov).unwrap();
        assert!(out2["alreadyClean"].as_bool().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 换路:gateway 托管 → host direct → custom 段切到供应商直连;unhost 仍还原到
    /// 最初 pre-host 快照(pre-switch 备份不污染快照链)。
    #[test]
    fn host_direct_switch_from_gateway_and_unhost_restores() {
        let (root, cfg, bk, home, prov) = sandbox("direct-switch");
        std::fs::write(&cfg, "my_custom_setting = \"keep_me\"\n").unwrap();
        write_providers(&prov, vec![provider("p1", "2xapi")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert_eq!(detect_hosting(&cfg, &prov)["way"], "gateway");

        let r = host(&cfg, &bk, &home, &prov, "p1", "direct").unwrap();
        assert!(
            r["switched"].as_bool().unwrap(),
            "已托管态换路应报 switched"
        );
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            written.contains("base_url = \"https://up.example.com\""),
            "custom 应切到供应商直连:\n{written}"
        );
        assert!(written.contains("experimental_bearer_token = \"sk-test-secret\""));
        assert_eq!(detect_hosting(&cfg, &prov)["way"], "direct");

        // unhost 还原到最初 pre-host(用户原始字段回来,bearer 不残留)
        unhost(&cfg, &bk, &home, &prov).unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            after.contains("my_custom_setting"),
            "应还原到最初 pre-host:\n{after}"
        );
        assert!(!after.contains("experimental_bearer_token"));
        assert!(!after.contains("[model_providers.custom]"));
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
        let custom_before: String = before
            .split("[model_providers.custom]")
            .nth(1)
            .unwrap()
            .to_string();
        let r = host(&cfg, &bk, &home, &prov, "p2", "gateway").unwrap();
        assert!(r["switched"].as_bool().unwrap());
        let after = std::fs::read_to_string(&cfg).unwrap();
        let custom_after: String = after
            .split("[model_providers.custom]")
            .nth(1)
            .unwrap()
            .lines()
            .take(5)
            .collect::<String>();
        assert!(
            after.contains("[model_providers.custom]")
                && after.contains("base_url = \"http://127.0.0.1:8787\""),
            "custom 段应保留(网关指向不变):\n{after}"
        );
        assert!(
            after.contains("model = \"model-b\""),
            "换供应商应同步 model(真机故障:旧模型名发给新上游):\n{after}"
        );
        assert_eq!(
            custom_before.lines().take(5).collect::<String>(),
            custom_after,
            "custom 段内容不应变化"
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
        // gateway 托管 + active
        std::fs::write(&cfg, "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:8787\"\n").unwrap();
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
        // 此前托管过、后来清空供应商 → config 残留网关 custom 段,仍必须未托管(null)
        std::fs::write(
            &cfg,
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:8787\"\n",
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
        assert!(!written.contains("[model_providers.custom]"));
        assert!(!written.contains("model_provider ="));
        assert!(!written.contains("model_catalog_json"));
        assert!(!written.contains("model ="));
        assert!(!home.join(MODEL_CATALOG_FILENAME).exists());
        // auth 回到 host 前的别家 key
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
        assert_eq!(out["way"], "official");
        let written = std::fs::read_to_string(&cfg).unwrap();
        assert!(written.contains("model_provider = \"openai\""));
        assert!(!written.contains("[model_providers.custom]"));
        assert!(std::fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("OFFICIAL"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unhost_without_bak_removes_only_our_key() {
        let (root, cfg, bk, home, prov) = sandbox("unhost-nobak");
        // host 前无 auth.json → 无 .bak;host 后 auth 只有我们写的 key
        write_providers(&prov, vec![provider("p1", "A")]);
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert!(!home.join(AUTH_OFFICIAL_BAK).exists());

        unhost(&cfg, &bk, &home, &prov).unwrap();
        let auth = read_auth_json(&home.join("auth.json"));
        assert!(
            auth.get("OPENAI_API_KEY").is_none(),
            "我们写的 key 应被移除:\n{auth}"
        );
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

        // host 产生 pre-host 快照
        host(&cfg, &bk, &home, &prov, "p1", "gateway").unwrap();
        assert!(
            find_pre_host_snapshot(&bk, &cfg).is_some(),
            "host 应留下 pre-host 快照"
        );

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
