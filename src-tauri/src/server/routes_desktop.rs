//! 桌面版托管开关与多平台 agent 路由(host/unhost/state/start/login/recovery/生态管理)。
//! 自 server.rs 原样迁出(仅可见性调整为 pub(crate)),行为零变化。

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{claude_home, err_env, ok_env, AppState};

// ── 桌面版托管开关（阶段 1，任务书 §1.1）────────────────────

// GET /api/desktop/state
pub(crate) async fn handle_desktop_state(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::desktop::state(
        &s.config_path,
        &s.providers_path,
        &s.codex_home,
    ))
}

// POST /api/desktop/host {providerId, way}
pub(crate) async fn handle_desktop_host(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    desktop_host_impl(&s, &body)
}

// POST /api/desktop/unhost
pub(crate) async fn handle_desktop_unhost(State(s): State<Arc<AppState>>) -> Response {
    desktop_unhost_impl(&s)
}

// POST /api/desktop/recovery/preview {mode: reset-config|reset-all}
#[derive(Debug, Deserialize)]
pub(crate) struct DesktopRecoveryPreviewQuery {
    mode: Option<String>,
}

pub(crate) async fn handle_desktop_recovery_preview_get(
    State(s): State<Arc<AppState>>,
    Query(query): Query<DesktopRecoveryPreviewQuery>,
) -> Response {
    match crate::codex_recovery::preview(
        &s.codex_home,
        &s.config_path,
        query.mode.as_deref().unwrap_or("reset-config"),
    ) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_RESET_PREVIEW", &error, None),
    }
}

pub(crate) async fn handle_desktop_recovery_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let mode = body
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("reset-config");
    match crate::codex_recovery::preview(&s.codex_home, &s.config_path, mode) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_RESET_PREVIEW", &error, None),
    }
}

// POST /api/desktop/recovery/apply {mode,previewToken,confirmed}
pub(crate) async fn handle_desktop_recovery_apply(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let mode = body.get("mode").and_then(Value::as_str).unwrap_or("");
    let token = body
        .get("previewToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let confirmed = body
        .get("confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match crate::codex_recovery::apply(&s.codex_home, &s.config_path, mode, token, confirmed) {
        Ok(data) => ok_env(data),
        Err(error) => {
            let code = if error.starts_with("E_EXTERNAL_CONFIG_MANAGER_ACTIVE") {
                "E_EXTERNAL_CONFIG_MANAGER_ACTIVE"
            } else if error.starts_with("E_CODEX_CONFIG_CHANGED") {
                "E_CODEX_CONFIG_CHANGED"
            } else if error.starts_with("E_CODEX_RESET_ARTIFACT_CHANGED") {
                "E_CODEX_RESET_ARTIFACT_CHANGED"
            } else if error.starts_with("E_CONFIRM_REQUIRED") {
                "E_CONFIRM_REQUIRED"
            } else if error.starts_with("E_RESET_PREVIEW_EXPIRED") {
                "E_RESET_PREVIEW_EXPIRED"
            } else {
                "E_RESET_APPLY"
            };
            err_env(StatusCode::CONFLICT, code, &error, None)
        }
    }
}

// GET /api/desktop/login/status
pub(crate) async fn handle_desktop_login_status(State(s): State<Arc<AppState>>) -> Response {
    ok_env(
        serde_json::to_value(crate::codex_security::probe_login_cached(&s.codex_home))
            .unwrap_or_else(|_| json!({"state":"unknown"})),
    )
}

// POST /api/desktop/login/start {deviceAuth?}
pub(crate) async fn handle_desktop_login_start(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let device_auth = body
        .get("deviceAuth")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match crate::codex_security::start_login(&s.codex_home, device_auth) {
        Ok(data) => ok_env(serde_json::to_value(data).unwrap_or_else(|_| json!({"started":true}))),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_CODEX_LOGIN_START", &error, None),
    }
}

// POST /api/desktop/claude-start { way? }
// 内部兼容入口：只写入 Claude Code settings.json，不生成、不返回启动命令或终端环境变量。
pub(crate) async fn handle_desktop_claude_start(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    desktop_claude_start_impl(&s, &body)
}

// POST /api/desktop/claude-launch { way?, providerId? }
// 配置写入成功后校验 Claude CLI 并打开 macOS Terminal；成功响应不返回命令、环境变量或上游 Key。
pub(crate) async fn handle_desktop_claude_launch(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    desktop_claude_launch_impl(&s, &body)
}

pub(crate) async fn handle_desktop_claude_state(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::agents::claude_code::state(
        claude_home(&s.codex_home),
        &s.backup_dir,
        &s.providers_path,
    ))
}

pub(crate) async fn handle_desktop_claude_host(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let way = body
        .get("way")
        .and_then(|value| value.as_str())
        .unwrap_or("gateway")
        .trim();
    let provider_id = body
        .get("providerId")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim();
    agent_op_response(crate::agents::claude_code::host(
        claude_home(&s.codex_home),
        &s.backup_dir,
        &s.providers_path,
        provider_id,
        way,
    ))
}

pub(crate) async fn handle_desktop_claude_unhost(State(s): State<Arc<AppState>>) -> Response {
    agent_op_response(crate::agents::claude_code::unhost(
        claude_home(&s.codex_home),
        &s.backup_dir,
        &s.providers_path,
    ))
}

// ── 多平台 agent 注册表与泛化路由(方案 §2.1,A 阶段;具名路由保留为别名,B 阶段各平台 adapter 挂 :agent 段)──

// GET /api/desktop/agents —— 注册表元数据(前端数据驱动导航,D3 决策「A 后一次全亮」)
pub(crate) async fn handle_desktop_agents() -> Response {
    ok_env(crate::agents::registry_json())
}

fn desktop_host_impl(s: &AppState, body: &Value) -> Response {
    let provider_id = body
        .get("providerId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let way = body
        .get("way")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if provider_id.is_empty() || way.is_empty() {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_BAD_REQUEST",
            "缺少 providerId 或 way",
            None,
        );
    }
    match crate::desktop::host(
        &s.config_path,
        &s.backup_dir,
        &s.codex_home,
        &s.providers_path,
        provider_id,
        way,
    ) {
        Ok(v) => ok_env(v),
        Err((status, code, msg)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": code, "message": msg })),
        )
            .into_response(),
    }
}

fn desktop_unhost_impl(s: &AppState) -> Response {
    match crate::desktop::unhost(
        &s.config_path,
        &s.backup_dir,
        &s.codex_home,
        &s.providers_path,
    ) {
        Ok(v) => ok_env(v),
        Err((status, code, msg)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": code, "message": msg })),
        )
            .into_response(),
    }
}

fn desktop_claude_start_impl(s: &AppState, body: &Value) -> Response {
    let way = body
        .get("way")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let pid = body
        .get("providerId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    match crate::desktop::claude_start(&s.providers_path, way, &pid) {
        Ok(data) => {
            let mut v = data;
            if let Value::Object(m) = &mut v {
                m.insert("ok".into(), Value::Bool(true));
            }
            Json(v).into_response()
        }
        Err((status, code, msg)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "ok": false, "error": { "code": code, "message": msg } })),
        )
            .into_response(),
    }
}

fn desktop_claude_launch_impl(s: &AppState, body: &Value) -> Response {
    let way = body
        .get("way")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let pid = body
        .get("providerId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match crate::desktop::claude_launch(&s.providers_path, way, pid) {
        Ok(data) => {
            let mut value = data;
            if let Value::Object(object) = &mut value {
                object.insert("ok".into(), Value::Bool(true));
            }
            Json(value).into_response()
        }
        Err((status, code, message)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "ok": false, "error": { "code": code, "message": message } })),
        )
            .into_response(),
    }
}

/// 泛化路由 agent 段校验:Some = 拒绝应答(未知 404 / 未实现 501);None = 已实现平台,继续分发。
pub(crate) fn reject_agent(agent: &str) -> Option<(StatusCode, &'static str, String)> {
    match crate::agents::find(agent) {
        None => Some((
            StatusCode::NOT_FOUND,
            "E_UNKNOWN_AGENT",
            format!("未知平台: {agent}"),
        )),
        Some(m) if !m.available => Some((
            StatusCode::NOT_IMPLEMENTED,
            "E_AGENT_NOT_IMPLEMENTED",
            format!("「{}」即将上线", m.name),
        )),
        Some(_) => None,
    }
}

fn agent_reject_response(st: StatusCode, code: &str, msg: &str) -> Response {
    (st, Json(json!({ "error": code, "message": msg }))).into_response()
}

fn agent_unsupported_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "E_AGENT_UNSUPPORTED", "message": "该平台无此操作" })),
    )
        .into_response()
}

/// workbuddy host/unhost/start 的统一响应包装(与 codex impl 的错误形态一致)。
fn agent_op_response(r: Result<Value, (u16, String, String)>) -> Response {
    match r {
        Ok(v) => ok_env(v),
        Err((status, code, msg)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": code, "message": msg })),
        )
            .into_response(),
    }
}

// GET /api/desktop/:agent/state —— agent=codex 与旧 /api/desktop/state 等价。
pub(crate) async fn handle_agent_state(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
) -> Response {
    if let Some((st, code, msg)) = reject_agent(&agent) {
        return agent_reject_response(st, code, &msg);
    }
    match agent.as_str() {
        "codex" => ok_env(crate::desktop::state(
            &s.config_path,
            &s.providers_path,
            &s.codex_home,
        )),
        "claude" => ok_env(crate::agents::claude_code::state(
            claude_home(&s.codex_home),
            &s.backup_dir,
            &s.providers_path,
        )),
        "workbuddy" => ok_env(crate::agents::workbuddy::state(&s.wb_home)),
        "hermes" => ok_env(crate::agents::hermes::detect_state(
            &s.hermes_home.join("config.yaml"),
        )),
        "gemini" => ok_env(crate::agents::gemini::state(&s.gem_home)),
        "grokbuild" => ok_env(crate::agents::grok::state(&s.grok_home)),
        "opencode" => ok_env(crate::agents::opencode::state(&s.oc_home)),
        "openclaw" => ok_env(crate::agents::openclaw::state(&s.oclaw_home)),
        "claude-desktop" => ok_env(crate::agents::claude_desktop::state(&s.cd_home)),
        "cursor" => ok_env(crate::agents::cursor::state(&s.cursor_home)),
        _ => agent_unsupported_response(),
    }
}

// POST /api/desktop/:agent/host {providerId, way}
pub(crate) async fn handle_agent_host(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some((st, code, msg)) = reject_agent(&agent) {
        return agent_reject_response(st, code, &msg);
    }
    match agent.as_str() {
        "codex" => desktop_host_impl(&s, &body),
        "claude" => agent_op_response(crate::agents::claude_code::host(
            claude_home(&s.codex_home),
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("gateway")
                .trim(),
        )),
        "workbuddy" => agent_op_response(crate::agents::workbuddy::host(
            &s.wb_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "hermes" => agent_op_response(crate::agents::hermes::host(
            &s.hermes_home.join("config.yaml"),
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "gemini" => agent_op_response(crate::agents::gemini::host(
            &s.gem_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "grokbuild" => agent_op_response(crate::agents::grok::host(
            &s.grok_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "opencode" => agent_op_response(crate::agents::opencode::host(
            &s.oc_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "openclaw" => agent_op_response(crate::agents::openclaw::host(
            &s.oclaw_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "claude-desktop" => agent_op_response(crate::agents::claude_desktop::host(
            &s.cd_home,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "cursor" => agent_op_response(crate::agents::cursor::host(
            &s.cursor_home,
            &s.backup_dir,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        _ => agent_unsupported_response(),
    }
}

// POST /api/desktop/:agent/unhost
pub(crate) async fn handle_agent_unhost(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
) -> Response {
    if let Some((st, code, msg)) = reject_agent(&agent) {
        return agent_reject_response(st, code, &msg);
    }
    match agent.as_str() {
        "codex" => desktop_unhost_impl(&s),
        "claude" => agent_op_response(crate::agents::claude_code::unhost(
            claude_home(&s.codex_home),
            &s.backup_dir,
            &s.providers_path,
        )),
        "workbuddy" => {
            agent_op_response(crate::agents::workbuddy::unhost(&s.wb_home, &s.backup_dir))
        }
        "hermes" => agent_op_response(crate::agents::hermes::unhost(
            &s.hermes_home.join("config.yaml"),
            &s.backup_dir,
            &s.providers_path,
        )),
        "gemini" => agent_op_response(crate::agents::gemini::unhost(&s.gem_home, &s.backup_dir)),
        "grokbuild" => agent_op_response(crate::agents::grok::unhost(&s.grok_home, &s.backup_dir)),
        "opencode" => agent_op_response(crate::agents::opencode::unhost(&s.oc_home, &s.backup_dir)),
        "openclaw" => agent_op_response(crate::agents::openclaw::unhost(
            &s.oclaw_home,
            &s.backup_dir,
        )),
        "claude-desktop" => agent_op_response(crate::agents::claude_desktop::unhost(&s.cd_home)),
        "cursor" => agent_op_response(crate::agents::cursor::unhost(&s.cursor_home, &s.backup_dir)),
        _ => agent_unsupported_response(),
    }
}

// POST /api/desktop/:agent/start —— agent=claude 与旧 /api/desktop/claude-start 等价
pub(crate) async fn handle_agent_start(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if let Some((st, code, msg)) = reject_agent(&agent) {
        return agent_reject_response(st, code, &msg);
    }
    match agent.as_str() {
        "claude" => desktop_claude_start_impl(&s, &body),
        "workbuddy" => agent_op_response(crate::agents::workbuddy::start(
            &s.providers_path,
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("gateway")
                .trim(),
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            &s.wb_home,
        )),
        "gemini" => agent_op_response(crate::agents::gemini::start(
            &s.providers_path,
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("gateway")
                .trim(),
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            &s.gem_home,
        )),
        "grokbuild" => agent_op_response(crate::agents::grok::start(
            &s.providers_path,
            body.get("way")
                .and_then(|v| v.as_str())
                .unwrap_or("gateway")
                .trim(),
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            &s.grok_home,
        )),
        "hermes" => agent_op_response(crate::agents::hermes::start(
            &s.hermes_home.join("config.yaml"),
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "opencode" => agent_op_response(crate::agents::opencode::start(
            &s.oc_home,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        "openclaw" => agent_op_response(crate::agents::openclaw::start(
            &s.oclaw_home,
            &s.providers_path,
            body.get("providerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
        )),
        _ => agent_unsupported_response(),
    }
}

// ── 生态管理(开发组·生态中心 A 段)──────────────────────────

/// eco store 构造:支持表校验 + adapter 实例化。
fn eco_store_for(
    s: &AppState,
    agent: &str,
) -> Result<Box<dyn crate::agents::eco::EcoStore>, Box<Response>> {
    use crate::agents::eco;
    match eco::supported(agent) {
        Some("codex") => Ok(Box::new(eco::codex::TomlStore::new(&s.config_path))),
        Some("cursor") => Ok(Box::new(eco::cursor::JsonStore::new(&s.cursor_home))),
        // Claude Code:User scope(~/.claude.json 顶层 mcpServers,跨项目;E5 定案,官方文档)
        Some("claude") => Ok(Box::new(eco::cursor::JsonStore::at(
            "claude",
            &s.wb_home.join(".claude.json"),
        ))),
        // WorkBuddy:标准 .mcp.json(E4 定案,本机 http 型 connector-proxy 在用=手动条目只读)
        Some("workbuddy") => Ok(Box::new(eco::cursor::JsonStore::at(
            "workbuddy",
            &s.wb_home.join(".workbuddy").join(".mcp.json"),
        ))),
        Some("gemini") => Ok(Box::new(eco::cursor::JsonStore::at(
            "gemini",
            &s.gem_home.join(".gemini").join("settings.json"),
        ))),
        Some("claude-desktop") => Ok(Box::new(eco::cursor::JsonStore::at(
            "claude-desktop",
            &s.cd_home.join("Claude").join("claude_desktop_config.json"),
        ))),
        Some("grokbuild") => Ok(Box::new(eco::codex::TomlStore::at(
            "grokbuild",
            &s.grok_home.join("config.toml"),
        ))),
        // OpenClaw:openclaw.json 的 mcp.servers 嵌套段(补齐,2026-08-17 CLI 隔离 HOME 实证);
        // enabled:false=原生停用,与 Codex 同走 native_enabled 通路
        Some("openclaw") => Ok(Box::new(eco::cursor::JsonStore::nested(
            "openclaw",
            &s.oclaw_home.join("openclaw.json"),
            &["mcp", "servers"],
            true,
        ))),
        Some("opencode") => Ok(Box::new(eco::opencode::OpencodeStore::new(&s.oc_home))),
        Some("hermes") => Ok(Box::new(eco::hermes::HermesStore::new(&s.hermes_home))),
        _ => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "E_ECO_UNKNOWN_AGENT", "message": format!("「{agent}」暂未支持生态管理") })),
        )
            .into_response()
            .into()), // Box:Response 过大,clippy result_large_err
    }
}

fn eco_op_response(r: Result<Value, (u16, String, String)>) -> Response {
    match r {
        Ok(v) => ok_env(v),
        Err((status, code, msg)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            Json(json!({ "error": code, "message": msg })),
        )
            .into_response(),
    }
}

// GET /api/desktop/:agent/eco —— MCP 条目列表(来源标记 manual/console)
pub(crate) async fn handle_agent_eco(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
) -> Response {
    let store = match eco_store_for(&s, &agent) {
        Ok(st) => st,
        Err(resp) => return *resp,
    };
    eco_op_response(crate::agents::eco::list(store.as_ref(), &s.codex_home))
}

// POST /api/desktop/:agent/eco {op: install|uninstall|enable|disable, name?, presetId?, spec?}
pub(crate) async fn handle_agent_eco_op(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(agent): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let store = match eco_store_for(&s, &agent) {
        Ok(st) => st,
        Err(resp) => return *resp,
    };
    let op = body
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let name = body
        .get("name")
        .or_else(|| body.get("presetId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let r = match op.as_str() {
        "install" => {
            if let Some(pid) = body.get("presetId").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
                let Some(p) = crate::agents::eco::find_preset(pid) else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": "E_ECO_PRESET_NOT_FOUND", "message": format!("预设不存在: {pid}") })),
                    )
                        .into_response();
                };
                let n = if name.is_empty() { p.id.to_string() } else { name };
                match crate::agents::eco::preset_spec(p, body.get("params")) {
                    Ok(spec) => crate::agents::eco::install(
                        store.as_ref(), &s.codex_home, &s.backup_dir, &n, &spec,
                    ),
                    Err(e) => Err(e),
                }
            } else if let Some(spec) = body.get("spec") {
                crate::agents::eco::install(store.as_ref(), &s.codex_home, &s.backup_dir, &name, spec)
            } else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "E_BAD_REQUEST", "message": "install 需要 presetId 或 spec(+name)" })),
                )
                    .into_response();
            }
        }
        "uninstall" => crate::agents::eco::uninstall(store.as_ref(), &s.codex_home, &s.backup_dir, &name),
        "disable" => crate::agents::eco::disable(store.as_ref(), &s.codex_home, &s.backup_dir, &name),
        "enable" => crate::agents::eco::enable(store.as_ref(), &s.codex_home, &s.backup_dir, &name),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "E_BAD_REQUEST", "message": "op 仅支持 install / uninstall / enable / disable" })),
            )
                .into_response()
        }
    };
    eco_op_response(r)
}

// GET /api/desktop/eco-presets —— MCP 预设市场(静态目录 + 支持平台表)
pub(crate) async fn handle_eco_presets() -> Response {
    ok_env(crate::agents::eco::presets_json())
}
