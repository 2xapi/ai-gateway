//! 供应商数据层（M1，契约 02 冻结）。
//!
//! - `Provider` 结构逐字段对齐 02 §1（23 字段）。
//! - `AccessMode`/`WireApi` 序列化按 02 §4（snake_case）；**反序列化兼容历史 camelCase 值**，避免旧 providers.json 被清空。
//! - `providers.json` 存储按 02 §3（兼容旧 `active_provider_id`，新增 `active_provider_ids` 按平台保存），原子写（临时文件→rename）。
//! - 字段校验按 02 §2（返回 `Vec<ValidationError>` 字段级错误，供 M4 映射为 422 `E_VALIDATION`）。
//! - CRUD 按 FR-1.1~1.6。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

// ── 枚举（02 §4）────────────────────────────────────────────

/// 接入模式：序列化 `"official"`/`"mixed"`/`"pure_api"`；反序列化兼容历史 `"pureApi"` 等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum AccessMode {
    Official,
    Mixed,
    #[default]
    PureApi,
}

impl AccessMode {
    /// 宽松解析：接受 snake_case / camelCase / 全小写。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "official" => Some(Self::Official),
            "mixed" => Some(Self::Mixed),
            "pure_api" | "pureapi" => Some(Self::PureApi),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for AccessMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        AccessMode::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("未知 access_mode: {s}")))
    }
}

/// 上游协议:序列化 `"responses"`/`"chat_completions"`/`"anthropic"`/`"gemini"`;反序列化兼容 `"chat"`/`"messages"` 等。
/// `gemini`(多平台阶段 C):上游原生 Google generateContent 协议(2xa 实测支持),/v1beta 入口透传不转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum WireApi {
    #[default]
    Responses,
    ChatCompletions,
    Anthropic,
    Gemini,
}

impl WireApi {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "responses" => Some(Self::Responses),
            "chat_completions" | "chatcompletions" | "chat" => Some(Self::ChatCompletions),
            "anthropic" | "messages" => Some(Self::Anthropic),
            "gemini" | "generatecontent" => Some(Self::Gemini),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for WireApi {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        WireApi::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("未知 wire_api: {s}")))
    }
}

// ── 数据结构（02 §1）─────────────────────────────────────────

/// 单个模型条目（02 §1）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub is_multimodal: bool,
    #[serde(default)]
    pub send_as_is: bool,
}

/// Claude Desktop 菜单角色到上游实际模型的映射。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeDesktopModelRoute {
    pub role: String,
    #[serde(default)]
    pub model: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "label_override"
    )]
    pub label_override: Option<String>,
    #[serde(default = "default_true", rename = "supports1m", alias = "supports_1m")]
    pub supports_1m: bool,
}

/// Provider 完整结构（02 §1，23 字段，逐字段一致）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Provider {
    // ── 基础 ──
    pub id: String,
    pub name: String,
    /// 归属 agent:默认 `"codex"`(Claude 接入时再扩展)。旧 providers.json 无该字段 → 反序列化补默认,不写回文件。
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub icon_color: Option<String>,
    #[serde(default)]
    pub sort_index: i64,
    #[serde(default)]
    pub created_at: i64, // unix 秒（02 §1；旧代码用毫秒，已修正）
    #[serde(default)]
    pub website_url: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,

    // ── 连接 ──
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Key 资源池(超融合 A 线一期):多 Key 轮询+故障切换;空=单 Key 模式(行为不变)。
    /// api_key 恒为「主 Key」(池首),旧文件无该字段 → 空,读侧回退 api_key。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    #[serde(default)]
    pub access_mode: AccessMode,
    #[serde(default)]
    pub wire_api: WireApi,

    // ── 协议 ──
    #[serde(default)]
    pub user_agent: Option<String>,

    // ── 模型 ──
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_desktop_model_routes: Vec<ClaudeDesktopModelRoute>,
    #[serde(default)]
    pub context_window: Option<String>,

    // ── 网络 ──
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    // ── Sub2API（本期 stub，01-D6）──
    #[serde(default)]
    pub sub2api_enabled: bool,
    #[serde(default = "default_multiplier")]
    pub sub2api_multiplier: f64,

    // ── 高级 ──
    #[serde(default)]
    pub custom_headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub config_toml_snapshot: Option<String>,
    #[serde(default)]
    pub auth_json_snapshot: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Option<Vec<String>>,
}

fn default_multiplier() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
}

fn default_agent() -> String {
    "codex".to_string()
}

/// agent 白名单归一化:空 / 未知 → 默认 `"codex"`(白名单来自 agents 注册表的已实现平台;
/// gemini 随阶段 C 第一段在注册表置 available=true,此处零改动)。
fn normalize_agent(agent: &str) -> String {
    let norm = agent.trim().to_ascii_lowercase();
    if crate::agents::find(&norm).is_some_and(|m| m.available) {
        norm
    } else {
        default_agent()
    }
}

/// providers.json 顶层结构（02 §3）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderData {
    #[serde(default)]
    pub schema_version: i64,
    /// 兼容旧客户端的最后一次选择；网关路由不再依赖该全局字段。
    #[serde(default)]
    pub active_provider_id: Option<String>,
    /// 每个平台独立的 active provider，避免跨平台选择互相覆盖。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub active_provider_ids: HashMap<String, String>,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

/// 字段级校验错误（02 §2）。M4 将映射为 422 `E_VALIDATION` + fields。
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

/// 创建/更新输入（不含 id/created_at/sort_index/snapshot，这些由系统生成或不可变）。
/// FR-1.4「编辑=合并」以此结构承载新值。
#[derive(Debug, Clone, Default)]
pub struct ProviderInput {
    pub name: String,
    /// 归属 agent:缺省空串,经 value_to_input/create/update 归一化为 `"codex"`(缺省不崩)。
    pub agent: String,
    pub icon: Option<String>,
    pub icon_color: Option<String>,
    pub website_url: Option<String>,
    pub notes: Option<String>,
    pub base_url: String,
    pub api_key: String,
    pub access_mode: AccessMode,
    pub wire_api: WireApi,
    pub user_agent: Option<String>,
    pub model: String,
    pub models: Vec<ModelConfig>,
    /// None 表示旧客户端未提交该字段，更新时保留已有映射。
    pub claude_desktop_model_routes: Option<Vec<ClaudeDesktopModelRoute>>,
    pub context_window: Option<String>,
    pub proxy_url: Option<String>,
    pub timeout_secs: Option<u64>,
    pub sub2api_enabled: bool,
    pub sub2api_multiplier: f64,
    pub custom_headers: Option<HashMap<String, String>>,
    // ProviderInput 不经 serde 反序列化（由 value_to_input 解析），缺失键默认 None → 旧请求不带该字段时不会清空已保存档位。
    pub reasoning_levels: Option<Vec<String>>,
}

// ── 存储读写（02 §3，原子写）─────────────────────────────────

pub fn load(path: &Path) -> ProviderData {
    let mut data: ProviderData = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => ProviderData::default(),
    };
    // 兼容旧文件 / 空 default：保证 schema_version 落地。
    if data.schema_version == 0 {
        data.schema_version = SCHEMA_VERSION;
    }
    // agent 存量归一化：旧文件缺省(serde 默认已补 codex)/ 非法值 → codex。不写回文件,除非后续保存。
    for p in &mut data.providers {
        p.agent = normalize_agent(&p.agent);
    }
    data.active_provider_ids.retain(|agent, id| {
        data.providers
            .iter()
            .any(|provider| provider.id == *id && provider.agent == *agent)
    });
    // 旧文件只有全局 active：迁移到该 provider 所属平台；不立即写盘，下一次正常保存时落地。
    if let Some(id) = data.active_provider_id.as_deref() {
        if let Some(provider) = data.providers.iter().find(|provider| provider.id == id) {
            data.active_provider_ids
                .entry(provider.agent.clone())
                .or_insert_with(|| id.to_string());
        }
    }
    data
}

fn save_atomic(path: &Path, data: &ProviderData, op: &str) -> Result<(), String> {
    let raw = serde_json::to_string_pretty(data).map_err(|e| format!("序列化失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &raw).map_err(|e| format!("写临时文件失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置临时文件权限失败: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("重命名失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置供应商文件权限失败: {e}"))?;
    }
    // 审计(尽力而为,失败不影响主流程)——排查 providers.json 异常变动的唯一指望(§1.5-2)
    append_audit(path, op, data);
    Ok(())
}

/// 每次 providers.json 写操作追加一行 JSONL 到同目录 providers.audit.jsonl。
fn append_audit(providers_path: &Path, op: &str, data: &ProviderData) {
    use std::io::Write;
    let audit_path = providers_path.with_file_name("providers.audit.jsonl");
    let line = json!({
        "ts": chrono::Local::now().to_rfc3339(),
        "op": op,
        "active": data.active_provider_id,
        "activeByAgent": data.active_provider_ids,
        "providers": data.providers.iter().map(|p| json!({"id": p.id, "name": p.name})).collect::<Vec<_>>(),
        "count": data.providers.len(),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&audit_path)
    {
        let _ = writeln!(f, "{}", line);
    }
}

/// 持久化 ProviderData（供 config.rs 的 activate 编排用）。
pub fn store(path: &Path, data: &ProviderData) -> Result<(), String> {
    save_atomic(path, data, "store")
}

/// ProviderInput → Provider（id/created_at/sort_index 置空；供 preview 临时对象用）。
pub fn input_to_provider(input: ProviderInput) -> Provider {
    Provider {
        id: String::new(),
        name: input.name,
        agent: normalize_agent(&input.agent),
        icon: input.icon,
        icon_color: input.icon_color,
        sort_index: 0,
        created_at: 0,
        website_url: input.website_url,
        notes: input.notes,
        base_url: input.base_url,
        api_key: input.api_key,
        keys: vec![],
        access_mode: input.access_mode,
        wire_api: input.wire_api,
        user_agent: input.user_agent,
        model: input.model,
        models: input.models,
        claude_desktop_model_routes: input.claude_desktop_model_routes.unwrap_or_default(),
        context_window: input.context_window,
        proxy_url: input.proxy_url,
        timeout_secs: input.timeout_secs,
        sub2api_enabled: input.sub2api_enabled,
        sub2api_multiplier: input.sub2api_multiplier,
        custom_headers: input.custom_headers,
        config_toml_snapshot: None,
        auth_json_snapshot: None,
        reasoning_levels: input.reasoning_levels,
    }
}

// ── 校验（02 §2）─────────────────────────────────────────────

pub fn validate(input: &ProviderInput) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();

    let name_len = input.name.trim().chars().count();
    if name_len == 0 || name_len > 40 {
        errs.push(ValidationError {
            field: "name".into(),
            message: "名称需 1~40 字符".into(),
        });
    }

    // 非 Official 模式：base_url / api_key 必填
    if input.access_mode != AccessMode::Official {
        let base = input.base_url.trim();
        if base.is_empty() {
            errs.push(ValidationError {
                field: "base_url".into(),
                message: "非 Official 模式必填 base_url".into(),
            });
        } else if !(base.starts_with("http://") || base.starts_with("https://")) {
            errs.push(ValidationError {
                field: "base_url".into(),
                message: "base_url 须为 http(s):// 开头".into(),
            });
        } else if base.ends_with('/') {
            errs.push(ValidationError {
                field: "base_url".into(),
                message: "base_url 末尾不带 /".into(),
            });
        }
        if input.api_key.trim().is_empty() {
            errs.push(ValidationError {
                field: "api_key".into(),
                message: "非 Official 模式必填 api_key".into(),
            });
        }
    }

    if input.model.trim().is_empty() {
        errs.push(ValidationError {
            field: "model".into(),
            message: "model 不能为空".into(),
        });
    }

    if let Some(t) = input.timeout_secs {
        if !(5..=3600).contains(&t) {
            errs.push(ValidationError {
                field: "timeout_secs".into(),
                message: "timeout_secs 须在 5~3600".into(),
            });
        }
    }

    if input.sub2api_multiplier <= 0.0 {
        errs.push(ValidationError {
            field: "sub2api_multiplier".into(),
            message: "sub2api_multiplier 须 > 0".into(),
        });
    }

    // reasoning_levels：不做校验约束。归一化（逐项 trim + 去掉空串）已在 value_to_input 的
    // parse_reasoning_levels 入口完成；且不设白名单——上游档位可能超出 low/medium/high，避免过度约束。
    // 直接构造 ProviderInput 的调用方（如 gateway.rs 测试）传原始值也合法，validate 无需干预。

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

pub fn format_errors(errs: &[ValidationError]) -> String {
    errs.iter()
        .map(|e| format!("{}: {}", e.field, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

// ── CRUD（FR-1.1~1.6）────────────────────────────────────────

/// FR-1.1 新建：校验 → 生成 id/created_at/sort_index → 持久化。
pub fn create(path: &Path, input: ProviderInput) -> Result<Provider, Vec<ValidationError>> {
    validate(&input)?;
    let mut data = load(path);
    let sort_index = data
        .providers
        .iter()
        .map(|p| p.sort_index)
        .max()
        .unwrap_or(-1)
        + 1;
    let provider = Provider {
        id: uuid::Uuid::new_v4().to_string(),
        name: input.name,
        agent: normalize_agent(&input.agent),
        icon: input.icon,
        icon_color: input.icon_color,
        sort_index,
        created_at: chrono::Utc::now().timestamp(),
        website_url: input.website_url,
        notes: input.notes,
        base_url: input.base_url,
        api_key: input.api_key,
        keys: vec![],
        access_mode: input.access_mode,
        wire_api: input.wire_api,
        user_agent: input.user_agent,
        model: input.model,
        models: input.models,
        claude_desktop_model_routes: input.claude_desktop_model_routes.unwrap_or_default(),
        context_window: input.context_window,
        proxy_url: input.proxy_url,
        timeout_secs: input.timeout_secs,
        sub2api_enabled: input.sub2api_enabled,
        sub2api_multiplier: input.sub2api_multiplier,
        custom_headers: input.custom_headers,
        config_toml_snapshot: None,
        auth_json_snapshot: None,
        reasoning_levels: input.reasoning_levels,
    };
    data.providers.push(provider.clone());
    save_atomic(path, &data, "create").map_err(io_errs)?;
    Ok(provider)
}

/// FR-1.4 编辑：合并更新，**id/created_at/sort_index/snapshot 不变**。
/// key 敏感：编辑时不回填（06 §7），传入空 api_key 则保留旧值。
pub fn update(
    path: &Path,
    id: &str,
    input: ProviderInput,
) -> Result<Provider, Vec<ValidationError>> {
    let mut data = load(path);
    let existing_key = data
        .providers
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.api_key.clone())
        .ok_or_else(|| {
            vec![ValidationError {
                field: "id".into(),
                message: "供应商不存在".into(),
            }]
        })?;

    let mut eff = input;
    if eff.api_key.trim().is_empty() {
        eff.api_key = existing_key; // 保留旧 key
    }
    validate(&eff)?;

    let p = data
        .providers
        .iter_mut()
        .find(|p| p.id == id)
        .expect("已校验存在");
    p.name = eff.name;
    p.agent = normalize_agent(&eff.agent);
    p.icon = eff.icon;
    p.icon_color = eff.icon_color;
    p.website_url = eff.website_url;
    p.notes = eff.notes;
    p.base_url = eff.base_url;
    p.api_key = eff.api_key;
    p.access_mode = eff.access_mode;
    p.wire_api = eff.wire_api;
    p.user_agent = eff.user_agent;
    p.model = eff.model;
    p.models = eff.models;
    if let Some(routes) = eff.claude_desktop_model_routes {
        p.claude_desktop_model_routes = routes;
    }
    p.context_window = eff.context_window;
    p.proxy_url = eff.proxy_url;
    p.timeout_secs = eff.timeout_secs;
    p.sub2api_enabled = eff.sub2api_enabled;
    p.sub2api_multiplier = eff.sub2api_multiplier;
    p.custom_headers = eff.custom_headers;
    // reasoning_levels：Some → 更新为传入值；None → 保留现值（旧客户端/PUT 不带该字段时不清空已保存档位）。
    if let Some(levels) = eff.reasoning_levels {
        p.reasoning_levels = Some(levels);
    }
    let updated = p.clone();
    save_atomic(path, &data, "update").map_err(io_errs)?;
    Ok(updated)
}

/// FR-1.5 删除：若删的是 active，active 置 None（不自动切换）。
pub fn delete(path: &Path, id: &str) {
    let mut data = load(path);
    data.providers.retain(|p| p.id != id);
    data.active_provider_ids
        .retain(|_, active_id| active_id != id);
    if data.active_provider_id.as_deref() == Some(id) {
        data.active_provider_id = None;
    }
    let _ = save_atomic(path, &data, "delete");
}

/// FR-1.3 列表：按 sort_index 升序。
#[allow(dead_code)] // 路由用 load；保留作数据层 API/测试
pub fn list(path: &Path) -> Vec<Provider> {
    let mut data = load(path);
    data.providers.sort_by_key(|p| p.sort_index);
    data.providers
}

/// FR-1.6 重排：按给定 id 顺序重写 sort_index。
pub fn reorder(path: &Path, ids: &[String]) {
    let mut data = load(path);
    for (idx, id) in ids.iter().enumerate() {
        if let Some(p) = data.providers.iter_mut().find(|p| &p.id == id) {
            p.sort_index = idx as i64;
        }
    }
    let _ = save_atomic(path, &data, "reorder");
}

// ── active 管理（数据层；config 写入在 M2）──────────────────

#[allow(dead_code)] // 测试/未来路由用
pub fn set_active(path: &Path, id: &str) {
    let mut data = load(path);
    let Some(agent) = data
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .map(|provider| provider.agent.clone())
    else {
        return;
    };
    data.active_provider_ids.insert(agent, id.to_string());
    data.active_provider_id = Some(id.to_string());
    let _ = save_atomic(path, &data, "set_active");
}

/// 只切换指定平台的 active，不改兼容旧客户端使用的全局 active。
/// 多平台场景下，Claude 选中的供应商不能覆盖 Codex 的 active。
pub fn set_active_for_agent(path: &Path, id: &str) -> Result<(), String> {
    let mut data = load(path);
    let Some(provider) = data.providers.iter().find(|provider| provider.id == id) else {
        return Err("供应商不存在".into());
    };
    data.active_provider_ids
        .insert(provider.agent.clone(), id.to_string());
    save_atomic(path, &data, "set_active_for_agent")
}

#[allow(dead_code)] // activate-official（M2）会用到
pub fn clear_active(path: &Path) {
    clear_active_for_agent(path, "codex");
}

pub fn clear_active_for_agent(path: &Path, agent: &str) {
    let mut data = load(path);
    let agent = normalize_agent(agent);
    let removed = data.active_provider_ids.remove(&agent);
    if removed.as_deref() == data.active_provider_id.as_deref()
        || data
            .active_provider_id
            .as_deref()
            .and_then(|id| data.providers.iter().find(|provider| provider.id == id))
            .is_some_and(|provider| provider.agent == agent)
    {
        data.active_provider_id = data.active_provider_ids.values().next().cloned();
    }
    let _ = save_atomic(path, &data, "clear_active");
}

pub fn get_active(path: &Path) -> Option<Provider> {
    let data = load(path);
    let id = data.active_provider_id.as_ref()?;
    data.providers.into_iter().find(|p| &p.id == id)
}

pub fn get_active_for_agent(path: &Path, agent: &str) -> Option<Provider> {
    let data = load(path);
    let agent = normalize_agent(agent);
    let id = data.active_provider_ids.get(&agent)?;
    data.providers
        .into_iter()
        .find(|provider| provider.id == *id && provider.agent == agent)
}

/// 按 agent 取当前供应商。优先使用明确 active；仅当该平台只有一个候选时兼容旧数据。
/// 多候选但缺少 active 时安全失败，禁止静默选首项把请求或 Key 发往错误上游。
///
/// **本期语义**(写死便于复核):
/// - 优先使用该 agent 的 `active_provider_ids`；
/// - 兼容旧文件时，仅接受恰好归属该 agent 的全局 `active_provider_id`；
/// - 缺少明确 active 时，只有单候选可安全兼容，多候选必须返回 `None`；
/// - 该 agent 无任何供应商 → `None`(调用方报「请先选择 X 供应商」)。
///
/// 由此 Codex 与 Claude 的 active 互不串台:即便全局 active 是 codex,`/anthropic/*`
/// 仍取 claude 供应商(claude 里首个);同理 `/v1/*` 只认 codex,绝不把 Codex 流量发给 claude 供应商。
pub fn get_provider_for_agent(path: &Path, agent: &str) -> Option<Provider> {
    let data = load(path);
    let agent = normalize_agent(agent);
    let provs: Vec<Provider> = data
        .providers
        .iter()
        .filter(|p| p.agent == agent)
        .cloned()
        .collect();
    if provs.is_empty() {
        return None;
    }
    if let Some(id) = data.active_provider_ids.get(&agent) {
        if let Some(provider) = provs.iter().find(|provider| provider.id == *id) {
            return Some(provider.clone());
        }
    }
    if let Some(id) = data.active_provider_id.as_deref() {
        if let Some(p) = provs.iter().find(|p| p.id == id) {
            return Some(p.clone());
        }
    }
    if provs.len() == 1 {
        provs.into_iter().next()
    } else {
        None
    }
}

// ── 边界映射 / 兼容（供 server.rs，camelCase ↔ snake_case；M4 会以正式路由替代）──

/// 旧入口：接收前端 camelCase JSON，转成 ProviderInput 后 create-or-update。
#[allow(dead_code)] // 兼容入口；04 契约路由已用 create/update，保留备用
pub fn save(path: &Path, body: &Value) -> Result<Provider, Vec<ValidationError>> {
    let input = value_to_input(body);
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        create(path, input)
    } else {
        update(path, id, input)
    }
}

/// 脱敏后的 Provider（前端用，camelCase，不回传明文 key）。
pub fn public_provider(p: &Provider) -> Value {
    json!({
        "id": p.id, "name": p.name,
        "agent": p.agent,
        "icon": p.icon, "iconColor": p.icon_color,
        "sortIndex": p.sort_index, "createdAt": p.created_at,
        "websiteUrl": p.website_url, "notes": p.notes,
        "baseUrl": p.base_url, "apiKeyMasked": mask_key(&p.api_key),
        "accessMode": serde_json::to_value(p.access_mode).unwrap_or(json!("pure_api")),
        "wireApi": serde_json::to_value(p.wire_api).unwrap_or(json!("responses")),
        "userAgent": p.user_agent,
        "model": p.model, "models": p.models,
        "claudeDesktopModelRoutes": p.claude_desktop_model_routes,
        "contextWindow": p.context_window,
        "proxyUrl": p.proxy_url, "timeoutSecs": p.timeout_secs,
        "sub2apiEnabled": p.sub2api_enabled, "sub2apiMultiplier": p.sub2api_multiplier,
        "customHeaders": p.custom_headers,
        // 思考档位:前端读取名=snake reasoning_levels(app.js:318/577/715/782),None 序列化为 null。
        "reasoning_levels": p.reasoning_levels,
    })
}

fn mask_key(key: &str) -> String {
    if key.len() > 8 {
        format!("{}...{}", &key[..5], &key[key.len() - 4..])
    } else {
        String::new()
    }
}

fn io_errs(e: String) -> Vec<ValidationError> {
    vec![ValidationError {
        field: "_io".into(),
        message: e,
    }]
}

/// 前端 camelCase JSON → ProviderInput。
pub fn value_to_input(body: &Value) -> ProviderInput {
    ProviderInput {
        name: body
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        agent: normalize_agent(body.get("agent").and_then(|v| v.as_str()).unwrap_or("")),
        icon: opt_str(body, &["icon"]),
        icon_color: opt_str(body, &["iconColor", "icon_color"]),
        website_url: opt_str(body, &["websiteUrl", "website_url"]),
        notes: opt_str(body, &["notes"]),
        base_url: opt_str(body, &["baseUrl", "base_url"]).unwrap_or_default(),
        api_key: opt_str(body, &["apiKey", "api_key"]).unwrap_or_default(),
        access_mode: opt_str(body, &["accessMode", "access_mode"])
            .and_then(|s| AccessMode::parse(&s))
            .unwrap_or_default(),
        wire_api: opt_str(body, &["wireApi", "wire_api"])
            .and_then(|s| WireApi::parse(&s))
            .unwrap_or_default(),
        user_agent: opt_str(body, &["userAgent", "user_agent"]),
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        models: body.get("models").map(parse_models).unwrap_or_default(),
        claude_desktop_model_routes: body
            .get("claudeDesktopModelRoutes")
            .or_else(|| body.get("claude_desktop_model_routes"))
            .map(parse_claude_desktop_model_routes),
        context_window: opt_str(body, &["contextWindow", "context_window"]),
        proxy_url: opt_str(body, &["proxyUrl", "proxy_url"]),
        timeout_secs: body
            .get("timeoutSecs")
            .or_else(|| body.get("timeout_secs"))
            .and_then(|v| v.as_u64()),
        sub2api_enabled: body
            .get("sub2apiEnabled")
            .or_else(|| body.get("sub2api_enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        sub2api_multiplier: body
            .get("sub2apiMultiplier")
            .or_else(|| body.get("sub2api_multiplier"))
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
        custom_headers: body
            .get("customHeaders")
            .or_else(|| body.get("custom_headers"))
            .map(parse_headers),
        reasoning_levels: body
            .get("reasoningLevels")
            .or_else(|| body.get("reasoning_levels"))
            .and_then(parse_reasoning_levels),
    }
}

fn opt_str(body: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| body.get(*k).and_then(|v| v.as_str()).map(|s| s.to_string()))
}

fn parse_models(v: &Value) -> Vec<ModelConfig> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        return None;
                    }
                    Some(ModelConfig {
                        name,
                        display_name: opt_str(m, &["displayName", "display_name"]),
                        context_window: m
                            .get("contextWindow")
                            .or_else(|| m.get("context_window"))
                            .and_then(|x| x.as_u64()),
                        is_multimodal: m
                            .get("isMultimodal")
                            .or_else(|| m.get("is_multimodal"))
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                        send_as_is: m
                            .get("sendAsIs")
                            .or_else(|| m.get("send_as_is"))
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_claude_desktop_model_routes(v: &Value) -> Vec<ClaudeDesktopModelRoute> {
    v.as_array()
        .map(|routes| {
            routes
                .iter()
                .filter_map(|route| {
                    let role = opt_str(route, &["role"])?;
                    let role = match role.trim().to_ascii_lowercase().as_str() {
                        "sonnet" | "claude-sonnet-5" => "sonnet",
                        "opus" | "claude-opus-5" => "opus",
                        "fable" | "claude-fable-5" => "fable",
                        "haiku" | "claude-haiku-4-5" => "haiku",
                        _ => return None,
                    };
                    Some(ClaudeDesktopModelRoute {
                        role: role.to_string(),
                        model: opt_str(route, &["model"]).unwrap_or_default(),
                        label_override: opt_str(route, &["labelOverride", "label_override"])
                            .map(|label| label.trim().to_string())
                            .filter(|label| !label.is_empty()),
                        supports_1m: route
                            .get("supports1m")
                            .or_else(|| route.get("supports_1m"))
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_headers(v: &Value) -> HashMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 reasoning_levels 数组：逐项 trim 后去掉空串（不设白名单，上游档位可超出 low/medium/high）。
fn parse_reasoning_levels(v: &Value) -> Option<Vec<String>> {
    v.as_array().map(|arr| {
        arr.iter()
            .filter_map(|lv| {
                lv.as_str()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .collect()
    })
}

// ── 单测（M1 Gate）────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!(
            "2xapi-m1-{}-{}-{}.json",
            label,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn sample_input(name: &str, mode: AccessMode) -> ProviderInput {
        ProviderInput {
            name: name.into(),
            model: "m".into(),
            access_mode: mode,
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        }
    }

    fn sample_provider() -> Provider {
        Provider {
            id: "uuid-1".into(),
            name: "Demo".into(),
            agent: "codex".into(),
            icon: Some("🚀".into()),
            icon_color: Some("#fff".into()),
            sort_index: 3,
            created_at: 1_700_000_000,
            website_url: Some("https://x.test".into()),
            notes: Some("n".into()),
            base_url: "https://up.test".into(),
            api_key: "sk-secret".into(),
            keys: vec![],
            access_mode: AccessMode::Mixed,
            wire_api: WireApi::ChatCompletions,
            user_agent: Some("ua".into()),
            reasoning_levels: None,
            model: "gpt-demo".into(),
            models: vec![ModelConfig {
                name: "gpt-demo".into(),
                display_name: Some("Demo".into()),
                context_window: Some(128000),
                is_multimodal: true,
                send_as_is: false,
            }],
            claude_desktop_model_routes: vec![],
            context_window: Some("128k".into()),
            proxy_url: Some("http://127.0.0.1:7890".into()),
            timeout_secs: Some(120),
            sub2api_enabled: true,
            sub2api_multiplier: 1.5,
            custom_headers: Some(HashMap::from([("X-Test".into(), "1".into())])),
            config_toml_snapshot: Some("...toml...".into()),
            auth_json_snapshot: Some("...json...".into()),
        }
    }

    fn has_field(errs: &[ValidationError], field: &str) -> bool {
        errs.iter().any(|e| e.field == field)
    }

    #[test]
    fn enum_serialization_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&AccessMode::Official).unwrap(),
            "\"official\""
        );
        assert_eq!(
            serde_json::to_string(&AccessMode::Mixed).unwrap(),
            "\"mixed\""
        );
        assert_eq!(
            serde_json::to_string(&AccessMode::PureApi).unwrap(),
            "\"pure_api\""
        );
        assert_eq!(
            serde_json::to_string(&WireApi::Responses).unwrap(),
            "\"responses\""
        );
        assert_eq!(
            serde_json::to_string(&WireApi::ChatCompletions).unwrap(),
            "\"chat_completions\""
        );
        // 历史 camelCase 反序列化兼容
        let am: AccessMode = serde_json::from_str("\"pureApi\"").unwrap();
        assert!(matches!(am, AccessMode::PureApi));
    }

    #[test]
    fn provider_round_trip() {
        let p = sample_provider();
        let s = serde_json::to_string(&p).unwrap();
        let back: Provider = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn validation_branches() {
        // 空 name
        assert!(has_field(
            &validate(&sample_input("", AccessMode::Official)).unwrap_err(),
            "name"
        ));
        // name 过长
        let mut long = sample_input(&"x".repeat(41), AccessMode::Official);
        assert!(has_field(&validate(&long).unwrap_err(), "name"));
        long.name = "ok".into();

        // PureApi 缺 base_url
        let mut i = sample_input("X", AccessMode::PureApi);
        assert!(has_field(&validate(&i).unwrap_err(), "base_url"));
        // 非法 url
        i.base_url = "not-a-url".into();
        assert!(has_field(&validate(&i).unwrap_err(), "base_url"));
        // 末尾带 /
        i.base_url = "https://up.test/".into();
        assert!(has_field(&validate(&i).unwrap_err(), "base_url"));
        // 合法 url 但缺 key
        i.base_url = "https://up.test".into();
        assert!(has_field(&validate(&i).unwrap_err(), "api_key"));
        // 合法
        i.api_key = "sk".into();
        assert!(validate(&i).is_ok(), "{:?}", validate(&i));

        // Official：无需 base_url/key
        assert!(validate(&sample_input("O", AccessMode::Official)).is_ok());

        // timeout 越界
        let mut t = sample_input("T", AccessMode::Official);
        t.timeout_secs = Some(1);
        assert!(has_field(&validate(&t).unwrap_err(), "timeout_secs"));
        t.timeout_secs = Some(4000);
        assert!(has_field(&validate(&t).unwrap_err(), "timeout_secs"));
        t.timeout_secs = Some(120);
        assert!(validate(&t).is_ok());

        // multiplier <= 0
        let mut m = sample_input("M", AccessMode::Official);
        m.sub2api_multiplier = 0.0;
        assert!(has_field(&validate(&m).unwrap_err(), "sub2api_multiplier"));

        // model 空
        let mut e = sample_input("E", AccessMode::Official);
        e.model = "".into();
        assert!(has_field(&validate(&e).unwrap_err(), "model"));
    }

    #[test]
    fn crud_persists_and_reloads() {
        let path = tmp_path("crud");

        let mut a = sample_input("Alpha", AccessMode::PureApi);
        a.base_url = "https://up.test".into();
        a.api_key = "sk-a".into();
        a.model = "gpt-a".into();
        let pa = create(&path, a).expect("create");
        assert!(!pa.id.is_empty());
        assert_eq!(pa.sort_index, 0);

        let mut b = sample_input("Beta", AccessMode::PureApi);
        b.base_url = "https://up2.test".into();
        b.api_key = "sk-b".into();
        b.model = "gpt-b".into();
        let pb = create(&path, b).expect("create2");
        assert_eq!(pb.sort_index, 1);

        // 列表升序
        let l = list(&path);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].name, "Alpha");

        // 重启后持久（重新从文件 load）
        let l2 = list(&path);
        assert_eq!(l2.len(), 2);

        // 编辑：name 变，created_at 不变
        let mut up = sample_input("Alpha2", AccessMode::PureApi);
        up.base_url = "https://up.test".into();
        up.api_key = "sk-a".into();
        up.model = "gpt-a".into();
        let updated = update(&path, &pa.id, up).expect("update");
        assert_eq!(updated.name, "Alpha2");
        assert_eq!(updated.created_at, pa.created_at);

        // 删除
        delete(&path, &pa.id);
        assert_eq!(list(&path).len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_version_present() {
        let path = tmp_path("schema");
        let mut i = sample_input("S", AccessMode::Official);
        i.model = "m".into();
        let _ = create(&path, i).unwrap();
        let data = load(&path);
        assert_eq!(data.schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn active_management() {
        let path = tmp_path("active");
        let mut i = sample_input("A", AccessMode::PureApi);
        i.base_url = "https://up.test".into();
        i.api_key = "sk".into();
        i.model = "m".into();
        let p = create(&path, i).unwrap();

        set_active(&path, &p.id);
        assert_eq!(get_active(&path).map(|x| x.id), Some(p.id.clone()));

        // 删除 active → active 置 None
        delete(&path, &p.id);
        assert!(get_active(&path).is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reorder_recomputes_sort_index() {
        let path = tmp_path("reorder");
        let a = create(&path, sample_input("A", AccessMode::Official)).unwrap();
        let b = create(&path, sample_input("B", AccessMode::Official)).unwrap();
        let c = create(&path, sample_input("C", AccessMode::Official)).unwrap();

        // 新顺序：C, A, B
        reorder(&path, &[c.id.clone(), a.id.clone(), b.id.clone()]);

        let l = list(&path);
        assert_eq!(l[0].name, "C");
        assert_eq!(l[1].name, "A");
        assert_eq!(l[2].name, "B");
        assert_eq!(
            (l[0].sort_index, l[1].sort_index, l[2].sort_index),
            (0, 1, 2)
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_file_loads_without_wipe() {
        // 模拟旧格式（current_provider_id + camelCase access_mode + 11 字段），不应崩、不应清空。
        let path = tmp_path("legacy");
        let legacy = r#"{
            "schema_version": 1,
            "current_provider_id": "old-1",
            "providers": [
                {"id":"old-1","name":"Old","base_url":"https://up.test","api_key":"sk","model":"m","wire_api":"responses","access_mode":"pureApi","sort_index":0,"created_at":1700000000}
            ]
        }"#;
        std::fs::write(&path, legacy).unwrap();
        let data = load(&path);
        assert_eq!(data.providers.len(), 1);
        assert_eq!(data.providers[0].name, "Old");
        assert!(matches!(data.providers[0].access_mode, AccessMode::PureApi));
        let _ = std::fs::remove_file(&path);
    }

    /// §1.5-2:每次写操作应在同目录 providers.audit.jsonl 追加一行(时间/操作/摘要)。
    #[test]
    fn audit_log_appended_on_write() {
        // 专有隔离目录(避免与并行测试共享 /tmp 下任何路径)
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("2xapi-audit-iso-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("providers.json");
        let audit_path = root.join("providers.audit.jsonl");

        // create 两次 + set_active 一次 → 3 行
        let input = ProviderInput {
            name: "AuditA".into(),
            base_url: "https://a.test".into(),
            api_key: "sk-a".into(),
            model: "m".into(),
            access_mode: AccessMode::PureApi,
            sub2api_multiplier: 1.0,
            ..Default::default()
        };
        let a = create(&path, input.clone()).unwrap();
        let b = create(&path, input).unwrap();
        set_active(&path, &a.id);

        let raw = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 3, "三次写应有三条审计:\n{raw}");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["op"], "create");
        assert_eq!(first["count"], 1);
        assert_eq!(first["providers"][0]["name"], "AuditA");
        let third: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(third["op"], "set_active");
        assert_eq!(third["active"], a.id);
        // 删除后审计记录 b 消失
        delete(&path, &b.id);
        let raw2 = std::fs::read_to_string(&audit_path).unwrap();
        assert_eq!(raw2.lines().filter(|l| !l.trim().is_empty()).count(), 4);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_persists_reasoning_levels() {
        let path = tmp_path("rl_create");
        let mut i = sample_input("RL", AccessMode::Official);
        i.model = "m".into();
        i.reasoning_levels = Some(vec!["low".into(), "high".into()]);
        let p = create(&path, i).expect("create");
        assert_eq!(p.reasoning_levels, Some(vec!["low".into(), "high".into()]));
        // 重载后仍在
        let data = load(&path);
        assert_eq!(
            data.providers[0].reasoning_levels,
            Some(vec!["low".into(), "high".into()])
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_reasoning_levels_some_updates_none_preserves() {
        let path = tmp_path("rl_update");
        let mut i = sample_input("RLU", AccessMode::Official);
        i.model = "m".into();
        i.reasoning_levels = Some(vec!["low".into()]);
        let p = create(&path, i).expect("create");

        // None → 保留原值
        let keep = sample_input("RLU2", AccessMode::Official);
        let p2 = update(&path, &p.id, keep).expect("update-keep");
        assert_eq!(p2.reasoning_levels, Some(vec!["low".into()]));

        // Some → 更新
        let mut set = sample_input("RLU3", AccessMode::Official);
        set.reasoning_levels = Some(vec!["medium".into(), "high".into()]);
        let p3 = update(&path, &p.id, set).expect("update-set");
        assert_eq!(
            p3.reasoning_levels,
            Some(vec!["medium".into(), "high".into()])
        );

        // 持久化后仍在
        let data = load(&path);
        assert_eq!(
            data.providers[0].reasoning_levels,
            Some(vec!["medium".into(), "high".into()])
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_request_without_reasoning_levels_is_fine() {
        let path = tmp_path("rl_old");
        let mut i = sample_input("Old", AccessMode::Official);
        i.model = "m".into();
        // 直接构造且不带 reasoning_levels（None）→ create 不崩、字段为 None
        let p = create(&path, i).expect("create");
        assert_eq!(p.reasoning_levels, None);

        // 经 value_to_input 的旧请求 body（无该字段）→ 字段为 None
        let input = value_to_input(&json!({"name":"ViaJson","model":"m","accessMode":"official"}));
        assert_eq!(input.reasoning_levels, None);

        // update 不带该字段 → 保留原值（仍为 None），不崩
        let keep = sample_input("Old2", AccessMode::Official);
        let p2 = update(&path, &p.id, keep).expect("update");
        assert_eq!(p2.reasoning_levels, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn value_to_input_parses_and_cleans_reasoning_levels() {
        // camelCase 优先于 snake_case（与其他字段一致）；逐项 trim 后去掉空串
        let body = json!({
            "name": "X", "model": "m",
            "reasoning_levels": [" low ", " ", "medium", ""],
            "reasoningLevels": ["high"]
        });
        assert_eq!(
            value_to_input(&body).reasoning_levels,
            Some(vec!["high".into()])
        );
        // 仅 snake_case：trim + 去空
        let body2 = json!({"name":"Y","model":"m","reasoning_levels":[" low ",""," high ","  "]});
        assert_eq!(
            value_to_input(&body2).reasoning_levels,
            Some(vec!["low".into(), "high".into()])
        );
    }

    #[test]
    fn public_provider_includes_reasoning_levels() {
        // None → 输出 null，不崩（前端 app.js 用 (p.reasoning_levels || []) 容错）
        let p_none = sample_provider(); // sample_provider 的 reasoning_levels 为 None
        let v_none = public_provider(&p_none);
        assert_eq!(v_none["reasoning_levels"], serde_json::Value::Null);

        // Some → 字段名 snake、值正确（前端读取名，app.js:318/577/715/782）
        let mut p = sample_provider();
        p.reasoning_levels = Some(vec!["low".into(), "medium".into()]);
        let v = public_provider(&p);
        assert_eq!(v["reasoning_levels"], serde_json::json!(["low", "medium"]));
        assert_eq!(v["reasoning_levels"].as_array().map(|a| a.len()), Some(2));
    }

    #[test]
    fn claude_desktop_routes_parse_publish_and_preserve_on_old_updates() {
        let body = json!({
            "name": "Desktop",
            "model": "gpt-5.6",
            "accessMode": "official",
            "claudeDesktopModelRoutes": [
                {"role":"sonnet","model":"gpt-5.6","labelOverride":"GPT 5.6","supports1m":true},
                {"role":"claude-opus-5","model":"gpt-5.6-sol","supports_1m":false},
                {"role":"unknown","model":"ignored"}
            ]
        });
        let input = value_to_input(&body);
        let routes = input.claude_desktop_model_routes.as_ref().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].role, "sonnet");
        assert_eq!(routes[0].label_override.as_deref(), Some("GPT 5.6"));
        assert!(routes[0].supports_1m);
        assert_eq!(routes[1].role, "opus");
        assert!(!routes[1].supports_1m);

        let path = tmp_path("claude_desktop_routes");
        let created = create(&path, input).unwrap();
        let public = public_provider(&created);
        assert_eq!(
            public["claudeDesktopModelRoutes"][0]["labelOverride"],
            "GPT 5.6"
        );
        assert_eq!(public["claudeDesktopModelRoutes"][1]["supports1m"], false);

        let mut old_client_update = sample_input("Desktop 2", AccessMode::Official);
        old_client_update.model = "gpt-5.7".into();
        let updated = update(&path, &created.id, old_client_update).unwrap();
        assert_eq!(
            updated.claude_desktop_model_routes,
            created.claude_desktop_model_routes
        );
        assert!(value_to_input(&json!({"name":"Old","model":"m"}))
            .claude_desktop_model_routes
            .is_none());
        let _ = std::fs::remove_file(path);
    }

    // ── UA 伪装(user_agent:预设字符串原样存取;None=网关默认 UA)──────

    #[test]
    fn user_agent_round_trip_create_update_public() {
        let path = tmp_path("ua_rt");

        // create 落盘 + 重载仍在
        let mut i = sample_input("UA", AccessMode::Official);
        i.model = "m".into();
        i.user_agent = Some("curl/8.6.0".into());
        let p = create(&path, i).expect("create");
        assert_eq!(p.user_agent.as_deref(), Some("curl/8.6.0"));
        let data = load(&path);
        assert_eq!(data.providers[0].user_agent.as_deref(), Some("curl/8.6.0"));

        // value_to_input 接 userAgent / user_agent;缺省 → None(不清旧档)
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","userAgent":"curl/8.6.0"}))
                .user_agent
                .as_deref(),
            Some("curl/8.6.0")
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","user_agent":"PostmanRuntime/7.37.3"}))
                .user_agent
                .as_deref(),
            Some("PostmanRuntime/7.37.3")
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m"})).user_agent,
            None
        );

        // public 输出 userAgent
        let v = public_provider(&p);
        assert_eq!(v["userAgent"], "curl/8.6.0");

        // update 换新值 → 重载仍在
        let mut up = sample_input("UA2", AccessMode::Official);
        up.user_agent = Some("Mozilla/5.0 Chrome/126".into());
        let p2 = update(&path, &p.id, up).expect("update");
        assert_eq!(p2.user_agent.as_deref(), Some("Mozilla/5.0 Chrome/126"));
        let data = load(&path);
        assert_eq!(
            data.providers[0].user_agent.as_deref(),
            Some("Mozilla/5.0 Chrome/126")
        );

        // update 不带该字段 → 清空(回到网关默认;与网关 filter 空串语义一致)
        let keep = sample_input("UA3", AccessMode::Official);
        let p3 = update(&path, &p.id, keep).expect("update-none");
        assert_eq!(p3.user_agent, None);
        let _ = std::fs::remove_file(&path);
    }

    // ── agent 归属(UI2)──────────────────────────────────────

    #[test]
    fn create_persists_agent() {
        let path = tmp_path("agent_create");
        let mut i = sample_input("AG", AccessMode::Official);
        i.model = "m".into();
        i.agent = "codex".into();
        let p = create(&path, i).expect("create");
        assert_eq!(p.agent, "codex");
        // 重载后仍在
        let data = load(&path);
        assert_eq!(data.providers[0].agent, "codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_defaults_agent_to_codex() {
        let path = tmp_path("agent_default");
        let mut i = sample_input("AGD", AccessMode::Official);
        i.model = "m".into();
        // 未显式设置 agent → ProviderInput 缺省空串,create 归一化为 codex
        let p = create(&path, i).expect("create");
        assert_eq!(p.agent, "codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_load_backfills_agent_codex() {
        let path = tmp_path("agent_legacy");
        // 旧文件无 agent 字段 → 补 codex(serde 默认 + load 归一化,不写回文件)
        let legacy = r#"{"schema_version":1,"providers":[{"id":"old","name":"Old","model":"m"}]}"#;
        std::fs::write(&path, legacy).unwrap();
        let data = load(&path);
        assert_eq!(data.providers[0].agent, "codex");
        // 文件内非法值 → 归一化为 codex
        let bad = r#"{"schema_version":1,"providers":[{"id":"b","name":"Bad","model":"m","agent":"evil"}]}"#;
        std::fs::write(&path, bad).unwrap();
        let data2 = load(&path);
        assert_eq!(data2.providers[0].agent, "codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn active_provider_is_isolated_per_agent() {
        let path = tmp_path("active_per_agent");
        let mut codex_a = sample_input("Codex A", AccessMode::Official);
        codex_a.agent = "codex".into();
        let codex_a = create(&path, codex_a).unwrap();
        let mut codex_b = sample_input("Codex B", AccessMode::Official);
        codex_b.agent = "codex".into();
        let codex_b = create(&path, codex_b).unwrap();
        let mut claude_a = sample_input("Claude A", AccessMode::Official);
        claude_a.agent = "claude".into();
        let claude_a = create(&path, claude_a).unwrap();
        let mut claude_b = sample_input("Claude B", AccessMode::Official);
        claude_b.agent = "claude".into();
        let claude_b = create(&path, claude_b).unwrap();

        set_active(&path, &codex_b.id);
        set_active(&path, &claude_b.id);

        assert_eq!(
            get_provider_for_agent(&path, "codex").map(|p| p.id),
            Some(codex_b.id)
        );
        assert_eq!(
            get_provider_for_agent(&path, "claude").map(|p| p.id),
            Some(claude_b.id)
        );
        assert_ne!(codex_a.id, claude_a.id);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn public_provider_includes_agent() {
        let p = sample_provider(); // agent: codex
        let v = public_provider(&p);
        assert_eq!(v["agent"], "codex");
    }

    #[test]
    fn value_to_input_and_update_normalize_agent() {
        // 缺省 / 空 / 非法(白名单外)→ codex;白名单内 → 原样(codex / claude / gemini)
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m"})).agent,
            "codex"
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":""})).agent,
            "codex"
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":"codex"})).agent,
            "codex"
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":"  Claude  "})).agent,
            "claude"
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":"CLAUDE"})).agent,
            "claude"
        );
        // gemini(多平台阶段 C 白名单扩容)原样保留;未知(白名单外)→ codex
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":"gemini"})).agent,
            "gemini"
        );
        assert_eq!(
            value_to_input(&json!({"name":"X","model":"m","agent":"grok"})).agent,
            "codex"
        );
        // update 对缺省 agent 输入归一化为 codex
        let path = tmp_path("agent_update");
        let mut i = sample_input("AU", AccessMode::Official);
        i.model = "m".into();
        i.agent = "codex".into();
        let p = create(&path, i).expect("create");
        let keep = sample_input("AU2", AccessMode::Official); // agent 缺省空串
        let p2 = update(&path, &p.id, keep).expect("update");
        assert_eq!(p2.agent, "codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn get_provider_for_agent_filters_by_agent() {
        // 建 1 个 codex + 2 个 claude(先后 sort 顺序),active 指向 codex
        let path = tmp_path("agent_select");
        let mut i_c = sample_input("Cx", AccessMode::Official);
        i_c.model = "m".into();
        i_c.agent = "codex".into();
        let p_c = create(&path, i_c).expect("create codex");

        let mut i1 = sample_input("Cl1", AccessMode::Official);
        i1.model = "m".into();
        i1.agent = "claude".into();
        let _p1 = create(&path, i1).expect("create claude1");
        let mut i2 = sample_input("Cl2", AccessMode::Official);
        i2.model = "m".into();
        i2.agent = "claude".into();
        let p2 = create(&path, i2).expect("create claude2");

        set_active(&path, &p_c.id); // 全局 active 是 codex

        // active 归属 codex → codex 路径取 p_c
        assert_eq!(
            get_provider_for_agent(&path, "codex").map(|p| p.id),
            Some(p_c.id.clone())
        );
        // claude 尚未明确选择 → 安全失败，不静默回退首项。
        assert!(get_provider_for_agent(&path, "claude").is_none());

        // 切 active 到 claude2 → claude 路径取 p2；codex 保留独立 active。
        set_active(&path, &p2.id);
        assert_eq!(
            get_provider_for_agent(&path, "claude").map(|p| p.id),
            Some(p2.id.clone())
        );
        assert_eq!(
            get_provider_for_agent(&path, "codex").map(|p| p.id),
            Some(p_c.id.clone())
        );

        // 无该 agent → None
        let empty = tmp_path("agent_select_empty");
        std::fs::write(&empty, r#"{"schema_version":1,"providers":[]}"#).unwrap();
        assert!(get_provider_for_agent(&empty, "claude").is_none());
        assert!(get_provider_for_agent(&empty, "codex").is_none());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&empty);
    }

    #[test]
    fn legacy_multi_candidate_agent_without_active_fails_closed() {
        let path = tmp_path("legacy_multi_candidate");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "schema_version": 2,
                "active_provider_id": "codex-active",
                "providers": [
                    { "id": "codex-active", "name": "Codex", "agent": "codex", "base_url": "https://codex.example", "api_key": "sk-c", "model": "c" },
                    { "id": "claude-first", "name": "Claude A", "agent": "claude", "base_url": "https://a.example", "api_key": "sk-a", "model": "a", "sort_index": 0 },
                    { "id": "claude-second", "name": "Claude B", "agent": "claude", "base_url": "https://b.example", "api_key": "sk-b", "model": "b", "sort_index": 1 }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            get_provider_for_agent(&path, "codex").map(|provider| provider.id),
            Some("codex-active".into())
        );
        assert!(
            get_provider_for_agent(&path, "claude").is_none(),
            "旧文件没有 claude active 且存在多候选时必须安全失败"
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn provider_store_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = tmp_path("private_store");
        let mut input = sample_input("Private", AccessMode::PureApi);
        input.model = "m".into();
        input.api_key = "sk-private".into();
        input.base_url = "https://private.example/v1".into();
        create(&path, input).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wire_api_parses_anthropic_aliases() {
        assert!(matches!(
            WireApi::parse("anthropic"),
            Some(WireApi::Anthropic)
        ));
        assert!(matches!(
            WireApi::parse("Messages"),
            Some(WireApi::Anthropic)
        ));
        assert!(matches!(
            WireApi::parse("responses"),
            Some(WireApi::Responses)
        ));
        assert!(WireApi::parse("grpc").is_none());
    }
}
