//! 通用路由:health/version/更新检查/自启/2xapi 账号鉴权/backups/history/配置快照/启动器。
//! 自 server.rs 原样迁出(仅可见性调整为 pub(crate)),行为零变化。

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

use super::{err_env, err_json, ok_env, ok_json, AppState};

// --- Health & session ---

pub(crate) async fn handle_health(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_gateway_health(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_open_url(Json(body): Json<Value>) -> Response {
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

pub(crate) async fn handle_session(State(s): State<Arc<AppState>>) -> Response {
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

pub(crate) async fn handle_auth_captcha(State(_s): State<Arc<AppState>>) -> Response {
    match crate::auth::fetch_captcha_settings().await {
        Ok(settings) => ok_json(settings),
        Err(_) => ok_json(json!({ "enabled": false, "provider": null })),
    }
}

/// 打开内嵌官网登录窗口;登录态由 site_login 模块在导航信标里回收后落盘,
/// 主窗自行 reload 呈现已登录。
pub(crate) async fn handle_auth_site_login(State(_s): State<Arc<AppState>>) -> Response {
    match crate::site_login::open() {
        Ok(()) => ok_json(json!({ "opened": true })),
        Err(e) => err_env(StatusCode::INTERNAL_SERVER_ERROR, "E_WINDOW", &e, None),
    }
}

pub(crate) async fn handle_auth_login(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
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

pub(crate) async fn handle_auth_logout(State(s): State<Arc<AppState>>) -> Response {
    crate::auth::clear_session(&s.codex_home);
    ok_json(json!({ "ok": true }))
}

pub(crate) async fn handle_auth_remembered(State(s): State<Arc<AppState>>) -> Response {
    match crate::auth::load_remembered(&s.codex_home) {
        Some((email, _)) => ok_json(json!({ "remembered": true, "email": email, "password": "" })),
        None => ok_json(json!({ "remembered": false, "email": "", "password": "" })),
    }
}

pub(crate) async fn handle_auth_remember(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let email = body.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    match crate::auth::save_remembered(&s.codex_home, email, password) {
        Ok(()) => ok_json(json!({ "ok": true })),
        Err(error) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &error),
    }
}

pub(crate) async fn handle_auth_forget(State(s): State<Arc<AppState>>) -> Response {
    crate::auth::clear_remembered(&s.codex_home);
    ok_json(json!({ "ok": true }))
}

pub(crate) async fn handle_key_groups(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_auth_me(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_auth_api_keys(State(s): State<Arc<AppState>>) -> Response {
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

// --- Backups & history ---

pub(crate) async fn handle_backups(State(s): State<Arc<AppState>>) -> Response {
    let entries = crate::backups::list(&s.backup_dir);
    ok_json(json!({ "backups": entries }))
}

pub(crate) async fn handle_history(State(s): State<Arc<AppState>>) -> Response {
    let result = crate::history::inspect(&s.codex_home);
    ok_json(result)
}

// ── 开机自启(竞品吸收 1.1-3):launchd plist 写/删 ──────────

// GET /api/version → {ok:true,data:{version}}(构建版本,与 Cargo.toml 对齐)
pub(crate) async fn handle_version() -> Response {
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

pub(crate) async fn handle_check_update() -> Response {
    let app = match crate::updater::app_handle() {
        Some(app) => app,
        None => return app_not_ready(),
    };
    match crate::updater::check(&app).await {
        Ok(info) => ok_env(serde_json::to_value(info).unwrap()),
        Err(error) => err_env(StatusCode::BAD_GATEWAY, "E_UPDATE_CHECK", &error, None),
    }
}

pub(crate) async fn handle_update_install() -> Response {
    let app = match crate::updater::app_handle() {
        Some(app) => app,
        None => return app_not_ready(),
    };
    match crate::updater::start_install(app) {
        Ok(()) => ok_env(json!({ "started": true })),
        Err(error) => err_env(StatusCode::CONFLICT, "E_UPDATE_RUNNING", &error, None),
    }
}

pub(crate) async fn handle_update_status() -> Response {
    ok_env(serde_json::to_value(crate::updater::status()).unwrap())
}

// GET /api/autostart → { enabled }
pub(crate) async fn handle_autostart() -> Response {
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
pub(crate) async fn handle_autostart_set(Json(body): Json<Value>) -> Response {
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

pub(crate) async fn handle_config_snapshot(State(s): State<Arc<AppState>>) -> Response {
    match crate::config::create_snapshot(&s.config_path, &s.backup_dir) {
        Ok(entry) => ok_json(entry),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

pub(crate) async fn handle_config_restore(
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

// ── 能力注册表 ─────────────────────────────────────────────

// GET /api/fusion-registry —— 插件和内置工具注册表
pub(crate) async fn handle_fusion_registry(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::registry::list_json(&s.codex_home))
}

// ── Codex 启动器（M7，直连版）──────────────────────────────

// POST /api/launcher/preflight { providerId } | { baseUrl, apiKey } —— 测试连接(阶段 2,任务书 §三)
pub(crate) async fn handle_launcher_preflight(
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
pub(crate) async fn handle_launcher_start(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    match crate::launcher::start(&s.launcher, &body, &s.providers_path) {
        Ok(data) => ok_env(data),
        Err(msg) => err_env(StatusCode::BAD_REQUEST, "E_LAUNCHER", &msg, None),
    }
}

// POST /api/launcher/stop { sessionId }
pub(crate) async fn handle_launcher_stop(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let id = body.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
    match crate::launcher::stop(&s.launcher, id) {
        Ok(data) => ok_env(data),
        Err(msg) => err_env(StatusCode::BAD_REQUEST, "E_LAUNCHER", &msg, None),
    }
}

// GET /api/launcher/status
pub(crate) async fn handle_launcher_status(State(s): State<Arc<AppState>>) -> Response {
    ok_env(crate::launcher::status(&s.launcher))
}
