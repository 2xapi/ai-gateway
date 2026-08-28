//! WorkBuddy / CodeBuddy adapter(多平台方案 B 阶段;批次任务书 workbuddy,2026-08-16)
//!
//! 叠加平台(D1 语义):CLI(`~/.codebuddy/models.json`)与桌面版(`~/.workbuddy/models.json`)
//! 两个载体的 `models` 数组仅追加/覆盖 `vendor=2xapi-gateway` 的条目;`availableModels`
//! (项目级完全覆盖语义,写了会隐藏用户已有自定义模型)与用户条目零触碰;
//! `settings.json` 的 `model` 指针属用户偏好,本产品恒不写(unhost 也不碰)。
//!
//! 实证依据(workbuddy批次探索结论.md,2026-08-16 罗盘):
//! - 双路径完全分离,桌面版 CustomModelsProductProvider 热监听 models.json
//! - `${VAR}` 仅解析真实进程 env,桌面 App 注不进 → 统一直接写 provider.api_key 值
//! - url 必须完整路径以 `/chat/completions` 结尾;协议仅 OpenAI Chat → 网关零改动
//! - 同 id 覆盖/异 id 追加(SmartMerge);首版单条目,多模型条目集留后续批次

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 本产品在 models.json 里的身份标记:条目 vendor 与 id 均用它,unhost 按 vendor 整集移除。
pub const VENDOR: &str = "2xapi-gateway";
/// 网关 Chat 入口(gateway.rs 根路径直收 /chat/completions)。
const GATEWAY_CHAT_URL: &str = "http://127.0.0.1:8787/workbuddy/v1/chat/completions";
const PLACEHOLDER_KEY: &str = "2xapi-gateway-managed";

type OpError = (u16, String, String);

/// 两个配置载体:CLI 与桌面版,互不读取对方目录(实证),同一套条目各写一份。
fn config_roots(home: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("cli", home.join(".codebuddy")),
        ("desktop", home.join(".workbuddy")),
    ]
}

fn models_path(root: &Path) -> PathBuf {
    root.join("models.json")
}

/// 读 models.json;无文件→空对象;坏 JSON→E_PARSE 拒碰用户文件(不擅自治愈)。
fn read_models(root: &Path) -> Result<Value, OpError> {
    let p = models_path(root);
    if !p.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(&p).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    serde_json::from_str(&raw).map_err(|_| {
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

/// 移除本产品条目集后写回前的载荷;返回 (新对象, 是否移除过条目)。
fn strip_ours(cfg: &Value) -> (Value, bool) {
    let mut out = cfg.clone();
    let mut removed = false;
    if let Some(models) = out.get_mut("models").and_then(|v| v.as_array_mut()) {
        let before = models.len();
        models.retain(|m| m.get("vendor").and_then(|v| v.as_str()) != Some(VENDOR));
        removed = models.len() != before;
    }
    (out, removed)
}

/// 生成条目。**id 必须是真实模型名**(真机缺口修正 2026-08-16:CLI 把条目 id 作为请求的
/// model 参数,网关与上游按模型名路由——网关 dispatch 原样转发 model,固定 id 会让上游
/// 收到未知模型名返回空流);本产品身份由 vendor=2xapi-gateway 标记(unhost 按 vendor
/// 整集移除),x-provider-id 溯源。direct url 拼法(真机实证:2xapi.cc.cd 无 /v1 的 chat
/// 路径 404,带 /v1 通;与 dispatch_anthropic 同规则——base 已带 /v1 则续接,否则补 /v1)。
fn build_entry(provider: &crate::providers::Provider, way: &str) -> Value {
    let url = if way == "gateway" {
        GATEWAY_CHAT_URL.to_string()
    } else {
        let base = provider.base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    };
    json!({
        "id": provider.model,
        "name": format!("2xapi 网关({})", provider.name),
        "vendor": VENDOR,
        "apiKey": if way == "gateway" { PLACEHOLDER_KEY } else { &provider.api_key },
        "url": url,
        "maxInputTokens": 128000,
        "maxOutputTokens": 16384,
        "supportsToolCall": true,
        "supportsImages": false,
        // 本产品私有键(x- 前缀,CLI 宽松解析已实证容忍):识别当前托管供应商
        "x-provider-id": provider.id,
    })
}

/// 原子写(临时文件+rename),权限 600(models.json 官方建议,含 Key)。
fn write_models_atomic(root: &Path, cfg: &Value) -> Result<(), OpError> {
    std::fs::create_dir_all(root).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    let p = models_path(root);
    let tmp = p.with_extension("json.tmp");
    let raw = format!(
        "{}\n",
        serde_json::to_string_pretty(cfg).map_err(|e| (500, "E_IO".into(), e.to_string()))?
    );
    std::fs::write(&tmp, raw).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| (500, "E_IO".into(), e.to_string()))?;
    }
    std::fs::rename(&tmp, &p).map_err(|e| (500, "E_IO".into(), e.to_string()))
}

/// host 单载体计划:返回合并结果与是否需要写入，不在计划阶段改盘。
fn plan_host_root(root: &Path, entry: &Value) -> Result<(Value, bool), OpError> {
    let cfg = read_models(root)?;
    let (mut merged, _) = strip_ours(&cfg); // 先清旧条目(同 id 覆盖语义)
    if let Some(obj) = merged.as_object_mut() {
        let models = obj.entry("models").or_insert(json!([]));
        if let Some(arr) = models.as_array_mut() {
            arr.push(entry.clone());
        }
    }
    if serde_json::to_string_pretty(&merged).ok() == serde_json::to_string_pretty(&cfg).ok() {
        return Ok((merged, false)); // 内容一致(幂等):不写盘不备份
    }
    Ok((merged, true))
}

fn backup_models(root: &Path, backup_dir: &Path, purpose: &str) -> Result<(), OpError> {
    let p = models_path(root);
    if p.exists() {
        std::fs::create_dir_all(backup_dir).map_err(|e| (500, "E_IO".into(), e.to_string()))?;
        crate::config::backup_file(&p, backup_dir, "workbuddy-models", purpose)
            .map_err(|e| (500, "E_IO".into(), e))?;
    }
    Ok(())
}

/// POST /api/desktop/workbuddy/host {providerId, way}
pub fn host(
    wb_home: &Path,
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
    crate::desktop::validate_provider_agent(&provider, "workbuddy")?;
    if provider.model.is_empty() {
        return Err((
            422,
            "E_NO_MODEL".to_string(),
            "该供应商未配置默认模型,请先在编辑里拉取模型或手填".into(),
        ));
    }
    let entry = build_entry(&provider, way);
    let mut plans = Vec::new();
    for (key, root) in config_roots(wb_home) {
        let (merged, wrote) = plan_host_root(&root, &entry)?;
        plans.push((key, root, merged, wrote));
    }
    let mut paths: Vec<PathBuf> = plans
        .iter()
        .filter(|(_, _, _, wrote)| *wrote)
        .map(|(_, root, _, _)| models_path(root))
        .collect();
    paths.push(providers_path.to_path_buf());
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    for (_, root, _, wrote) in &plans {
        if *wrote {
            backup_models(root, backup_dir, "pre-host")?;
        }
    }
    let outcome = (|| {
        let mut changed = serde_json::Map::new();
        for (key, root, merged, wrote) in &plans {
            if *wrote {
                write_models_atomic(root, merged)?;
            }
            changed.insert((*key).into(), json!(wrote));
        }
        crate::desktop::set_active_checked(providers_path, &provider, "workbuddy")?;
        Ok(json!({
            "hosted": true,
            "way": way,
            "entryId": provider.model,
            "entryName": format!("2xapi 网关({})", provider.name),
            "changed": Value::Object(changed),
            "hint": "叠加平台:模型条目已写入,请在 CodeBuddy/WorkBuddy 模型列表选择「2xapi 网关」",
        }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// POST /api/desktop/workbuddy/unhost —— 仅移除本产品条目集;用户条目与 availableModels 不动。
pub fn unhost(wb_home: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let providers_path = crate::desktop::providers_path_from_backup_dir(backup_dir);
    let mut plans = Vec::new();
    for (key, root) in config_roots(wb_home) {
        let cfg = read_models(&root)?;
        let (merged, removed) = strip_ours(&cfg);
        plans.push((key, root, merged, removed));
    }
    let mut paths: Vec<PathBuf> = plans
        .iter()
        .filter(|(_, _, _, removed)| *removed)
        .map(|(_, root, _, _)| models_path(root))
        .collect();
    paths.push(providers_path.clone());
    let snapshots = paths
        .iter()
        .map(|path| crate::desktop::snapshot_file(path).map(|snapshot| (path.clone(), snapshot)))
        .collect::<Result<Vec<_>, _>>()?;
    for (_, root, _, removed) in &plans {
        if *removed {
            backup_models(root, backup_dir, "pre-unhost")?;
        }
    }
    let outcome = (|| {
        let mut changed = serde_json::Map::new();
        for (key, root, merged, removed) in &plans {
            if *removed {
                write_models_atomic(root, merged)?;
            }
            changed.insert((*key).into(), json!(removed));
        }
        crate::desktop::clear_active_checked(&providers_path, "workbuddy")?;
        Ok(json!({ "hosted": false, "changed": Value::Object(changed) }))
    })();
    outcome.map_err(|error| crate::desktop::rollback_files(error, &snapshots))
}

/// GET /api/desktop/workbuddy/state —— 托管态 + 安装检测(安装是验收/UX 提示用,不门控)。
pub fn state(wb_home: &Path) -> Value {
    let mut entries = serde_json::Map::new();
    let mut hosted_any = false;
    let mut provider_id: Option<String> = None;
    for (key, root) in config_roots(wb_home) {
        let p = models_path(&root);
        let (file_exists, ours, pid) = match read_models(&root) {
            Ok(cfg) => {
                let mine: Vec<&Value> = cfg
                    .get("models")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter(|m| m.get("vendor").and_then(|v| v.as_str()) == Some(VENDOR))
                            .collect()
                    })
                    .unwrap_or_default();
                let pid = mine
                    .first()
                    .and_then(|m| m.get("x-provider-id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (p.exists(), mine.len(), pid)
            }
            Err(_) => (p.exists(), 0, None), // 坏 JSON:文件在但无法确认,不冒充托管态
        };
        hosted_any = hosted_any || ours > 0;
        if provider_id.is_none() {
            provider_id = pid;
        }
        entries.insert(key.into(), json!({ "file": file_exists, "ours": ours }));
    }
    let cli_installed = which_codebuddy().is_some();
    let desktop_installed = [
        "/Applications/WorkBuddy.app",
        &format!(
            "{}/Applications/WorkBuddy.app",
            std::env::var("HOME").unwrap_or_default()
        ),
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    json!({
        "agent": "workbuddy",
        // hosting 契约对齐 B 阶段通用世界(grokbuild/opencode 等:{…}|null);hosted 保留兼容
        "hosting": if hosted_any { json!({ "way": "gateway", "entryId": VENDOR }) } else { Value::Null },
        "hosted": hosted_any,
        "providerId": provider_id,
        "entries": Value::Object(entries),
        "installed": { "cli": cli_installed, "desktop": desktop_installed },
    })
}

/// PATH 及常见安装位找 codebuddy CLI(只为 UI 提示,找不到不报错)。
fn which_codebuddy() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "codebuddy.exe"
    } else {
        "codebuddy"
    };
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = Path::new(dir).join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let fallback = [
        format!("{home}/.local/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        format!("/opt/homebrew/bin/{name}"),
    ];
    fallback.iter().map(PathBuf::from).find(|p| p.exists())
}

/// POST /api/desktop/workbuddy/start —— CLI 注入式启动信息(命令可复制;桌面版无命令,UI 提示自选模型)。
pub fn start(
    providers_path: &Path,
    way: &str,
    provider_id: &str,
    wb_home: &Path,
) -> Result<Value, OpError> {
    let p = if !provider_id.trim().is_empty() {
        let data = crate::providers::load(providers_path);
        data.providers
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()
            .ok_or((
                400u16,
                "E_NO_PROVIDER".to_string(),
                "供应商不存在".to_string(),
            ))?
    } else {
        crate::providers::get_provider_for_agent(providers_path, "workbuddy").ok_or((
            503u16,
            "E_NO_WORKBUDDY_PROVIDER".to_string(),
            "请先选择 WorkBuddy 供应商".to_string(),
        ))?
    };
    // 条目须先 host(url/apiKey 都在条目里,start 不再传 Key)
    let hosted = state(wb_home)
        .get("hosted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !hosted {
        return Err((
            409u16,
            "E_NOT_HOSTED".to_string(),
            "请先托管,再启动".to_string(),
        ));
    }
    Ok(json!({
        // --model 须用真实模型名(=条目 id,真机实证;见 build_entry 注释)
        "command": format!("codebuddy --model {}", p.model),
        "model": p.model,
        "way": way,
        "providerId": p.id,
        "providerName": p.name,
        "desktopHint": "WorkBuddy 桌面版:打开 App,在模型列表选择「2xapi 网关」即可",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wb-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_provider_file(dir: &Path) -> PathBuf {
        let p = dir.join("providers.json");
        fs::write(
            &p,
            serde_json::json!({
                "providers": [{
                    "id": "pv1", "name": "测试站", "agent": "workbuddy",
                    "base_url": "https://example.com/v1", "api_key": "sk-test-key",
                    "model": "gpt-test",
                }]
            })
            .to_string(),
        )
        .unwrap();
        p
    }

    /// host 写入断言:双路径条目追加、用户条目与 availableModels 与未知字段不动、幂等。
    #[test]
    fn host_appends_entry_and_preserves_user_data() {
        let home = tmp("host1");
        let backup = home.join("backups");
        fs::create_dir_all(&backup).unwrap();
        let pp = write_provider_file(&home);
        // 用户已有 CLI 配置:自条目 + availableModels + 未知顶层字段
        fs::create_dir_all(home.join(".codebuddy")).unwrap();
        fs::write(
            home.join(".codebuddy/models.json"),
            serde_json::json!({
                "models": [
                    {"id": "user-model", "name": "User", "vendor": "other", "apiKey": "sk-user",
                     "url": "https://u.example/v1/chat/completions"}
                ],
                "availableModels": ["user-model"],
                "futureField": {"keep": true}
            })
            .to_string(),
        )
        .unwrap();

        let v = host(&home, &backup, &pp, "pv1", "gateway").unwrap();
        assert_eq!(v["hosted"], json!(true));
        assert_eq!(v["changed"]["cli"], json!(true));
        assert_eq!(v["changed"]["desktop"], json!(true)); // 桌面目录不存在 → 新建写入

        for d in [".codebuddy", ".workbuddy"] {
            let cfg: Value = serde_json::from_str(
                &fs::read_to_string(home.join(d).join("models.json")).unwrap(),
            )
            .unwrap();
            let models = cfg["models"].as_array().unwrap();
            assert_eq!(
                models.len(),
                if d == ".codebuddy" { 2 } else { 1 },
                "{d} 应为本产品条目+用户条目"
            );
            let ours = models.iter().find(|m| m["vendor"] == VENDOR).unwrap();
            assert_eq!(ours["url"], GATEWAY_CHAT_URL);
            assert_eq!(
                ours["apiKey"], PLACEHOLDER_KEY,
                "网关托管不得落真实上游 Key"
            );
            assert_eq!(
                ours["id"], "gpt-test",
                "条目 id 必须是真实模型名(CLI 以之作请求 model)"
            );
            assert_eq!(ours["x-provider-id"], "pv1");
            if d == ".codebuddy" {
                assert!(
                    models.iter().any(|m| m["id"] == "user-model"),
                    "用户条目零触碰"
                );
                assert_eq!(
                    cfg["availableModels"],
                    json!(["user-model"]),
                    "availableModels 零触碰"
                );
                assert_eq!(cfg["futureField"]["keep"], json!(true), "未知字段保留");
            }
        }

        // 幂等:同参再 host → 全 no-op,文件字节不变
        let before = fs::read(home.join(".codebuddy/models.json")).unwrap();
        let v2 = host(&home, &backup, &pp, "pv1", "gateway").unwrap();
        assert_eq!(v2["changed"]["cli"], json!(false));
        let after = fs::read(home.join(".codebuddy/models.json")).unwrap();
        assert_eq!(before, after);
    }

    /// direct 的 url 拼接对齐 gateway.rs(trim_end('/') + /chat/completions)。
    #[test]
    fn direct_url_join() {
        let home = tmp("direct");
        let pp = write_provider_file(&home);
        host(&home, &home.join("bk"), &pp, "pv1", "direct").unwrap();
        let cfg: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codebuddy/models.json")).unwrap())
                .unwrap();
        assert_eq!(
            cfg["models"][0]["url"],
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(cfg["models"][0]["id"], "gpt-test");
        // base 不带 /v1(2xapi.cc.cd 形态)→ 补 /v1(真机实证:无 /v1 路径 404)
        let mut d2: Value = serde_json::from_str(&fs::read_to_string(&pp).unwrap()).unwrap();
        d2["providers"].as_array_mut().unwrap()[0]["base_url"] = json!("https://no-v1.example");
        fs::write(&pp, d2.to_string()).unwrap();
        host(&home, &home.join("bk"), &pp, "pv1", "direct").unwrap();
        let cfg2: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codebuddy/models.json")).unwrap())
                .unwrap();
        assert_eq!(
            cfg2["models"][0]["url"],
            "https://no-v1.example/v1/chat/completions"
        );
    }

    /// unhost 仅移除本产品条目;二次 unhost no-op。
    #[test]
    fn unhost_removes_only_ours() {
        let home = tmp("unhost");
        let backup = home.join("backups");
        fs::create_dir_all(&backup).unwrap();
        let pp = write_provider_file(&home);
        host(&home, &backup, &pp, "pv1", "gateway").unwrap();

        let v = unhost(&home, &backup).unwrap();
        assert_eq!(v["hosted"], json!(false));
        assert_eq!(v["changed"]["cli"], json!(true));
        let cfg: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codebuddy/models.json")).unwrap())
                .unwrap();
        assert!(cfg["models"].as_array().unwrap().is_empty());
        assert!(
            crate::providers::get_active_for_agent(&pp, "workbuddy").is_none(),
            "unhost 必须清理 WorkBuddy active"
        );

        let v2 = unhost(&home, &backup).unwrap();
        assert_eq!(v2["changed"]["cli"], json!(false), "二次 unhost no-op");
    }

    #[test]
    fn host_rejects_foreign_agent_without_writes() {
        let home = tmp("foreign-agent");
        let pp = write_provider_file(&home);
        let mut data: Value = serde_json::from_str(&fs::read_to_string(&pp).unwrap()).unwrap();
        data["providers"][0]["agent"] = json!("codex");
        fs::write(&pp, data.to_string()).unwrap();

        let error = host(&home, &home.join("backups"), &pp, "pv1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_PROVIDER_AGENT_MISMATCH");
        assert!(!home.join(".codebuddy/models.json").exists());
        assert!(!home.join(".workbuddy/models.json").exists());
    }

    #[test]
    fn host_rolls_back_cli_when_desktop_write_fails() {
        let home = tmp("host-rollback");
        let backup = home.join("backups");
        fs::create_dir_all(&backup).unwrap();
        let pp = write_provider_file(&home);
        fs::create_dir_all(home.join(".codebuddy")).unwrap();
        let original = json!({ "models": [{ "id": "user", "vendor": "user" }] }).to_string();
        fs::write(home.join(".codebuddy/models.json"), &original).unwrap();
        fs::create_dir_all(home.join(".workbuddy")).unwrap();
        fs::create_dir(home.join(".workbuddy/models.json.tmp")).unwrap();

        let error = host(&home, &backup, &pp, "pv1", "gateway").unwrap_err();

        assert_eq!(error.1, "E_IO");
        assert_eq!(
            fs::read_to_string(home.join(".codebuddy/models.json")).unwrap(),
            original
        );
        assert!(!home.join(".workbuddy/models.json").exists());
        assert!(crate::providers::get_active_for_agent(&pp, "workbuddy").is_none());
    }

    /// 坏 JSON 拒碰:E_PARSE 且文件原样。
    #[test]
    fn bad_json_refuses() {
        let home = tmp("bad");
        fs::create_dir_all(home.join(".codebuddy")).unwrap();
        let raw = "{broken json";
        fs::write(home.join(".codebuddy/models.json"), raw).unwrap();
        let pp = write_provider_file(&home);
        let err = host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap_err();
        assert_eq!(err.1, "E_PARSE");
        assert_eq!(
            fs::read_to_string(home.join(".codebuddy/models.json")).unwrap(),
            raw
        );
    }

    /// 无模型供应商拒绝 host(与 codex E_NO_MODEL 口径一致)。
    #[test]
    fn no_model_rejects() {
        let home = tmp("nomodel");
        let pp = home.join("providers.json");
        fs::write(
            &pp,
            serde_json::json!({
                "providers": [{"id": "pv2", "name": "空", "agent": "workbuddy",
                    "base_url": "https://x.example", "api_key": "k", "model": ""}]
            })
            .to_string(),
        )
        .unwrap();
        let err = host(&home, &home.join("bk"), &pp, "pv2", "gateway").unwrap_err();
        assert_eq!(err.1, "E_NO_MODEL");
    }

    /// state:安装检测不依赖本机真实状态——只断言结构;host 后 hosted=true。
    #[test]
    fn state_shape_and_hosted() {
        let home = tmp("state");
        let pp = write_provider_file(&home);
        let s0 = state(&home);
        assert_eq!(s0["hosted"], json!(false));
        assert!(s0["entries"]["cli"]["file"].is_boolean());
        assert!(s0["installed"]["cli"].is_boolean());
        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let s1 = state(&home);
        assert_eq!(s1["hosted"], json!(true));
        assert_eq!(
            s1["hosting"]["way"], "gateway",
            "通用世界前端读 hosting 判定托管态"
        );
        assert_eq!(s1["entries"]["desktop"]["ours"], 1);
    }

    /// start:未托管 409;托管后返回命令与提示。
    #[test]
    fn start_requires_hosting() {
        let home = tmp("start");
        let pp = write_provider_file(&home);
        let err = start(&pp, "gateway", "pv1", &home).unwrap_err();
        assert_eq!(err.1, "E_NOT_HOSTED");
        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        let v = start(&pp, "gateway", "pv1", &home).unwrap();
        assert_eq!(v["command"], "codebuddy --model gpt-test");
        assert!(v["desktopHint"].is_string());
    }

    /// 换 way 重写:同 id 覆盖(gateway→direct url 变),条目数不涨。
    #[test]
    fn switch_way_overrides_entry() {
        let home = tmp("switch");
        let pp = write_provider_file(&home);
        host(&home, &home.join("bk"), &pp, "pv1", "gateway").unwrap();
        host(&home, &home.join("bk"), &pp, "pv1", "direct").unwrap();
        let cfg: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".codebuddy/models.json")).unwrap())
                .unwrap();
        let models = cfg["models"].as_array().unwrap();
        assert_eq!(models.len(), 1, "同 id 覆盖,条目不重复");
        assert_eq!(models[0]["url"], "https://example.com/v1/chat/completions");
    }
}
