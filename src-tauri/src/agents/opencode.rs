//! OpenCode adapter(叠加托管,D1 拍板形态;侦察实证 2026-08-16 沙盒全闭环)。
//!
//! 载体:`~/.config/opencode/opencode.json` 的 `provider["2xapi-gateway"]` 条目(upsert);
//! 用户已有条目与 plugin 段零触碰;默认模型指针按 D1——仅原值为空/缺失才切,否则不动并
//! 响应 suggested 提示(真机默认=第三方 custom 条目)。占位 Key 实证零校验照发 → 真实 Key
//! 只在网关。jsonc(opencode.jsonc 深度合并覆盖 json)存在同 id 条目时仅告警不阻断。
//!
//! 已知首版边界(交接日志备案):不尊重 XDG_CONFIG_HOME 重定向;opencode.json 含注释
//! (非法 JSON)时拒绝写入而非保格式合并。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub type OpError = (u16, String, String);

const GATEWAY_BASE: &str = "http://127.0.0.1:8787";
const PLACEHOLDER_KEY: &str = "2xapi-gateway-managed";
pub const PROVIDER_ID: &str = "2xapi-gateway";

/// 配置文件路径:`<oc_home>/.config/opencode/opencode.json`(oc_home=HOME 根,测试传 tempdir)。
pub fn config_path(oc_home: &Path) -> PathBuf {
    oc_home
        .join(".config")
        .join("opencode")
        .join("opencode.json")
}

/// 模型 id slug 化(引用形态 `2xapi-gateway/<slug>`;非法字符归并为 `-`)。
fn slug(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "model".into()
    } else {
        s
    }
}

fn read_root(oc_home: &Path) -> Result<Map<String, Value>, OpError> {
    let path = config_path(oc_home);
    if !path.exists() {
        return Ok(Map::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| {
        (
            422,
            "E_CONFIG_PARSE".into(),
            "opencode.json 含注释或非法 JSON,暂不支持安全合并,请先整理为标准 JSON".into(),
        )
    })?;
    Ok(v.as_object().cloned().unwrap_or_default())
}

fn write_root(
    oc_home: &Path,
    backup_dir: &Path,
    root: &Map<String, Value>,
    purpose: &str,
) -> Result<bool, OpError> {
    let path = config_path(oc_home);
    let new_text = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    if path.exists() {
        let cur = std::fs::read_to_string(&path).unwrap_or_default();
        if cur == new_text {
            return Ok(false); // 幂等:无变化不写盘
        }
        crate::config::backup_file(&path, backup_dir, "config-apply", purpose)
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    } else if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    std::fs::write(&path, new_text).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    Ok(true)
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
    crate::desktop::validate_provider_agent(&provider, "opencode")?;
    Ok(provider)
}

/// 托管态:我们条目是否存在 + 指针归属 + jsonc 冲突告警。
pub fn state(oc_home: &Path) -> Value {
    let root = match read_root(oc_home) {
        Ok(r) => r,
        Err((_, code, msg)) => return json!({ "hosting": null, "warn": format!("[{code}] {msg}") }),
    };
    let entry = root.get("provider").and_then(|p| p.get(PROVIDER_ID));
    let hosting = entry.map(|e| {
        json!({
            "providerId": PROVIDER_ID,
            "models": e.get("models").cloned().unwrap_or_else(|| json!({})),
            "options": e.get("options").map(|o| json!({"baseURL": o.get("baseURL")})).unwrap_or(json!({})),
        })
    });
    let default_model = root.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let jsonc_conflict = {
        let jsonc = oc_home
            .join(".config")
            .join("opencode")
            .join("opencode.jsonc");
        jsonc.exists()
            && std::fs::read_to_string(&jsonc)
                .map(|t| t.contains(PROVIDER_ID))
                .unwrap_or(false)
    };
    json!({
        "hosting": hosting,
        "defaultModel": default_model,
        "defaultModelIsOurs": default_model.starts_with(&format!("{PROVIDER_ID}/")),
        "jsoncConflict": jsonc_conflict,
    })
}

pub fn host(
    oc_home: &Path,
    backup_dir: &Path,
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

    let mut root = read_root(oc_home)?;
    let (base_url, api_key, key_note) = if way == "gateway" {
        (
            format!("{GATEWAY_BASE}/opencode/v1"),
            PLACEHOLDER_KEY.to_string(),
            "占位(真实 Key 只在网关)",
        )
    } else {
        let b = provider.base_url.trim().trim_end_matches('/').to_string();
        (
            b,
            provider.api_key.clone(),
            "直连:真实 Key 落盘于 opencode.json",
        )
    };

    let model_ids: Vec<(String, String)> = if provider.models.is_empty() {
        vec![(slug(&provider.model), provider.model.clone())]
    } else {
        provider
            .models
            .iter()
            .map(|m| (slug(&m.name), m.name.clone()))
            .collect()
    };
    let mut models = Map::new();
    for (id, name) in &model_ids {
        models.insert(id.clone(), json!({ "name": name }));
    }
    let entry = json!({
        "name": "2xapi 网关",
        "npm": "@ai-sdk/openai-compatible",
        "options": { "apiKey": api_key, "baseURL": base_url },
        "models": models,
    });
    let providers = root
        .entry("provider".to_string())
        .or_insert_with(|| json!({}));
    providers
        .as_object_mut()
        .ok_or_else(|| {
            (
                422,
                "E_CONFIG_PARSE".into(),
                "provider 段存在但不是对象,拒绝写入".into(),
            )
        })?
        .insert(PROVIDER_ID.into(), entry);

    // D1:默认指针仅原值空/缺失才切;否则不动并建议(suggested 供前端提示「已写入,可在 opencode 内选择」)
    let existing_model = root
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut switched = false;
    if existing_model.is_empty() {
        root.insert(
            "model".to_string(),
            json!(format!("{PROVIDER_ID}/{}", model_ids[0].0)),
        );
        switched = true;
    }

    let paths = [config_path(oc_home), providers_path.to_path_buf()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let written = write_root(
            oc_home,
            backup_dir,
            &root,
            if switched { "pre-host" } else { "pre-switch" },
        )?;
        crate::desktop::set_active_checked(providers_path, &provider, "opencode")?;
        Ok(json!({
            "hosted": true, "way": way, "switched": !existing_model.is_empty(),
            "defaultModelSwitched": switched,
            "suggested": !switched,
            "changed": { "config": written },
            "keyNote": key_note,
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

pub fn unhost(oc_home: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let providers_path = crate::desktop::providers_path_from_backup_dir(backup_dir);
    let mut root = read_root(oc_home)?;
    let ours_prefix = format!("{PROVIDER_ID}/");
    let removed = root
        .get_mut("provider")
        .and_then(|p| p.as_object_mut())
        .map(|m| m.remove(PROVIDER_ID).is_some())
        .unwrap_or(false);
    if !removed {
        crate::desktop::clear_active_checked(&providers_path, "opencode")?;
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }
    // 指针仅在原值空/缺失时才会被我们设置 → 指向我们即移除(恢复「未设置」)
    let pointer_removed = root
        .get("model")
        .and_then(|v| v.as_str())
        .map(|m| m.starts_with(&ours_prefix))
        .unwrap_or(false);
    if pointer_removed {
        root.remove("model");
    }
    let paths = [config_path(oc_home), providers_path.clone()];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = (|| {
        let written = write_root(oc_home, backup_dir, &root, "pre-unhost")?;
        crate::desktop::clear_active_checked(&providers_path, "opencode")?;
        Ok(
            json!({ "restored": true, "changed": { "config": written }, "defaultModelRemoved": pointer_removed }),
        )
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// POST /api/desktop/opencode/start —— 未托管 409;托管后返回直接运行提示
/// (opencode 为整平台托管,条目含真实 base/key,命令本体无需 env 前缀,providerId 仅回显)。
pub fn start(oc_home: &Path, providers_path: &Path, provider_id: &str) -> Result<Value, OpError> {
    if state(oc_home)["hosting"].is_null() {
        return Err((409, "E_NOT_HOSTED".into(), "请先托管,再启动".into()));
    }
    super::cli_start_response(providers_path, provider_id, "opencode run")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str, agent: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("2xapi-opencode-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let backup = root.join("backups");
        let providers = root.join("providers.json");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(
            &providers,
            json!({
                "providers": [{
                    "id": "p1", "name": "test", "agent": agent,
                    "base_url": "https://up.example.com/v1", "api_key": "sk-test",
                    "model": "gpt-test"
                }]
            })
            .to_string(),
        )
        .unwrap();
        (home, backup, providers)
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let (home, backup, providers) = setup("foreign", "codex");

        let error = host(&home, &backup, &providers, "p1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!config_path(&home).exists());
    }

    #[test]
    fn host_rolls_back_config_when_active_write_fails() {
        let (home, backup, providers) = setup("active-rollback", "opencode");
        let original = json!({ "keep": true });
        std::fs::create_dir_all(config_path(&home).parent().unwrap()).unwrap();
        std::fs::write(config_path(&home), original.to_string()).unwrap();
        std::fs::create_dir(providers.with_extension("json.tmp")).unwrap();

        let error = host(&home, &backup, &providers, "p1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_ACTIVE_WRITE");
        assert_eq!(
            read_root(&home).unwrap(),
            original.as_object().unwrap().clone()
        );
    }

    #[test]
    fn unhost_clears_opencode_active() {
        let (home, backup, providers) = setup("unhost-active", "opencode");
        host(&home, &backup, &providers, "p1", "gateway").unwrap();
        assert!(crate::providers::get_active_for_agent(&providers, "opencode").is_some());

        unhost(&home, &backup).unwrap();

        assert!(crate::providers::get_active_for_agent(&providers, "opencode").is_none());
    }
}
