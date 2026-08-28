#[cfg(not(test))]
use axum::http::Request;
#[cfg(not(test))]
use axum::middleware::{self, Next};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// 与 tauri.conf.json security.csp 保持一致；Axum 侧必须自己输出，
/// Tauri 的 CSP 注入只对 tauri:// 资产协议生效，External URL 页面不生效。
const CSP: &str = "default-src 'self'; connect-src 'self' https://turing.captcha.qcloud.com https://www.tycaptcha.com https://rce.tencentrio.com; img-src 'self' data: https://*.captcha.gtimg.com https://*.qcloud.com; style-src 'self' 'unsafe-inline'; script-src 'self' https://turing.captcha.qcloud.com https://turing.captcha.gtimg.com https://global.turing.captcha.gtimg.com https://cloudcache.tencentcs.com; frame-src https://turing.captcha.qcloud.com https://ca.turing.captcha.qcloud.com https://www.tycaptcha.com; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

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

fn ok_json(data: Value) -> Response {
    (StatusCode::OK, Json(data)).into_response()
}

fn err_json(status: StatusCode, msg: &str) -> Response {
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

fn val_errs_env(errs: &[crate::providers::ValidationError]) -> Response {
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

// --- Health & session ---

async fn handle_health(State(s): State<Arc<AppState>>) -> Response {
    let cfg = crate::config::read_toml(&s.config_path);
    let provider = cfg
        .get("model_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("openai");
    let model = cfg.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let login = crate::codex_security::probe_login_cached(&s.codex_home);
    ok_json(json!({
        "ok": true,
        "provider": { "providerId": provider },
        "model": model,
        "configPath": s.config_path.to_string_lossy(),
        "codexHome": s.codex_home.to_string_lossy(),
        "login": login,
    }))
}

// 网关健康检查（FR-4.1）。
// 注意：`/health` 不走统一响应信封；按 04 §2 直接返回 {status, active_provider_id, access_mode}。
// 动态读 active provider（供前端顶栏同步：active 状态变更后刷新 /health）。
async fn handle_gateway_health(State(s): State<Arc<AppState>>) -> Response {
    let (active_id, access_mode) =
        match crate::providers::get_active_for_agent(&s.providers_path, "codex") {
            Some(p) => (
                json!(p.id),
                serde_json::to_value(p.access_mode).unwrap_or(json!(null)),
            ),
            None => (json!(null), json!(null)),
        };
    ok_json(json!({
        "status": "ok",
        "active_provider_id": active_id,
        "access_mode": access_mode,
    }))
}

/// POST /api/open-url {url} —— 用系统默认浏览器打开外部链接(官网等)。
/// 仅允许 http(s)(防 file:// 等协议);CSP 下 window.open 不会走系统浏览器,故经后端。
async fn handle_open_url(Json(body): Json<Value>) -> Response {
    let url = body
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "仅支持 http(s) 链接" })),
        )
            .into_response();
    }
    // Windows 走 cmd /C start:url 含 cmd 元字符即命令注入(&|^%! 与空白、引号)——拒绝。
    // macOS/Linux 走参数向量(open/xdg-open),无 shell 解释,仅拒绝空白与引号类危险字符,
    // 允许 &、%(URL query/编码的合法字符),避免误拒带参数的正常链接。
    #[cfg(target_os = "windows")]
    let url_invalid = url
        .chars()
        .any(|c| c.is_whitespace() || "&|^<>%!\"'`".contains(c));
    #[cfg(not(target_os = "windows"))]
    let url_invalid = url.chars().any(|c| c.is_whitespace() || "\"'`".contains(c));
    if url_invalid {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "链接包含非法字符" })),
        )
            .into_response();
    }
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url.clone()]);
    #[cfg(target_os = "windows")]
    let cmd = (
        "cmd",
        vec!["/C".into(), "start".into(), "".into(), url.clone()],
    );
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url.clone()]);
    match std::process::Command::new(cmd.0).args(&cmd.1).spawn() {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("打开失败: {e}") })),
        )
            .into_response(),
    }
}

async fn handle_session(State(s): State<Arc<AppState>>) -> Response {
    if let Some(session) = crate::auth::load_session(&s.codex_home) {
        return ok_json(json!({ "authenticated": true, "user": session.user }));
    }
    // 过期:refresh_token 免验证码自动续期(「保存登录」;滑块登录只需一次)
    match crate::auth::refresh_session(&s.codex_home).await {
        Some(session) => {
            ok_json(json!({ "authenticated": true, "user": session.user, "refreshed": true }))
        }
        None => ok_json(json!({ "authenticated": false })),
    }
}

// --- Auth ---

async fn handle_auth_captcha(State(_s): State<Arc<AppState>>) -> Response {
    match crate::auth::fetch_captcha_settings().await {
        Ok(settings) => ok_json(settings),
        Err(_) => ok_json(json!({ "enabled": false, "provider": null })),
    }
}

async fn handle_auth_login(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let email = body.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    // 腾讯滑块票据(前端人工完成后随请求带上;未开启验证码的站点为空)
    let ticket = body
        .get("captchaTicket")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let remember = body
        .get("remember")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let randstr = body
        .get("captchaRandstr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if email.is_empty() || password.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "邮箱和密码不能为空");
    }
    match crate::auth::login(email, password, ticket, randstr).await {
        Ok(result) => match crate::auth::save_session(&s.codex_home, &result, remember) {
            Ok(()) => ok_json(json!({ "authenticated": true, "user": result.user })),
            Err(error) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &error),
        },
        Err(e) => err_json(StatusCode::UNAUTHORIZED, &format!("登录失败: {}", e)),
    }
}

async fn handle_auth_logout(State(s): State<Arc<AppState>>) -> Response {
    crate::auth::clear_session(&s.codex_home);
    ok_json(json!({ "ok": true }))
}

async fn handle_auth_remembered(State(s): State<Arc<AppState>>) -> Response {
    match crate::auth::load_remembered(&s.codex_home) {
        Some((email, _)) => ok_json(json!({ "remembered": true, "email": email, "password": "" })),
        None => ok_json(json!({ "remembered": false, "email": "", "password": "" })),
    }
}

async fn handle_auth_remember(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let email = body.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    match crate::auth::save_remembered(&s.codex_home, email, password) {
        Ok(()) => ok_json(json!({ "ok": true })),
        Err(error) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

async fn handle_auth_forget(State(s): State<Arc<AppState>>) -> Response {
    crate::auth::clear_remembered(&s.codex_home);
    ok_json(json!({ "ok": true }))
}

async fn handle_key_groups(State(s): State<Arc<AppState>>) -> Response {
    match crate::auth::load_session(&s.codex_home) {
        Some(session) => match crate::auth::fetch_key_groups(&session.access_token).await {
            Ok(groups) => ok_json(groups),
            Err(e) => err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("获取分组失败: {}", e),
            ),
        },
        None => err_json(StatusCode::UNAUTHORIZED, "请先登录 2xapi 账号"),
    }
}

// GET /api/auth/me —— 实时账号信息(余额);失败退回 session 快照
async fn handle_auth_me(State(s): State<Arc<AppState>>) -> Response {
    let session = match crate::auth::load_session(&s.codex_home) {
        Some(sess) => sess,
        None => match crate::auth::refresh_session(&s.codex_home).await {
            Some(sess) => sess,
            None => return err_json(StatusCode::UNAUTHORIZED, "请先登录 2xapi 账号"),
        },
    };
    match crate::auth::fetch_me(&session.access_token).await {
        Ok(user) if !user.is_null() => ok_json(json!({ "user": user })),
        _ => ok_json(json!({ "user": session.user })), // 外呼失败退回快照(余额可能滞后)
    }
}

// GET /api/auth/api-keys —— 一键导入数据源:用户 Key 列表 + relay 上游地址
async fn handle_auth_api_keys(State(s): State<Arc<AppState>>) -> Response {
    // session 过期自动续期(与 /api/session 同策略)
    let session = match crate::auth::load_session(&s.codex_home) {
        Some(sess) => Some(sess),
        None => crate::auth::refresh_session(&s.codex_home).await,
    };
    let Some(session) = session else {
        return err_json(StatusCode::UNAUTHORIZED, "请先登录 2xapi 账号");
    };
    let (keys_result, base_url_result) = tokio::join!(
        crate::auth::fetch_api_keys(&session.access_token),
        crate::auth::fetch_relay_base_url()
    );
    let keys = match keys_result {
        Ok(v) => {
            let d = v.get("data").cloned().unwrap_or(json!([]));
            // 部署版为 {items:[...]}(main 分支为直接数组)——两种都兼容
            if d.is_array() {
                d
            } else {
                d.get("items").cloned().unwrap_or(json!([]))
            }
        }
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("获取 Key 列表失败: {}", e),
            )
        }
    };
    let base_url = base_url_result.unwrap_or_else(|_| "https://2xa.cc.cd".into());
    ok_json(json!({ "keys": keys, "baseUrl": base_url }))
}

// --- Providers（04 契约：统一信封 + 错误码）---

// GET /api/providers
async fn handle_providers_list(State(s): State<Arc<AppState>>) -> Response {
    let data = crate::providers::load(&s.providers_path);
    let providers: Vec<Value> = data
        .providers
        .iter()
        .map(crate::providers::public_provider)
        .collect();
    ok_env(json!({
        "providers": providers,
        "active_provider_id": data.active_provider_id,
        "active_provider_ids": data.active_provider_ids,
    }))
}

// POST /api/providers
async fn handle_providers_create(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let input = crate::providers::value_to_input(&body);
    match crate::providers::create(&s.providers_path, input) {
        Ok(p) => ok_env(crate::providers::public_provider(&p)),
        Err(errs) => val_errs_env(&errs),
    }
}

// PUT /api/providers/:id
async fn handle_providers_update(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    // PUT 可能来自旧版前端，仅提交了少量字段。数据层按字段保留未提交值，
    // 并在当前 Codex 托管该供应商时立即重应用 config/catalog，避免磁盘配置滞后。
    let hosted_codex = crate::desktop::detect_hosting(&s.config_path, &s.providers_path)
        .get("way")
        .and_then(Value::as_str)
        == Some("gateway");
    let active_codex = crate::providers::get_active_for_agent(&s.providers_path, "codex")
        .is_some_and(|provider| provider.id == id);
    let snapshot = if hosted_codex && active_codex {
        match crate::desktop::snapshot_file(&s.providers_path) {
            Ok(value) => Some(value),
            Err((_, _, message)) => {
                return err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_IO", &message, None)
            }
        }
    } else {
        None
    };
    match crate::providers::update_from_value(&s.providers_path, &id, &body) {
        Ok(p) if hosted_codex && active_codex && p.agent == "codex" => {
            match crate::desktop::host(
                &s.config_path,
                &s.backup_dir,
                &s.codex_home,
                &s.providers_path,
                &id,
                "gateway",
            ) {
                Ok(_) => ok_env(crate::providers::public_provider(&p)),
                Err((status, code, message)) => {
                    if let Some(snapshot) = snapshot.as_ref() {
                        let _ = crate::desktop::restore_file_snapshot(&s.providers_path, snapshot);
                    }
                    err_env(
                        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                        &code,
                        &message,
                        None,
                    )
                }
            }
        }
        Ok(p) => ok_env(crate::providers::public_provider(&p)),
        Err(errs) => {
            if errs.len() == 1 && errs[0].field == "id" {
                err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None)
            } else {
                val_errs_env(&errs)
            }
        }
    }
}

// DELETE /api/providers/:id
async fn handle_providers_delete(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    // 持久化托管平台按「平台托管态 + 平台 active provider」双重判断，避免删除后磁盘仍引用旧供应商。
    // Claude Code 的托管快照位于 backup_dir，状态由配置托管模块统一判断。
    let hosted_agents = [
        (
            "codex",
            !crate::desktop::detect_hosting(&s.config_path, &s.providers_path).is_null(),
        ),
        (
            "hermes",
            !crate::agents::hermes::detect_state(&s.hermes_home.join("config.yaml"))["hosting"]
                .is_null(),
        ),
        (
            "gemini",
            !crate::agents::gemini::state(&s.gem_home)["hosting"].is_null(),
        ),
        (
            "claude",
            crate::agents::claude_code::state(
                claude_home(&s.codex_home),
                &s.backup_dir,
                &s.providers_path,
            )["hosted"]
                .as_bool()
                .unwrap_or(false),
        ),
        (
            "grokbuild",
            !crate::agents::grok::state(&s.grok_home)["hosting"].is_null(),
        ),
        (
            "opencode",
            !crate::agents::opencode::state(&s.oc_home)["hosting"].is_null(),
        ),
        (
            "openclaw",
            !crate::agents::openclaw::state(&s.oclaw_home)["hosting"].is_null(),
        ),
        (
            "claude-desktop",
            !crate::agents::claude_desktop::state(&s.cd_home)["hosting"].is_null(),
        ),
        (
            "workbuddy",
            !crate::agents::workbuddy::state(&s.wb_home)["hosting"].is_null(),
        ),
        (
            "cursor",
            !crate::agents::cursor::state(&s.cursor_home)["hosting"].is_null(),
        ),
    ];
    let hosted_by = hosted_agents.iter().find_map(|(agent, hosted)| {
        if *hosted
            && crate::providers::get_active_for_agent(&s.providers_path, agent)
                .is_some_and(|provider| provider.id == id)
        {
            Some(*agent)
        } else {
            None
        }
    });
    if let Some(agent) = hosted_by {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_PROVIDER_HOSTED",
            &format!("托管中的供应商不能删除,请先在「{agent}」还原官方"),
            None,
        );
    }
    match crate::providers::delete_checked(&s.providers_path, &id) {
        Ok(()) => ok_env(json!({ "id": id, "deleted": true })),
        Err(error) if error == "供应商不存在" => {
            err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", &error, None)
        }
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PROVIDER_WRITE",
            &error,
            None,
        ),
    }
}

// PUT /api/providers/reorder { ids }
async fn handle_providers_reorder(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let ids: Vec<String> = body
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match crate::providers::reorder_checked(&s.providers_path, &ids) {
        Ok(()) => ok_env(json!({ "reordered": true, "count": ids.len() })),
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_PROVIDER_WRITE",
            &error,
            None,
        ),
    }
}

// GET /api/providers/active
async fn handle_providers_active(State(s): State<Arc<AppState>>) -> Response {
    match crate::providers::get_active(&s.providers_path) {
        Some(p) => ok_env(crate::providers::public_provider(&p)),
        None => ok_env(Value::Null),
    }
}

// POST /api/providers/activate { id }
async fn handle_providers_activate(
    State(_s): State<Arc<AppState>>,
    Json(_body): Json<Value>,
) -> Response {
    err_env(
        StatusCode::GONE,
        "E_CODEX_CONFIG_MUTATION_RETIRED",
        "Codex 旧版 activate 会改写官方凭据，已停用；请使用 /api/desktop/host 的 gateway 模式",
        None,
    )
}

// POST /api/providers/activate-official
async fn handle_providers_activate_official(State(_s): State<Arc<AppState>>) -> Response {
    err_env(
        StatusCode::GONE,
        "E_CODEX_CONFIG_MUTATION_RETIRED",
        "Codex 旧版 activate-official 会回灌 auth.json，已停用；请使用官方恢复预览与二次确认流程",
        None,
    )
}

// POST /api/providers/preview-config { id? 或临时 provider 对象 }
async fn handle_providers_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let provider = if let Some(id) = body
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|x| !x.is_empty())
    {
        match crate::providers::load(&s.providers_path)
            .providers
            .into_iter()
            .find(|p| p.id == id)
        {
            Some(p) => p,
            None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
        }
    } else {
        crate::providers::input_to_provider(crate::providers::value_to_input(&body))
    };
    if provider.agent == "codex" {
        let fingerprint =
            crate::codex_overlay::fingerprint(&s.config_path).map_err(|error| error.to_string());
        return match fingerprint {
            Ok(fingerprint) => ok_env(json!({
                "mode": "gateway",
                "config": fingerprint,
                "auth_action": "noop",
                "auth_diff": Value::Null,
                "backup_will_create": false,
                "warning": "Codex 官方凭据由 CLI/keyring 管理，预览不会读取或改写 auth.json",
            })),
            Err(error) => err_env(
                StatusCode::BAD_REQUEST,
                "E_CODEX_CONFIG_PARSE",
                &error,
                None,
            ),
        };
    }
    match crate::config::preview_provider(&s.config_path, &s.codex_home, &provider) {
        Ok(o) => ok_env(json!({
            "config_toml": o.config_toml,
            "auth_action": o.auth_action,
            "auth_diff": o.auth_diff,
            "backup_will_create": o.backup_will_create,
        })),
        Err(e) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_INTERNAL", &e, None),
    }
}

// POST /api/providers/fetch-models { id? 或 baseUrl+apiKey（新建未保存时也能拉）}
async fn handle_providers_fetch_models(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let (base_url, api_key, write_back_id): (String, String, Option<String>) = if !id.is_empty() {
        let data = crate::providers::load(&s.providers_path);
        match data.providers.iter().find(|p| p.id == id).cloned() {
            Some(mut p) => {
                // 改 Key 后按新 Key 拉取:body.apiKey(未保存的新 Key)若提供,覆盖存储 Key 用于本次探测
                // (不落盘——保存动作走 PUT /api/providers/:id;前端编辑表单改 Key 后先拉模型再保存)。
                let k = body
                    .get("apiKey")
                    .or_else(|| body.get("api_key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !k.is_empty() {
                    p.api_key = k.to_string();
                }
                (p.base_url, p.api_key, Some(id.to_string()))
            }
            None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
        }
    } else {
        let b = body
            .get("baseUrl")
            .or_else(|| body.get("base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let k = body
            .get("apiKey")
            .or_else(|| body.get("api_key"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (b, k, None)
    };
    if base_url.trim().is_empty() || api_key.trim().is_empty() {
        return err_env(
            StatusCode::BAD_REQUEST,
            "E_BAD_REQUEST",
            "需要 baseUrl + apiKey",
            None,
        );
    }
    let probed = crate::probe::probe_endpoint(&base_url, &api_key).await;
    let models: Vec<crate::providers::ModelConfig> = probed
        .iter()
        .map(|(n, ctx)| crate::providers::ModelConfig {
            name: n.clone(),
            context_window: *ctx,
            ..Default::default()
        })
        .collect();
    // reasoning levels 探测已移出同步路径(2026-08-15 真机:2xa 上游对该探测挂满 15s 超时,
    // 把拉模型拖到 25s+,用户感知"拉取用不了")。levels 为空时 catalog 用默认 5 级,
    // 真机对话已验证无影响;显式探测挪到阶段 2 preflight。写回仅更新 models,保留已存 levels。
    let levels: Vec<String> = if let Some(wid) = &write_back_id {
        crate::providers::load(&s.providers_path)
            .providers
            .iter()
            .find(|p| p.id == *wid)
            .and_then(|p| p.reasoning_levels.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let Some(wid) = write_back_id {
        // 探测失败(网络/上游异常)返回空列表时不落盘清空已存模型表,保留原值
        if !models.is_empty() {
            let mut data = crate::providers::load(&s.providers_path);
            if let Some(p) = data.providers.iter_mut().find(|p| p.id == wid) {
                p.models = models.clone();
            }
            let _ = crate::providers::store(&s.providers_path, &data);
        }
    }
    ok_env(json!({ "models": models, "reasoning_levels": levels }))
}

// POST /api/providers/fetch-balance（01-D6 stub）
async fn handle_providers_fetch_balance() -> Response {
    ok_env(json!({ "balance": Value::Null, "note": "stub" }))
}

// POST /api/providers/diagnose { id }
async fn handle_providers_diagnose(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let data = crate::providers::load(&s.providers_path);
    let provider = match data.providers.iter().find(|p| p.id == id).cloned() {
        Some(p) => p,
        None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
    };
    let result = crate::diagnose::diagnose(&provider).await;
    ok_env(serde_json::to_value(&result).unwrap_or(json!({})))
}

// ── P0 配置档案 ─────────────────────────────────────────────

async fn handle_profiles_list(
    State(s): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let agent = query.get("agent").map(String::as_str);
    let data = crate::profiles::list(&s.codex_home.join("2xapi-profiles.json"), agent);
    ok_env(json!({
        "profiles": data.profiles,
        "activeProfiles": data.active_profiles,
        "activeProfileId": data.active_profiles.get(agent.unwrap_or("codex")),
    }))
}

async fn handle_profiles_create(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let provider_id = body
        .get("providerId")
        .or_else(|| body.get("provider_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !crate::providers::load(&s.providers_path)
        .providers
        .iter()
        .any(|provider| provider.id == provider_id)
    {
        return err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_PROFILE_PROVIDER_NOT_FOUND",
            "profile 引用的供应商不存在",
            None,
        );
    }
    match crate::profiles::create(&s.codex_home.join("2xapi-profiles.json"), &body) {
        Ok(profile) => ok_env(json!({ "profile": profile })),
        Err(error) => err_env(StatusCode::UNPROCESSABLE_ENTITY, "E_PROFILE", &error, None),
    }
}

async fn handle_profiles_update(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let provider_id = body
        .get("providerId")
        .or_else(|| body.get("provider_id"))
        .and_then(Value::as_str);
    if let Some(provider_id) = provider_id {
        if !crate::providers::load(&s.providers_path)
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
        {
            return err_env(
                StatusCode::UNPROCESSABLE_ENTITY,
                "E_PROFILE_PROVIDER_NOT_FOUND",
                "profile 引用的供应商不存在",
                None,
            );
        }
    }
    match crate::profiles::update(&s.codex_home.join("2xapi-profiles.json"), &id, &body) {
        Ok(profile) => ok_env(json!({ "profile": profile })),
        Err(error) => err_env(StatusCode::UNPROCESSABLE_ENTITY, "E_PROFILE", &error, None),
    }
}

async fn handle_profiles_delete(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match crate::profiles::delete(&s.codex_home.join("2xapi-profiles.json"), &id) {
        Ok(()) => ok_env(json!({ "deleted": true })),
        Err(error) => err_env(StatusCode::NOT_FOUND, "E_PROFILE_NOT_FOUND", &error, None),
    }
}

async fn handle_profiles_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match crate::profiles::preview(&s.codex_home, &s.config_path, &s.providers_path, &body) {
        Ok(result) => ok_env(result),
        Err(error) => err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_PROFILE_PREVIEW",
            &error,
            None,
        ),
    }
}

async fn handle_profiles_apply(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").trim();
    let token = body
        .get("previewToken")
        .or_else(|| body.get("preview_token"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let confirmed = body
        .get("confirmed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match crate::profiles::apply(
        &s.codex_home,
        &s.config_path,
        &s.backup_dir,
        &s.providers_path,
        id,
        token,
        confirmed,
    ) {
        Ok(result) => ok_env(result),
        Err((status, code, message)) => err_env(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            &code,
            &message,
            None,
        ),
    }
}

// --- Backups & history ---

async fn handle_backups(State(s): State<Arc<AppState>>) -> Response {
    let entries = crate::backups::list(&s.backup_dir);
    ok_json(json!({ "backups": entries }))
}

async fn handle_history(State(s): State<Arc<AppState>>) -> Response {
    let result = crate::history::inspect(&s.codex_home);
    ok_json(result)
}

// ── 开机自启(竞品吸收 1.1-3):launchd plist 写/删 ──────────

// GET /api/version → {ok:true,data:{version}}(构建版本,与 Cargo.toml 对齐)
async fn handle_version() -> Response {
    ok_env(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

fn app_not_ready() -> Response {
    err_env(
        StatusCode::SERVICE_UNAVAILABLE,
        "E_APP_NOT_READY",
        "应用仍在启动，请稍后重试",
        None,
    )
}

async fn handle_check_update() -> Response {
    let app = match crate::updater::app_handle() {
        Some(app) => app,
        None => return app_not_ready(),
    };
    match crate::updater::check(&app).await {
        Ok(info) => ok_env(serde_json::to_value(info).unwrap()),
        Err(error) => err_env(StatusCode::BAD_GATEWAY, "E_UPDATE_CHECK", &error, None),
    }
}

async fn handle_update_install() -> Response {
    let app = match crate::updater::app_handle() {
        Some(app) => app,
        None => return app_not_ready(),
    };
    match crate::updater::start_install(app) {
        Ok(()) => ok_env(json!({ "started": true })),
        Err(error) => err_env(StatusCode::CONFLICT, "E_UPDATE_RUNNING", &error, None),
    }
}

async fn handle_update_status() -> Response {
    ok_env(serde_json::to_value(crate::updater::status()).unwrap())
}

// GET /api/autostart → { enabled }
async fn handle_autostart() -> Response {
    if !crate::autostart::supported() {
        return err_env(
            StatusCode::NOT_IMPLEMENTED,
            "E_UNSUPPORTED_PLATFORM",
            "当前平台不支持 macOS launchd 自启",
            None,
        );
    }
    ok_env(json!({
        "enabled": crate::autostart::enabled(&crate::autostart::launch_agents_dir())
    }))
}

// POST /api/autostart { enabled: bool } → { enabled }
async fn handle_autostart_set(Json(body): Json<Value>) -> Response {
    if !crate::autostart::supported() {
        return err_env(
            StatusCode::NOT_IMPLEMENTED,
            "E_UNSUPPORTED_PLATFORM",
            "当前平台不支持 macOS launchd 自启",
            None,
        );
    }
    let enable = body
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let dir = crate::autostart::launch_agents_dir();
    match crate::autostart::set(&dir, enable) {
        Ok(()) => ok_env(json!({ "enabled": enable })),
        Err(e) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_INTERNAL", &e, None),
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

fn validate_accel_endpoint(endpoint: &str) -> Result<String, &'static str> {
    let endpoint = endpoint.trim();
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| "节点地址格式无效")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("节点地址须为 http(s):// 开头");
    }
    if parsed.host_str().is_none() {
        return Err("节点地址缺少有效主机");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("节点地址不得包含用户名或密码");
    }
    Ok(endpoint.to_string())
}

/// scopeNote 纯函数(供单测):mode=official 且 active 供应商 base_url 未被任何线路命中
/// → 提示「不在官方线路范围,已直连」;命中或无 active → 空串;off/custom → 空串。
fn compute_scope_note(
    mode: &str,
    active_base_url: Option<&str>,
    lines: &[crate::acclines::AccLine],
) -> String {
    if mode != "official" {
        return String::new();
    }
    let Some(base) = active_base_url else {
        return String::new();
    };
    if crate::acclines::match_line(base, lines).is_none() {
        "该供应商不在官方线路范围,已直连".to_string()
    } else {
        String::new()
    }
}

/// 加速路由错误信封:{ok:false, error} (与前端画师契约一致,非统一信封)。
fn err_accel(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": msg }))).into_response()
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

/// Key 脱敏:前 3 + … + 尾 4(契约:usage.keyMasked);过短 Key 只留省略号。
fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n >= 8 {
        let head: String = key.chars().take(3).collect();
        let tail: String = key.chars().skip(n - 4).collect();
        format!("{head}…{tail}")
    } else {
        "…".to_string()
    }
}

/// usage 块(契约一字不差):ok:true + keyMasked/quota/percent(4 位)/degraded/issuedAt。
/// pass 永不输出(安全约定:usage 块只含 keyMasked)。
fn usage_block(api_key: &str, cred: &crate::nodecreds::NodeCred) -> Value {
    let percent = if cred.quota_total_bytes > 0 {
        (cred.quota_used_bytes as f64 / cred.quota_total_bytes as f64 * 10_000.0).round() / 10_000.0
    } else {
        0.0
    };
    json!({
        "ok": true,
        "keyMasked": mask_key(api_key),
        "quotaTotalBytes": cred.quota_total_bytes,
        "quotaUsedBytes": cred.quota_used_bytes,
        "quotaPercent": percent,
        "degradedToDirect": cred.degraded_to_direct,
        "issuedAt": cred.issued_at,
    })
}

/// 未换取成功/非 official 的兜底 usage 块。
fn usage_none() -> Value {
    json!({ "ok": false, "degradedToDirect": false })
}

/// state 的 usage 计算(纯内存,不失败):mode=official 且 active provider 的 Key
/// 在凭证表有项 → ok:true;否则 ok:false。任何缺省都不影响 state 主体。
fn usage_for_state(state: &AppState, mode: &str) -> Value {
    if mode != "official" {
        return usage_none();
    }
    let Some(p) = crate::providers::get_active_for_agent(&state.providers_path, "codex") else {
        return usage_none();
    };
    if p.api_key.trim().is_empty() {
        return usage_none();
    }
    let st = state.nodecreds.read().unwrap();
    match st.get_for_key(&p.api_key) {
        Some(c) => usage_block(&p.api_key, c),
        None => usage_none(),
    }
}

// GET /api/accel/state → {mode, customNode, lines, scopeNote, usage}
// GET /api/usage-stats → 用量仪表盘聚合(读取 usage-stats.jsonl;无数据空数组)。
async fn handle_usage_stats(State(s): State<Arc<AppState>>) -> Response {
    // 统计查询含磁盘读+全量 JSONL 解析,移出 async 热路径,避免阻塞网关请求
    let home = s.codex_home.clone();
    let summary = tokio::task::spawn_blocking(move || crate::usage_stats::summary(&home))
        .await
        .unwrap_or_else(|_| json!({ "providers": [], "err": "统计查询失败" }));
    ok_json(summary)
}

#[derive(Debug, Deserialize)]
struct UsageRangeQuery {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default = "default_usage_days")]
    days: u32,
}

#[derive(Debug, Deserialize, Default)]
struct UsageSummaryQuery {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

fn default_usage_days() -> u32 {
    30
}

async fn handle_usage_summary(
    State(s): State<Arc<AppState>>,
    Query(query): Query<UsageSummaryQuery>,
) -> Response {
    if let Some(date) = query.date.as_deref() {
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_err() {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_BAD_REQUEST",
                "date 必须是 YYYY-MM-DD",
                Some(vec!["date".into()]),
            );
        }
    }
    let home = s.codex_home.clone();
    let provider_id = query.provider_id.clone();
    let date = query.date.clone();
    let summary = tokio::task::spawn_blocking(move || {
        crate::usage_stats::usage_summary_filtered(
            &home,
            chrono::Utc::now().timestamp(),
            provider_id.as_deref(),
            date.as_deref(),
        )
    })
    .await
    .unwrap_or_else(|_| json!({}));
    ok_env(summary)
}

async fn handle_usage_history(
    State(s): State<Arc<AppState>>,
    Query(query): Query<UsageRangeQuery>,
) -> Response {
    let days = query.days.clamp(1, 366);
    let home = s.codex_home.clone();
    let provider_id = query.provider_id.clone();
    let items = tokio::task::spawn_blocking(move || {
        crate::usage_stats::usage_history_filtered(
            &home,
            chrono::Utc::now().timestamp(),
            days,
            provider_id.as_deref(),
        )
    })
    .await
    .unwrap_or_default();
    ok_env(json!({ "days": days, "items": items }))
}

async fn handle_usage_models(
    State(s): State<Arc<AppState>>,
    Query(query): Query<UsageRangeQuery>,
) -> Response {
    let days = query.days.clamp(1, 366);
    let home = s.codex_home.clone();
    let provider_id = query.provider_id.clone();
    let items = tokio::task::spawn_blocking(move || {
        crate::usage_stats::usage_models_filtered(
            &home,
            chrono::Utc::now().timestamp(),
            days,
            provider_id.as_deref(),
        )
    })
    .await
    .unwrap_or_default();
    ok_env(json!({ "days": days, "items": items }))
}

async fn handle_usage_models_history(
    State(s): State<Arc<AppState>>,
    Query(query): Query<UsageRangeQuery>,
) -> Response {
    let days = query.days.clamp(1, 366);
    let home = s.codex_home.clone();
    let provider_id = query.provider_id.clone();
    let items = tokio::task::spawn_blocking(move || {
        crate::usage_stats::usage_models_history_filtered(
            &home,
            chrono::Utc::now().timestamp(),
            days,
            provider_id.as_deref(),
        )
    })
    .await
    .unwrap_or_default();
    ok_env(json!({ "days": days, "items": items }))
}

async fn handle_usage_refresh(State(s): State<Arc<AppState>>) -> Response {
    let home = s.codex_home.clone();
    let summary = tokio::task::spawn_blocking(move || {
        crate::usage_stats::usage_summary(&home, chrono::Utc::now().timestamp())
    })
    .await
    .unwrap_or_else(|_| json!({}));
    ok_env(json!({
        "syncedAt": chrono::Utc::now().to_rfc3339(),
        "source": "local_gateway",
        "summary": summary,
    }))
}

async fn handle_usage_overlay_get(State(s): State<Arc<AppState>>) -> Response {
    match crate::usage_overlay::load_settings(&s.codex_home) {
        Ok(settings) => ok_env(serde_json::to_value(settings).unwrap_or_else(|_| json!({}))),
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SETTINGS",
            &error,
            None,
        ),
    }
}

async fn handle_usage_overlay_put(
    State(s): State<Arc<AppState>>,
    Json(settings): Json<crate::usage_overlay::UsageOverlaySettings>,
) -> Response {
    if let Err(errors) = settings.validate() {
        return err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_VALIDATION",
            &errors.join("；"),
            None,
        );
    }
    if let Err(error) = crate::usage_overlay::save_settings(&s.codex_home, &settings) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SETTINGS",
            &error,
            None,
        );
    }
    let window_result = crate::usage_overlay::with_window(|window| {
        crate::usage_overlay::apply_window_settings(window, &settings)
    });
    if let Err(error) = window_result {
        eprintln!("[overlay] 应用窗口设置失败: {error}");
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_WINDOW",
            &format!("设置已保存，但悬浮窗应用失败: {error}"),
            None,
        );
    }
    ok_env(serde_json::to_value(settings).unwrap_or_else(|_| json!({})))
}

#[derive(Debug, Deserialize)]
struct UsageOverlayAction {
    action: String,
}

async fn handle_usage_overlay_action(
    State(s): State<Arc<AppState>>,
    Json(body): Json<UsageOverlayAction>,
) -> Response {
    let mut settings = match crate::usage_overlay::load_settings(&s.codex_home) {
        Ok(value) => value,
        Err(error) => {
            return err_env(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_SETTINGS",
                &error,
                None,
            )
        }
    };
    match body.action.as_str() {
        "show" => settings.enabled = true,
        "hide" => settings.enabled = false,
        "toggle" => settings.enabled = !settings.enabled,
        "refresh" => {}
        _ => {
            return err_env(
                StatusCode::BAD_REQUEST,
                "E_ACTION",
                "不支持的悬浮窗操作",
                None,
            )
        }
    }
    if let Err(error) = crate::usage_overlay::save_settings(&s.codex_home, &settings) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SETTINGS",
            &error,
            None,
        );
    }
    if body.action != "refresh" {
        if let Err(error) = crate::usage_overlay::with_window(|window| {
            crate::usage_overlay::apply_window_settings(window, &settings)
        }) {
            return err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_WINDOW", &error, None);
        }
    }
    ok_env(json!({
        "visible": settings.enabled,
        "settings": serde_json::to_value(settings).unwrap_or_else(|_| json!({})),
    }))
}

async fn handle_accel_state(State(s): State<Arc<AppState>>) -> Response {
    let (mode, custom_node) = {
        let cfg = s.accel.lock().unwrap();
        (cfg.mode.clone(), cfg.custom_node.clone())
    };
    let lines: Vec<Value> = {
        let ls = s.health.lines.lock().unwrap();
        let table = s.health.table.lock().unwrap();
        ls.iter()
            .map(|l| {
                let h = table.get(&l.id);
                json!({
                    "id": l.id,
                    "name": l.name,
                    "endpoint": l.endpoint,
                    "scope": l.scope,
                    "priority": l.priority,
                    "enabled": l.enabled,
                    "latency": h.map(|h| h.latency_ms).unwrap_or(0),
                    "fails": h.map(|h| h.fails).unwrap_or(0),
                    // 摘除态显式可见(3 败摘除/1 成恢复;网关请求路径按它跳线)。
                    // 此处已持有 table 锁,不能调 is_available(内部会再锁 → 死锁)
                    "available": h.map(|h| !h.is_unhealthy()).unwrap_or(true),
                })
            })
            .collect()
    };
    let active_base =
        crate::providers::get_active_for_agent(&s.providers_path, "codex").map(|p| p.base_url);
    let scope_note = {
        let ls = s.health.lines.lock().unwrap();
        compute_scope_note(&mode, active_base.as_deref(), &ls)
    };
    // 星图 任务 B:顶层 usage 块(计算失败不致 state 500——纯内存查表,无 unwrap 于 Result)
    let usage = usage_for_state(&s, &mode);
    ok_json(json!({
        "mode": mode,
        "customNode": custom_node,
        "lines": lines,
        "scopeNote": scope_note,
        "usage": usage,
    }))
}

// POST /api/accel/mode {mode}
// 星图 任务 B:切到 official 时 best-effort 预签发一次每账号凭证(成功落 store;
// 失败忽略——不阻断切 mode;后续请求由网关凭证确保段兜底)。
async fn handle_accel_mode(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !matches!(mode.as_str(), "off" | "official" | "custom") {
        return err_accel(StatusCode::BAD_REQUEST, "mode 须为 off/official/custom");
    }
    {
        let mut cfg = s.accel.lock().unwrap();
        if mode == "custom" && cfg.custom_node.trim().is_empty() {
            return err_accel(StatusCode::BAD_REQUEST, "请先配置自定义加速节点");
        }
        let mut next = cfg.clone();
        next.mode = mode.clone();
        if let Err(e) = save_accel_cfg(&s.codex_home, &next) {
            eprintln!("[accel] 保存 mode 失败: {e}");
            return err_accel(StatusCode::INTERNAL_SERVER_ERROR, "加速配置保存失败");
        }
        *cfg = next;
    } // 锁在 await 前释放(async fn 的后续 await 需要非阻塞占有)
    if mode == "official" {
        if let Some(p) = crate::providers::get_active_for_agent(&s.providers_path, "codex") {
            if !p.api_key.trim().is_empty() {
                match crate::nodecreds::issue_node_cred(&issue_base(), &p.api_key).await {
                    Ok(cred) => {
                        let mut st = s.nodecreds.write().unwrap();
                        st.set_for_key(&p.api_key, cred); // 新凭证自带 degraded=false
                        let _ = crate::nodecreds::save_store(&s.codex_home, &st);
                        eprintln!("[accel] 切 official:已预签发每账号凭证");
                    }
                    Err(_) => {
                        eprintln!("[accel] 切 official:预签发失败(忽略,不阻断切 mode)");
                    }
                }
            }
        }
    }
    ok_json(json!({ "ok": true }))
}

// POST /api/accel/refresh-cred —— 手动重签每账号节点凭证(星图 任务 B 契约)。
// 200 {ok:true,usage} / 400 未配供应商 / 401 Key 无效 / 403 配额满 / 502 节点不可达。
async fn handle_accel_refresh_cred(State(s): State<Arc<AppState>>) -> Response {
    let provider = match crate::providers::get_active_for_agent(&s.providers_path, "codex") {
        Some(p) => p,
        None => return err_accel(StatusCode::BAD_REQUEST, "请先配置供应商"),
    };
    if provider.api_key.trim().is_empty() {
        return err_accel(StatusCode::BAD_REQUEST, "请先配置供应商");
    }
    match crate::nodecreds::issue_node_cred(&issue_base(), &provider.api_key).await {
        Ok(cred) => {
            // 刷新成功:落 store(新凭证 degraded_to_direct=false,天然清除降级)+ 回 usage
            let usage = {
                let mut st = s.nodecreds.write().unwrap();
                st.set_for_key(&provider.api_key, cred.clone());
                let _ = crate::nodecreds::save_store(&s.codex_home, &st);
                usage_block(&provider.api_key, &cred)
            };
            ok_json(json!({ "ok": true, "usage": usage }))
        }
        Err(crate::nodecreds::IssueErr::QuotaFull(snap)) => {
            // 快照若带配额数字 → 更新 store 该 key 的用量(前端下次 state 可见)
            if let Some(snap) = &snap {
                let mut st = s.nodecreds.write().unwrap();
                if let Some(e) = st
                    .creds
                    .get_mut(&crate::nodecreds::hash_key(&provider.api_key))
                {
                    if let Some(u) = snap.quota_used_bytes {
                        e.quota_used_bytes = u;
                    }
                    if let Some(t) = snap.quota_total_bytes {
                        e.quota_total_bytes = t;
                    }
                    let _ = crate::nodecreds::save_store(&s.codex_home, &st);
                }
            }
            err_accel(StatusCode::FORBIDDEN, "该账号本月已用满 10G")
        }
        Err(crate::nodecreds::IssueErr::KeyInvalid) => {
            err_accel(StatusCode::UNAUTHORIZED, "Key 无效或未充值")
        }
        Err(crate::nodecreds::IssueErr::Unreachable(_)) => {
            err_accel(StatusCode::BAD_GATEWAY, "加速节点暂不可达,请稍后再试")
        }
    }
}

// POST /api/accel/custom-node {endpoint}
async fn handle_accel_custom_node(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(endpoint) = body.get("endpoint").and_then(|v| v.as_str()) else {
        return err_accel(StatusCode::BAD_REQUEST, "endpoint 须为字符串");
    };
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        let mut cfg = s.accel.lock().unwrap();
        let mut next = cfg.clone();
        next.custom_node.clear();
        if next.mode == "custom" {
            next.mode = "off".into();
        }
        if let Err(e) = save_accel_cfg(&s.codex_home, &next) {
            eprintln!("[accel] 清空 custom-node 失败: {e}");
            return err_accel(StatusCode::INTERNAL_SERVER_ERROR, "加速配置保存失败");
        }
        *cfg = next;
        return ok_json(json!({ "ok": true, "mode": cfg.mode }));
    }
    let endpoint = match validate_accel_endpoint(endpoint) {
        Ok(endpoint) => endpoint,
        Err(message) => return err_accel(StatusCode::BAD_REQUEST, message),
    };
    let mut cfg = s.accel.lock().unwrap();
    let mut next = cfg.clone();
    next.custom_node = endpoint;
    if let Err(e) = save_accel_cfg(&s.codex_home, &next) {
        eprintln!("[accel] 保存 custom-node 失败: {e}");
        return err_accel(StatusCode::INTERNAL_SERVER_ERROR, "加速配置保存失败");
    }
    *cfg = next;
    ok_json(json!({ "ok": true }))
}

// POST /api/accel/test-node {endpoint}
async fn handle_accel_test_node(
    State(_s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(endpoint) = body.get("endpoint").and_then(|v| v.as_str()) else {
        return err_accel(StatusCode::BAD_REQUEST, "endpoint 须为字符串");
    };
    let endpoint = match validate_accel_endpoint(endpoint) {
        Ok(endpoint) => endpoint,
        Err(message) => return err_accel(StatusCode::BAD_REQUEST, message),
    };
    let outcome = crate::gateway::test_node_via(
        &endpoint,
        "https://api.2xa.cc.cd/models",
        None,
        std::time::Duration::from_secs(5),
    )
    .await;
    match outcome {
        crate::gateway::NodeTestOutcome::Ok { latency_ms } => {
            ok_json(json!({ "ok": true, "latencyMs": latency_ms }))
        }
        crate::gateway::NodeTestOutcome::Timeout => {
            err_accel(StatusCode::BAD_GATEWAY, "连不上:检查地址或网络")
        }
        crate::gateway::NodeTestOutcome::Auth => err_accel(StatusCode::BAD_GATEWAY, "节点凭证无效"),
        crate::gateway::NodeTestOutcome::Unavailable => {
            err_accel(StatusCode::BAD_GATEWAY, "节点不可用")
        }
    }
}

// ── 历史会话管理(阶段 3,任务书 §四)─────────────────────

// GET /api/sessions?page=&size= → Codex++ 风格分页、统计和数据库路径
async fn handle_sessions_list(
    State(s): State<Arc<AppState>>,
    query: axum::extract::Query<Value>,
) -> Response {
    let page = query
        .get("page")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    let size = query
        .get("size")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50);
    let snapshot_id = query
        .get("snapshotId")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let data = match snapshot_id {
        Some(snapshot_id) => match crate::sessions::list_sessions_page_from_snapshot(
            &s.codex_home,
            snapshot_id,
            page,
            size,
        ) {
            Ok(data) => data,
            Err(error) => return err_env(StatusCode::CONFLICT, "E_SESSION_SNAPSHOT", &error, None),
        },
        None => {
            return err_env(
                StatusCode::CONFLICT,
                "E_SESSION_SNAPSHOT",
                "请先完成会话检查，再加载列表",
                None,
            )
        }
    };
    ok_env(data)
}

async fn handle_sessions_inspect(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::sessions::inspect_sessions(&s.codex_home))
}

async fn handle_sessions_repair_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let target = body
        .get("targetProvider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    match crate::sessions::preview_repair(&s.codex_home, target) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::BAD_REQUEST,
            "E_SESSION_REPAIR_PREVIEW",
            &error,
            None,
        ),
    }
}

async fn handle_sessions_repair(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let target = body
        .get("targetProvider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if let Some(token) = body.get("previewToken").and_then(Value::as_str) {
        if let Err(error) = crate::sessions::validate_repair_preview(target, token) {
            return err_env(
                StatusCode::CONFLICT,
                "E_SESSION_REPAIR_PREVIEW",
                &error,
                None,
            );
        }
    }
    match crate::sessions::start_repair_job(
        s.codex_home.clone(),
        s.backup_dir.clone(),
        target.to_string(),
    ) {
        Ok(job_id) => ok_env(json!({"jobId": job_id})),
        Err(error) => err_env(
            if error.contains("正在运行") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            },
            "E_SESSION_REPAIR",
            &error,
            None,
        ),
    }
}

async fn handle_sessions_job(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match crate::sessions::get_repair_job_for_home(&s.codex_home, &id) {
        Some(job) => ok_env(job),
        None => err_env(
            StatusCode::NOT_FOUND,
            "E_SESSION_JOB",
            "修复任务不存在",
            None,
        ),
    }
}

async fn handle_sessions_job_cancel(Path(id): Path<String>) -> Response {
    match crate::sessions::cancel_repair_job(&id) {
        Ok(job) => ok_env(job),
        Err(error) => err_env(StatusCode::CONFLICT, "E_SESSION_JOB_CANCEL", &error, None),
    }
}

async fn handle_sessions_job_resume(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    // 应用重启后中断任务只存在于磁盘;先按 codex_home 补载进任务表,恢复才能找到检查点。
    crate::sessions::get_repair_job_for_home(&s.codex_home, &id);
    match crate::sessions::resume_repair_job(&id) {
        Ok(job_id) => ok_env(json!({"jobId": job_id, "resumedFrom": id})),
        Err(error) => err_env(StatusCode::CONFLICT, "E_SESSION_JOB_RESUME", &error, None),
    }
}

async fn handle_sessions_delete_preview(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let ids: Vec<String> = body
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    match crate::sessions::preview_delete(&s.codex_home, &ids) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::BAD_REQUEST,
            "E_SESSION_DELETE_PREVIEW",
            &error,
            None,
        ),
    }
}

async fn handle_sessions_delete(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let token = body
        .get("confirmToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    match crate::sessions::apply_delete(&s.codex_home, &s.backup_dir, token) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_DELETE", &error, None),
    }
}

async fn handle_sessions_delete_undo(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let backup = body.get("backupId").and_then(Value::as_str).unwrap_or("");
    match crate::sessions::undo_delete(&s.codex_home, &s.backup_dir, backup) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_UNDO", &error, None),
    }
}

async fn handle_sessions_restart_codex(State(s): State<Arc<AppState>>) -> Response {
    if let Err(error) = crate::sessions::auto_repair_before_launch(&s.codex_home, &s.backup_dir) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_AUTO_REPAIR",
            &error,
            None,
        );
    }
    match crate::sessions::restart_codex() {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_RESTART", &error, None),
    }
}

// POST /api/sessions/:id/resume → 在系统 Terminal 打开固定 codex resume 命令
async fn handle_sessions_resume(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(error) = crate::sessions::auto_repair_before_launch(&s.codex_home, &s.backup_dir) {
        return err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_AUTO_REPAIR",
            &error,
            None,
        );
    }
    match crate::sessions::resume_session(&s.codex_home, &id) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(StatusCode::BAD_REQUEST, "E_SESSION_RESUME", &error, None),
    }
}

// GET/POST /api/sessions/settings
async fn handle_sessions_settings(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::sessions::get_settings(&s.codex_home))
}
async fn handle_sessions_settings_set(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(value) = body.get("autoRepairBeforeLaunch").and_then(Value::as_bool) else {
        return err_env(
            StatusCode::UNPROCESSABLE_ENTITY,
            "E_VALIDATION",
            "autoRepairBeforeLaunch 必须是 boolean",
            Some(vec!["autoRepairBeforeLaunch".into()]),
        );
    };
    match crate::sessions::set_settings(&s.codex_home, value) {
        Ok(data) => ok_env(data),
        Err(error) => err_env(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_SESSION_SETTINGS",
            &error,
            None,
        ),
    }
}

async fn handle_config_snapshot(State(s): State<Arc<AppState>>) -> Response {
    match crate::config::create_snapshot(&s.config_path, &s.backup_dir) {
        Ok(entry) => ok_json(entry),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

async fn handle_config_restore(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let backup_path = body
        .get("backupPath")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if backup_path.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "缺少备份路径");
    }
    if let Err(e) = crate::config::backup_file(
        &s.config_path,
        &s.backup_dir,
        "config-restore",
        "pre-restore",
    ) {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("恢复前快照失败,已取消恢复: {e}"),
        );
    }
    match crate::config::restore_from_dir(&s.config_path, &s.backup_dir, backup_path) {
        Ok(_) => ok_json(json!({ "written": true, "restored": backup_path })),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
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

// ── 桌面版托管开关（阶段 1，任务书 §1.1）────────────────────

// GET /api/desktop/state
async fn handle_desktop_state(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::desktop::state(
        &s.config_path,
        &s.providers_path,
        &s.codex_home,
    ))
}

// POST /api/desktop/host {providerId, way}
async fn handle_desktop_host(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    desktop_host_impl(&s, &body)
}

// POST /api/desktop/unhost
async fn handle_desktop_unhost(State(s): State<Arc<AppState>>) -> Response {
    desktop_unhost_impl(&s)
}

// POST /api/desktop/recovery/preview {mode: reset-config|reset-all}
#[derive(Debug, Deserialize)]
struct DesktopRecoveryPreviewQuery {
    mode: Option<String>,
}

async fn handle_desktop_recovery_preview_get(
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

async fn handle_desktop_recovery_preview(
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
async fn handle_desktop_recovery_apply(
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
async fn handle_desktop_login_status(State(s): State<Arc<AppState>>) -> Response {
    ok_env(
        serde_json::to_value(crate::codex_security::probe_login_cached(&s.codex_home))
            .unwrap_or_else(|_| json!({"state":"unknown"})),
    )
}

// POST /api/desktop/login/start {deviceAuth?}
async fn handle_desktop_login_start(
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
async fn handle_desktop_claude_start(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    desktop_claude_start_impl(&s, &body)
}

// POST /api/desktop/claude-launch { way?, providerId? }
// 配置写入成功后校验 Claude CLI 并打开 macOS Terminal；成功响应不返回命令、环境变量或上游 Key。
async fn handle_desktop_claude_launch(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    desktop_claude_launch_impl(&s, &body)
}

async fn handle_desktop_claude_state(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::agents::claude_code::state(
        claude_home(&s.codex_home),
        &s.backup_dir,
        &s.providers_path,
    ))
}

async fn handle_desktop_claude_host(
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

async fn handle_desktop_claude_unhost(State(s): State<Arc<AppState>>) -> Response {
    agent_op_response(crate::agents::claude_code::unhost(
        claude_home(&s.codex_home),
        &s.backup_dir,
        &s.providers_path,
    ))
}

// ── 多平台 agent 注册表与泛化路由(方案 §2.1,A 阶段;具名路由保留为别名,B 阶段各平台 adapter 挂 :agent 段)──

// GET /api/desktop/agents —— 注册表元数据(前端数据驱动导航,D3 决策「A 后一次全亮」)
async fn handle_desktop_agents() -> Response {
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
fn reject_agent(agent: &str) -> Option<(StatusCode, &'static str, String)> {
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

fn claude_home(codex_home: &std::path::Path) -> &std::path::Path {
    codex_home.parent().unwrap_or(codex_home)
}

// GET /api/desktop/:agent/state —— agent=codex 与旧 /api/desktop/state 等价。
async fn handle_agent_state(
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
async fn handle_agent_host(
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
async fn handle_agent_unhost(
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
async fn handle_agent_start(
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

// ── 能力注册表 ─────────────────────────────────────────────

// GET /api/fusion-registry —— 插件和内置工具注册表
async fn handle_fusion_registry(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::registry::list_json(&s.codex_home))
}

// ── Codex 启动器（M7，直连版）──────────────────────────────

// POST /api/launcher/preflight { providerId } | { baseUrl, apiKey } —— 测试连接(阶段 2,任务书 §三)
async fn handle_launcher_preflight(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let (base_url, api_key, model_hint, wire_api): (
        String,
        String,
        String,
        crate::providers::WireApi,
    ) = {
        let id = body
            .get("providerId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !id.is_empty() {
            let data = crate::providers::load(&s.providers_path);
            match data.providers.iter().find(|p| p.id == id) {
                Some(p) => (
                    p.base_url.clone(),
                    p.api_key.clone(),
                    p.model.clone(),
                    p.wire_api,
                ),
                None => return err_env(StatusCode::NOT_FOUND, "E_NOT_FOUND", "供应商不存在", None),
            }
        } else {
            let b = body
                .get("baseUrl")
                .or_else(|| body.get("base_url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let k = body
                .get("apiKey")
                .or_else(|| body.get("api_key"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let m = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
            if b.is_empty() || k.is_empty() {
                return err_env(
                    StatusCode::BAD_REQUEST,
                    "E_BAD_REQUEST",
                    "需要 providerId,或 baseUrl+apiKey",
                    None,
                );
            }
            let wire_api = body
                .get("wireApi")
                .or_else(|| body.get("wire_api"))
                .and_then(|value| value.as_str())
                .and_then(crate::providers::WireApi::parse)
                .unwrap_or_default();
            (b.to_string(), k.to_string(), m.to_string(), wire_api)
        }
    };

    let r = crate::probe::preflight(&base_url, &api_key, &model_hint, wire_api).await;

    // 人话错误映射(任务书 §三):timeout/auth/notfound
    let human_error: Option<&str> = match r.error {
        Some("timeout") => Some("连不上:检查地址或网络"),
        Some("auth") => Some("Key 无效或未充值"),
        Some("notfound") => Some("地址不对,或该站不支持这个协议"),
        _ => None,
    };

    ok_env(json!({
        "keyOk": r.key_ok,
        "models": r.models.iter().map(|(n, c)| json!({ "name": n, "contextWindow": c })).collect::<Vec<_>>(),
        "responsesCompat": r.responses_compat,
        "chatOk": r.chat_ok,
        "anthropicOk": r.anthropic_ok,
        "geminiOk": r.gemini_ok,
        "nativeOk": r.responses_compat || r.chat_ok || r.anthropic_ok || r.gemini_ok,
        "wireApi": r.wire_api,
        "latencyMs": r.latency_ms,
        "suggest": r.suggest,
        "error": r.error,          // 机器码:timeout|auth|notfound|null
        "message": human_error,    // 人话提示(前端展示;失败时高亮具体字段)
    }))
}

// POST /api/launcher/start { providerId?, baseUrl?, apiKey?, model?, projectDir, wireApi? }
async fn handle_launcher_start(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match crate::launcher::start(&s.launcher, &body, &s.providers_path) {
        Ok(data) => ok_env(data),
        Err(msg) => err_env(StatusCode::BAD_REQUEST, "E_LAUNCHER", &msg, None),
    }
}

// POST /api/launcher/stop { sessionId }
async fn handle_launcher_stop(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let id = body.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
    match crate::launcher::stop(&s.launcher, id) {
        Ok(data) => ok_env(data),
        Err(msg) => err_env(StatusCode::BAD_REQUEST, "E_LAUNCHER", &msg, None),
    }
}

// GET /api/launcher/status
async fn handle_launcher_status(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::launcher::status(&s.launcher))
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
async fn handle_agent_eco(
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
async fn handle_agent_eco_op(
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
async fn handle_eco_presets() -> Response {
    ok_env(crate::agents::eco::presets_json())
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
        assert_eq!(v["data"]["providers"].as_array().unwrap().len(), 1);
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
}
