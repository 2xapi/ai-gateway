//! Gemini CLI adapter(多平台方案 §3.1,阶段 C 第二段;探索结论 2026-08-16)
//!
//! 载体:`~/.gemini/.env`(三键)+ `~/.gemini/settings.json`(`security.auth.selectedType`)。
//! - 受控三键:`GOOGLE_GEMINI_BASE_URL` / `GEMINI_API_KEY` / `GEMINI_MODEL`
//!   (env 名一手实证:gemini-cli Issue #1679 维护者确认 + cc-switch 20 家预设一致 + 本机 CLI 实测)
//! - **坑 #15430**:存在 OAuth 缓存登录态时 CLI 忽略 `GOOGLE_GEMINI_BASE_URL` → host 必须同步
//!   `selectedType: "gemini-api-key"`;unhost 恢复 host 前快照值(sidecar 记录)
//! - Key 语义(对齐 codex direct / claude 注入式先例):gateway 方式 `.env` 写占位(真 Key 只进网关,
//!   网关按 agent=gemini 取供应商注入);direct 方式写真实 Key(CLI 只讲 Gemini 协议,仅 wire=gemini
//!   供应商可选 direct)
//! - `.env` 布局保留:upsert/删除都逐行处理,用户注释、空行、其余键零触碰(cc-switch 手法)

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 网关入口根(CLI 自动拼 /v1beta/models/…;实测:base 指 9111 → 打 {base}/v1beta/…)。
const GATEWAY_BASE: &str = "http://127.0.0.1:8787";
/// gateway 方式写入 .env 的占位 Key(真实 Key 只在网关;CLI 原样透传此值,网关忽略)。
const PLACEHOLDER_KEY: &str = "2xapi-gateway-managed";
/// host 写入 .env 的受控三键。
const OUR_KEYS: [&str; 3] = ["GOOGLE_GEMINI_BASE_URL", "GEMINI_API_KEY", "GEMINI_MODEL"];
/// sidecar:本产品托管前状态快照(受控还原依据;unhost 后删除)。
const SIDECAR: &str = ".2xapi-gateway-state.json";

type OpError = (u16, String, String);

fn gemini_dir(home: &Path) -> PathBuf {
    home.join(".gemini")
}
fn env_path(home: &Path) -> PathBuf {
    gemini_dir(home).join(".env")
}
fn settings_path(home: &Path) -> PathBuf {
    gemini_dir(home).join("settings.json")
}
fn sidecar_path(home: &Path) -> PathBuf {
    gemini_dir(home).join(SIDECAR)
}

// ── .env 逐行处理(保布局)──────────────────────────────────

/// 从 .env 文本提取某键当前值(宽松解析,同 cc-switch:跳过注释/无效行)。
fn env_get(content: &str, key: &str) -> Option<String> {
    content.lines().find_map(|l| {
        let t = l.trim();
        if t.starts_with('#') {
            return None;
        }
        t.split_once('=')
            .and_then(|(k, v)| (k.trim() == key).then(|| v.trim().to_string()))
    })
}

/// upsert 键值:同名行(任意值)只保留首行并替换为新值,其余同名行删除;未出现的键追加到尾部。
/// 注释/空行/其余键逐字保留。
fn env_upsert(content: &str, updates: &[(&str, String)]) -> String {
    let mut lines: Vec<String> = content
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();
    for (key, val) in updates {
        let mut replaced = false;
        let mut kept: Vec<String> = Vec::new();
        for line in lines.drain(..) {
            let t = line.trim();
            let is_key = !t.starts_with('#')
                && t.split_once('=')
                    .map(|(k, _)| k.trim() == *key)
                    .unwrap_or(false);
            if is_key && !replaced {
                kept.push(format!("{key}={val}"));
                replaced = true;
            } else if is_key {
                // 重复同名行:last-wins 语义下本就被遮蔽,清理
            } else {
                kept.push(line);
            }
        }
        lines = kept;
        if !replaced {
            lines.push(format!("{key}={val}"));
        }
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// 定向删键(任意值匹配即删该行,同名多行全删;其余逐字保留)。无命中返回 None。
fn env_remove_keys(content: &str, keys: &[&str]) -> Option<String> {
    let mut removed = false;
    let kept: Vec<&str> = content
        .split('\n')
        .filter(|l| {
            let t = l.trim();
            let hit = !t.is_empty() && !t.starts_with('#') && {
                t.split_once('=')
                    .map(|(k, _)| keys.contains(&k.trim()))
                    .unwrap_or(false)
            };
            if hit {
                removed = true;
            }
            !hit
        })
        .collect();
    removed.then(|| kept.join("\n"))
}

/// 读 .env;坏 IO → E_IO(不存在按空)。
fn read_env(home: &Path) -> Result<String, OpError> {
    let p = env_path(home);
    if !p.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(&p).map_err(|e| (500, "E_IO".into(), e.to_string()))
}

/// 读 settings.json(不存在→null;坏 JSON→E_PARSE 拒碰)。
fn read_settings(home: &Path) -> Result<Option<Value>, OpError> {
    let p = settings_path(home);
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    serde_json::from_str(&raw).map(Some).map_err(|_| {
        (
            422,
            "E_PARSE".into(),
            format!(
                "{} 不是合法 JSON,请先手动修复(本产品不改动坏文件)",
                p.display()
            ),
        )
    })
}

/// 原子写(临时文件+rename;目录 700、文件 600,cc-switch 同款)。
fn write_atomic(path: &Path, content: &str) -> Result<(), OpError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| (500, "E_IO".into(), e.to_string()))
}

// ── 四接口(泛化路由分发:server.rs /api/desktop/gemini/*)──

/// POST /api/desktop/gemini/host {providerId, way}
pub fn host(
    gem_home: &Path,
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
    crate::desktop::validate_provider_agent(&provider, "gemini")?;
    if provider.model.is_empty() {
        return Err((
            422,
            "E_NO_MODEL".into(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".into(),
        ));
    }
    if way == "direct" && provider.wire_api != crate::providers::WireApi::Gemini {
        return Err((400, "E_DIRECT_NEEDS_GEMINI".into(),
            "直连方式要求协议为 Gemini 的供应商(Gemini CLI 只讲 Google 协议);Chat 协议供应商请走网关(自动转换)".into()));
    }
    let (base, key, key_hint) = if way == "gateway" {
        (
            GATEWAY_BASE.to_string(),
            PLACEHOLDER_KEY.to_string(),
            "占位(真实 Key 只在网关)",
        )
    } else {
        (
            provider.base_url.trim_end_matches('/').to_string(),
            provider.api_key.clone(),
            "真实 Key(落盘)",
        )
    };

    // sidecar:首次 host 记录还原基线;重复 host(换供应商/way)保留最初快照并更新托管指纹。
    let sp = sidecar_path(gem_home);
    let mut snap = if sp.exists() {
        let raw = std::fs::read_to_string(&sp)
            .map_err(|e| (500, "E_IO".into(), format!("读取 sidecar 失败: {e}")))?;
        serde_json::from_str::<Value>(&raw)
            .ok()
            .filter(Value::is_object)
            .ok_or((
                422,
                "E_PARSE".into(),
                "Gemini 托管 sidecar 损坏,为避免误删用户配置已中止".into(),
            ))?
    } else {
        let env_raw = read_env(gem_home)?;
        let settings = read_settings(gem_home)?;
        let prev_env: serde_json::Map<String, Value> = OUR_KEYS
            .iter()
            .map(|k| {
                (
                    k.to_string(),
                    env_get(&env_raw, k)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                )
            })
            .collect();
        json!({
            "prev_env": Value::Object(prev_env),
            "prev_selected_type": settings
                .as_ref()
                .and_then(|s| s.pointer("/security/auth/selectedType").cloned())
                .unwrap_or(Value::Null),
        })
    };
    snap["way"] = json!(way);
    snap["provider_id"] = json!(provider.id);
    snap["managed_env"] = json!({
        "GOOGLE_GEMINI_BASE_URL": base,
        "GEMINI_API_KEY": key,
        "GEMINI_MODEL": provider.model,
    });
    // 三保险:全部载体先计算并快照,备份全部成功后再写；任一步失败恢复所有 live 文件。
    let ep = env_path(gem_home);
    let env_raw = read_env(gem_home)?;
    let updates = [
        ("GOOGLE_GEMINI_BASE_URL", base),
        ("GEMINI_API_KEY", key),
        ("GEMINI_MODEL", provider.model.clone()),
    ];
    let updates_ref: Vec<(&str, String)> = updates.iter().map(|(k, v)| (*k, v.clone())).collect();
    let target_env = env_upsert(&env_raw, &updates_ref);
    let stp = settings_path(gem_home);
    let mut settings = read_settings(gem_home)?.unwrap_or(json!({}));
    if let Some(obj) = settings.as_object_mut() {
        let security = obj.entry("security").or_insert(json!({}));
        if let Some(sec) = security.as_object_mut() {
            let auth = sec.entry("auth").or_insert(json!({}));
            if let Some(a) = auth.as_object_mut() {
                a.insert("selectedType".into(), json!("gemini-api-key"));
            }
        }
    }
    let target_settings =
        serde_json::to_string_pretty(&settings).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    let paths = [
        sp.clone(),
        ep.clone(),
        stp.clone(),
        providers_path.to_path_buf(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    std::fs::create_dir_all(backup_dir).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    if ep.exists() {
        crate::config::backup_file(&ep, backup_dir, "gemini-env", "pre-host")
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    if stp.exists() {
        crate::config::backup_file(&stp, backup_dir, "gemini-settings", "pre-host")
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    let outcome = (|| {
        write_atomic(&sp, &snap.to_string())?;
        write_atomic(&ep, &target_env)?;
        write_atomic(&stp, &target_settings)?;
        crate::desktop::set_active_checked(providers_path, &provider, "gemini")?;
        Ok(json!({
            "hosted": true,
            "way": way,
            "providerId": provider.id,
            "envKeys": OUR_KEYS,
            "keyHint": key_hint,
            "hint": if way == "gateway" {
                "已写入 ~/.gemini/.env 指向网关(真实 Key 只在网关);终端直接运行 gemini 即生效"
            } else {
                "直连方式:Key 已写入 ~/.gemini/.env(与网关零 Key 不同,注意保管)"
            },
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// POST /api/desktop/gemini/unhost —— 受控还原:三键恢复 host 前快照值(无原值则删行),
/// settings.json selectedType 同步恢复(sidecar 缺失时兜底 oauth-personal),sidecar 移除。
pub fn unhost(gem_home: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let sidecar = sidecar_path(gem_home);
    let providers_path = crate::desktop::providers_path_from_backup_dir(backup_dir);
    if !sidecar.exists() {
        crate::desktop::clear_active_checked(&providers_path, "gemini")?;
        return Ok(json!({ "hosted": false, "restored": false, "alreadyClean": true }));
    }
    let snap: Value = serde_json::from_str(
        &std::fs::read_to_string(&sidecar)
            .map_err(|e| (500, "E_IO".into(), format!("读取 sidecar 失败: {e}")))?,
    )
    .map_err(|e| {
        (
            422,
            "E_PARSE".into(),
            format!("Gemini 托管 sidecar 损坏: {e}"),
        )
    })?;
    if !snap.is_object() {
        return Err((
            422,
            "E_PARSE".into(),
            "Gemini 托管 sidecar 顶层不是对象".into(),
        ));
    }
    let ep = env_path(gem_home);
    let target_env = if ep.exists() {
        let raw = read_env(gem_home)?;
        let mut cur = raw;
        for k in OUR_KEYS {
            let prev = snap.pointer(&format!("/prev_env/{k}")).cloned();
            match prev {
                Some(Value::String(v)) => cur = env_upsert(&cur, &[(k, v)]),
                _ => {
                    if let Some(cleaned) = env_remove_keys(&cur, &[k]) {
                        cur = cleaned;
                    }
                }
            }
        }
        Some(cur)
    } else {
        None
    };
    let stp = settings_path(gem_home);
    let target_settings = if stp.exists() {
        if let Some(mut settings) = read_settings(gem_home)? {
            let prev = snap
                .pointer("/prev_selected_type")
                .cloned()
                .unwrap_or(Value::Null);
            let restore = if prev.is_null() {
                let cur = settings
                    .pointer("/security/auth/selectedType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if cur == "gemini-api-key" {
                    json!("oauth-personal")
                } else {
                    Value::Null
                }
            } else {
                prev
            };
            if let Some(obj) = settings.as_object_mut() {
                if let Some(sec) = obj.get_mut("security").and_then(|v| v.as_object_mut()) {
                    if let Some(a) = sec.get_mut("auth").and_then(|v| v.as_object_mut()) {
                        if restore.is_null() {
                            a.remove("selectedType");
                        } else {
                            a.insert("selectedType".into(), restore);
                        }
                    }
                }
            }
            Some(
                serde_json::to_string_pretty(&settings)
                    .map_err(|e| (500, "E_IO".into(), e.to_string()))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    let paths = [
        sidecar.clone(),
        ep.clone(),
        stp.clone(),
        providers_path.clone(),
    ];
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    if ep.exists() {
        crate::config::backup_file(&ep, backup_dir, "gemini-env", "pre-unhost")
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    if stp.exists() {
        crate::config::backup_file(&stp, backup_dir, "gemini-settings", "pre-unhost")
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    let outcome = (|| {
        if let Some(content) = &target_env {
            write_atomic(&ep, content)?;
        }
        if let Some(content) = &target_settings {
            write_atomic(&stp, content)?;
        }
        crate::desktop::clear_active_checked(&providers_path, "gemini")?;
        std::fs::remove_file(&sidecar)
            .map_err(|e| (500, "E_IO".into(), format!("删除 sidecar 失败: {e}")))?;
        Ok(json!({ "hosted": false, "restored": snap.as_object().is_some_and(|o| !o.is_empty()) }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// GET /api/desktop/gemini/state —— 托管态(.env 含受控键)+ 认证形态 + CLI 安装检测。
/// hosting 契约对齐 B 阶段通用世界(grokbuild/opencode 等:`{way,…}|null`);hosted 保留兼容。
pub fn state(gem_home: &Path) -> Value {
    let raw = read_env(gem_home).unwrap_or_default();
    let sidecar = std::fs::read_to_string(sidecar_path(gem_home))
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .filter(Value::is_object);
    let auth_type = read_settings(gem_home).ok().flatten().and_then(|s| {
        s.pointer("/security/auth/selectedType")
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    let managed_matches = sidecar.as_ref().is_some_and(|snapshot| {
        let managed = snapshot.get("managed_env").and_then(Value::as_object);
        OUR_KEYS.iter().all(|key| {
            let current = env_get(&raw, key);
            match managed
                .and_then(|values| values.get(*key))
                .and_then(Value::as_str)
            {
                Some(expected) => current.as_deref() == Some(expected),
                None => current.is_some(),
            }
        })
    });
    let hosted =
        sidecar.is_some() && managed_matches && auth_type.as_deref() == Some("gemini-api-key");
    let way = sidecar
        .as_ref()
        .and_then(|v| v.get("way"))
        .and_then(Value::as_str)
        .map(String::from);
    json!({
        "agent": "gemini",
        "hosting": if hosted {
            json!({ "way": way.unwrap_or_else(|| "gateway".into()), "authType": "gemini-api-key" })
        } else {
            Value::Null
        },
        "hosted": hosted,
        "authType": auth_type,
        "installed": which_gemini().is_some(),
        "model": env_get(&raw, "GEMINI_MODEL"),
    })
}

/// PATH 及常见安装位找 gemini CLI(只做 UI 提示)。
fn which_gemini() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "gemini.exe"
    } else {
        "gemini"
    };
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    [
        format!("{home}/.local/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        format!("/opt/homebrew/bin/{name}"),
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// POST /api/desktop/gemini/start —— 注入式启动命令(可复制;direct 方式 Key 出现在命令中,提示保管)。
pub fn start(
    providers_path: &Path,
    way: &str,
    provider_id: &str,
    gem_home: &Path,
) -> Result<Value, OpError> {
    let hosted = state(gem_home)
        .get("hosted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !hosted {
        return Err((409, "E_NOT_HOSTED".into(), "请先托管,再启动".into()));
    }
    let p = if !provider_id.trim().is_empty() {
        let data = crate::providers::load(providers_path);
        data.providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or((
                404u16,
                "E_NO_PROVIDER".to_string(),
                "供应商不存在".to_string(),
            ))?
    } else {
        crate::providers::get_provider_for_agent(providers_path, "gemini").ok_or((
            503u16,
            "E_NO_GEMINI_PROVIDER".to_string(),
            "请先选择 Gemini 供应商".to_string(),
        ))?
    };
    let (base, key) = if way == "direct" {
        (
            p.base_url.trim_end_matches('/').to_string(),
            p.api_key.clone(),
        )
    } else {
        (GATEWAY_BASE.to_string(), PLACEHOLDER_KEY.to_string())
    };
    let envs = json!([
        ["GOOGLE_GEMINI_BASE_URL", base],
        ["GEMINI_API_KEY", key],
        ["GEMINI_MODEL", p.model],
    ]);
    let command = start_command(&base, &key, &p.model, cfg!(windows));
    Ok(json!({
        "command": command,
        "env": envs,
        "way": way,
        "providerId": p.id,
        "providerName": p.name,
        "keyMasked": if key.len() > 8 { format!("{}...{}", &key[..5], &key[key.len()-4..]) } else { key.clone() },
        "hint": if way == "direct" { "直连命令含真实 Key,复制时注意保管" } else { "网关方式:命令中的 Key 为占位,真实 Key 只在网关" },
    }))
}

fn start_command(base: &str, key: &str, model: &str, windows: bool) -> String {
    if windows {
        let quote = |value: &str| value.replace('\'', "''");
        format!(
            "powershell -NoProfile -Command \"$env:GOOGLE_GEMINI_BASE_URL='{}'; $env:GEMINI_API_KEY='{}'; $env:GEMINI_MODEL='{}'; gemini\"",
            quote(base), quote(key), quote(model)
        )
    } else {
        let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        format!(
            "GOOGLE_GEMINI_BASE_URL={} GEMINI_API_KEY={} GEMINI_MODEL={} gemini",
            quote(base),
            quote(key),
            quote(model)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gem-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_provider_file(dir: &Path, wire: &str) -> PathBuf {
        let p = dir.join("providers.json");
        fs::write(
            &p,
            serde_json::json!({
                "providers": [{
                    "id": "pv1", "name": "测试站", "agent": "gemini",
                    "base_url": "https://example.com", "api_key": "sk-test-key",
                    "model": "gemini-2.5-flash", "wire_api": wire,
                }]
            })
            .to_string(),
        )
        .unwrap();
        p
    }

    /// host(gateway):三键写入 + selectedType=gemini-api-key;用户注释/其余键/用户 selectedType 其他字段零触碰;幂等。
    #[test]
    fn host_gateway_writes_keys_and_selected_type() {
        let home = tmp("host1");
        let pp = write_provider_file(&home, "chat_completions");
        fs::create_dir_all(gemini_dir(&home)).unwrap();
        // 用户已有 .env:注释 + 无关键;settings 有用户字段
        fs::write(
            env_path(&home),
            "# 用户注释\nGOOGLE_CLOUD_PROJECT=my-proj\nGEMINI_API_KEY=user-own-key\n",
        )
        .unwrap();
        fs::write(
            settings_path(&home),
            r#"{"theme":"dark","security":{"auth":{"oauthPrev":"x"}}}"#,
        )
        .unwrap();

        let v = host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        assert_eq!(v["hosted"], json!(true));

        let env = fs::read_to_string(env_path(&home)).unwrap();
        assert!(env.contains("# 用户注释"), "注释保留:\n{env}");
        assert!(
            env.contains("GOOGLE_CLOUD_PROJECT=my-proj"),
            "无关键保留:\n{env}"
        );
        assert_eq!(
            env_get(&env, "GOOGLE_GEMINI_BASE_URL").unwrap(),
            GATEWAY_BASE
        );
        assert_eq!(
            env_get(&env, "GEMINI_API_KEY").unwrap(),
            PLACEHOLDER_KEY,
            "用户原 key 被受控覆盖(网关占位)"
        );
        assert_eq!(env_get(&env, "GEMINI_MODEL").unwrap(), "gemini-2.5-flash");

        let st: Value =
            serde_json::from_str(&fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(st["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(st["theme"], "dark", "用户字段保留");
        assert_eq!(
            st["security"]["auth"]["oauthPrev"], "x",
            "auth 其他字段保留"
        );

        // 幂等:同参再 host,sidecar 保留最初快照(原 GEMINI_API_KEY=user-own-key)
        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let snap: Value =
            serde_json::from_str(&fs::read_to_string(sidecar_path(&home)).unwrap()).unwrap();
        assert_eq!(
            snap["prev_env"]["GEMINI_API_KEY"], "user-own-key",
            "重复 host 不得覆盖最初快照"
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// host(direct):要求 wire=gemini 供应商;写上游地址与真实 Key。
    #[test]
    fn host_direct_needs_gemini_wire() {
        let home = tmp("direct");
        let pp_chat = write_provider_file(&home, "chat_completions");
        let err = host(&home, &home.join("bk"), &pp_chat, "pv1", "direct").unwrap_err();
        assert_eq!(err.1, "E_DIRECT_NEEDS_GEMINI");

        let pp_gem = write_provider_file(&home, "gemini");
        let v = host(&home, &home.join("bk"), &pp_gem, "pv1", "direct").unwrap();
        assert_eq!(v["hosted"], json!(true));
        let env = fs::read_to_string(env_path(&home)).unwrap();
        assert_eq!(
            env_get(&env, "GOOGLE_GEMINI_BASE_URL").unwrap(),
            "https://example.com"
        );
        assert_eq!(env_get(&env, "GEMINI_API_KEY").unwrap(), "sk-test-key");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let home = tmp("foreign-agent");
        let pp = write_provider_file(&home, "chat_completions");
        let mut data: Value = serde_json::from_str(&fs::read_to_string(&pp).unwrap()).unwrap();
        data["providers"][0]["agent"] = json!("codex");
        fs::write(&pp, data.to_string()).unwrap();

        let error = host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!env_path(&home).exists());
        assert!(!settings_path(&home).exists());
        assert!(!sidecar_path(&home).exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn host_rolls_back_env_and_sidecar_when_settings_write_fails() {
        let home = tmp("host-rollback");
        let pp = write_provider_file(&home, "chat_completions");
        fs::create_dir_all(gemini_dir(&home)).unwrap();
        let original_env = "GEMINI_API_KEY=user-key\n";
        let original_settings = r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#;
        fs::write(env_path(&home), original_env).unwrap();
        fs::write(settings_path(&home), original_settings).unwrap();
        fs::create_dir(settings_path(&home).with_extension("tmp")).unwrap();

        let error = host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_IO");
        assert_eq!(fs::read_to_string(env_path(&home)).unwrap(), original_env);
        assert_eq!(
            fs::read_to_string(settings_path(&home)).unwrap(),
            original_settings
        );
        assert!(!sidecar_path(&home).exists());
        assert!(crate::providers::get_active_for_agent(&pp, "gemini").is_none());
        let _ = fs::remove_dir_all(&home);
    }

    /// unhost 受控还原:三键恢复快照(用户原 GEMINI_API_KEY 复原、BASE_URL 原本无 → 删)、selectedType 复原 oauth-personal、sidecar 删除;二次 no-op。
    #[test]
    fn unhost_restores_snapshot() {
        let home = tmp("unhost");
        let pp = write_provider_file(&home, "chat_completions");
        fs::create_dir_all(gemini_dir(&home)).unwrap();
        fs::write(env_path(&home), "GEMINI_API_KEY=user-own-key\n").unwrap();
        fs::write(
            settings_path(&home),
            r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#,
        )
        .unwrap();

        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let v = unhost(&home, &home.join("bk")).unwrap();
        assert_eq!(v["hosted"], json!(false));
        assert_eq!(v["restored"], json!(true));

        let env = fs::read_to_string(env_path(&home)).unwrap();
        assert_eq!(
            env_get(&env, "GEMINI_API_KEY").unwrap(),
            "user-own-key",
            "用户原值恢复"
        );
        assert!(
            env_get(&env, "GOOGLE_GEMINI_BASE_URL").is_none(),
            "原本无此键 → 删净"
        );
        assert!(env_get(&env, "GEMINI_MODEL").is_none());
        let st: Value =
            serde_json::from_str(&fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(
            st["security"]["auth"]["selectedType"], "oauth-personal",
            "selectedType 恢复快照值"
        );
        assert!(!sidecar_path(&home).exists(), "sidecar 应删除");
        assert!(
            crate::providers::get_active_for_agent(&pp, "gemini").is_none(),
            "unhost 必须清理 Gemini active"
        );

        // 二次 unhost no-op(不报错)
        let v2 = unhost(&home, &home.join("bk")).unwrap();
        assert_eq!(v2["restored"], json!(false));
        let _ = fs::remove_dir_all(&home);
    }

    /// 无 sidecar 不能证明由本产品托管:必须零触碰用户自有三键与认证设置。
    #[test]
    fn unhost_without_sidecar_is_noop() {
        let home = tmp("noside");
        fs::create_dir_all(gemini_dir(&home)).unwrap();
        fs::write(
            env_path(&home),
            "GOOGLE_GEMINI_BASE_URL=https://user.example\nGEMINI_API_KEY=user-key\nGEMINI_MODEL=user-model\n",
        )
        .unwrap();
        fs::write(
            settings_path(&home),
            r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
        )
        .unwrap();

        let result = unhost(&home, &home.join("bk")).unwrap();
        assert_eq!(result["restored"], false);
        let env = fs::read_to_string(env_path(&home)).unwrap();
        assert_eq!(
            env_get(&env, "GOOGLE_GEMINI_BASE_URL").as_deref(),
            Some("https://user.example")
        );
        assert_eq!(env_get(&env, "GEMINI_API_KEY").as_deref(), Some("user-key"));
        let st: Value =
            serde_json::from_str(&fs::read_to_string(settings_path(&home)).unwrap()).unwrap();
        assert_eq!(st["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(state(&home)["hosted"], false, "用户自有 .env 不得冒充托管");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn windows_start_command_uses_powershell_env() {
        let command = start_command(
            "https://api.example/v1beta",
            "key value",
            "gemini-pro",
            true,
        );
        assert!(command.starts_with("powershell -NoProfile -Command"));
        assert!(command.contains("$env:GEMINI_API_KEY='key value'"));
    }

    /// state:host 前 false;host 后 hosted+authType;坏 settings 不冒充托管。
    #[test]
    fn state_detection() {
        let home = tmp("state");
        let pp = write_provider_file(&home, "chat_completions");
        let s0 = state(&home);
        assert_eq!(s0["hosted"], json!(false));
        assert!(s0["installed"].is_boolean());

        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let s1 = state(&home);
        assert_eq!(s1["hosted"], json!(true));
        assert_eq!(
            s1["hosting"]["way"], "gateway",
            "通用世界前端读 hosting 判定托管态"
        );
        assert_eq!(s1["hosting"]["authType"], "gemini-api-key");
        assert_eq!(s1["authType"], "gemini-api-key");
        assert_eq!(s1["model"], "gemini-2.5-flash");
        let _ = fs::remove_dir_all(&home);
    }

    /// start:未托管 409;托管后返回命令(env 注入式,占位 Key 不含真实值)。
    #[test]
    fn start_command_shape() {
        let home = tmp("start");
        let pp = write_provider_file(&home, "chat_completions");
        let err = start(&pp, "gateway", "pv1", &home).unwrap_err();
        assert_eq!(err.1, "E_NOT_HOSTED");

        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let v = start(&pp, "gateway", "pv1", &home).unwrap();
        let cmd = v["command"].as_str().unwrap();
        assert!(
            cmd.contains("GOOGLE_GEMINI_BASE_URL='http://127.0.0.1:8787'"),
            "命令:\n{cmd}"
        );
        assert!(
            cmd.contains(PLACEHOLDER_KEY),
            "网关方式命令用占位 Key:\n{cmd}"
        );
        assert!(cmd.ends_with("gemini"));
        let _ = fs::remove_dir_all(&home);
    }

    /// .env upsert/删除的布局保留细测(注释、空行、无关键、重复键清理)。
    #[test]
    fn env_layout_helpers() {
        let src = "# top comment\n\nA=1\n# mid\nB=2\nA=3\n";
        let up = env_upsert(src, &[("A", "9".into()), ("C", "c".into())]);
        assert!(up.contains("# top comment"));
        assert!(up.contains("# mid"));
        assert!(up.contains("B=2"));
        assert_eq!(up.matches("A=").count(), 1, "重复 A 行收敛为一:\n{up}");
        assert!(up.contains("A=9"));
        assert!(up.contains("C=c"));
        let rm = env_remove_keys(src, &["A"]).unwrap();
        assert!(rm.contains("B=2") && rm.contains("# mid"));
        assert!(!rm.contains("A="));
        assert!(env_remove_keys(src, &["Z"]).is_none());
    }
}
