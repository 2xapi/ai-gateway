//! 加速线路/官方通道代理/用量仪表盘/悬浮窗设置路由。
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

use super::{
    err_env, issue_base, load_official_proxy, ok_env, ok_json, save_accel_cfg, save_official_proxy,
    AppState,
};

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
pub(crate) fn compute_scope_note(
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
pub(crate) async fn handle_usage_stats(State(s): State<Arc<AppState>>) -> Response {
    // 统计查询含磁盘读+全量 JSONL 解析,移出 async 热路径,避免阻塞网关请求
    let home = s.codex_home.clone();
    let summary = tokio::task::spawn_blocking(move || crate::usage_stats::summary(&home))
        .await
        .unwrap_or_else(|_| json!({ "providers": [], "err": "统计查询失败" }));
    ok_json(summary)
}

#[derive(Debug, Deserialize)]
pub(crate) struct UsageRangeQuery {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default = "default_usage_days")]
    days: u32,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct UsageSummaryQuery {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

fn default_usage_days() -> u32 {
    30
}

pub(crate) async fn handle_usage_summary(
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

pub(crate) async fn handle_usage_history(
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

pub(crate) async fn handle_usage_models(
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

pub(crate) async fn handle_usage_models_history(
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

pub(crate) async fn handle_usage_refresh(State(s): State<Arc<AppState>>) -> Response {
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

pub(crate) async fn handle_usage_overlay_get(State(s): State<Arc<AppState>>) -> Response {
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

pub(crate) async fn handle_usage_overlay_put(
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
pub(crate) struct UsageOverlayAction {
    action: String,
}

pub(crate) async fn handle_usage_overlay_action(
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

/// 官方通道代理读取（完整值返回：本机 token 鉴权下的本人设置页，表单回填需要）。
pub(crate) async fn handle_official_proxy(State(s): State<Arc<AppState>>) -> Response {
    ok_env(json!({ "proxyUrl": load_official_proxy(&s.codex_home) }))
}

/// 官方通道代理保存；非法 scheme 400。
pub(crate) async fn handle_official_proxy_save(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let url = body
        .get("proxyUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    match save_official_proxy(&s.codex_home, &url) {
        Ok(()) => ok_env(json!({ "saved": true })),
        Err(e) => err_env(StatusCode::BAD_REQUEST, "E_OFFICIAL_PROXY", &e, None),
    }
}

pub(crate) async fn handle_accel_state(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_accel_mode(
    State(s): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
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
pub(crate) async fn handle_accel_refresh_cred(State(s): State<Arc<AppState>>) -> Response {
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
pub(crate) async fn handle_accel_custom_node(
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
pub(crate) async fn handle_accel_test_node(
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
