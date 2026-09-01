#[cfg(not(test))]
use axum::extract::State;
#[cfg(not(test))]
use axum::http::Request;
#[cfg(not(test))]
use axum::middleware::{self, Next};
use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

// ── handler 子模块(按功能域拆分;mod.rs 保留路由注册/公共辅助/鉴权,测试整体留在文件末尾)──

mod routes_desktop;
mod routes_misc;
mod routes_providers;
mod routes_sessions;
mod routes_settings;

use routes_desktop::*;
use routes_misc::*;
use routes_providers::*;
use routes_sessions::*;
use routes_settings::*;

/// 与 tauri.conf.json security.csp 保持一致；Axum 侧必须自己输出，
/// Tauri 的 CSP 注入只对 tauri:// 资产协议生效，External URL 页面不生效。
const CSP: &str = "default-src 'self'; connect-src 'self' https://turing.captcha.qcloud.com https://ca.turing.captcha.qcloud.com https://www.tycaptcha.com https://rce.tencentrio.com https://cloudcache.tencentcs.com; img-src 'self' data: https://*.captcha.gtimg.com https://*.qcloud.com; style-src 'self' 'unsafe-inline'; script-src 'self' https://turing.captcha.qcloud.com https://ca.turing.captcha.qcloud.com https://turing.captcha.gtimg.com https://global.turing.captcha.gtimg.com https://cloudcache.tencentcs.com; frame-src https://turing.captcha.qcloud.com https://ca.turing.captcha.qcloud.com https://www.tycaptcha.com; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// 本次运行的网关鉴权 token：注入到页面 HTML(data-twoxapi-token)供前端带上
/// `X-2xapi-Token`，防 DNS rebinding/跨源网页直接调用本地 API。
/// 静态路径(/, /app.js 等)公开；/api/* 与全部代理路径需要 token。
static GATEWAY_TOKEN: OnceLock<String> = OnceLock::new();

pub fn init_gateway_token() -> String {
    GATEWAY_TOKEN
        .get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
        .clone()
}

#[cfg(not(test))]
fn host_allowed(headers: &header::HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host.trim();
    let hostname = if host.starts_with('[') {
        host.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(hostname, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(not(test))]
fn origin_allowed(headers: &header::HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    matches!(origin, "http://127.0.0.1:8787" | "http://localhost:8787")
}

fn path_needs_auth(path: &str) -> bool {
    if path.starts_with("/api/") {
        return path != "/api/bootstrap";
    }
    [
        "/v1/",
        "/v1beta/",
        "/anthropic/",
        "/hermes/",
        "/cursor/",
        "/opencode/",
        "/openclaw/",
        "/grokbuild/",
        "/grok/",
        "/workbuddy/",
        "/claude-desktop/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
        || matches!(
            path,
            "/responses" | "/chat/completions" | "/images/generations" | "/models"
        )
}

#[cfg(not(test))]
async fn gateway_auth(State(token): State<String>, req: Request<Body>, next: Next) -> Response {
    let headers = req.headers();
    if !host_allowed(headers) || !origin_allowed(headers) {
        return err_json(StatusCode::UNAUTHORIZED, "非法请求来源");
    }
    if !path_needs_auth(req.uri().path()) {
        return next.run(req).await;
    }
    // 威胁模型:本防护针对「浏览器跨源(DNS rebinding/恶意网页)」。
    // 恶意网页的请求必带恶意 Host(URL 主机名)或跨源 Origin,已被上方拦截。
    // CLI 客户端(Codex/Claude Code/Cursor/Gemini 等)经 /v1/* 代理路径请求时
    // 无 Origin(非浏览器)且无法携带页面 token——视为本机可信进程放行,
    // 与「同用户进程可读本地文件」的既有威胁模型一致。
    if headers.get(header::ORIGIN).is_none() {
        return next.run(req).await;
    }
    let provided = headers
        .get("x-2xapi-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided == token {
        next.run(req).await
    } else {
        err_json(StatusCode::UNAUTHORIZED, "缺少网关访问凭证")
    }
}

#[derive(RustEmbed)]
#[folder = "../frontend/"]
struct FrontendAsset;

/// 加速配置(阶段 4,任务书 §五)。mode ∈ off|official|custom;custom_node 为用户自定义节点地址。
/// 持久化到 `{codex_home}/2xapi-settings.json` 的 `accel` 段(camelCase 与前端契约一致)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccelCfg {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub custom_node: String,
}

impl Default for AccelCfg {
    fn default() -> Self {
        AccelCfg {
            mode: "off".into(),
            custom_node: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config_path: PathBuf,
    pub backup_dir: PathBuf,
    pub providers_path: PathBuf,
    pub codex_home: PathBuf,
    /// workbuddy 双配置载体(~/.codebuddy 与 ~/.workbuddy)的公共根(即用户 home;测试传 tempdir)。
    pub wb_home: PathBuf,
    /// hermes 配置根(HERMES_HOME 优先,默认 ~/.hermes;测试传 tempdir,杜绝写真实活配置)。
    pub hermes_home: PathBuf,
    /// gemini 配置载体根(~/.gemini 所在;adapter 内 join(".gemini");测试传 tempdir)。
    pub gem_home: PathBuf,
    /// grok 配置根(~/.grok;adapter 内 join("config.toml");测试传 tempdir)。
    pub grok_home: PathBuf,
    /// opencode 载体根(HOME;adapter 内 join(".config/opencode/opencode.json");测试传 tempdir)。
    pub oc_home: PathBuf,
    /// openclaw 配置根(~/.openclaw;adapter 内 join("openclaw.json");测试传 tempdir)。
    pub oclaw_home: PathBuf,
    /// Claude Desktop 配置父目录(Application Support 根;adapter 内 join("Claude")/"Claude-3p";测试传 tempdir)。
    pub cd_home: PathBuf,
    /// Cursor 配置根(用户 HOME;eco adapter 内 join(".cursor/mcp.json");测试传 tempdir)。
    pub cursor_home: PathBuf,
    pub launcher: std::sync::Arc<crate::launcher::LauncherState>,
    /// 加速线路健康状态(启动时由 load_lines 填充;健康循环每 30s 刷新)。
    pub health: std::sync::Arc<crate::acclines::HealthState>,
    /// 加速开关配置(mode + 自定义节点;内存态 + 2xapi-settings.json 持久化)。
    pub accel: std::sync::Arc<std::sync::Mutex<AccelCfg>>,
    /// 每账号节点凭证表(星图 任务 B;启动时 load_store 装配,签发/降级时更新并落盘)。
    pub nodecreds: std::sync::Arc<std::sync::RwLock<crate::nodecreds::Store>>,
    /// Key 资源池(超融合 A 线一期):多 Key 轮询+冷却切换;内存态,单 Key 恒直返。
    pub keypool: std::sync::Arc<crate::keypool::KeyPool>,
    /// 网关总开关(托盘「网关开/关」内存态):false 时网关代理入口统一 503「网关已由托盘关闭」。
    pub tray_gate_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn build_router(state: AppState) -> Router {
    let state = Arc::new(state);
    let router = Router::new()
        // --- Static frontend ---
        .route("/", get(serve_index))
        .fallback(serve_static)
        // --- 网关健康（FR-4.1，不走统一响应信封）---
        .route("/health", get(handle_gateway_health))
        // --- 网关代理 /v1/* 和 /*（Codex 可能带或不带 /v1 前缀）---
        .route("/v1/responses", post(crate::gateway::proxy_responses))
        .route("/responses", post(crate::gateway::proxy_responses))
        .route("/v1/chat/completions", post(crate::gateway::proxy_chat))
        .route("/v1/images/generations", post(crate::gateway::proxy_images))
        .route("/chat/completions", post(crate::gateway::proxy_chat))
        .route("/images/generations", post(crate::gateway::proxy_images))
        .route("/v1/models", get(crate::gateway::proxy_models))
        .route("/models", get(crate::gateway::proxy_models))
        // --- 网关代理 /anthropic/*（Claude 接入；Claude Code 以 /anthropic 为 base 会请求 /anthropic/v1/messages）---
        .route(
            "/anthropic/v1/messages",
            post(crate::gateway::proxy_anthropic),
        )
        .route("/anthropic/messages", post(crate::gateway::proxy_anthropic))
        .route(
            "/anthropic/v1/models",
            get(crate::gateway::proxy_anthropic_models),
        )
        .route(
            "/anthropic/models",
            get(crate::gateway::proxy_anthropic_models),
        )
        .route(
            "/anthropic/v1/models/:model_id",
            get(crate::gateway::proxy_anthropic_model),
        )
        .route(
            "/anthropic/models/:model_id",
            get(crate::gateway::proxy_anthropic_model),
        )
        // --- 网关代理 /hermes/*（Hermes 接入;hermes 条目 base_url=网关+/hermes,SDK 追加 /chat/completions）---
        .route(
            "/hermes/v1/chat/completions",
            post(crate::gateway::proxy_hermes_chat),
        )
        .route(
            "/hermes/chat/completions",
            post(crate::gateway::proxy_hermes_chat),
        )
        // Cursor 入口(F 阶段;托管 base=网关+/v1,客户端直发 chat/completions)
        .route(
            "/cursor/v1/chat/completions",
            post(crate::gateway::proxy_cursor_chat),
        )
        .route(
            "/cursor/chat/completions",
            post(crate::gateway::proxy_cursor_chat),
        )
        // OpenCode/OpenClaw 入口(多平台 B 阶段收尾;条目 baseURL=网关+/{agent}/v1)
        .route(
            "/opencode/v1/chat/completions",
            post(crate::gateway::proxy_opencode_chat),
        )
        .route(
            "/opencode/chat/completions",
            post(crate::gateway::proxy_opencode_chat),
        )
        .route(
            "/openclaw/v1/chat/completions",
            post(crate::gateway::proxy_openclaw_chat),
        )
        .route(
            "/openclaw/chat/completions",
            post(crate::gateway::proxy_openclaw_chat),
        )
        // Grok Build 入口(responses 协议)/ WorkBuddy 入口(chat 完整 URL 直指)
        .route(
            "/grokbuild/responses",
            post(crate::gateway::proxy_grokbuild_responses),
        )
        .route(
            "/grokbuild/v1/responses",
            post(crate::gateway::proxy_grokbuild_responses),
        )
        .route(
            "/workbuddy/v1/chat/completions",
            post(crate::gateway::proxy_workbuddy_chat),
        )
        .route(
            "/workbuddy/chat/completions",
            post(crate::gateway::proxy_workbuddy_chat),
        )
        // Claude Desktop 入口(阶段 D;3p gateway baseUrl=网关+/claude-desktop,app 追加 /v1/messages)
        .route(
            "/claude-desktop/v1/messages",
            post(crate::gateway::proxy_claude_desktop_messages),
        )
        .route(
            "/claude-desktop/messages",
            post(crate::gateway::proxy_claude_desktop_messages),
        )
        // Gemini 入口(多平台阶段 C):段内冒号无特殊含义,`gemini-2.5-flash:generateContent` 整段捕获
        .route(
            "/v1beta/models/:model_action",
            post(crate::gateway::proxy_gemini),
        )
        // --- Health & session ---
        .route("/api/health", get(handle_health))
        .route("/api/session", get(handle_session))
        .route("/api/open-url", post(handle_open_url))
        // --- Auth ---
        .route("/api/auth/captcha", get(handle_auth_captcha))
        .route("/api/auth/login", post(handle_auth_login))
        .route("/api/auth/logout", post(handle_auth_logout))
        .route("/api/auth/remembered", get(handle_auth_remembered))
        .route("/api/auth/remember", post(handle_auth_remember))
        .route("/api/auth/forget", post(handle_auth_forget))
        .route("/api/key-groups", get(handle_key_groups))
        .route("/api/auth/api-keys", get(handle_auth_api_keys))
        .route("/api/auth/me", get(handle_auth_me))
        // --- Providers（04 契约）---
        .route(
            "/api/providers",
            get(handle_providers_list).post(handle_providers_create),
        )
        .route("/api/providers/active", get(handle_providers_active))
        .route("/api/providers/reorder", put(handle_providers_reorder))
        .route("/api/providers/activate", post(handle_providers_activate))
        .route(
            "/api/providers/activate-official",
            post(handle_providers_activate_official),
        )
        .route(
            "/api/providers/preview-config",
            post(handle_providers_preview),
        )
        .route(
            "/api/providers/fetch-models",
            post(handle_providers_fetch_models),
        )
        .route(
            "/api/providers/fetch-balance",
            post(handle_providers_fetch_balance),
        )
        .route("/api/providers/diagnose", post(handle_providers_diagnose))
        .route(
            "/api/providers/:id",
            put(handle_providers_update).delete(handle_providers_delete),
        )
        // --- P0 配置档案（只保存路由元数据，不保存凭据）---
        .route(
            "/api/profiles",
            get(handle_profiles_list).post(handle_profiles_create),
        )
        .route("/api/profiles/preview", post(handle_profiles_preview))
        .route("/api/profiles/apply", post(handle_profiles_apply))
        .route(
            "/api/profiles/:id",
            put(handle_profiles_update).delete(handle_profiles_delete),
        )
        // --- Codex 启动器（M7，直连版）---
        .route("/api/launcher/preflight", post(handle_launcher_preflight))
        .route("/api/launcher/start", post(handle_launcher_start))
        .route("/api/launcher/stop", post(handle_launcher_stop))
        .route("/api/launcher/status", get(handle_launcher_status))
        // --- 桌面版托管开关(阶段 1,任务书 §1.1)---
        .route("/api/desktop/state", get(handle_desktop_state))
        .route("/api/desktop/host", post(handle_desktop_host))
        .route("/api/desktop/unhost", post(handle_desktop_unhost))
        .route(
            "/api/desktop/recovery/preview",
            get(handle_desktop_recovery_preview_get).post(handle_desktop_recovery_preview),
        )
        .route(
            "/api/desktop/recovery/apply",
            post(handle_desktop_recovery_apply),
        )
        .route(
            "/api/desktop/login/status",
            get(handle_desktop_login_status),
        )
        .route("/api/desktop/login/start", post(handle_desktop_login_start))
        // --- Claude Code 配置托管与裸 CLI 启动 ---
        .route(
            "/api/desktop/claude-start",
            post(handle_desktop_claude_start),
        )
        // --- Claude Code 配置写入后的 macOS 一键启动 ---
        .route(
            "/api/desktop/claude-launch",
            post(handle_desktop_claude_launch),
        )
        .route(
            "/api/desktop/claude-state",
            get(handle_desktop_claude_state),
        )
        .route("/api/desktop/claude-host", post(handle_desktop_claude_host))
        .route(
            "/api/desktop/claude-unhost",
            post(handle_desktop_claude_unhost),
        )
        // --- 多平台 agent 注册表 + 泛化路由(方案 §2.1,A 阶段;具名路由保留为别名,B 阶段新平台挂 :agent 段)---
        .route("/api/desktop/agents", get(handle_desktop_agents))
        .route("/api/desktop/:agent/state", get(handle_agent_state))
        .route("/api/desktop/:agent/host", post(handle_agent_host))
        .route("/api/desktop/:agent/unhost", post(handle_agent_unhost))
        .route("/api/desktop/:agent/start", post(handle_agent_start))
        // --- 生态管理(开发组·生态中心 A 段):MCP 服务器列表/操作 + 预设市场;
        // 支持表独立于托管注册表(cursor 无托管世界也可管理生态),故不走 reject_agent ---
        .route(
            "/api/desktop/:agent/eco",
            get(handle_agent_eco).post(handle_agent_eco_op),
        )
        .route("/api/desktop/eco-presets", get(handle_eco_presets))
        // --- Backups & history ---
        .route("/api/backups", get(handle_backups))
        .route("/api/history/inspect", get(handle_history))
        // --- 开机自启(竞品吸收 1.1-3):launchd plist 写/删;读=文件存在 ---
        .route("/api/version", get(handle_version))
        .route("/api/check-update", get(handle_check_update))
        .route("/api/update/install", post(handle_update_install))
        .route("/api/update/status", get(handle_update_status))
        .route(
            "/api/autostart",
            get(handle_autostart).post(handle_autostart_set),
        )
        .route("/api/sessions", get(handle_sessions_list))
        .route("/api/sessions/inspect", get(handle_sessions_inspect))
        .route(
            "/api/sessions/repair/preview",
            post(handle_sessions_repair_preview),
        )
        .route("/api/sessions/repair", post(handle_sessions_repair))
        .route("/api/sessions/jobs/:id", get(handle_sessions_job))
        .route(
            "/api/sessions/jobs/:id/cancel",
            post(handle_sessions_job_cancel),
        )
        .route(
            "/api/sessions/jobs/:id/resume",
            post(handle_sessions_job_resume),
        )
        .route(
            "/api/sessions/delete-preview",
            post(handle_sessions_delete_preview),
        )
        .route("/api/sessions/delete", post(handle_sessions_delete))
        .route(
            "/api/sessions/delete/undo",
            post(handle_sessions_delete_undo),
        )
        .route(
            "/api/sessions/restart-codex",
            post(handle_sessions_restart_codex),
        )
        .route("/api/sessions/:id/resume", post(handle_sessions_resume))
        .route(
            "/api/sessions/settings",
            get(handle_sessions_settings).post(handle_sessions_settings_set),
        )
        // --- Claude 会话历史(R2:只读,~/.claude/projects 的 jsonl;handler 直取 HOME,无 AppState)---
        .route(
            "/api/claude/sessions",
            get(crate::claude_sessions::handle_list),
        )
        // --- 加速线路(阶段 4,任务书 §五)---
        .route("/api/accel/state", get(handle_accel_state))
        .route(
            "/api/settings/official-proxy",
            get(handle_official_proxy).put(handle_official_proxy_save),
        )
        .route("/api/accel/mode", post(handle_accel_mode))
        .route("/api/accel/custom-node", post(handle_accel_custom_node))
        .route("/api/accel/test-node", post(handle_accel_test_node))
        // --- 每账号节点凭证(星图 任务 B:usage 刷新)---
        .route("/api/accel/refresh-cred", post(handle_accel_refresh_cred))
        // --- 能力注册表（保留插件和内置工具消费）---
        .route("/api/fusion-registry", get(handle_fusion_registry))
        .route("/api/config/snapshot", post(handle_config_snapshot))
        .route("/api/config/restore", post(handle_config_restore))
        // --- 媒体服务(超融合 A 线二期 B 段·开发组·媒体服务):原件暂存+URL 回传;
        // /media/* 不走统一信封(直接出字节),/api/media* 走 ok_env/err_env ---
        .route("/media/:file", get(crate::media::handle_serve))
        .route(
            "/api/media",
            get(crate::media::handle_list)
                .post(crate::media::handle_upload)
                // data_b64 比原始媒体膨胀约 4/3；媒体模块自身仍按 MIME 限制解码后大小。
                .layer(DefaultBodyLimit::max(140 * 1024 * 1024)),
        )
        .route("/api/media/:id", delete(crate::media::handle_delete))
        // 用量仪表盘(竞品吸收项):按 provider 聚合 P50/P90/请求量/成功率
        .route("/api/usage-stats", get(handle_usage_stats))
        .route("/api/usage/summary", get(handle_usage_summary))
        .route("/api/usage/history", get(handle_usage_history))
        .route("/api/usage/models", get(handle_usage_models))
        .route(
            "/api/usage/models-history",
            get(handle_usage_models_history),
        )
        .route("/api/usage/refresh", post(handle_usage_refresh))
        .route(
            "/api/settings/usage-overlay",
            get(handle_usage_overlay_get).put(handle_usage_overlay_put),
        )
        .route(
            "/api/settings/usage-overlay/action",
            post(handle_usage_overlay_action),
        )
        // http 型插件(超融合二期):登记/启停/删除/invoke 代理
        .merge(crate::plugins::routes())
        .with_state(state);
    // 网关鉴权:生产构建启用(防 DNS rebinding/跨源网页调本地 API);
    // 测试构建豁免,鉴权行为由运行时冒烟验证(业务单测不携带 token)
    #[cfg(not(test))]
    let router = router.layer(middleware::from_fn_with_state(
        init_gateway_token(),
        gateway_auth,
    ));
    router
}

// --- Helpers ---

pub(crate) fn ok_json(data: Value) -> Response {
    (StatusCode::OK, Json(data)).into_response()
}

pub(crate) fn err_json(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

// 统一响应信封(04 §0):{ok:true,data} / {ok:false,error:{code,message,fields?}}
pub(crate) fn ok_env(data: Value) -> Response {
    (StatusCode::OK, Json(json!({ "ok": true, "data": data }))).into_response()
}

pub(crate) fn err_env(
    status: StatusCode,
    code: &str,
    message: &str,
    fields: Option<Vec<String>>,
) -> Response {
    let mut error = json!({ "code": code, "message": message });
    if let Some(f) = fields {
        error["fields"] = json!(f);
    }
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

pub(crate) fn val_errs_env(errs: &[crate::providers::ValidationError]) -> Response {
    let fields: Vec<String> = errs.iter().map(|e| e.field.clone()).collect();
    err_env(
        StatusCode::UNPROCESSABLE_ENTITY,
        "E_VALIDATION",
        &crate::providers::format_errors(errs),
        Some(fields),
    )
}

fn index_html_with_token() -> Option<Body> {
    let file = FrontendAsset::get("index.html")?;
    let token = init_gateway_token();
    let html = String::from_utf8_lossy(file.data.as_ref());
    let html = html.replacen("<html", &format!("<html data-twoxapi-token=\"{token}\""), 1);
    Some(Body::from(html))
}

async fn serve_index() -> Response {
    match index_html_with_token() {
        Some(body) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_SECURITY_POLICY, CSP)
            .body(body)
            .unwrap(),
        None => (StatusCode::NOT_FOUND, "index.html not found").into_response(),
    }
}

async fn serve_static(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return err_json(StatusCode::NOT_FOUND, "route not found");
    }
    let file = if path.is_empty() {
        FrontendAsset::get("index.html")
    } else {
        FrontendAsset::get(path)
    };
    match file {
        Some(f) => {
            let mime = mime_from_path(path);
            Response::builder()
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, "no-store")
                .header(header::CONTENT_SECURITY_POLICY, CSP)
                .body(Body::from(f.data.into_owned()))
                .unwrap()
        }
        None => match index_html_with_token() {
            Some(body) => Response::builder()
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CONTENT_SECURITY_POLICY, CSP)
                .body(body)
                .unwrap(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

fn mime_from_path(path: &str) -> &'static str {
    if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

// ── 加速线路(阶段 4,任务书 §五)─────────────────────────

fn accel_settings_path(codex_home: &std::path::Path) -> std::path::PathBuf {
    codex_home.join("2xapi-settings.json")
}

/// 读 `{codex_home}/2xapi-settings.json` 的 `accel` 段;缺失/非法 → 默认(off)。
/// 复用 sessions 读写该文件的模式(autoRepairBeforeHost 同文件,互不覆盖)。
pub fn load_accel_cfg(codex_home: &std::path::Path) -> AccelCfg {
    let raw = std::fs::read_to_string(accel_settings_path(codex_home)).unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v.get("accel")
        .and_then(|a| serde_json::from_value(a.clone()).ok())
        .unwrap_or_default()
}

/// 写 `accel` 段(保留文件其余段);失败时返回错误,避免接口误报成功。
pub fn save_accel_cfg(codex_home: &std::path::Path, cfg: &AccelCfg) -> Result<(), String> {
    let _save_guard = crate::usage_overlay::settings_write_lock()?;
    std::fs::create_dir_all(codex_home).map_err(|e| format!("创建配置目录失败: {e}"))?;
    let path = accel_settings_path(codex_home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("读取配置失败: {e}")),
    };
    let mut value: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&raw).map_err(|e| format!("配置文件格式错误: {e}"))?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| "配置文件根节点须为对象".to_string())?;
    object.insert(
        "accel".into(),
        serde_json::to_value(cfg).map_err(|e| format!("序列化加速配置失败: {e}"))?,
    );
    let encoded =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化配置失败: {e}"))?;
    // 原子写(与 usage_overlay 一致):临时文件 + 0600 + rename,崩溃不截断整个设置文件
    // 原子写(与 usage_overlay 一致):临时文件 + 0600 + rename,崩溃不截断整个设置文件。
    // 临时名带进程内计数器,避免并行调用(含并发测试)共用同一 tmp 路径互相覆盖。
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, encoded).map_err(|e| format!("写临时配置失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置临时配置权限失败: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败: {e}"))
}

/// 读「官方通道代理」（official-passthrough-gateway）：`official.proxyUrl` 段，空 = 直连。
pub fn load_official_proxy(codex_home: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(accel_settings_path(codex_home)).unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v.get("official")
        .and_then(|o| o.get("proxyUrl"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 校验并保存「官方通道代理」；仅接受 http(s)/socks5(h)://，空串清空。
pub fn save_official_proxy(codex_home: &std::path::Path, url: &str) -> Result<(), String> {
    let url = url.trim();
    let ok = url.is_empty()
        || ["http://", "https://", "socks5://", "socks5h://"]
            .iter()
            .any(|p| url.starts_with(p));
    if !ok {
        return Err("官方通道代理须为 http(s):// 或 socks5(h):// 地址，或留空".into());
    }
    let _save_guard = crate::usage_overlay::settings_write_lock()?;
    std::fs::create_dir_all(codex_home).map_err(|e| format!("创建配置目录失败: {e}"))?;
    let path = accel_settings_path(codex_home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("读取配置失败: {e}")),
    };
    let mut value: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&raw).map_err(|e| format!("配置文件格式错误: {e}"))?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| "配置文件根节点须为对象".to_string())?;
    let mut official = object
        .get("official")
        .and_then(|o| o.as_object().cloned())
        .unwrap_or_default();
    official.insert("proxyUrl".into(), json!(url));
    object.insert("official".into(), Value::Object(official));
    let encoded =
        serde_json::to_string_pretty(&value).map_err(|e| format!("序列化配置失败: {e}"))?;
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, encoded).map_err(|e| format!("写临时配置失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置临时配置权限失败: {e}"))?;
    }
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换配置失败: {e}"))
}

// ── 每账号节点凭证(星图 任务 B:usage 块 + refresh-cred)────────

/// 节点签发服务地址:生产恒为 DEFAULT_ISSUE_BASE;测试经 set_issue_base_for_tests
/// 整体替换为本地 mock(全局 + 串行,防并行用例互串)。gateway.rs 的凭证确保段共用。
static ISSUE_BASE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

pub fn issue_base() -> String {
    ISSUE_BASE
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| crate::nodecreds::DEFAULT_ISSUE_BASE.to_string())
}

/// 测试辅助:替换签发地址;返回的 guard 持有期间生效,drop 自动还原;
/// guard 同时持全局锁串行化所有依赖 override 的用例(并行测试安全)。
#[cfg(test)]
pub fn set_issue_base_for_tests(base: &str) -> IssueBaseGuard {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = SERIAL.lock().unwrap();
    *ISSUE_BASE.write().unwrap() = Some(base.to_string());
    IssueBaseGuard { _serial: g }
}

/// set_issue_base_for_tests 的 guard(drop 时还原 override 并释放串行锁)。仅测试用。
/// 字段仅在存活期持有锁(不读),下划线前缀豁免 dead_code。
#[cfg(test)]
pub struct IssueBaseGuard {
    _serial: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for IssueBaseGuard {
    fn drop(&mut self) {
        *ISSUE_BASE.write().unwrap() = None;
    }
}

// ── 测试公共件:mock 节点签发服务(星图 任务 B;server/gateway 测试共用)──

/// 可控 mock 签发服务:任意请求固定回 status_line + JSON body。
/// 与 nodecreds.rs 的样板同构(该模块只读,不得复用其私有测试件)。
#[cfg(test)]
pub async fn spawn_issue_mock(status_line: &'static str, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await; // 消费请求(不求完整)
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

/// 必拒连地址(discard 端口):签发 Unreachable 分支用,无端口竞争。
#[cfg(test)]
pub const DEAD_ISSUE_BASE: &str = "http://127.0.0.1:9";

#[cfg(test)]
pub fn test_node_cred(user: &str, pass: &str) -> crate::nodecreds::NodeCred {
    crate::nodecreds::NodeCred {
        user: user.into(),
        pass: pass.into(),
        quota_total_bytes: 10_737_418_240,
        quota_used_bytes: 1_073_741_824,
        proxy_endpoint: crate::nodecreds::DEFAULT_PROXY_ENDPOINT.into(),
        issued_at: chrono::Utc::now().timestamp(),
        degraded_to_direct: false,
    }
}

pub(crate) fn claude_home(codex_home: &std::path::Path) -> &std::path::Path {
    codex_home.parent().unwrap_or(codex_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn path_needs_auth_covers_all_proxy_and_api_routes() {
        // 代理前缀与具名代理路径必须全部要求鉴权(防漏配)
        for path in [
            "/api/providers",
            "/api/desktop/host",
            "/api/plugins/abc/invoke",
            "/v1/responses",
            "/v1/chat/completions",
            "/v1beta/models/generateContent",
            "/anthropic/v1/messages",
            "/hermes/chat/completions",
            "/cursor/v1/chat/completions",
            "/opencode/chat/completions",
            "/openclaw/chat/completions",
            "/grokbuild/chat/completions",
            "/grok/v1/chat/completions",
            "/workbuddy/chat/completions",
            "/claude-desktop/v1/messages",
            "/responses",
            "/chat/completions",
            "/images/generations",
            "/models",
        ] {
            assert!(path_needs_auth(path), "{path} 应要求鉴权");
        }
        // 静态页面与健康检查不要求鉴权
        for path in [
            "/",
            "/app.js",
            "/styles.css",
            "/api/bootstrap",
            "/health",
            "/media/x.png",
        ] {
            assert!(!path_needs_auth(path), "{path} 不应要求鉴权");
        }
    }

    fn dummy_state() -> AppState {
        AppState {
            config_path: PathBuf::from("/tmp/2xapi-m0-cfg.toml"),
            backup_dir: PathBuf::from("/tmp/2xapi-m0-bk"),
            providers_path: PathBuf::from("/tmp/2xapi-m0-providers.json"),
            codex_home: PathBuf::from("/tmp/2xapi-m0-codex-home"),
            wb_home: PathBuf::from("/tmp/2xapi-m0-wb-home"),
            hermes_home: PathBuf::from("/tmp/2xapi-m0-hermes-home"),
            gem_home: PathBuf::from("/tmp/2xapi-m0-gem-home"),
            grok_home: PathBuf::from("/tmp/2xapi-m0-grok-home"),
            oc_home: PathBuf::from("/tmp/2xapi-m0-oc-home"),
            oclaw_home: PathBuf::from("/tmp/2xapi-m0-oclaw-home"),
            cd_home: PathBuf::from("/tmp/2xapi-m0-cd-home"),
            cursor_home: PathBuf::from("/tmp/2xapi-m0-cursor-home"),
            launcher: Default::default(),
            health: std::sync::Arc::new(crate::acclines::HealthState::new(vec![])),
            accel: std::sync::Arc::new(std::sync::Mutex::new(AccelCfg::default())),
            nodecreds: std::sync::Arc::new(
                std::sync::RwLock::new(crate::nodecreds::Store::empty()),
            ),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// 媒体服务路由 e2e(超融合 A 线二期 B 段):上传→GET 字节一致+Content-Type→
    /// ext 错位 404(不暴露)→伪装 mime 415→列表→删除→GET 404。tempdir 隔离,零真实配置。
    /// 删除守卫:供应商正被 Codex 托管(desktop hosting)→ 400 人话拒删;未托管 → 正常删除。
    #[tokio::test]
    async fn providers_delete_blocked_while_codex_hosted() {
        let dir =
            std::env::temp_dir().join(format!("2xapi-delguard-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = dummy_state();
        st.config_path = dir.join("config.toml");
        st.providers_path = dir.join("providers.json");
        // 托管态:custom 段指向网关(与 desktop host gateway 同形态)+ active=p1
        std::fs::write(
            &st.config_path,
            "model_provider = \"2xapi_gateway\"\n[model_providers.2xapi_gateway]\nbase_url = \"http://127.0.0.1:8787\"\n",
        )
        .unwrap();
        std::fs::write(
            &st.providers_path,
            r#"{"schema_version":3,"active_provider_id":"p1","providers":[{"id":"p1","name":"t","agent":"codex","base_url":"https://up.example.com","api_key":"sk-1","model":"m1"}]}"#,
        )
        .unwrap();
        let app = build_router(st.clone());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/providers/p1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "托管中应拒删");
        assert!(
            String::from_utf8_lossy(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap()
            )
            .contains("托管中的供应商不能删除"),
            "拒删应带人话"
        );
        // 未托管(custom 段移除)→ 删除成功
        std::fs::write(&st.config_path, "").unwrap();
        let app2 = build_router(st.clone());
        let resp2 = app2
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/providers/p1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK, "未托管应可删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 媒体服务路由 e2e(超融合 A 线二期 B 段):上传→GET 字节一致+Content-Type→
    /// ext 错位 404(不暴露)→伪装 mime 415→列表→删除→GET 404。tempdir 隔离,零真实配置。
    #[tokio::test]
    async fn media_routes_roundtrip() {
        let mut st = dummy_state();
        let dir =
            std::env::temp_dir().join(format!("2xapi-media-e2e-{}", uuid::Uuid::new_v4().simple()));
        st.codex_home = dir.clone();
        let app = build_router(st);

        // 1x1 红点 PNG(与 media.rs 单测同资产)
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let body = format!(r#"{{"mime":"image/png","data_b64":"{png_b64}","origin":"e2e"}}"#);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/media")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["ok"], true);
        let url = v["data"]["url"].as_str().unwrap().to_string();
        let id = v["data"]["id"].as_str().unwrap().to_string();
        assert!(
            url.starts_with("/media/") && url.ends_with(".png"),
            "URL 形态: {url}"
        );

        // GET 原件:200+Content-Type+PNG 魔数字节一致
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("content-type").unwrap(), "image/png");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..4], &[0x89, b'P', b'N', b'G']);

        // ext 错位(.jpg)→404;列表可见;伪装 mime(image/jpeg 实为 png)→415
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/media/{id}.jpg"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/media")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["data"]["items"].as_array().unwrap().len(), 1);
        let bad = format!(r#"{{"mime":"image/jpeg","data_b64":"{png_b64}"}}"#);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/media")
                    .header("content-type", "application/json")
                    .body(Body::from(bad))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 415);

        // 删除→GET 404
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/media/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 阶段:GET /api/desktop/agents 返回 8 平台注册表(导航数据源,D3「一次全亮」)
    #[tokio::test]
    async fn desktop_agents_returns_registry() {
        let app = build_router(dummy_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let arr = v["data"]["agents"].as_array().unwrap();
        assert_eq!(arr.len(), 10);
        assert_eq!(arr[0]["id"], "codex");
        assert_eq!(arr[0]["available"], Value::Bool(true));
        assert_eq!(arr[1]["id"], "claude");
        assert_eq!(arr[1]["available"], Value::Bool(true));
        assert!(arr
            .iter()
            .any(|m| m["id"] == "workbuddy" && m["available"] == Value::Bool(true)));
        assert!(arr
            .iter()
            .any(|m| m["id"] == "cursor" && m["available"] == Value::Bool(true)));
        assert!(!arr.iter().any(|m| m["id"] == "pi"), "pi 已裁撤不得出现");
    }

    /// workbuddy 泛化路由 e2e:host 写入双载体 → state 报 hosted → unhost 还原(隔离 wb_home)
    #[tokio::test]
    async fn workbuddy_routes_e2e() {
        let (state, root) = unique_state("wb-e2e");
        let app = build_router(state);
        std::fs::write(
            root.join("providers.json"),
            serde_json::json!({"providers": [{"id": "wbp", "name": "测站", "agent": "workbuddy",
                "base_url": "https://w.example/v1", "api_key": "sk-w", "model": "m1"}]})
            .to_string(),
        )
        .unwrap();

        // host
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/workbuddy/host")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"providerId":"wbp","way":"gateway"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["hosted"], Value::Bool(true));
        let cli: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".codebuddy/models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cli["models"][0]["vendor"], "2xapi-gateway");
        assert!(
            root.join(".workbuddy/models.json").exists(),
            "桌面版载体同步写入"
        );

        // state
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/workbuddy/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["hosted"], Value::Bool(true));

        // start(已托管 → 命令返回)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/workbuddy/start")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"providerId":"wbp","way":"gateway"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // unhost
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/workbuddy/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cli: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".codebuddy/models.json")).unwrap(),
        )
        .unwrap();
        assert!(
            cli["models"].as_array().unwrap().is_empty(),
            "unhost 后条目移除"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// hermes 泛化路由 e2e:host 写 YAML 条目+指针 → state 报托管 → unhost 还原(隔离 hermes_home)
    #[tokio::test]
    async fn hermes_routes_e2e() {
        let (state, root) = unique_state("hermes-e2e");
        let app = build_router(state);
        let hermes_dir = root.join("hermes");
        std::fs::create_dir_all(&hermes_dir).unwrap();
        let original_yaml = "model:\n  provider: openai-api\n  default: gpt-5.5\nagent:\n  reasoning_effort: max\n_config_version: 33\nmcp_servers: {}\n";
        std::fs::write(hermes_dir.join("config.yaml"), original_yaml).unwrap();
        std::fs::write(
            root.join("providers.json"),
            serde_json::json!({"providers": [{"id": "hp", "name": "测站", "agent": "hermes",
                "base_url": "https://2xa.example/v1", "api_key": "sk-h", "model": "glm-5"}]})
            .to_string(),
        )
        .unwrap();

        // host(gateway)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/hermes/host")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"providerId":"hp","way":"gateway"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["hosted"], Value::Bool(true));
        assert_eq!(
            v["data"]["pointerSwitched"],
            Value::Bool(true),
            "官方指针应切换"
        );
        let yaml = std::fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
        assert!(yaml.contains("2xapi-gateway"));
        assert!(yaml.contains("http://127.0.0.1:8787/hermes"));
        assert!(!yaml.contains("sk-h"), "真 Key 不得落盘");
        assert!(yaml.contains("_config_version: 33"), "用户字段保留");

        // state
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/hermes/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["hosting"]["way"].as_str(), Some("gateway"));

        // way=direct 拒绝(叠加平台仅 gateway)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/hermes/host")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"providerId":"hp","way":"direct"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // 网关专属入口存在(405 证路由注册;POST 语义在 gateway 测试覆盖)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/hermes/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        // unhost → 指针回官方、条目移除
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/hermes/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let yaml = std::fs::read_to_string(hermes_dir.join("config.yaml")).unwrap();
        assert!(!yaml.contains("2xapi-gateway"));
        assert!(yaml.contains("provider: openai-api"), "指针恢复官方默认");
        assert!(yaml.contains("_config_version: 33"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A 阶段:泛化路由 /api/desktop/codex/state 与旧具名路由响应完全一致(别名等价)
    #[tokio::test]
    async fn agent_state_alias_equals_legacy() {
        let app = build_router(dummy_state());
        let legacy = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let generic = app
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/codex/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::OK);
        assert_eq!(generic.status(), StatusCode::OK);
        let lb = axum::body::to_bytes(legacy.into_body(), usize::MAX)
            .await
            .unwrap();
        let gb = axum::body::to_bytes(generic.into_body(), usize::MAX)
            .await
            .unwrap();
        let lv: Value = serde_json::from_slice(&lb).unwrap();
        let gv: Value = serde_json::from_slice(&gb).unwrap();
        assert_eq!(lv, gv, "泛化路由与旧路由响应必须一致");
    }

    /// 泛化路由拒绝规则：未知平台 404；Claude host 已实现，缺少供应商时返回业务错误。
    #[tokio::test]
    async fn agent_routes_reject_rules() {
        let app = build_router(dummy_state());
        let unknown = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/vscode/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        // 501 断言已移除:九平台 available 满编后 E_AGENT_NOT_IMPLEMENTED 无触发者
        //(reject_agent 保留该分支供未来新平台灰度期使用;未知平台 404 断言仍在上方)

        let nohost = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude/host")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"providerId":"x","way":"gateway"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(nohost.status(), StatusCode::BAD_REQUEST);
        let nb = axum::body::to_bytes(nohost.into_body(), usize::MAX)
            .await
            .unwrap();
        let nv: Value = serde_json::from_slice(&nb).unwrap();
        assert_eq!(nv["error"], "E_NO_PROVIDER");
    }

    /// M0 DoD③ 证据：GET /health 返回 200 + {status:"ok", active_provider_id:null, access_mode:null}
    #[tokio::test]
    async fn gateway_health_returns_ok_with_null_active() {
        let app = build_router(dummy_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["active_provider_id"], Value::Null);
        assert_eq!(v["access_mode"], Value::Null);
    }

    /// M0 DoD③ 实端口证据：真实绑定 127.0.0.1:8787 + 真实 HTTP GET（headless，无需启动 GUI）。
    /// 若 8787 已被占用（app 正在跑），跳过而不误报失败。
    #[tokio::test]
    async fn gateway_health_served_on_real_port_8787() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:8787").await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[skip] 127.0.0.1:8787 已被占用({e})，假设 app 在跑");
                return;
            }
        };
        let app = build_router(dummy_state());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let resp = reqwest::get("http://127.0.0.1:8787/health").await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["active_provider_id"], Value::Null);
        assert_eq!(v["access_mode"], Value::Null);
    }

    // ── M4 路由（04 契约：统一信封 + 错误码）──

    fn unique_state(label: &str) -> (AppState, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("2xapi-m4-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("backups")).unwrap();
        let state = AppState {
            config_path: root.join("config.toml"),
            backup_dir: root.join("backups"),
            providers_path: root.join("providers.json"),
            codex_home: root.join("codex"),
            wb_home: root.clone(),
            hermes_home: root.join("hermes"),
            gem_home: root.clone(),
            grok_home: root.join("grok"),
            oc_home: root.join("ochome"),
            oclaw_home: root.join("oclaw"),
            cd_home: root.join("cdsupport"),
            cursor_home: root.join("cursorhome"),
            launcher: Default::default(),
            health: std::sync::Arc::new(crate::acclines::HealthState::new(vec![])),
            accel: std::sync::Arc::new(std::sync::Mutex::new(AccelCfg::default())),
            nodecreds: std::sync::Arc::new(
                std::sync::RwLock::new(crate::nodecreds::Store::empty()),
            ),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, root)
    }

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn sessions_list_requires_and_accepts_inspect_snapshot() {
        let app = build_router(dummy_state());
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions?page=1&size=50")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::CONFLICT);
        let missing_body = body_json(missing).await;
        assert_eq!(missing_body["error"]["code"], "E_SESSION_SNAPSHOT");

        let inspected = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/sessions/inspect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inspected.status(), StatusCode::OK);
        let inspected_body = body_json(inspected).await;
        let snapshot_id = inspected_body["data"]["snapshotId"].as_str().unwrap();
        let listed = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions?page=1&size=50&snapshotId={snapshot_id}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(body_json(listed).await["ok"], true);
    }

    #[tokio::test]
    async fn version_route_uses_unified_envelope_and_cargo_version() {
        let response = build_router(dummy_state())
            .oneshot(
                Request::builder()
                    .uri("/api/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = body_json(response).await;
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            value.get("version").is_none(),
            "版本必须位于统一 data 信封内"
        );
    }

    #[tokio::test]
    async fn update_status_route_defaults_to_idle() {
        let response = build_router(dummy_state())
            .oneshot(
                Request::builder()
                    .uri("/api/update/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value = body_json(response).await;
        assert_eq!(value["data"]["state"], "idle");
        assert_eq!(value["data"]["downloaded"], 0);
    }

    /// 生态管理 e2e(开发组·生态中心 A 段):codex TOML 段级 + cursor JSON 全链;
    /// 手动条目 409 拒写、其他段零触碰、预设市场、未知平台 404。
    #[tokio::test]
    async fn eco_routes_e2e() {
        let (state, root) = unique_state("eco-e2e");
        let app = build_router(state.clone());
        // 预置带手动 MCP 条目与其他段的真实形状 config.toml
        std::fs::write(
            root.join("config.toml"),
            "model = \"gpt-5\"\n[mcp_servers.computer-use]\ncommand = \"node\"\nargs = [\"cu.js\"]\n[custom]\nbase_url = \"http://x\"\n",
        )
        .unwrap();

        // GET 列表:手动条目识别
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/codex/eco")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let servers = v["data"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["id"], "computer-use");
        assert_eq!(servers[0]["source"], "manual");

        // install 预设 → 写入 [mcp_servers.fetch],其他段保留
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"install","presetId":"fetch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "install 应成功");
        let toml = std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        assert_eq!(
            toml["mcp_servers"]["fetch"]["command"].as_str(),
            Some("uvx")
        );
        assert_eq!(
            toml["mcp_servers"]["computer-use"]["command"].as_str(),
            Some("node"),
            "已有条目零触碰"
        );
        assert_eq!(
            toml["custom"]["base_url"].as_str(),
            Some("http://x"),
            "其他段零触碰"
        );

        // 手动条目拒写
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"op":"install","name":"computer-use","spec":{"command":"x"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // disable(Codex 原生):条目保留+enabled=false;enable:恢复;uninstall:移除
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"disable","name":"fetch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let toml = std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        assert_eq!(
            toml["mcp_servers"]["fetch"]["enabled"].as_bool(),
            Some(false),
            "原生 enabled=false,条目保留"
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"enable","name":"fetch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let toml = std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        assert_eq!(
            toml["mcp_servers"]["fetch"]["enabled"].as_bool(),
            Some(true),
            "enable 恢复"
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"uninstall","name":"fetch"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let toml = std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        assert!(
            toml["mcp_servers"].get("fetch").is_none(),
            "uninstall 后条目应移除"
        );
        let reg = std::fs::read_to_string(root.join("codex").join("eco-managed.json"))
            .unwrap_or_default();
        assert!(!reg.contains("fetch"), "uninstall 后登记表应清除该条");

        // 备份链:eco-apply 备份存在
        let backups: Vec<_> = std::fs::read_dir(root.join("backups"))
            .unwrap()
            .flatten()
            .collect();
        assert!(!backups.is_empty(), "应有备份快照");

        // cursor:文件不存在 → 空列表;install 建文件
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/cursor/eco")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["servers"].as_array().unwrap().len(), 0);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/cursor/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"install","presetId":"playwright"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mcp = std::fs::read_to_string(root.join("cursorhome").join(".cursor").join("mcp.json"))
            .unwrap();
        let doc: Value = serde_json::from_str(&mcp).unwrap();
        assert_eq!(doc["mcpServers"]["playwright"]["command"], "npx");

        // Gemini 属于产品十平台生态集合;非法 op 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/gemini/eco")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let gemini = body_json(resp).await;
        assert!(gemini["data"]["servers"].is_array());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/codex/eco")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"op":"nuke"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // 预设市场
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/eco-presets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["presets"].as_array().unwrap().len(), 8);
        assert_eq!(v["data"]["agents"].as_array().unwrap().len(), 10);
    }

    /// 生态管理 B 段 e2e:五平台 install→形状+零触碰→uninstall;装时填参 400。
    #[tokio::test]
    async fn eco_b_routes_e2e() {
        let (state, root) = unique_state("eco-b");
        let app = build_router(state.clone());
        let post = |agent: String, body: String| {
            let uri = format!("/api/desktop/{agent}/eco");
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        macro_rules! post {
            ($a:expr, $b:expr) => {
                post($a.to_string(), $b.to_string()).await
            };
        }

        // ── claude-desktop:5 条用户 MCP + 其他键(mcpServers 之外零触碰)──
        let cd = root.join("cdsupport").join("Claude");
        std::fs::create_dir_all(&cd).unwrap();
        let cd_cfg = cd.join("claude_desktop_config.json");
        std::fs::write(&cd_cfg, r#"{"globalShortcut":"Ctrl+X","mcpServers":{"zhipu-vision":{"command":"node","args":["z.js"]},"GPT-image":{"command":"g"}},"other":1}"#).unwrap();
        let resp = post!("claude-desktop", r#"{"op":"install","presetId":"fetch"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "cd install");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cd_cfg).unwrap()).unwrap();
        assert_eq!(
            doc["mcpServers"]["zhipu-vision"]["command"], "node",
            "用户 MCP 保留"
        );
        assert_eq!(doc["mcpServers"]["GPT-image"]["command"], "g");
        assert_eq!(doc["mcpServers"]["fetch"]["command"], "uvx", "新条目写入");
        assert_eq!(doc["globalShortcut"], "Ctrl+X", "其他键保留");
        let resp = post!("claude-desktop", r#"{"op":"uninstall","name":"fetch"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cd_cfg).unwrap()).unwrap();
        assert!(doc["mcpServers"].get("fetch").is_none());
        assert_eq!(doc["mcpServers"]["zhipu-vision"]["command"], "node");

        // ── grokbuild:[models]/[cli] 段零触碰 ──
        let grok_cfg = root.join("grok").join("config.toml");
        std::fs::create_dir_all(root.join("grok")).unwrap();
        std::fs::write(
            &grok_cfg,
            "[models]\ndefault = \"x\"\n[cli]\ntui = true\n[mcp_servers.manual]\ncommand = \"manual\"\n",
        )
        .unwrap();
        let resp = post!("grokbuild", r#"{"op":"install","presetId":"memory"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "grok install");
        let t: toml::Value = std::fs::read_to_string(&grok_cfg).unwrap().parse().unwrap();
        assert_eq!(t["models"]["default"].as_str(), Some("x"), "models 段保留");
        assert_eq!(t["mcp_servers"]["memory"]["command"].as_str(), Some("npx"));
        let resp = post!("grokbuild", r#"{"op":"disable","name":"memory"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "grok disable");
        let t: toml::Value = std::fs::read_to_string(&grok_cfg).unwrap().parse().unwrap();
        assert!(
            t["mcp_servers"].get("memory").is_none(),
            "Grok Build 不支持 enabled=false，disable 必须移除条目"
        );
        assert_eq!(
            t["mcp_servers"]["manual"]["command"].as_str(),
            Some("manual"),
            "disable 不得误删手动条目"
        );
        let resp = post!("grokbuild", r#"{"op":"enable","name":"memory"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "grok enable");
        let resp = post!("grokbuild", r#"{"op":"uninstall","name":"memory"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let t: toml::Value = std::fs::read_to_string(&grok_cfg).unwrap().parse().unwrap();
        assert!(t["mcp_servers"].get("memory").is_none());
        assert_eq!(
            t["mcp_servers"]["manual"]["command"].as_str(),
            Some("manual")
        );
        assert_eq!(t["models"]["default"].as_str(), Some("x"));

        // ── opencode:theme 键保留 + local 形状 ──
        let oc_cfg = root.join("ochome").join(".config/opencode/opencode.json");
        std::fs::create_dir_all(oc_cfg.parent().unwrap()).unwrap();
        std::fs::write(&oc_cfg, r#"{ "theme": "dark", "mcp": {} }"#).unwrap();
        let resp = post!("opencode", r#"{"op":"install","presetId":"context7"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "oc install");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&oc_cfg).unwrap()).unwrap();
        assert_eq!(doc["theme"], "dark");
        assert_eq!(doc["mcp"]["context7"]["type"], "local");
        assert_eq!(doc["mcp"]["context7"]["command"][0], "npx");
        let resp = post!("opencode", r#"{"op":"uninstall","name":"context7"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&oc_cfg).unwrap()).unwrap();
        assert!(doc.get("mcp").is_none() || doc["mcp"].as_object().unwrap().is_empty());

        // ── hermes:model/agent 段保留 ──
        let her_cfg = root.join("hermes").join("config.yaml");
        std::fs::create_dir_all(root.join("hermes")).unwrap();
        std::fs::write(&her_cfg, "_config_version: 33\nmodel:\n  provider: openai-api\nmcp_servers: {}\nagent:\n  name: demo\n").unwrap();
        let resp = post!("hermes", r#"{"op":"install","presetId":"fetch"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "hermes install");
        let raw = std::fs::read_to_string(&her_cfg).unwrap();
        assert!(raw.contains("provider: openai-api"), "model 段保留");
        assert!(raw.contains("name: demo"), "agent 段保留");
        assert!(raw.contains("mcp-server-fetch"));
        let resp = post!("hermes", r#"{"op":"uninstall","name":"fetch"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let raw = std::fs::read_to_string(&her_cfg).unwrap();
        assert!(!raw.contains("mcp-server-fetch"));
        assert!(raw.contains("_config_version: 33"));

        // ── gemini:settings.json 无文件 install 创建 ──
        let gemini_cfg = root.join(".gemini").join("settings.json");
        let resp = post!("gemini", r#"{"op":"install","presetId":"playwright"}"#);
        assert_eq!(resp.status(), StatusCode::OK, "gemini install");
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&gemini_cfg).unwrap()).unwrap();
        assert_eq!(doc["mcpServers"]["playwright"]["command"], "npx");
        let resp = post!("gemini", r#"{"op":"uninstall","name":"playwright"}"#);
        assert_eq!(resp.status(), StatusCode::OK);
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&gemini_cfg).unwrap()).unwrap();
        assert!(doc.get("mcpServers").is_none());

        // ── 装时填参:filesystem 缺参 400,带参成功 ──
        let resp = post!("codex", r#"{"op":"install","presetId":"filesystem"}"#);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "缺 DIR 应 400");
        let resp = post!(
            "codex",
            r#"{"op":"install","presetId":"filesystem","params":{"DIR":"/tmp/eco-test"}}"#
        );
        assert_eq!(resp.status(), StatusCode::OK, "带参 install");
        let t: toml::Value = std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            t["mcp_servers"]["filesystem"]["args"][2].as_str(),
            Some("/tmp/eco-test"),
            "$DIR 替换"
        );
    }

    /// C 段 e2e:claude-code/workbuddy MCP 补平台 + codex 原生停用 list 层 + 技能路由。
    #[tokio::test]
    async fn eco_c_routes_e2e() {
        let (state, root) = unique_state("eco-c");
        let app = build_router(state.clone());
        let post = |uri: String, body: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        // claude-code(User scope):projects 等键保留
        let cj = root.join(".claude.json");
        std::fs::write(
            &cj,
            r#"{"numStartups": 42, "mcpServers": {}, "projects": {"a": {"x": 1}}}"#,
        )
        .unwrap();
        let resp = post(
            "/api/desktop/claude/eco".into(),
            r#"{"op":"install","presetId":"fetch"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "claude install");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert_eq!(doc["numStartups"], 42, "其他键保留");
        assert_eq!(doc["mcpServers"]["fetch"]["command"], "uvx");
        let resp = post(
            "/api/desktop/claude/eco".into(),
            r#"{"op":"uninstall","name":"fetch"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cj).unwrap()).unwrap();
        assert!(
            doc.get("mcpServers").is_none() || doc["mcpServers"].as_object().unwrap().is_empty()
        );
        assert_eq!(doc["numStartups"], 42);

        // workbuddy(.mcp.json):connector-proxy 手动条目零触碰
        let wm = root.join(".workbuddy").join(".mcp.json");
        std::fs::create_dir_all(root.join(".workbuddy")).unwrap();
        std::fs::write(&wm, r#"{"mcpServers":{"connector-proxy":{"type":"http","url":"http://127.0.0.1:63685/mcp"}}}"#).unwrap();
        let resp = post(
            "/api/desktop/workbuddy/eco".into(),
            r#"{"op":"install","presetId":"memory"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "workbuddy install");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&wm).unwrap()).unwrap();
        assert_eq!(
            doc["mcpServers"]["connector-proxy"]["url"], "http://127.0.0.1:63685/mcp",
            "手动 http 条目零触碰"
        );
        assert_eq!(doc["mcpServers"]["memory"]["command"], "npx");
        let resp = post(
            "/api/desktop/workbuddy/eco".into(),
            r#"{"op":"install","name":"connector-proxy","spec":{"command":"x"}}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "手动条目拒写");
        let resp = post(
            "/api/desktop/workbuddy/eco".into(),
            r#"{"op":"uninstall","name":"memory"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // codex 原生停用 list 层
        let resp = post(
            "/api/desktop/codex/eco".into(),
            r#"{"op":"install","presetId":"fetch"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = post(
            "/api/desktop/codex/eco".into(),
            r#"{"op":"disable","name":"fetch"}"#.into(),
        )
        .await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let fetch = v["data"]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == "fetch")
            .unwrap();
        assert_eq!(fetch["enabled"], Value::Bool(false), "list 应报停用(原生)");

        // openclaw MCP 补齐(mcp.servers 载体,技能裁撤批的 404 已反转):CLI 同款文档全链
        let oj = root.join("oclaw").join("openclaw.json");
        std::fs::create_dir_all(root.join("oclaw")).unwrap();
        std::fs::write(
            &oj,
            r#"{"commands":{"restart":true},"mcp":{"servers":{"probe-x":{"command":"npx","args":["hello-server"]}}},"meta":{"lastTouchedVersion":"2026.7.1-2"}}"#,
        )
        .unwrap();
        let resp = post(
            "/api/desktop/openclaw/eco".into(),
            r#"{"op":"install","name":"memory","spec":{"command":"npx","args":["-y","server-memory"]}}"#
                .into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "openclaw install");
        let v = body_json(resp).await;
        let servers = v["data"]["servers"].as_array().unwrap();
        let manual = servers.iter().find(|s| s["id"] == "probe-x").unwrap();
        assert_eq!(manual["source"], "manual", "CLI 已有条目标手动");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&oj).unwrap()).unwrap();
        assert_eq!(
            doc["mcp"]["servers"]["memory"]["command"], "npx",
            "嵌套段形状"
        );
        assert_eq!(doc["mcp"]["servers"]["probe-x"]["args"][0], "hello-server");
        assert_eq!(doc["commands"]["restart"], true, "其他顶层键保留");

        // disable → enabled:false 原位落盘(原生停用,与 Codex 同款);条目不移除
        let resp = post(
            "/api/desktop/openclaw/eco".into(),
            r#"{"op":"disable","name":"memory"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let mem = v["data"]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "memory")
            .unwrap();
        assert_eq!(mem["enabled"], Value::Bool(false), "list 应报停用(原生)");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&oj).unwrap()).unwrap();
        assert_eq!(
            doc["mcp"]["servers"]["memory"]["enabled"],
            Value::Bool(false),
            "enabled:false 原位落盘"
        );

        // enable → enabled:true;uninstall → 条目移除,probe-x 与其他键原样
        let resp = post(
            "/api/desktop/openclaw/eco".into(),
            r#"{"op":"enable","name":"memory"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = post(
            "/api/desktop/openclaw/eco".into(),
            r#"{"op":"uninstall","name":"memory"}"#.into(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&oj).unwrap()).unwrap();
        assert!(doc["mcp"]["servers"].get("memory").is_none());
        assert_eq!(doc["mcp"]["servers"]["probe-x"]["args"][0], "hello-server");
        assert_eq!(doc["commands"]["restart"], true);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/hermes/skills")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "skills 路由已裁撤");
    }

    /// grokbuild 泛化路由 e2e:建 agent=grokbuild 供应商 → host 写 ~/.grok TOML → state 托管中 → unhost 还原(隔离 grok_home)
    #[tokio::test]
    async fn grokbuild_routes_e2e() {
        let (state, root) = unique_state("grok-e2e");
        let app = build_router(state.clone());

        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"GrokT","agent":"grokbuild","baseUrl":"https://xai.example.com","apiKey":"sk-grok-test","model":"grok-4"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            created.status(),
            StatusCode::OK,
            "agent=grokbuild 供应商应可建(available=true)"
        );
        let cv = body_json(created).await;
        let pid = cv["data"]["id"].as_str().unwrap().to_string();

        let hosted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/grokbuild/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hosted.status(), StatusCode::OK);
        let hv = body_json(hosted).await;
        assert_eq!(hv["data"]["hosted"], Value::Bool(true));

        let st = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/grokbuild/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sv = body_json(st).await;
        assert!(!sv["data"].is_null(), "host 后 state 应非 null");

        let un = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/grokbuild/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(un.status(), StatusCode::OK);
        let uv = body_json(un).await;
        assert!(
            uv["data"]["restored"].is_boolean(),
            "unhost 应返回 restored 字段:{uv}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// opencode 泛化路由 e2e:建供应商 → host 叠加写 opencode.json(指针缺失才切,D1) → state → unhost 还原(隔离 oc_home)
    #[tokio::test]
    async fn opencode_routes_e2e() {
        let (state, root) = unique_state("oc-e2e");
        let app = build_router(state.clone());
        let created = app.clone().oneshot(
            Request::builder().method("POST").uri("/api/providers")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"OcT","agent":"opencode","baseUrl":"https://oc.example.com","apiKey":"sk-oc-test","model":"gpt-5.6-sol"}"#)).unwrap(),
        ).await.unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let cv = body_json(created).await;
        let pid = cv["data"]["id"].as_str().unwrap().to_string();

        let hosted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/opencode/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hosted.status(), StatusCode::OK);
        let hv = body_json(hosted).await;
        assert_eq!(hv["data"]["hosted"], Value::Bool(true));
        assert_eq!(
            hv["data"]["defaultModelSwitched"],
            Value::Bool(true),
            "指针缺失时应切"
        );

        // 已有第三方指针时再 host:不切指针,suggested=true(D1)
        let raw =
            std::fs::read_to_string(root.join("ochome/.config/opencode/opencode.json")).unwrap();
        let mut j: Value = serde_json::from_str(&raw).unwrap();
        j["model"] = json!("custom/gpt-4o");
        std::fs::write(
            root.join("ochome/.config/opencode/opencode.json"),
            serde_json::to_string(&j).unwrap(),
        )
        .unwrap();
        let rehost = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/opencode/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let rv = body_json(rehost).await;
        assert_eq!(
            rv["data"]["suggested"],
            Value::Bool(true),
            "已有指针不动(D1)"
        );

        let st = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/opencode/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sv = body_json(st).await;
        assert!(
            sv["data"]["hosting"].is_object(),
            "host 后 state.hosting 非空"
        );

        let un = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/opencode/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let uv = body_json(un).await;
        assert_eq!(uv["data"]["restored"], Value::Bool(true));
        let _ = std::fs::remove_dir_all(root);
    }

    /// openclaw 泛化路由 e2e:建供应商 → host 叠加写 openclaw.json → state → unhost;JSON5(注释)文件拒绝写入
    #[tokio::test]
    async fn openclaw_routes_e2e() {
        let (state, root) = unique_state("oclaw-e2e");
        let app = build_router(state.clone());
        let created = app.clone().oneshot(
            Request::builder().method("POST").uri("/api/providers")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"ClawT","agent":"openclaw","baseUrl":"https://claw.example.com","apiKey":"sk-claw-test","model":"claude-opus-4"}"#)).unwrap(),
        ).await.unwrap();
        let cv = body_json(created).await;
        let pid = cv["data"]["id"].as_str().unwrap().to_string();

        let hosted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/openclaw/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hosted.status(), StatusCode::OK);
        let hv = body_json(hosted).await;
        assert_eq!(hv["data"]["hosted"], Value::Bool(true));

        let raw = std::fs::read_to_string(root.join("oclaw/openclaw.json")).unwrap();
        let j: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            j["models"]["providers"]["2xapi-gateway"]["api"],
            json!("openai-completions")
        );
        assert!(j["agents"]["defaults"]["model"]
            .as_str()
            .unwrap()
            .starts_with("2xapi-gateway/"));

        let st = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/openclaw/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sv = body_json(st).await;
        assert!(sv["data"]["hosting"].is_object());

        let un = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/openclaw/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let uv = body_json(un).await;
        assert_eq!(uv["data"]["restored"], Value::Bool(true));

        // JSON5 边界:含注释的既有文件拒绝写入(E_CONFIG_JSON5)
        std::fs::create_dir_all(root.join("oclaw2")).unwrap();
        std::fs::write(root.join("oclaw2/openclaw.json"), "{\n  // my config\n}").unwrap();
        let mut s2 = state.clone();
        s2.oclaw_home = root.join("oclaw2");
        let app2 = build_router(s2);
        let rejected = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/openclaw/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let _ = std::fs::remove_dir_all(root);
    }

    /// claude-desktop 泛化路由 e2e:建供应商 → host 写 3p 四件套(部署模式×2+profile+_meta) → state → unhost 还原(隔离 cd_home)
    #[tokio::test]
    async fn claude_desktop_routes_e2e() {
        let (state, root) = unique_state("cd-e2e");
        let app = build_router(state.clone());
        let created = app.clone().oneshot(
            Request::builder().method("POST").uri("/api/providers")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"CdT","agent":"claude-desktop","baseUrl":"https://gw.example.com","apiKey":"sk-cd-test","model":"claude-sonnet-5"}"#)).unwrap(),
        ).await.unwrap();
        assert_eq!(
            created.status(),
            StatusCode::OK,
            "agent=claude-desktop 供应商应可建"
        );
        let cv = body_json(created).await;
        let pid = cv["data"]["id"].as_str().unwrap().to_string();

        let hosted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-desktop/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"providerId": pid, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hosted.status(), StatusCode::OK);
        let hv = body_json(hosted).await;
        assert_eq!(hv["data"]["hosted"], Value::Bool(true));

        let support = root.join("cdsupport");
        let main_cfg: Value = serde_json::from_str(
            &std::fs::read_to_string(support.join("Claude/claude_desktop_config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(main_cfg["deploymentMode"], json!("3p"));
        let p3_cfg: Value = serde_json::from_str(
            &std::fs::read_to_string(support.join("Claude-3p/claude_desktop_config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(p3_cfg["deploymentMode"], json!("3p"));
        let profile: Value = serde_json::from_str(
            &std::fs::read_to_string(support.join("Claude-3p/configLibrary").join(format!(
                "{}.json",
                crate::agents::claude_desktop::PROFILE_ID
            )))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            profile["inferenceGatewayBaseUrl"],
            json!("http://127.0.0.1:8787/claude-desktop")
        );
        assert_eq!(
            profile["inferenceGatewayApiKey"],
            json!("2xapi-gateway-managed")
        );
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(support.join("Claude-3p/configLibrary/_meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            meta["appliedId"],
            json!(crate::agents::claude_desktop::PROFILE_ID)
        );

        let st = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/desktop/claude-desktop/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let sv = body_json(st).await;
        assert!(
            sv["data"]["hosting"].is_object(),
            "host 后 state.hosting 非空"
        );

        let un = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-desktop/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let uv = body_json(un).await;
        assert_eq!(uv["data"]["restored"], Value::Bool(true));
        let main_after: Value = serde_json::from_str(
            &std::fs::read_to_string(support.join("Claude/claude_desktop_config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            main_after["deploymentMode"],
            json!("1p"),
            "无簿记原值→恢复官方模式"
        );
        assert!(!support
            .join("Claude-3p/configLibrary")
            .join(format!(
                "{}.json",
                crate::agents::claude_desktop::PROFILE_ID
            ))
            .exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn create_then_list_uses_envelope() {
        let (state, root) = unique_state("crud");
        let app = build_router(state.clone());
        let body = json!({"name":"T","baseUrl":"https://up.test","apiKey":"sk","model":"m","accessMode":"pure_api"});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["name"], "T");
        assert_eq!(v["data"]["accessMode"], "pure_api");

        let app2 = build_router(state.clone());
        let resp = app2
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        // 内置官方 ChatGPT 条目 + 新建 1 个 = 2
        let providers = v["data"]["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["name"], "官方 ChatGPT");
        assert!(providers
            .iter()
            .any(|p| p["id"] == json!(crate::providers::OFFICIAL_PROVIDER_ID)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn profile_create_preview_apply_keeps_auth_boundary() {
        let (state, root) = unique_state("profiles");
        let app = build_router(state.clone());
        let provider_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"name":"P","baseUrl":"https://up.test","apiKey":"sk","model":"m","accessMode":"pure_api"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let provider = body_json(provider_resp).await;
        let provider_id = provider["data"]["id"].as_str().unwrap().to_string();
        std::fs::create_dir_all(&state.codex_home).unwrap();
        std::fs::write(state.codex_home.join("auth.json"), "official-auth").unwrap();

        let profile_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profiles")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"name":"本地档案","agent":"codex","providerId":provider_id,"model":"m","wireApi":"responses"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(profile_resp.status(), StatusCode::OK);
        let profile = body_json(profile_resp).await;
        let profile_id = profile["data"]["profile"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let preview_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profiles/preview")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id":profile_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preview_resp.status(), StatusCode::OK);
        let preview = body_json(preview_resp).await;
        assert_eq!(preview["data"]["officialAuth"], "preserved");
        let token = preview["data"]["previewToken"].as_str().unwrap();

        let apply_resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/profiles/apply")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"id":profile_id,"previewToken":token,"confirmed":true}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(apply_resp.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(state.codex_home.join("auth.json")).unwrap(),
            "official-auth"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn create_invalid_returns_422_validation() {
        let (state, root) = unique_state("valid");
        let app = build_router(state);
        let body = json!({"name":"","accessMode":"official"});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_VALIDATION");
        assert!(v["error"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "name"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn fetch_balance_stub() {
        let (state, root) = unique_state("bal");
        let app = build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers/fetch-balance")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["balance"], Value::Null);
        assert_eq!(v["data"]["note"], "stub");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Codex 安全边界回归：旧 activate 接口不得再触碰 config/auth。
    #[tokio::test]
    async fn e2e_create_activate_health_reflects() {
        let (state, root) = unique_state("e2e");
        // create
        let app = build_router(state.clone());
        let body = json!({"name":"E2E","baseUrl":"https://up.test","apiKey":"sk","model":"m","accessMode":"mixed"});
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["ok"], true);
        let id = v["data"]["id"].as_str().unwrap().to_string();

        // 旧 activate 曾经写 auth/config；现在必须稳定拒绝
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers/activate")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"id": id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let v = body_json(resp).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "E_CODEX_CONFIG_MUTATION_RETIRED");

        // GET /api/health 仍回到官方默认，且不伪造 active
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let h: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(h["provider"]["providerId"], "openai");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 阶段 1 E2E（任务书 §1.3）：state → host → state → 换供应商 → direct 拒绝 → unhost → state 全链。
    #[tokio::test]
    async fn e2e_desktop_host_unhost_full_chain() {
        let (state, root) = unique_state("desk-e2e");
        std::fs::create_dir_all(&state.codex_home).unwrap();
        // 无官方登录：auth 只有别家 key（还原后应回到它）
        std::fs::write(
            state.codex_home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-old"}"#,
        )
        .unwrap();

        // 建两个供应商
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder().method("POST").uri("/api/providers").header("content-type", "application/json")
                    .body(Body::from(json!({"name":"A","baseUrl":"https://a.test","apiKey":"sk-1","model":"m-a","accessMode":"mixed"}).to_string())).unwrap(),
            )
            .await
            .unwrap();
        let id1 = body_json(resp).await["data"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder().method("POST").uri("/api/providers").header("content-type", "application/json")
                    .body(Body::from(json!({"name":"B","baseUrl":"https://b.test","apiKey":"sk-2","model":"m-b","accessMode":"mixed"}).to_string())).unwrap(),
            )
            .await
            .unwrap();
        let id2 = body_json(resp).await["data"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 初始 state：未托管、无官方登录
        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/desktop/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["hasOfficial"], false);
        assert!(v["data"]["hosting"].is_null());

        // host gateway
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"providerId": id1, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["data"]["hosted"], true);

        let cfg_after_host = std::fs::read_to_string(&state.config_path).unwrap();
        assert!(cfg_after_host.contains("base_url = \"http://127.0.0.1:8787\""));
        assert!(cfg_after_host.contains("requires_openai_auth = false"));
        assert!(!cfg_after_host.contains("experimental_bearer_token"));
        let auth = std::fs::read_to_string(state.codex_home.join("auth.json")).unwrap();
        assert_eq!(auth, r#"{"OPENAI_API_KEY":"sk-old"}"#);

        // state 反映托管
        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/desktop/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["data"]["hosting"]["way"], "gateway");
        assert_eq!(v["data"]["hosting"]["providerId"], id1.as_str());

        // 换供应商：仅 set_active，config 不变
        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"providerId": id2, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["data"]["switched"], true);
        let cfg_after_switch = std::fs::read_to_string(&state.config_path).unwrap();
        assert!(
            cfg_after_switch.contains("base_url = \"http://127.0.0.1:8787\""),
            "custom 段(网关指向)不变"
        );
        assert!(
            cfg_after_switch.contains("model = \"m-b\""),
            "model 同步为新供应商(真机故障修复)"
        );

        // direct 已永久退役：不得把上游 Key 写入 Codex 配置
        let app = build_router(state.clone());
        let direct_resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"providerId": id2, "way": "direct"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(direct_resp.status(), StatusCode::GONE);
        let direct_body = body_json(direct_resp).await;
        assert_eq!(direct_body["error"], "E_DESKTOP_DIRECT_RETIRED");

        // unhost：回到干净态
        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/unhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["data"]["restored"], true);

        let cfg_after = std::fs::read_to_string(&state.config_path).unwrap();
        assert!(!cfg_after.contains("[model_providers.2xapi_gateway]"));
        assert_eq!(
            std::fs::read_to_string(state.codex_home.join("auth.json")).unwrap(),
            r#"{"OPENAI_API_KEY":"sk-old"}"#,
            "auth 应恢复 host 前状态"
        );

        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/desktop/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(v["data"]["hosting"].is_null());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Claude Code 配置托管接口 ─────────────────────────────

    #[tokio::test]
    async fn e2e_claude_start_writes_settings_without_returning_key() {
        let (state, root) = unique_state("claude-start");
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let app = build_router(state.clone());
        // 建 claude 供应商(agent=claude)
        let resp = app
            .oneshot(
                Request::builder().method("POST").uri("/api/providers").header("content-type", "application/json")
                    .body(Body::from(json!({"name":"ClaudeT","agent":"claude","baseUrl":"https://up.claude.example.com","apiKey":"sk-claude-test-secret","model":"claude-sonnet","accessMode":"pure_api"}).to_string())).unwrap(),
            )
            .await
            .unwrap();
        let id = body_json(resp).await["data"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let app = build_router(state.clone());
        let v = body_json(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-start")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(v["ok"], true, "成功应带 ok:true");
        assert_eq!(v["way"], "gateway");
        assert_eq!(v["providerId"], id.as_str());
        assert!(v.get("env").is_none());
        assert!(v.get("command").is_none());
        let settings = root.join(".claude/settings.json");
        let written = std::fs::read_to_string(settings).unwrap();
        assert!(written.contains("http://127.0.0.1:8787/anthropic"));
        assert!(!written.contains("sk-claude-test-secret"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn e2e_claude_config_host_state_unhost_roundtrip() {
        let (state, root) = unique_state("claude-config-host");
        let settings = root.join(".claude/settings.json");
        let original = b"{\n  \"theme\": \"dark\",\n  \"env\": {\"CUSTOM\": \"keep\"}\n}\n";
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, original).unwrap();

        let provider = crate::providers::create(
            &state.providers_path,
            crate::providers::ProviderInput {
                name: "Claude Config".into(),
                agent: "claude".into(),
                base_url: "https://upstream.invalid".into(),
                api_key: "sk-server-test-secret".into(),
                model: "gpt-5.6".into(),
                sub2api_multiplier: 1.0,
                ..Default::default()
            },
        )
        .unwrap();
        let app = build_router(state.clone());
        let hosted = body_json(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude/host")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"providerId": provider.id, "way": "gateway"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(hosted["ok"], true);

        let written = std::fs::read_to_string(&settings).unwrap();
        let written_json: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(written_json["theme"], "dark");
        assert_eq!(written_json["env"]["CUSTOM"], "keep");
        assert_eq!(
            written_json["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:8787/anthropic"
        );
        assert_eq!(
            written_json["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
            "1"
        );
        assert!(!written.contains("sk-server-test-secret"));

        let state_value = body_json(
            build_router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/desktop/claude/state")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(state_value["ok"], true);
        assert_eq!(state_value["data"]["hosted"], true);

        let unhosted = body_json(
            build_router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/desktop/claude/unhost")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(unhosted["ok"], true);
        assert_eq!(unhosted["data"]["restored"], true);
        assert_eq!(std::fs::read(&settings).unwrap(), original);
        assert!(crate::providers::get_active_for_agent(&state.providers_path, "claude").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn anthropic_models_route_lists_selected_claude_provider_models() {
        let (state, root) = unique_state("claude-models");
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "name": "Claude Models",
                            "agent": "claude",
                            "baseUrl": "https://up.claude.example.com",
                            "apiKey": "sk-claude-models",
                            "model": "gpt-5.6",
                            "models": [
                                {"name": "gpt-5.6", "display_name": "GPT 5.6"},
                                {"name": "gpt-5.5", "display_name": "GPT 5.5"}
                            ],
                            "accessMode": "pure_api"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let provider_id = body_json(resp).await["data"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 启动选择会把该供应商设为 Claude active，网关后续不能再静默取其他供应商。
        let app = build_router(state.clone());
        let start = body_json(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-start")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"providerId": provider_id}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(start["ok"], true);

        let models = body_json(
            build_router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/anthropic/v1/models")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(models["has_more"], false);
        assert_eq!(models["data"][0]["id"], "gpt-5.6");
        assert_eq!(models["data"][0]["display_name"], "GPT 5.6");
        assert_eq!(models["data"][1]["id"], "gpt-5.5");

        let detail = body_json(
            build_router(state)
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/anthropic/v1/models/gpt-5.5")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(detail["id"], "gpt-5.5");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn e2e_claude_start_no_provider_returns_error_envelope() {
        let (state, root) = unique_state("claude-start-noprov");
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-start")
                    .header("content-type", "application/json")
                    .body(Body::from("{}".to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "E_NO_CLAUDE_PROVIDER");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn e2e_claude_launch_no_provider_returns_error_envelope() {
        let (state, root) = unique_state("claude-launch-noprov");
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let resp = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/desktop/claude-launch")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let value = body_json(resp).await;
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "E_NO_CLAUDE_PROVIDER");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 阶段 4 加速线路路由(任务书 §五)──

    fn accel_line(id: &str, scope: &[&str]) -> crate::acclines::AccLine {
        crate::acclines::AccLine {
            id: id.into(),
            name: id.into(),
            endpoint: "http://line.test:1".into(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            priority: 1,
            enabled: true,
            credential: None,
        }
    }

    fn accel_state(
        mode: &str,
        custom_node: &str,
        lines: Vec<crate::acclines::AccLine>,
    ) -> (AppState, std::path::PathBuf) {
        let (state, root) = unique_state("accel");
        *state.accel.lock().unwrap() = AccelCfg {
            mode: mode.into(),
            custom_node: custom_node.into(),
        };
        state.health.set_lines(lines);
        (state, root)
    }

    async fn accel_get(app: &Router, uri: &str) -> Value {
        body_json(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    }

    async fn accel_post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        (resp.status(), body_json(resp).await)
    }

    /// 用量仪表盘路由:空数据 → 空 providers;落一行后聚合出该 provider。
    #[tokio::test]
    async fn usage_stats_route_empty_then_aggregates() {
        let (state, root) = unique_state("usage-route");
        let app = build_router(state.clone());
        let v = accel_get(&app, "/api/usage-stats").await;
        assert_eq!(v["providers"].as_array().unwrap().len(), 0);
        crate::usage_stats::log_request(
            &state.codex_home,
            &crate::usage_stats::ReqLog {
                ts: 1,
                provider_id: "p".into(),
                provider_name: "P".into(),
                key_masked: "sk-…abcd".into(),
                route: "codex".into(),
                line: "direct".into(),
                degraded_to_direct: false,
                latency_ms: 25,
                ok: true,
            },
        );
        let v = accel_get(&app, "/api/usage-stats").await;
        assert_eq!(v["providers"][0]["providerId"], "p");
        assert_eq!(v["providers"][0]["count"], 1);
        assert_eq!(v["providers"][0]["p50Ms"], 25);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_state_default_off_no_scope_note() {
        let (state, root) = accel_state("off", "", vec![accel_line("l1", &["2xa.cc.cd"])]);
        let app = build_router(state.clone());
        let v = accel_get(&app, "/api/accel/state").await;
        assert_eq!(v["mode"], "off");
        assert_eq!(v["customNode"], "");
        assert_eq!(v["scopeNote"], "");
        let lines = v["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["id"], "l1");
        assert_eq!(lines[0]["latency"], 0, "未探测 latency 为 0");
        assert_eq!(lines[0]["fails"], 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_mode_roundtrip_persists_to_settings() {
        let (state, root) = accel_state("off", "", vec![]);
        // 先建 codex_home,验证写 2xapi-settings.json
        std::fs::create_dir_all(&state.codex_home).unwrap();

        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "official"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        // 落盘
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(state.codex_home.join("2xapi-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["accel"]["mode"], "official");
        assert_eq!(saved["accel"]["customNode"], "");
        // GET 反映
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["mode"], "official");
        // 往返回 off
        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "off"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["mode"], "off");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_custom_node_roundtrip_persists() {
        let (state, root) = accel_state("off", "", vec![]);
        std::fs::create_dir_all(&state.codex_home).unwrap();
        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/custom-node",
            &json!({"endpoint": "http://node.test:1"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["customNode"], "http://node.test:1");
        let saved: Value = serde_json::from_str(
            &std::fs::read_to_string(state.codex_home.join("2xapi-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["accel"]["customNode"], "http://node.test:1");
        // 非法地址 400
        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/custom-node",
            &json!({"endpoint": "ftp://bad"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["ok"], false);
        assert!(
            !v["error"].as_str().unwrap_or("").is_empty(),
            "400 应带人话 error"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_custom_node_rejects_malformed_missing_and_embedded_credentials() {
        let (state, root) = accel_state("off", "", vec![]);
        for body in [
            json!({"endpoint": "http://"}),
            json!({"endpoint": "http://bad host"}),
            json!({"endpoint": "http://user:pass@127.0.0.1:8080"}),
            json!({}),
            json!({"endpoint": 123}),
        ] {
            let (status, value) = accel_post(
                &build_router(state.clone()),
                "/api/accel/custom-node",
                &body,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "body={body}, value={value}"
            );
            assert_eq!(value["ok"], false, "body={body}, value={value}");
        }
        assert_eq!(state.accel.lock().unwrap().custom_node, "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_test_node_rejects_malformed_endpoint_before_network() {
        let (state, root) = accel_state("off", "", vec![]);
        let (status, value) = accel_post(
            &build_router(state),
            "/api/accel/test-node",
            &json!({"endpoint": "http://"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["ok"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_mode_write_failure_returns_500_and_keeps_previous_state() {
        let (mut state, root) = accel_state("off", "", vec![]);
        let blocked_home = root.join("blocked-home");
        std::fs::write(&blocked_home, "not a directory").unwrap();
        state.codex_home = blocked_home;

        let (status, value) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "official"}),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(value["ok"], false);
        assert_eq!(state.accel.lock().unwrap().mode, "off");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_custom_node_write_failure_returns_500_and_keeps_previous_state() {
        let (mut state, root) = accel_state("off", "", vec![]);
        let blocked_home = root.join("blocked-home");
        std::fs::write(&blocked_home, "not a directory").unwrap();
        state.codex_home = blocked_home;

        let (status, value) = accel_post(
            &build_router(state.clone()),
            "/api/accel/custom-node",
            &json!({"endpoint": "http://127.0.0.1:8080"}),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(value["ok"], false);
        assert_eq!(state.accel.lock().unwrap().custom_node, "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_mode_invalid_returns_400() {
        let (state, root) = accel_state("off", "", vec![]);
        let (st, v) = accel_post(
            &build_router(state),
            "/api/accel/mode",
            &json!({"mode": "bogus"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["ok"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_custom_mode_without_node_returns_400() {
        let (state, root) = accel_state("off", "", vec![]); // 无 custom_node
        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "custom"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(
            v["error"].as_str().unwrap_or("").contains("自定义"),
            "应提示先配节点: {v}"
        );
        // 已配节点 → 成功
        let (st, _v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/custom-node",
            &json!({"endpoint": "http://node.test:1"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "custom"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accel_scope_note_hit_and_miss() {
        let lines = vec![accel_line("l1", &["2xa.cc.cd"])];
        // official + 未命中 → 提示
        assert_eq!(
            compute_scope_note("official", Some("https://openai.com"), &lines),
            "该供应商不在官方线路范围,已直连"
        );
        // official + 命中 → 空串
        assert_eq!(
            compute_scope_note("official", Some("https://api.2xa.cc.cd"), &lines),
            ""
        );
        // official + 无 active → 空串
        assert_eq!(compute_scope_note("official", None, &lines), "");
        // off / custom → 空串
        assert_eq!(
            compute_scope_note("off", Some("https://openai.com"), &lines),
            ""
        );
        assert_eq!(
            compute_scope_note("custom", Some("https://openai.com"), &lines),
            ""
        );
    }

    #[tokio::test]
    async fn accel_state_scope_note_from_active_provider() {
        let (state, root) = accel_state("official", "", vec![accel_line("l1", &["2xa.cc.cd"])]);
        // active provider 的 base_url 未命中(openai.com)
        let app = build_router(state.clone());
        let body = json!({"name":"Miss","baseUrl":"https://openai.com","apiKey":"sk","model":"m","accessMode":"mixed"});
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let id = body_json(resp).await["data"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        crate::providers::set_active(&state.providers_path, &id);
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["scopeNote"], "该供应商不在官方线路范围,已直连");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accel_state_scope_note_ignores_other_agents_active_provider() {
        let (state, root) = accel_state("official", "", vec![accel_line("l1", &["2xa.cc.cd"])]);
        let codex = crate::providers::create(
            &state.providers_path,
            crate::providers::ProviderInput {
                name: "Codex".into(),
                agent: "codex".into(),
                base_url: "https://api.2xa.cc.cd".into(),
                api_key: "sk-codex".into(),
                model: "gpt".into(),
                sub2api_multiplier: 1.0,
                ..Default::default()
            },
        )
        .unwrap();
        crate::providers::set_active(&state.providers_path, &codex.id);
        let workbuddy = crate::providers::create(
            &state.providers_path,
            crate::providers::ProviderInput {
                name: "WorkBuddy".into(),
                agent: "workbuddy".into(),
                base_url: "https://outside.example.com".into(),
                api_key: "sk-workbuddy".into(),
                model: "workbuddy-model".into(),
                sub2api_multiplier: 1.0,
                ..Default::default()
            },
        )
        .unwrap();
        crate::providers::set_active(&state.providers_path, &workbuddy.id);

        assert_eq!(
            crate::providers::get_active(&state.providers_path)
                .unwrap()
                .id,
            workbuddy.id,
            "测试前提：全局 active 已切到其他平台"
        );
        let value = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(
            value["scopeNote"], "",
            "加速 scope 只应读取 Codex active provider"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 星图 任务 B:usage 块 / refresh-cred / mode 触发 ──

    fn add_active_provider(state: &AppState, api_key: &str) {
        let input = crate::providers::ProviderInput {
            name: "T".into(),
            base_url: "https://api.2xa.cc.cd".into(),
            api_key: api_key.into(),
            model: "m".into(),
            sub2api_multiplier: 1.0,
            ..Default::default()
        };
        let p = crate::providers::create(&state.providers_path, input).unwrap();
        crate::providers::set_active(&state.providers_path, &p.id);
    }

    fn put_store_entry(state: &AppState, api_key: &str, cred: crate::nodecreds::NodeCred) {
        state.nodecreds.write().unwrap().set_for_key(api_key, cred);
    }

    /// official + 有 entry → usage.ok:true + keyMasked(前3…尾4,断言不含整 Key)。
    #[tokio::test]
    async fn accel_state_usage_ok_when_official_with_entry() {
        let (state, root) = accel_state("official", "", vec![]);
        let key = format!("sk-live-{}", "2026abcd"); // 仅拼接,断言只用脱敏片段
        add_active_provider(&state, &key);
        let cred = test_node_cred("u", "p");
        put_store_entry(&state, &key, cred.clone());

        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["usage"]["ok"], true, "usage 块应为 ok:true: {v}");
        let km = v["usage"]["keyMasked"].as_str().unwrap().to_string();
        assert!(
            km.starts_with("sk-") && km.contains('…') && km.ends_with("abcd"),
            "keyMasked 应为前3…尾4脱敏: {km}"
        );
        assert_eq!(v["usage"]["quotaTotalBytes"], 10_737_418_240u64);
        assert_eq!(v["usage"]["quotaUsedBytes"], 1_073_741_824u64);
        assert_eq!(v["usage"]["quotaPercent"], 0.1);
        assert_eq!(v["usage"]["degradedToDirect"], false);
        assert_eq!(v["usage"]["issuedAt"], cred.issued_at);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 非 official / official 无 entry / 无 active → usage.ok:false(兜底块)。
    #[tokio::test]
    async fn accel_state_usage_false_fallbacks() {
        // ① mode=off(即使有 active + entry)→ false
        let (state, root) = accel_state("off", "", vec![]);
        let key = "sk-fallback-0001";
        add_active_provider(&state, key);
        put_store_entry(&state, key, test_node_cred("u", "p"));
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["usage"]["ok"], false);
        assert_eq!(v["usage"]["degradedToDirect"], false);
        let _ = std::fs::remove_dir_all(&root);

        // ② official + active 但 store 无该 key 项 → false
        let (state, root) = accel_state("official", "", vec![]);
        add_active_provider(&state, "sk-not-issued-0002");
        let v = accel_get(&build_router(state.clone()), "/api/accel/state").await;
        assert_eq!(v["usage"]["ok"], false);
        let _ = std::fs::remove_dir_all(&root);

        // ③ official 但无 active provider → false(整 state 不 500)
        let (state, root) = accel_state("official", "", vec![]);
        let v = accel_get(&build_router(state), "/api/accel/state").await;
        assert_eq!(v["usage"]["ok"], false);
        assert_eq!(v["mode"], "official");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// refresh-cred 200:签发成功 → 落 store + usage.ok:true,且清除降级位。
    #[tokio::test]
    async fn refresh_cred_ok_stores_and_clears_degraded() {
        let base = spawn_issue_mock(
            "200 OK",
            r#"{"user":"fresh-u","pass":"fresh-p","quotaTotalBytes":100,"quotaUsedBytes":25,"proxyEndpoint":"http://n"}"#,
        )
        .await;
        let _g = set_issue_base_for_tests(&base);
        let (state, root) = accel_state("official", "", vec![]);
        let key = "sk-refresh-0003";
        add_active_provider(&state, key);
        // 先放一个已降级的旧项(验证刷新清除 degraded_to_direct)
        let mut degraded = test_node_cred("old", "old");
        degraded.degraded_to_direct = true;
        put_store_entry(&state, key, degraded);

        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/refresh-cred",
            &json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["usage"]["ok"], true);
        assert_eq!(v["usage"]["quotaTotalBytes"], 100);
        assert_eq!(v["usage"]["quotaUsedBytes"], 25);
        assert_eq!(v["usage"]["quotaPercent"], 0.25);
        // store 落项且降级被清除
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key(key)
            .cloned()
            .expect("刷新后 store 应有该 key 项");
        assert!(!entry.degraded_to_direct, "刷新应清除 degraded_to_direct");
        assert_eq!(entry.quota_used_bytes, 25);
        // 落盘可见
        assert!(state.codex_home.join("accel-credentials.json").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// refresh-cred 401(Key 无效)→ 401 + 人话 error。
    #[tokio::test]
    async fn refresh_cred_401_key_invalid() {
        let base = spawn_issue_mock("401 Unauthorized", r#"{"error":"Key 无效或未充值"}"#).await;
        let _g = set_issue_base_for_tests(&base);
        let (state, root) = accel_state("official", "", vec![]);
        add_active_provider(&state, "sk-bad-0004");
        let (st, v) = accel_post(&build_router(state), "/api/accel/refresh-cred", &json!({})).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "Key 无效或未充值");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// refresh-cred 403(配额满)→ 403 + 人话 error + 快照更新 store used。
    #[tokio::test]
    async fn refresh_cred_403_quota_full_updates_snapshot() {
        let base = spawn_issue_mock(
            "403 Forbidden",
            r#"{"error":"该账号本月已用满 10G","quotaUsedBytes":999,"quotaTotalBytes":1000}"#,
        )
        .await;
        let _g = set_issue_base_for_tests(&base);
        let (state, root) = accel_state("official", "", vec![]);
        let key = "sk-full-0005";
        add_active_provider(&state, key);
        put_store_entry(&state, key, test_node_cred("u", "p"));

        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/refresh-cred",
            &json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(v["error"], "该账号本月已用满 10G");
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key(key)
            .cloned()
            .unwrap();
        assert_eq!(entry.quota_used_bytes, 999, "快照带 used → 应回写 store");
        assert_eq!(entry.quota_total_bytes, 1000);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// refresh-cred 502(节点不可达)→ 502 + 人话 error。
    #[tokio::test]
    async fn refresh_cred_502_unreachable() {
        let _g = set_issue_base_for_tests(DEAD_ISSUE_BASE);
        let (state, root) = accel_state("official", "", vec![]);
        add_active_provider(&state, "sk-any-0006");
        let (st, v) = accel_post(&build_router(state), "/api/accel/refresh-cred", &json!({})).await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
        assert_eq!(v["error"], "加速节点暂不可达,请稍后再试");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// refresh-cred 400:无 active provider / key 为空 → 「请先配置供应商」。
    #[tokio::test]
    async fn refresh_cred_400_no_provider() {
        let (state, root) = accel_state("official", "", vec![]);
        let (st, v) = accel_post(&build_router(state), "/api/accel/refresh-cred", &json!({})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "请先配置供应商");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 切 official 触发 best-effort 预签发:成功 → store 落项。
    #[tokio::test]
    async fn mode_official_best_effort_issue_success() {
        let base = spawn_issue_mock(
            "200 OK",
            r#"{"user":"mu","pass":"mp","quotaTotalBytes":10,"quotaUsedBytes":1,"proxyEndpoint":"http://n"}"#,
        )
        .await;
        let _g = set_issue_base_for_tests(&base);
        let (state, root) = accel_state("off", "", vec![]);
        let key = "sk-mode-ok-0007";
        add_active_provider(&state, key);

        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "official"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert!(
            state.nodecreds.read().unwrap().get_for_key(key).is_some(),
            "切 official 应 best-effort 预签发落 store"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 切 official 预签发失败(不可达)→ 忽略失败,mode 仍切换成功。
    #[tokio::test]
    async fn mode_official_best_effort_failure_does_not_block() {
        let _g = set_issue_base_for_tests(DEAD_ISSUE_BASE);
        let (state, root) = accel_state("off", "", vec![]);
        add_active_provider(&state, "sk-mode-fail-0008");

        let (st, v) = accel_post(
            &build_router(state.clone()),
            "/api/accel/mode",
            &json!({"mode": "official"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "预签发失败不得阻断切 mode");
        assert_eq!(v["ok"], true);
        let v = accel_get(&build_router(state), "/api/accel/state").await;
        assert_eq!(v["mode"], "official");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// official-passthrough：官方通道代理设置的校验与读写（保留同文件其他段）。
    #[test]
    fn official_proxy_validation_and_roundtrip() {
        let dir = std::env::temp_dir().join(format!("2xapi-official-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 空默认 = 直连
        assert_eq!(load_official_proxy(&dir), "");
        // 非法 scheme 拒绝
        assert!(save_official_proxy(&dir, "ftp://x").is_err());
        assert!(save_official_proxy(&dir, "not a url").is_err());
        // 合法形态接受
        for ok in [
            "http://127.0.0.1:7890",
            "socks5://127.0.0.1:1080",
            "socks5h://vpn.example:1080",
            "",
        ] {
            save_official_proxy(&dir, ok).unwrap();
            assert_eq!(load_official_proxy(&dir), ok);
        }
        // 保留同文件其他段（accel 等）
        save_official_proxy(&dir, "socks5://127.0.0.1:1080").unwrap();
        let raw = std::fs::read_to_string(dir.join("2xapi-settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["official"]["proxyUrl"], "socks5://127.0.0.1:1080");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
