//! Grok Build 配置引擎(阶段 B,多平台接入开发方案 §3.2)。
//!
//! `~/.grok/config.toml` 是 Grok CLI 的唯一供应商配置入口,TOML 文本,结构:
//! 顶层 `[models]` 的 `default` 指向选中的 profile 名;每个 profile 是一个
//! `[model."<name>"]` 表,字段 model / base_url / name / api_key|env_key /
//! api_backend("responses"|"chat_completions") / context_window(正整数)。
//!
//! 业务规则对齐 cc-switch v3.19.2 `grok_config.rs`(权威实现,已核实):
//! - **官方态**:文档完全没有 `[models]` 且没有 `[model.*]`(可含 `[mcp_servers]`
//!   等其他内容,空文件也合法)= Grok CLI 官方 xAI OAuth 登录态。官方态读取与
//!   原样写回必须放行;非官方供应商写入前必须过完整强校验。
//! - **语法校验与强校验分离**:读 live 只做 TOML 语法校验;写自定义供应商配置前
//!   做强校验(报错带缺失字段名)。
//! - **凭据解析安全规则**:`api_key` 优先;否则从 `env_key` 指定的环境变量取值
//!   (trim 后非空)。**绝对禁止**在声明的环境变量未设置时静默兜底到
//!   `XAI_API_KEY` 或任何其他变量——声明了间接引用但变量不存在时必须返回
//!   「无凭据」让调用方显式失败(防止把别的账号的密钥泄漏给任意 base_url)。
//! - **切换语义**(切换模式平台):host = 受控段整段替换——`[models]`/`[model.*]`
//!   归本产品(固定 profile 名 `2xapi`,即托管态受控标记),其余顶层段
//!   (如 `[mcp_servers]`)保留;unhost = pre-host 快照受控还原(§1.5-1 同手法),
//!   无快照则移除受控段回落官方态。与 codex 的字段级合并同一哲学,粒度为段级。
//! - **Key 落盘差异**(沿用 direct 先例):gateway 方式 base_url 指向网关、
//!   api_key 写占位值(零 Key 契约,真实 Key 只进网关);direct 方式 base_url
//!   直指供应商、api_key 写供应商 Key(落盘是已定稿的差异,与网关方式区分表述)。
//! - **保格式**:项目依赖只有 `toml`(无 toml_edit),段级合并经 parse→改→重渲染,
//!   注释不保留——与 codex config.toml 的 write_toml 同一取舍,不为保注释新增依赖。
//!
//! 阶段 A(agents/ 注册表)未并回 main 时的定位:本模块是独立配置引擎(与
//! config.rs 之于 codex 同位),不依赖注册表;接线(agents 注册表 available 翻真 +
//! 泛化路由 :agent 段 + 网关按 agent 取供应商 + 前端世界)在 A 合并后进行。

// A 阶段(agents 注册表 + 泛化路由)并回前,本引擎未被 main 调用——接线后移除本 allow。
#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::desktop::OpError;
use crate::providers::{Provider, WireApi};

/// 默认模型 ID(与 cc-switch 默认一致)。
pub const DEFAULT_MODEL: &str = "grok-4.5";
/// 默认 api_backend(responses;2xapi 对接默认,与 cc-switch 默认一致)。
pub const DEFAULT_API_BACKEND: &str = "responses";
/// 默认上下文窗口。
pub const DEFAULT_CONTEXT_WINDOW: i64 = 500_000;
/// 本产品在 `[model.*]` 里的固定 profile 名 = 托管态受控标记
/// (`[models].default` 指向它即本软件托管;用户/第三方自己的 profile 不受触碰)。
pub const MANAGED_PROFILE: &str = "2xapi";
/// gateway 托管时 live `api_key` 的占位值:真实 Key 只进网关不落盘(零 Key 契约)。
pub const GATEWAY_KEY_PLACEHOLDER: &str = "2xapi-gateway-managed";

/// 从 live 配置解析出的选中 profile(cc-switch `GrokModelConfig` 同构;
/// `base_url` 已 trim 尾部 `/`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokModelConfig {
    pub profile: String,
    pub model: String,
    pub base_url: String,
    pub name: String,
    pub api_key: Option<String>,
    pub env_key: Option<String>,
    pub api_backend: String,
    pub context_window: i64,
}

/// `~/.grok`(Windows 无 HOME → 回退 USERPROFILE,同 main.rs codex_home 教训)。
pub fn default_grok_home() -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_default();
    PathBuf::from(home).join(".grok")
}

/// live 配置路径 `~/.grok/config.toml`。
pub fn default_grok_config_path() -> PathBuf {
    default_grok_home().join("config.toml")
}

// ── 纯校验/解析(无 IO,单测主体)────────────────────────────

/// TOML 表里取「非空字符串」字段;缺失/非字符串/全空白 → 报「缺少有效的 {key} 字段」。
fn required_non_empty_string<'a>(
    table: &'a toml::value::Table,
    key: &str,
) -> Result<&'a str, String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("Grok Build 配置缺少有效的 {key} 字段"))
}

/// TOML 表里取可选的非空字符串(trim 后非空才算)。
fn optional_non_empty_string(table: &toml::value::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
}

/// 仅语法校验(空文档合法)。
///
/// 官方条目走 Grok CLI 自带 xAI OAuth,config.toml 通常没有(也不需要有)自定义
/// 模型表:空文档合法,非空只要求 TOML 语法合法。live 的读写与快照恢复都走它;
/// 「必须有完整自定义模型配置」的强校验见 `validate_config`。
pub fn validate_syntax(config_toml: &str) -> Result<(), String> {
    if config_toml.trim().is_empty() {
        return Ok(());
    }
    config_toml
        .parse::<toml::Value>()
        .map(|_| ())
        .map_err(|e| format!("Grok Build config.toml 格式错误: {e}"))
}

/// live 文档是否官方登录态(xAI OAuth)。
///
/// 官方态 = 语法合法且完全没有自定义模型痕迹(无 `[models]` 也无 `[model.*]`,
/// 允许 `[mcp_servers]` 等其它内容,空文件合法)。只要出现过任一自定义键就返回
/// false——让残缺的自定义配置继续走 `validate_config` 报出真实错误,而不是被
/// 误判成官方态静默吞掉。语法不合法同样返回 false。
pub fn is_official_live(config_toml: &str) -> bool {
    let Ok(document) = config_toml.parse::<toml::Value>() else {
        return false;
    };
    document
        .as_table()
        .is_some_and(|root| !root.contains_key("models") && !root.contains_key("model"))
}

/// 非官方(自定义供应商)配置的强校验:写入前必须通过,报错带缺失字段名。
pub fn validate_config(config_toml: &str) -> Result<(), String> {
    let document = config_toml
        .parse::<toml::Value>()
        .map_err(|e| format!("Grok Build config.toml 格式错误: {e}"))?;
    let root = match document {
        toml::Value::Table(t) => t,
        _ => return Err("Grok Build 配置必须是 TOML 表结构".into()),
    };
    let models = root
        .get("models")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "Grok Build 配置缺少 [models]".to_string())?;
    let default_model = required_non_empty_string(models, "default")?;
    let model_entries = root
        .get("model")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| "Grok Build 配置缺少 [model.<name>]".to_string())?;
    let selected = model_entries
        .get(default_model)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| format!("Grok Build 配置缺少 [model.\"{default_model}\"]"))?;

    required_non_empty_string(selected, "model")?;
    required_non_empty_string(selected, "base_url")?;
    required_non_empty_string(selected, "name")?;
    if optional_non_empty_string(selected, "api_key").is_none()
        && optional_non_empty_string(selected, "env_key").is_none()
    {
        return Err("Grok Build 配置缺少有效的 api_key 或 env_key 字段".into());
    }
    required_non_empty_string(selected, "api_backend")?;
    selected
        .get("context_window")
        .and_then(toml::Value::as_integer)
        .filter(|v| *v > 0)
        .ok_or_else(|| "Grok Build context_window 必须是正整数".to_string())?;
    Ok(())
}

/// 解析选中 profile(`[models].default` 指向的 `[model.<name>]`);任一必需字段
/// 缺失返回 None。`base_url` trim 尾部 `/`。
pub fn extract_model_config(config_toml: &str) -> Option<GrokModelConfig> {
    let document = config_toml.parse::<toml::Value>().ok()?;
    let root = document.as_table()?;
    let default_model = root
        .get("models")?
        .as_table()?
        .get("default")?
        .as_str()?
        .trim();
    let m = root
        .get("model")?
        .as_table()?
        .get(default_model)?
        .as_table()?;
    Some(GrokModelConfig {
        profile: default_model.to_string(),
        model: m.get("model")?.as_str()?.trim().to_string(),
        base_url: m
            .get("base_url")?
            .as_str()?
            .trim_end_matches('/')
            .to_string(),
        name: m.get("name")?.as_str()?.trim().to_string(),
        api_key: optional_non_empty_string(m, "api_key"),
        env_key: optional_non_empty_string(m, "env_key"),
        api_backend: m.get("api_backend")?.as_str()?.trim().to_string(),
        context_window: m.get("context_window")?.as_integer()?,
    })
}

/// 凭据解析(base_url, api_key)。
///
/// 只认两个显式声明的来源:①内联 `api_key`;②`env_key` 指定的进程环境变量
/// (trim 后非空)。**刻意不做** `XAI_API_KEY` 之类的无条件兜底:声明的 `env_key`
/// 变量未设置时静默借用别的账号密钥,会把该密钥泄漏给本配置指向的任意 base_url。
/// 声明缺失必须以「无凭据」(None)浮出,让调用方显式失败而非带着错误的密钥出门。
pub fn extract_credentials(config_toml: &str) -> Option<(String, String)> {
    let config = extract_model_config(config_toml)?;
    let api_key = config.api_key.or_else(|| {
        config
            .env_key
            .as_deref()
            .and_then(|key| std::env::var(key).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })?;
    Some((config.base_url, api_key))
}

// ── 托管写入(host)/受控还原(unhost)────────────────────────

/// 直连方式下 api_backend 从供应商 wire_api 映射;网关方式恒 "responses"
/// (2xapi 对接默认,与 cc-switch 默认一致;网关侧协议转换不依赖此字段)。
fn api_backend_for(provider: &Provider) -> &'static str {
    match provider.wire_api {
        WireApi::Responses => "responses",
        WireApi::ChatCompletions => "chat_completions",
        WireApi::Anthropic => DEFAULT_API_BACKEND, // Grok 不说 Anthropic 协议,回退默认
        WireApi::Gemini => DEFAULT_API_BACKEND,    // Grok 不说 Gemini 协议,回退默认
    }
}

/// context_window 取值链:供应商字段 > 同名模型条目 > 默认 500000。
fn resolve_context_window(provider: &Provider) -> i64 {
    if let Some(cw) = provider
        .context_window
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
    {
        return cw;
    }
    if let Some(cw) = provider
        .models
        .iter()
        .find(|m| m.name == provider.model)
        .and_then(|m| m.context_window)
        .filter(|v| *v > 0)
        .map(|v| v as i64)
    {
        return cw;
    }
    DEFAULT_CONTEXT_WINDOW
}

/// 文本 parse 成 TOML 表;空/缺失/语法坏 → 空表(原文件已由 backup 保全,同
/// config.rs read_toml 的容错口径)。
fn parse_or_empty(text: &str) -> toml::value::Table {
    match text.parse::<toml::Value>() {
        Ok(toml::Value::Table(t)) => t,
        _ => toml::value::Table::new(),
    }
}

fn render_toml(root: &toml::value::Table) -> Result<String, String> {
    toml::to_string_pretty(&toml::Value::Table(root.clone()))
        .map_err(|e| format!("TOML 编码失败: {e}"))
}

/// 原子写文本(临时文件→rename,同 config.rs write_toml 手法)。
fn write_text_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入临时文件失败: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("重命名失败: {e}"))
}

/// 合并出托管态配置文本:受控段(`[models]`/`[model.*]`)整段替换为本产品条目
/// (切换语义),其余顶层段原样保留。gateway = 零 Key(base_url 指网关 + 占位);
/// direct = Key 落盘(base_url 直指供应商 + 供应商 Key)。
pub fn build_hosted_toml(
    current_text: &str,
    provider: &Provider,
    way: &str,
) -> Result<String, String> {
    let (base_url, api_key, api_backend) = match way {
        "gateway" => (
            format!("{}/grokbuild", crate::config::GATEWAY_BASE_URL),
            GATEWAY_KEY_PLACEHOLDER.to_string(),
            DEFAULT_API_BACKEND.to_string(),
        ),
        "direct" => (
            provider.base_url.trim().trim_end_matches('/').to_string(),
            provider.api_key.clone(),
            api_backend_for(provider).to_string(),
        ),
        _ => return Err("未知托管方式,仅支持 gateway / direct".into()),
    };
    let display_name = {
        let n = provider.name.trim();
        if n.is_empty() {
            "2xapi"
        } else {
            n
        }
    };

    let mut root = parse_or_empty(current_text);
    root.remove("models");
    root.remove("model");

    let mut profile = toml::value::Table::new();
    profile.insert(
        "model".into(),
        toml::Value::String(provider.model.trim().to_string()),
    );
    profile.insert("base_url".into(), toml::Value::String(base_url));
    profile.insert("name".into(), toml::Value::String(display_name.to_string()));
    profile.insert("api_key".into(), toml::Value::String(api_key));
    profile.insert("api_backend".into(), toml::Value::String(api_backend));
    profile.insert(
        "context_window".into(),
        toml::Value::Integer(resolve_context_window(provider)),
    );

    let mut models = toml::value::Table::new();
    models.insert(
        "default".into(),
        toml::Value::String(MANAGED_PROFILE.to_string()),
    );

    let mut model_tbl = toml::value::Table::new();
    model_tbl.insert(MANAGED_PROFILE.into(), toml::Value::Table(profile));
    root.insert("model".into(), toml::Value::Table(model_tbl));
    root.insert("models".into(), toml::Value::Table(models));
    render_toml(&root)
}

/// 移除受控段后的配置文本(= 官方态:Grok CLI 回落 xAI OAuth);其余段保留。
fn build_unhosted_toml(current_text: &str) -> Result<String, String> {
    let mut root = parse_or_empty(current_text);
    root.remove("models");
    root.remove("model");
    render_toml(&root)
}

/// 受控还原(§1.5-1 同手法):`[models]`/`[model.*]` 取 pre-host 快照值,其余段
/// 保留当前。快照官方态(无这两段)→ 移除之,同样回到官方。
fn restore_controlled_sections(current_text: &str, snapshot_text: &str) -> Result<String, String> {
    let mut root = parse_or_empty(current_text);
    let snap = parse_or_empty(snapshot_text);
    root.remove("models");
    root.remove("model");
    if let Some(v) = snap.get("models") {
        root.insert("models".into(), v.clone());
    }
    if let Some(v) = snap.get("model") {
        root.insert("model".into(), v.clone());
    }
    render_toml(&root)
}

/// 当前是否本软件托管态。受控标记 = `[models].default` 指向固定 profile `2xapi`
/// (仅本软件 host 会写;用户自己的 profile / 官方态 → null,unhost 概不触碰)。
/// way:base_url(trim 尾 `/`)等于网关地址 → "gateway",否则 → "direct"。
pub fn detect_hosting(grok_config_path: &Path) -> Value {
    let text = std::fs::read_to_string(grok_config_path).unwrap_or_default();
    let Some(cfg) = extract_model_config(&text) else {
        return Value::Null;
    };
    if cfg.profile != MANAGED_PROFILE {
        return Value::Null;
    }
    let way = if cfg
        .base_url
        .trim_end_matches('/')
        .starts_with(crate::config::GATEWAY_BASE_URL)
    {
        "gateway"
    } else {
        "direct"
    };
    json!({ "way": way, "profile": MANAGED_PROFILE })
}

/// backup_dir 里 purpose=pre-host 的最新快照(原始文本,含官方态原文;
/// backup_file 对「原文件不存在」写的占位注释 parse 后为空表,等价官方态)。
fn find_pre_host_snapshot_text(backup_dir: &Path, grok_config_path: &Path) -> Option<String> {
    let mut candidates: Vec<(Option<std::time::SystemTime>, String)> = Vec::new();
    let rd = std::fs::read_dir(backup_dir).ok()?;
    for entry in rd.flatten() {
        let manifest = entry.path();
        let name = manifest.file_name()?.to_string_lossy().to_string();
        if !name.ends_with(".manifest.json") {
            continue;
        }
        let Ok(meta) =
            serde_json::from_str::<Value>(&std::fs::read_to_string(&manifest).unwrap_or_default())
        else {
            continue;
        };
        if meta.get("purpose").and_then(|v| v.as_str()) != Some("pre-host") {
            continue;
        }
        if !crate::config::backup_matches_target(&manifest, grok_config_path) {
            continue;
        }
        let toml_path = manifest.with_file_name(name.trim_end_matches(".manifest.json"));
        if let Ok(Some(data)) = crate::config::read_verified_backup(&toml_path, grok_config_path) {
            candidates.push((
                entry.metadata().and_then(|m| m.modified()).ok(),
                String::from_utf8_lossy(&data).to_string(),
            ));
        }
    }
    candidates.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts)); // 最新在前
    candidates.into_iter().next().map(|(_, text)| text)
}

/// live 读取:只做语法校验(官方态同样放行,供切换回填与界面展示)。
pub fn read_live_config(grok_config_path: &Path) -> Result<String, OpError> {
    if !grok_config_path.exists() {
        return Err((
            404,
            "E_GROK_CONFIG_MISSING".into(),
            "Grok Build 配置文件不存在".into(),
        ));
    }
    let text = std::fs::read_to_string(grok_config_path)
        .map_err(|e| (500, "E_IO".to_string(), e.to_string()))?;
    validate_syntax(&text).map_err(|e| (422, "E_CONFIG_SYNTAX".to_string(), e))?;
    Ok(text)
}

/// live 原样写回:同样只做语法校验(官方态必须可以原样写回——备份/恢复路径)。
/// 「完整自定义模型配置」的强校验仅由 `host` 的写入路径负责。
pub fn write_live_config(grok_config_path: &Path, text: &str) -> Result<(), OpError> {
    validate_syntax(text).map_err(|e| (422, "E_CONFIG_SYNTAX".to_string(), e))?;
    if let Some(p) = grok_config_path.parent() {
        std::fs::create_dir_all(p).map_err(|e| (500, "E_IO".to_string(), e.to_string()))?;
    }
    write_text_atomic(grok_config_path, text).map_err(|e| (500, "E_IO".to_string(), e))
}

/// 开启托管:备份→合并(段级)→强校验→原子写。幂等:同供应商同方式合并结果与
/// 现值相同 → 不写盘。换供应商/换方式 → pre-switch 备份后重写。
/// 注:不动 providers.json 的 active(全局单实例归 codex 桌面版;grokbuild 的
/// 供应商选取归未来泛化路由/网关 agent 路由,接线时决定)。
pub fn host(
    grok_config_path: &Path,
    backup_dir: &Path,
    provider: &Provider,
    way: &str,
) -> Result<Value, OpError> {
    if way != "gateway" && way != "direct" {
        return Err((
            400,
            "E_BAD_WAY".into(),
            "未知托管方式,仅支持 gateway / direct".into(),
        ));
    }
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
    let io = |e: String| -> OpError { (500, "E_IO".to_string(), e) };

    let current = std::fs::read_to_string(grok_config_path).unwrap_or_default();
    let new_text = build_hosted_toml(&current, provider, way)
        .and_then(|t| validate_config(&t).map(|_| t))
        .map_err(|e| (422, "E_CONFIG_INVALID".to_string(), e))?;

    let already = detect_hosting(grok_config_path);
    let config_written = if new_text != current {
        // 首次托管 pre-host(供 unhost 受控还原);换供应商/换路 pre-switch
        //(不新增 pre-host 快照,保住最初快照还原到首次托管前,同 codex host 语义)
        let purpose = if already.is_null() {
            "pre-host"
        } else {
            "pre-switch"
        };
        crate::config::backup_file(grok_config_path, backup_dir, "config-apply", purpose)
            .map_err(io)?;
        if let Some(p) = grok_config_path.parent() {
            std::fs::create_dir_all(p).map_err(|e| io(e.to_string()))?;
        }
        write_text_atomic(grok_config_path, &new_text).map_err(io)?;
        true
    } else {
        false
    };

    Ok(json!({
        "hosted": true, "way": way, "switched": !already.is_null(),
        "hosting": detect_hosting(grok_config_path),
        "changed": { "config": config_written },
    }))
}

/// 还原官方/托管前状态:未托管(官方态或用户自己的 profile)→ 幂等 no-op;
/// 托管中 → pre-host 快照受控还原,无快照则移除受控段回落官方态。
pub fn unhost(grok_config_path: &Path, backup_dir: &Path) -> Result<Value, OpError> {
    let io = |e: String| -> OpError { (500, "E_IO".to_string(), e) };

    if detect_hosting(grok_config_path).is_null() {
        return Ok(json!({ "restored": false, "alreadyClean": true }));
    }
    let current = std::fs::read_to_string(grok_config_path).unwrap_or_default();
    let merged = match find_pre_host_snapshot_text(backup_dir, grok_config_path) {
        Some(snapshot) => restore_controlled_sections(&current, &snapshot).map_err(io)?,
        None => build_unhosted_toml(&current).map_err(io)?,
    };
    let config_written = if merged != current {
        crate::config::backup_file(grok_config_path, backup_dir, "config-apply", "pre-unhost")
            .map_err(io)?;
        write_text_atomic(grok_config_path, &merged).map_err(io)?;
        true
    } else {
        false
    };
    Ok(json!({
        "restored": true,
        "hosting": detect_hosting(grok_config_path),
        "changed": { "config": config_written },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::AccessMode;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    /// 环境变量类测试互斥(项目无 serial_test 依赖,cargo 并行测试下自管互斥)。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env_lock<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    /// 捕获-设置-恢复一个环境变量(测试结束恢复原值)。
    fn set_env_var(key: &str, value: Option<&str>) -> Option<std::ffi::OsString> {
        let original = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        original
    }
    fn restore_env_var(key: &str, original: Option<std::ffi::OsString>) {
        match original {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    fn sandbox(label: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "2xapi-grokbuild-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let grok_home = root.join("grok");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&grok_home).unwrap();
        std::fs::create_dir_all(&backup_dir).unwrap();
        let config_path = grok_home.join("config.toml");
        (root, config_path, backup_dir)
    }

    fn provider(id: &str, name: &str) -> Provider {
        Provider {
            id: id.into(),
            name: name.into(),
            agent: "grokbuild".into(),
            base_url: "https://up.example.com/".into(), // 带尾 / 验证 trim
            api_key: "sk-test-secret".into(),
            access_mode: AccessMode::PureApi,
            wire_api: WireApi::Responses,
            model: "grok-4.5".into(),
            ..Default::default()
        }
    }

    fn valid_config() -> &'static str {
        r#"[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://example.com/v1"
name = "Example"
api_key = "secret"
api_backend = "responses"
context_window = 500000
"#
    }

    fn valid_env_key_config() -> &'static str {
        r#"[models]
default = "grok-env"

[model."grok-env"]
model = "grok-4.5"
base_url = "https://example.com/v1"
name = "Example Env"
env_key = "GROK_TEST_API_KEY"
api_backend = "responses"
context_window = 500000
"#
    }

    // ── 校验:合法形状 / 语法与强校验分离 ──

    #[test]
    fn validates_expected_config_shape() {
        validate_config(valid_config()).expect("合法自定义配置应通过强校验");
        validate_config(valid_env_key_config()).expect("env_key 形态同样合法");
    }

    #[test]
    fn syntax_validation_accepts_official_snapshots() {
        validate_syntax("").expect("空文档(官方态)合法");
        validate_syntax("  \n# comment only\n").expect("仅注释合法");
        validate_syntax("[mcp_servers.echo]\ncommand = \"echo\"\n").expect("官方态(无模型表)合法");
        assert!(validate_syntax("not = [valid").is_err(), "语法坏必须报错");
    }

    #[test]
    fn official_live_config_detection() {
        // 官方态:完全没有自定义模型痕迹(空/仅注释/仅 mcp 均算)
        assert!(is_official_live(""));
        assert!(is_official_live("  \n# comment only\n"));
        assert!(is_official_live("[mcp_servers.echo]\ncommand = \"echo\"\n"));
        // 出现过任一自定义键(哪怕残缺)都不是官方态,交给强校验报错
        assert!(!is_official_live(valid_config()));
        assert!(
            !is_official_live("[models]\ndefault = \"x\"\n"),
            "残缺 [models] 不算官方态"
        );
        assert!(
            !is_official_live("[model.x]\nmodel = \"x\"\n"),
            "残缺 [model.x] 不算官方态"
        );
        // 语法不合法不是官方态
        assert!(!is_official_live("not = [valid"));
    }

    // ── 强校验:缺任一必需字段报对应字段名 ──

    #[test]
    fn rejects_missing_required_fields_with_field_names() {
        let cases: Vec<(&str, &str)> = vec![
            ("model", r#"[model."grok-4.5"]"#),
            ("base_url", r#"base_url = """#),
            ("name", r#"name = " ""#),
            ("api_backend", r#"api_backend = """#),
        ];
        for (field, _) in &cases {
            let broken = valid_config()
                .lines()
                .filter(|l| !l.starts_with(&format!("{field} =")))
                .collect::<Vec<_>>()
                .join("\n");
            let err = validate_config(&broken).unwrap_err();
            assert!(err.contains(field), "缺 {field} 应报字段名,得到: {err}");
        }
        // 缺整段
        let no_models = valid_config().replace("[models]\ndefault = \"grok-4.5\"\n\n", "");
        assert!(validate_config(&no_models)
            .unwrap_err()
            .contains("[models]"));
        let no_default = valid_config().replace("default = \"grok-4.5\"", "");
        assert!(validate_config(&no_default)
            .unwrap_err()
            .contains("default"));
        let no_table = "[models]\ndefault = \"grok-4.5\"\n";
        assert!(validate_config(no_table).unwrap_err().contains("model"));
        // default 指向不存在的 profile
        let dangling = valid_config().replace("default = \"grok-4.5\"", "default = \"missing\"");
        assert!(validate_config(&dangling)
            .unwrap_err()
            .contains("[model.\"missing\"]"));
    }

    #[test]
    fn rejects_config_without_api_key_or_env_key() {
        let broken = valid_config().replace("api_key = \"secret\"\n", "");
        let err = validate_config(&broken).expect_err("凭据缺失必须报错");
        assert!(
            err.contains("api_key") && err.contains("env_key"),
            "报错应同时点名两字段: {err}"
        );
        // api_key 全空白同样视为缺失(env_key 也没有 → 报错)
        let blank = valid_config().replace("api_key = \"secret\"", "api_key = \"   \"");
        assert!(validate_config(&blank).is_err());
    }

    #[test]
    fn rejects_invalid_context_window() {
        for bad in [
            "context_window = 0",
            "context_window = -5",
            "context_window = \"big\"",
        ] {
            let broken = valid_config().replace("context_window = 500000", bad);
            let err = validate_config(&broken).unwrap_err();
            assert!(err.contains("context_window"), "应报 context_window: {err}");
        }
    }

    // ── 凭据解析(安全规则)──

    #[test]
    fn credentials_api_key_takes_priority_over_env_key() {
        with_env_lock(|| {
            let original = set_env_var("GROK_TEST_API_KEY", Some("env-secret"));
            let both = valid_env_key_config().replace(
                "name = \"Example Env\"",
                "name = \"Example Env\"\napi_key = \"inline-secret\"",
            );
            let (base, key) = extract_credentials(&both).expect("应解析出凭据");
            assert_eq!(base, "https://example.com/v1");
            assert_eq!(key, "inline-secret", "api_key 内联值必须优先于 env_key");
            restore_env_var("GROK_TEST_API_KEY", original);
        });
    }

    #[test]
    fn resolves_api_key_from_configured_environment_variable() {
        with_env_lock(|| {
            let original = set_env_var("GROK_TEST_API_KEY", Some("  env-secret  "));
            let (base, key) =
                extract_credentials(valid_env_key_config()).expect("应从环境变量解析");
            assert_eq!(base, "https://example.com/v1");
            assert_eq!(key, "env-secret", "环境变量取值应 trim");
            restore_env_var("GROK_TEST_API_KEY", original);
        });
    }

    #[test]
    fn does_not_fall_back_to_xai_api_key_when_declared_env_key_is_unset() {
        with_env_lock(|| {
            // 即使进程里恰好设了 XAI_API_KEY,也不能被静默借用到别的 base_url 上
            let original_xai = set_env_var("XAI_API_KEY", Some("xai-secret-should-not-leak"));
            let original_unset = set_env_var("GROK_TEST_DEFINITELY_UNSET_VAR", None);
            let config = valid_env_key_config()
                .replace("GROK_TEST_API_KEY", "GROK_TEST_DEFINITELY_UNSET_VAR")
                .replace("https://example.com/v1", "https://attacker.example/v1");
            let credentials = extract_credentials(&config);
            assert!(
                credentials.is_none(),
                "声明的 env_key 未设置必须返回 None,绝不兜底 XAI_API_KEY;得到 {credentials:?}"
            );
            restore_env_var("XAI_API_KEY", original_xai);
            restore_env_var("GROK_TEST_DEFINITELY_UNSET_VAR", original_unset);
        });
    }

    #[test]
    fn extract_trims_trailing_slash_from_base_url() {
        let cfg = extract_model_config(valid_config()).unwrap();
        assert_eq!(cfg.base_url, "https://example.com/v1");
        let trailing = valid_config().replace("https://example.com/v1", "https://example.com/v1/");
        assert_eq!(
            extract_model_config(&trailing).unwrap().base_url,
            "https://example.com/v1"
        );
    }

    // ── 托管写入 / 受控还原 / live 读写 ──

    #[test]
    fn build_hosted_toml_gateway_and_direct() {
        let p = provider("p1", "2xapi");
        // gateway:零 Key 契约(网关地址 + 占位),api_backend 恒 responses
        let gateway = build_hosted_toml("", &p, "gateway").unwrap();
        validate_config(&gateway).expect("生成的网关配置必须过强校验");
        assert!(gateway.contains(&format!(
            "base_url = \"{}/grokbuild\"",
            crate::config::GATEWAY_BASE_URL
        )));
        assert!(gateway.contains(&format!("api_key = \"{GATEWAY_KEY_PLACEHOLDER}\"")));
        assert!(gateway.contains("api_backend = \"responses\""));
        assert!(gateway.contains("context_window = 500000"));
        // direct:Key 落盘 + 供应商地址(尾 / 已 trim);wire_api=chat → chat_completions
        let mut chat = p.clone();
        chat.wire_api = WireApi::ChatCompletions;
        let direct = build_hosted_toml("", &chat, "direct").unwrap();
        validate_config(&direct).expect("生成的直连配置必须过强校验");
        assert!(direct.contains("base_url = \"https://up.example.com\""));
        assert!(direct.contains("api_key = \"sk-test-secret\""));
        assert!(direct.contains("api_backend = \"chat_completions\""));
        // 未知方式拒绝
        assert!(build_hosted_toml("", &p, "tunnel").is_err());
    }

    #[test]
    fn build_hosted_toml_preserves_foreign_sections() {
        let current =
            "[mcp_servers.echo]\ncommand = \"echo\"\n\n[models]\ndefault = \"user-own\"\n";
        let built = build_hosted_toml(current, &provider("p1", "2xapi"), "gateway").unwrap();
        assert!(
            built.contains("[mcp_servers.echo]"),
            "非受控段必须保留:\n{built}"
        );
        assert!(
            !built.contains("user-own"),
            "受控段([models]/[model.*])必须整段替换:\n{built}"
        );
        validate_config(&built).unwrap();
    }

    #[test]
    fn host_gateway_writes_expected_config() {
        let (_root, cfg, bk) = sandbox("host-gw");
        // live 初始为官方态(仅 mcp)
        std::fs::write(&cfg, "[mcp_servers.echo]\ncommand = \"echo\"\n").unwrap();
        let out = host(&cfg, &bk, &provider("p1", "2xapi"), "gateway").unwrap();
        assert_eq!(out["hosted"], json!(true));
        assert_eq!(out["changed"]["config"], json!(true));
        let written = std::fs::read_to_string(&cfg).unwrap();
        validate_config(&written).expect("写盘内容必须过强校验");
        assert!(
            written.contains("[mcp_servers.echo]"),
            "用户 mcp 段保留:\n{written}"
        );
        assert_eq!(detect_hosting(&cfg)["way"], json!("gateway"));
    }

    #[test]
    fn host_creates_missing_grok_dir_and_official_empty_live() {
        let (root, _cfg, bk) = sandbox("host-missing");
        let cfg = root.join("fresh").join("config.toml"); // ~/.grok 尚不存在
        host(&cfg, &bk, &provider("p1", "2xapi"), "gateway").unwrap();
        let written = std::fs::read_to_string(&cfg).unwrap();
        validate_config(&written).unwrap();
    }

    #[test]
    fn host_is_idempotent_same_provider_and_way() {
        let (_root, cfg, bk) = sandbox("host-idem");
        let p = provider("p1", "2xapi");
        host(&cfg, &bk, &p, "gateway").unwrap();
        let first = std::fs::read_to_string(&cfg).unwrap();
        let out = host(&cfg, &bk, &p, "gateway").unwrap();
        assert_eq!(
            out["changed"]["config"],
            json!(false),
            "同供应商同方式应 no-op"
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            first,
            "文件不得被重写"
        );
        // 换供应商 → pre-switch 备份 + 重写
        let out2 = host(&cfg, &bk, &provider("p2", "另一家"), "gateway").unwrap();
        assert_eq!(out2["switched"], json!(true));
        assert_eq!(out2["changed"]["config"], json!(true));
        assert_eq!(detect_hosting(&cfg)["way"], json!("gateway"));
    }

    #[test]
    fn host_rejects_unknown_way_and_empty_model() {
        let (_root, cfg, bk) = sandbox("host-reject");
        let err = host(&cfg, &bk, &provider("p1", "2xapi"), "tunnel").unwrap_err();
        assert_eq!(err.0, 400);
        assert_eq!(err.1, "E_BAD_WAY");
        let mut no_model = provider("p1", "2xapi");
        no_model.model = "  ".into();
        let err2 = host(&cfg, &bk, &no_model, "gateway").unwrap_err();
        assert_eq!(err2.0, 422);
        assert_eq!(err2.1, "E_NO_MODEL");
    }

    #[test]
    fn unhost_restores_pre_host_official_state() {
        let (_root, cfg, bk) = sandbox("unhost-official");
        let official = "[mcp_servers.echo]\ncommand = \"echo\"\n";
        std::fs::write(&cfg, official).unwrap();
        host(&cfg, &bk, &provider("p1", "2xapi"), "gateway").unwrap();
        assert!(!detect_hosting(&cfg).is_null());

        let out = unhost(&cfg, &bk).unwrap();
        assert_eq!(out["restored"], json!(true));
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(is_official_live(&after), "还原后应为官方态:\n{after}");
        assert!(
            after.contains("[mcp_servers.echo]"),
            "用户 mcp 段保留:\n{after}"
        );
        assert!(detect_hosting(&cfg).is_null());
        // 再 unhost:幂等 no-op
        let again = unhost(&cfg, &bk).unwrap();
        assert_eq!(again["alreadyClean"], json!(true));
    }

    #[test]
    fn unhost_restores_user_custom_profiles_from_snapshot() {
        let (_root, cfg, bk) = sandbox("unhost-user");
        // 用户自己的自定义 profile:unhost 必须原样归还,不得清空
        std::fs::write(&cfg, valid_config()).unwrap();
        host(&cfg, &bk, &provider("p1", "2xapi"), "gateway").unwrap();
        let hosted = std::fs::read_to_string(&cfg).unwrap();
        assert!(!hosted.contains("[model.\"grok-4.5\"]"));

        unhost(&cfg, &bk).unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            after.contains("[model.\"grok-4.5\"]"),
            "用户自己的 profile 应从快照还原:\n{after}"
        );
        validate_config(&after).expect("还原后仍是完整合法的自定义配置");
        // 还原后 default 指向用户 profile → 非托管态
        assert!(detect_hosting(&cfg).is_null());
    }

    #[test]
    fn unhost_without_snapshot_yields_official_state() {
        let (_root, cfg, bk) = sandbox("unhost-nosnap");
        host(&cfg, &bk, &provider("p1", "2xapi"), "direct").unwrap();
        // 清掉备份(模拟无 pre-host 快照)
        std::fs::remove_dir_all(&bk).unwrap();
        std::fs::create_dir_all(&bk).unwrap();
        unhost(&cfg, &bk).unwrap();
        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            is_official_live(&after),
            "无快照时移除受控段应回落官方态:\n{after}"
        );
    }

    #[test]
    fn unhost_is_noop_for_user_own_or_official_live() {
        let (_root, cfg, bk) = sandbox("unhost-foreign");
        // 官方态:概不触碰
        std::fs::write(&cfg, "").unwrap();
        let out = unhost(&cfg, &bk).unwrap();
        assert_eq!(out["alreadyClean"], json!(true));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "");
        // 用户自己的 profile(default 指向非 2xapi):同样不算我们托管
        std::fs::write(&cfg, valid_config()).unwrap();
        let out2 = unhost(&cfg, &bk).unwrap();
        assert_eq!(out2["alreadyClean"], json!(true));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), valid_config());
    }

    #[test]
    fn detect_hosting_gateway_vs_direct() {
        let (_root, cfg, _bk) = sandbox("detect");
        assert!(detect_hosting(&cfg).is_null(), "文件不存在 → 未托管");
        std::fs::write(&cfg, valid_config()).unwrap();
        assert!(
            detect_hosting(&cfg).is_null(),
            "用户自己的 profile → 未托管"
        );
        host(&cfg, &_bk, &provider("p1", "2xapi"), "direct").unwrap();
        assert_eq!(detect_hosting(&cfg)["way"], json!("direct"));
    }

    #[test]
    fn live_official_roundtrip_and_syntax_gate() {
        let (_root, cfg, _bk) = sandbox("live-rt");
        // 官方态原文(仅 mcp):读取与原样写回都必须放行
        let official = "[mcp_servers.echo]\ncommand = \"echo\"\n";
        write_live_config(&cfg, official).unwrap();
        assert_eq!(read_live_config(&cfg).unwrap(), official);
        assert!(is_official_live(&read_live_config(&cfg).unwrap()));
        // 空文件(官方态常见)同样合法
        write_live_config(&cfg, "").unwrap();
        assert_eq!(read_live_config(&cfg).unwrap(), "");
        // 语法坏:读写双双拒绝
        assert!(write_live_config(&cfg, "not = [valid").is_err());
        std::fs::write(&cfg, "not = [valid").unwrap();
        let err = read_live_config(&cfg).unwrap_err();
        assert_eq!(err.1, "E_CONFIG_SYNTAX");
        // 文件不存在:读取报缺失
        let (_r2, cfg2, _b2) = sandbox("live-missing");
        let err2 = read_live_config(&cfg2).unwrap_err();
        assert_eq!(err2.0, 404);
        assert_eq!(err2.1, "E_GROK_CONFIG_MISSING");
    }

    #[test]
    fn host_direct_writes_provider_url_and_key() {
        let (_root, cfg, bk) = sandbox("host-direct");
        let out = host(&cfg, &bk, &provider("p1", "2xapi"), "direct").unwrap();
        assert_eq!(out["way"], json!("direct"));
        let written = std::fs::read_to_string(&cfg).unwrap();
        validate_config(&written).unwrap();
        assert!(
            written.contains("base_url = \"https://up.example.com\""),
            "direct 应写供应商地址(尾 / trim):\n{written}"
        );
        assert!(
            written.contains("api_key = \"sk-test-secret\""),
            "direct = Key 落盘(已定稿差异)"
        );
        assert_eq!(detect_hosting(&cfg)["way"], json!("direct"));
    }
}
