//! 本地网关（M3）。监听由 main 装配的 `127.0.0.1:8787`，处理 `/v1/*`。
//!
//! 核心行为（01-D3/D5/D7，FR-4）：
//! - **逐请求实时读 active provider** → 天然热切换（FR-4.9）：切 active 后下一个请求即走新 provider，进行中请求不受影响。
//! - **Mixed/PureApi 按上游协议注入 `provider.api_key`**：OpenAI 兼容接口使用 `Authorization: Bearer`，Anthropic 接口使用 `x-api-key`（key 来源 = Provider Store，01-D3），不透传客户端带来的凭证。
//! - per-provider 代理、超时返回 504、User-Agent、custom_headers；上游 4xx/5xx 原样透传。
//! - 本文件为 **M3a：透传 + key 注入 + 热切换**。Responses↔Chat 协议转换（FR-5）在 M3b 实现（届时按 `wire_api=chat_completions` 在 `/responses` 入口做转换）。

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderValue, Request, Response, StatusCode},
    response::{IntoResponse, Json},
};
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::acclines::{AccLine, Cred};
use crate::providers::AccessMode;
use crate::server::AppState;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// 请求体上限(50MB,防超大请求拖垮本地网关)。
const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;
/// 流式响应单 chunk 读超时:SSE 长会话不走总超时(120s 必截断),只要求每块有进展。
const STREAM_CHUNK_TIMEOUT_SECS: u64 = 60;

pub async fn proxy_responses(State(s): State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    dispatch(&s, req, "responses", "codex").await
}

/// 文生图入口(C 段,`/v1/images/generations` 双形态):codex active 供应商经统一加速链转发。
/// 成功响应只记录用量,不写入模型能力标签。
pub async fn proxy_images(State(s): State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    // 托盘「网关开/关」守卫(/v1/images/generations 不走 dispatch,入口处单独拦截)
    if !s
        .tray_gate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return err_resp(StatusCode::SERVICE_UNAVAILABLE, "网关已由托盘关闭");
    }
    let provider = match crate::providers::get_active_for_agent(&s.providers_path, "codex") {
        Some(p) => p,
        None => return err_resp(StatusCode::SERVICE_UNAVAILABLE, "请先选择供应商"),
    };
    if provider.access_mode == AccessMode::Official {
        return err_resp(StatusCode::BAD_REQUEST, "Official 模式不走网关");
    }
    // Key 池(一期):多 Key 轮询
    let provider = crate::keypool::apply(&s.keypool, provider);
    let line = accel_plan(&s, &provider.base_url, &provider.api_key);
    let line = ensure_line_cred(&s, line, &provider.base_url, &provider.api_key).await;
    let direct_client = match build_client(&provider) {
        Ok(c) => c,
        Err(e) => {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build client: {e}"),
            )
        }
    };
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let line_client = match &line {
        Some((l, _)) => match build_line_client(l, timeout) {
            Ok(c) => Some(c),
            Err(e) => {
                return err_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("build line client: {e}"),
                )
            }
        },
        None => None,
    };
    if let Some((l, pk)) = &line {
        eprintln!(
            "[GW] images accel line={} endpoint={} per_key={} (直连兜底开启)",
            l.id, l.endpoint, pk
        );
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };
    let base = provider.base_url.trim_end_matches('/');
    let target = if base.ends_with("/v1") {
        format!("{base}/images/generations")
    } else {
        format!("{base}/v1/images/generations")
    };
    let usage_started = std::time::Instant::now();
    let build_rb = |client: &reqwest::Client| -> reqwest::RequestBuilder {
        let mut request = client.post(&target).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", provider.api_key),
        );
        if let Some(ua) = provider.user_agent.as_deref().filter(|s| !s.is_empty()) {
            request = request.header(reqwest::header::USER_AGENT, ua);
        }
        if let Some(headers) = provider.custom_headers.as_ref() {
            for (key, value) in headers {
                request = request.header(key, value);
            }
        }
        if let Some(content_type) = parts.headers.get(axum::http::header::CONTENT_TYPE) {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type.clone());
        }
        if !body_bytes.is_empty() {
            request = request.body(body_bytes.clone());
        }
        request
    };
    let (upstream, send_meta) = match send_with_accel(
        &s,
        &provider.api_key,
        &line,
        &line_client,
        &direct_client,
        timeout,
        &build_rb,
    )
    .await
    {
        Ok(sent) => sent,
        Err((resp, meta)) => {
            usage_log(&s, &provider, "images", usage_started, &meta, false);
            s.keypool.mark_failure(&provider.id, &provider.api_key);
            return resp;
        }
    };
    // 成功判定+单维实证标记(200 且 data[0] 有 b64_json/url → image_out=yes)
    let status = upstream.status();
    // 纵深防御:上游返回 HTML 页面(如 base_url 缺 /v1 命中中转站 Web UI)→ 不透传,人话错误
    if status.is_success() && is_html_upstream(&upstream) {
        usage_log(&s, &provider, "images", usage_started, &send_meta, false);
        s.keypool.mark_failure(&provider.id, &provider.api_key);
        return err_resp(
            StatusCode::BAD_GATEWAY,
            "上游返回了网页而非图片接口,请检查供应商地址是否包含 /v1",
        );
    }
    let bytes = match read_body_timed(upstream, timeout).await {
        Ok(b) => b,
        Err(e) => {
            usage_log(&s, &provider, "images", usage_started, &send_meta, false);
            return err_resp(StatusCode::BAD_GATEWAY, &e);
        }
    };
    let has_image = if status.is_success() {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("data").and_then(|d| d.as_array()).map(|a| {
                    !a.is_empty()
                        && a.iter()
                            .any(|x| x.get("b64_json").is_some() || x.get("url").is_some())
                })
            })
            .unwrap_or(false)
    } else {
        false
    };
    usage_log(
        &s,
        &provider,
        "images",
        usage_started,
        &send_meta,
        status.is_success() && has_image,
    );
    usage_log_response(
        &s,
        &provider,
        "images",
        usage_started,
        &send_meta,
        status.is_success(),
        &bytes,
    );
    if status.is_success() {
        // 图片能力标签已停用:只记录请求结果,不写入能力标签;成功清除 Key 冷却
        s.keypool.mark_success(&provider.id);
    } else {
        s.keypool.mark_failure(&provider.id, &provider.api_key);
    }
    // 原样透传(状态码+body+Content-Type)
    let mut resp = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    resp = resp.header(axum::http::header::CONTENT_TYPE, "application/json");
    match resp.body(Body::from(bytes.to_vec())) {
        Ok(r) => r,
        Err(_) => err_resp(StatusCode::BAD_GATEWAY, "build response body"),
    }
}

pub async fn proxy_chat(State(s): State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "codex").await
}

pub async fn proxy_models(State(s): State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    dispatch(&s, req, "models", "codex").await
}

/// Hermes 流量转发入口(`/hermes/chat/completions` 与 `/hermes/v1/chat/completions`,server.rs 注册)。
/// hermes 条目 base_url=网关+/hermes,OpenAI SDK 自动追加 `/chat/completions` 命中此处;
/// 与 Codex 共用 dispatch(取数按 agent=hermes 过滤,加速/407 体系同享)。
pub async fn proxy_hermes_chat(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "hermes").await
}

/// Cursor 流量转发入口(`/cursor/*`;Cursor vscdb 托管 base=网关+/v1,客户端直发 chat/completions)。
/// 取 agent=cursor 的 active 供应商(与 hermes 同模式)。
pub async fn proxy_cursor_chat(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "cursor").await
}

/// OpenCode 流量转发入口(`/opencode/*`;opencode 条目 baseURL=网关+/opencode/v1,
/// npm openai-compatible 追加 /chat/completions)。取 agent=opencode 的 active 供应商。
pub async fn proxy_opencode_chat(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "opencode").await
}

/// OpenClaw 流量转发入口(`/openclaw/*`;条目 api=openai-completions,baseURL=网关+/openclaw/v1)。
pub async fn proxy_openclaw_chat(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "openclaw").await
}

/// Grok Build 流量转发入口(`/grokbuild/*`;~/.grok TOML base_url=网关+/grokbuild,
/// api_backend=responses 时 CLI 追加 /responses)。取 agent=grokbuild 的 active 供应商。
pub async fn proxy_grokbuild_responses(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "responses", "grokbuild").await
}

/// WorkBuddy 流量转发入口(`/workbuddy/*`;models.json 条目 url=完整地址直指本入口)。
pub async fn proxy_workbuddy_chat(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch(&s, req, "chat/completions", "workbuddy").await
}

/// Claude 流量转发入口(`/anthropic/v1/messages` 与 `/anthropic/messages`,server.rs 注册)。
pub async fn proxy_anthropic(State(s): State<Arc<AppState>>, req: Request<Body>) -> Response<Body> {
    dispatch_anthropic(&s, req).await
}

/// Claude Code 模型目录：返回当前 Claude active 供应商登记的全部模型。
/// Claude CLI 会在 `/model` 和启动校验时读取该接口；不能把请求转成上游 `/models`,
/// 因为上游可能是 Responses 协议且不一定提供 Anthropic 模型目录。
pub async fn proxy_anthropic_models(State(s): State<Arc<AppState>>) -> Response<Body> {
    anthropic_models(&s, None)
}

/// Claude Code 模型详情：兼容 CLI 对单个模型的探测。
pub async fn proxy_anthropic_model(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(model_id): axum::extract::Path<String>,
) -> Response<Body> {
    anthropic_models(&s, Some(model_id.as_str()))
}

fn anthropic_models(state: &AppState, requested_id: Option<&str>) -> Response<Body> {
    let Some(provider) = crate::providers::get_provider_for_agent(&state.providers_path, "claude")
    else {
        return err_resp(StatusCode::SERVICE_UNAVAILABLE, "请先选择 Claude 供应商");
    };
    if provider.access_mode == AccessMode::Official {
        return err_resp(StatusCode::BAD_REQUEST, "Official 模式不走网关");
    }

    let mut names = provider
        .models
        .iter()
        .filter_map(|model| {
            let name = model.name.trim();
            (!name.is_empty()).then(|| (name.to_string(), model.display_name.clone()))
        })
        .collect::<Vec<_>>();
    if names.is_empty() && !provider.model.trim().is_empty() {
        names.push((provider.model.trim().to_string(), None));
    }
    names.dedup_by(|left, right| left.0 == right.0);

    let created_at = chrono::Utc::now().to_rfc3339();
    let entries = names
        .iter()
        .map(|(id, display_name)| {
            json!({
                "type": "model",
                "id": id,
                "display_name": display_name.as_deref().unwrap_or(id),
                "created_at": created_at,
            })
        })
        .collect::<Vec<_>>();

    if let Some(model_id) = requested_id {
        let Some(model) = entries.iter().find(|model| model["id"] == model_id) else {
            return err_resp(
                StatusCode::NOT_FOUND,
                "所选模型不在当前 Claude 供应商目录中",
            );
        };
        return Json(model.clone()).into_response();
    }

    let first_id = entries
        .first()
        .and_then(|model| model["id"].as_str())
        .map(ToOwned::to_owned);
    let last_id = entries
        .last()
        .and_then(|model| model["id"].as_str())
        .map(ToOwned::to_owned);
    Json(json!({
        "data": entries,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    }))
    .into_response()
}

/// Claude 转发(Claude 接入批次):与 Codex 路径(dispatch)隔离——
/// - 取 agent=claude 的 active 供应商(规则见 providers::get_provider_for_agent,global active 是 codex 时取 claude 首个);
/// - 注入该供应商 api_key 作 `x-api-key: <key>` 与 `anthropic-version`(Key 只走上游,不进日志),透传到供应商 base_url 的 `/v1/messages`;
/// - base_url 已以 `/v1` 结尾(挂载在根) → 拼 `/messages`;否则拼 `/v1/messages`(中转站两形态均可,CTO 实测 2xa.cc.cd 均 200);
/// - 透传原样 body,**不做** Responses/Chat 转换(中转站原生 Anthropic 兼容);
/// - R1 加速接线:与 Codex 同一体系——accel_plan 命中 → 首选 build_line_client;连接层失败且
///   未写首字节 → 直连重试一次;407 → per-Key 重签判别/降级直连/人话化 502(共用 send_with_accel)。
async fn dispatch_anthropic(state: &AppState, req: Request<Body>) -> Response<Body> {
    dispatch_anthropic_for(state, req, "claude").await
}

/// Claude Desktop 流量入口(`/claude-desktop/*`;3p profile baseUrl=网关+/claude-desktop,
/// app 追加 /v1/messages)。per-agent 取供应商,与 Claude Code 的 /anthropic/* 不串台。
pub async fn proxy_claude_desktop_messages(
    State(s): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch_anthropic_for(&s, req, "claude-desktop").await
}

/// Anthropic 协议转发(agent 参数化:Claude Code 与 Claude Desktop 各自取各自 active)。
async fn dispatch_anthropic_for(
    state: &AppState,
    req: Request<Body>,
    agent: &str,
) -> Response<Body> {
    // 托盘「网关开/关」守卫:关闭时全部代理入口统一 503 人话(同 dispatch/proxy_images)
    if !state
        .tray_gate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return err_resp(StatusCode::SERVICE_UNAVAILABLE, "网关已由托盘关闭");
    }
    let provider = match crate::providers::get_provider_for_agent(&state.providers_path, agent) {
        // Key 资源池(A 线一期):见 dispatch 注释
        Some(p) => crate::keypool::apply(&state.keypool, p),
        None => return err_resp(StatusCode::SERVICE_UNAVAILABLE, "请先选择 Claude 供应商"),
    };
    // Official 不应经网关（01-D1）；防御性拒绝
    if provider.access_mode == AccessMode::Official {
        return err_resp(StatusCode::BAD_REQUEST, "Official 模式不走网关");
    }

    // ── R1 加速装配(与 dispatch 同源):命中线 → 线 client 首选,直连兜底 ──
    let line = accel_plan(state, &provider.base_url, &provider.api_key);
    // per-Key 凭证确保段(2026-08-17 后端开发部补接,与 codex/gemini 同体系):
    // 凭证缺失/超 12h 同步签发;已降级直接直连
    let line = ensure_line_cred(state, line, &provider.base_url, &provider.api_key).await;
    let direct_client = match build_client(&provider) {
        Ok(c) => c,
        Err(e) => {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build client: {e}"),
            )
        }
    };
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let line_client = match &line {
        Some((l, _)) => match build_line_client(l, timeout) {
            Ok(c) => Some(c),
            Err(e) => {
                return err_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("build line client: {e}"),
                )
            }
        },
        None => None,
    };
    if let Some((l, pk)) = &line {
        eprintln!(
            "[GW] anthropic accel line={} endpoint={} per_key={} (直连兜底开启)",
            l.id, l.endpoint, pk
        );
    }

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };
    let body_bytes = rewrite_anthropic_request_model(agent, &provider, body_bytes);
    let request_stream = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|value| value.get("stream").and_then(|value| value.as_bool()))
        .unwrap_or(false);

    let base = provider.base_url.trim_end_matches('/');
    let target = if base.ends_with("/v1") {
        format!("{}/messages", base)
    } else {
        format!("{}/v1/messages", base)
    };
    let _usage_started = std::time::Instant::now();

    // 01-D3：注入 provider.api_key（覆盖任何来源的凭证）；抽为闭包以支持换线重试(同 dispatch)
    let build_rb = |client: &reqwest::Client| -> reqwest::RequestBuilder {
        let mut rb = client
            .request(method.clone(), target.clone())
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01");
        if let Some(ua) = provider.user_agent.as_deref().filter(|s| !s.is_empty()) {
            rb = rb.header(reqwest::header::USER_AGENT, ua);
        }
        if let Some(hs) = provider.custom_headers.as_ref() {
            for (k, v) in hs {
                rb = rb.header(k, v);
            }
        }
        if let Some(ct) = parts.headers.get(axum::http::header::CONTENT_TYPE) {
            rb = rb.header(reqwest::header::CONTENT_TYPE, ct.clone());
        }
        if !body_bytes.is_empty() {
            rb = rb.body(body_bytes.clone());
        }
        rb
    };

    let (upstream, send_meta) = match send_with_accel(
        state,
        &provider.api_key,
        &line,
        &line_client,
        &direct_client,
        timeout,
        &build_rb,
    )
    .await
    {
        Ok(sent) => sent,
        Err((resp, meta)) => {
            usage_log(state, &provider, "anthropic", _usage_started, &meta, false);
            return resp;
        }
    };

    // 纵深防御:上游返回 HTML 页面(如 base_url 缺 /v1 命中中转站 Web UI)→ 不透传,人话错误
    if is_html_upstream(&upstream) {
        usage_log(
            state,
            &provider,
            "anthropic",
            _usage_started,
            &send_meta,
            false,
        );
        return err_resp(StatusCode::BAD_GATEWAY, HTML_UPSTREAM_ERR);
    }

    // 用量台账(仪表盘后端):落一行(尽力而为,不阻塞)
    usage_log(
        state,
        &provider,
        "anthropic",
        _usage_started,
        &send_meta,
        upstream.status().is_success(),
    );

    // 原样透传（FR-4.11）：上游状态码 + body 流式回传，不做协议转换
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = Response::builder().status(status);
    if let Some(ct) = upstream.headers().get(reqwest::header::CONTENT_TYPE) {
        if let Ok(hv) = HeaderValue::from_bytes(ct.as_bytes()) {
            resp = resp.header(axum::http::header::CONTENT_TYPE, hv);
        }
    }
    if !request_stream {
        let bytes = match read_body_timed(upstream, timeout).await {
            Ok(bytes) => bytes,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
        };
        usage_log_response(
            state,
            &provider,
            "anthropic",
            _usage_started,
            &send_meta,
            status.is_success(),
            &bytes,
        );
        return resp
            .body(Body::from(bytes))
            .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"));
    }
    let body = stream_body_with_usage(
        upstream,
        state,
        &provider,
        "anthropic",
        _usage_started,
        status.is_success(),
    );
    resp.body(body)
        .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"))
}

fn rewrite_anthropic_request_model(
    agent: &str,
    provider: &crate::providers::Provider,
    body_bytes: Bytes,
) -> Bytes {
    if agent != "claude-desktop" {
        return body_bytes;
    }

    let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(&body_bytes) else {
        return body_bytes;
    };
    let Some(requested_model) = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
    else {
        return body_bytes;
    };
    let Some(upstream_model) =
        crate::agents::claude_desktop::map_request_model(provider, &requested_model)
    else {
        return body_bytes;
    };
    if upstream_model == requested_model {
        return body_bytes;
    }
    body["model"] = json!(upstream_model);
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .unwrap_or(body_bytes)
}

/// Gemini 流量转发入口(`/v1beta/models/{model}:{action}`,server.rs 注册)。
/// 路径段 `:model_action` 捕获整段(如 `gemini-2.5-flash:generateContent`,冒号在段内无特殊含义)。
pub async fn proxy_gemini(
    State(s): State<Arc<AppState>>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    req: Request<Body>,
) -> Response<Body> {
    dispatch_gemini(&s, &model_action, req).await
}

/// Gemini 错误响应:Gemini 标准形态 `{"error":{code,message,status}}`(google-genai 客户端解析 friendly)。
fn gemini_err(status: StatusCode, status_str: &str, msg: &str) -> Response<Body> {
    let body = serde_json::json!({ "error": { "code": status.as_u16(), "message": msg, "status": status_str } });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build gemini error body"))
}

/// Gemini 转发(多平台阶段 C 第一段)。两条路径按 provider.wire_api 分流:
/// - `chat_completions` → 转换:Gemini 请求 → Chat(gateway_gemini_conv),响应/流式转回;
/// - `gemini` → 透传:上游原生 generateContent(2xa 实测支持),原样转发,Key 注入 `x-goog-api-key`;
/// - 其他协议 → 400 人话。
///
/// agent=gemini 取供应商(规则同 dispatch_anthropic 的 claude);加速体系与 /anthropic 同款(R1 未接 per-Key 凭证确保段)。
async fn dispatch_gemini(
    state: &AppState,
    model_action: &str,
    req: Request<Body>,
) -> Response<Body> {
    // 托盘「网关开/关」守卫:关闭时全部代理入口统一 503 人话(同 dispatch/proxy_images)
    if !state
        .tray_gate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return gemini_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            "网关已由托盘关闭",
        );
    }
    let (model, action) = match model_action.rsplit_once(':') {
        Some(x) => x,
        None => {
            return gemini_err(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "路径应为 /v1beta/models/{model}:generateContent 或 :streamGenerateContent",
            )
        }
    };
    let stream = match action {
        "generateContent" => false,
        "streamGenerateContent" => {
            // gemini CLI 流式固定用 ?alt=sse;alt 缺失/其他值不支持(JSON 数组分块)
            let alt_ok = req
                .uri()
                .query()
                .map(|q| q.split('&').any(|kv| kv.trim() == "alt=sse"))
                .unwrap_or(false);
            if !alt_ok {
                return gemini_err(
                    StatusCode::BAD_REQUEST,
                    "INVALID_ARGUMENT",
                    "streamGenerateContent 仅支持 ?alt=sse 流式格式",
                );
            }
            true
        }
        other => {
            return gemini_err(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                &format!(
                    "暂不支持 Gemini 操作 {other}(首版仅 generateContent / streamGenerateContent)"
                ),
            );
        }
    };

    let provider = match crate::providers::get_provider_for_agent(&state.providers_path, "gemini") {
        // Key 资源池(A 线一期):见 dispatch 注释
        Some(p) => crate::keypool::apply(&state.keypool, p),
        None => {
            return gemini_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "UNAVAILABLE",
                "请先选择 Gemini 供应商",
            )
        }
    };
    if provider.access_mode == AccessMode::Official {
        return gemini_err(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "Official 模式不走网关",
        );
    }

    // ── 加速装配(与 dispatch 同源):per-Key 凭证覆盖/确保(缺失或超 12h → 同步签发)+407 判别/降级 ──
    let line = accel_plan(state, &provider.base_url, &provider.api_key);
    let line = ensure_line_cred(state, line, &provider.base_url, &provider.api_key).await;
    let direct_client = match build_client(&provider) {
        Ok(c) => c,
        Err(e) => {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build client: {e}"),
            )
        }
    };
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let line_client = match &line {
        Some((l, _)) => match build_line_client(l, timeout) {
            Ok(c) => Some(c),
            Err(e) => {
                return err_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("build line client: {e}"),
                )
            }
        },
        None => None,
    };
    if let Some((l, pk)) = &line {
        eprintln!(
            "[GW] gemini accel line={} endpoint={} per_key={} (直连兜底开启)",
            l.id, l.endpoint, pk
        );
    }

    let (_parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };

    let usage_started = std::time::Instant::now();
    match provider.wire_api {
        // ── 透传:上游原生 generateContent(2xa 实测路由存在,Key 头 x-goog-api-key 与 Bearer 均可)──
        crate::providers::WireApi::Gemini => {
            let base = provider.base_url.trim_end_matches('/');
            let suffix = if base.ends_with("/v1beta") {
                format!("/models/{model}:{action}")
            } else {
                format!("/v1beta/models/{model}:{action}")
            };
            let target = if stream {
                format!("{base}{suffix}?alt=sse")
            } else {
                format!("{base}{suffix}")
            };
            eprintln!(
                "[GW] gemini passthrough model={model} action={action} body={}B",
                body_bytes.len()
            );

            let build_rb = |client: &reqwest::Client| -> reqwest::RequestBuilder {
                let mut rb = client
                    .post(&target)
                    .header("x-goog-api-key", &provider.api_key)
                    .header(reqwest::header::CONTENT_TYPE, "application/json");
                if let Some(ua) = provider.user_agent.as_deref().filter(|s| !s.is_empty()) {
                    rb = rb.header(reqwest::header::USER_AGENT, ua);
                }
                if let Some(hs) = provider.custom_headers.as_ref() {
                    for (k, v) in hs {
                        rb = rb.header(k, v);
                    }
                }
                if !body_bytes.is_empty() {
                    rb = rb.body(body_bytes.clone());
                }
                rb
            };
            let (upstream, send_meta) = match send_with_accel(
                state,
                &provider.api_key,
                &line,
                &line_client,
                &direct_client,
                timeout,
                &build_rb,
            )
            .await
            {
                Ok(sent) => sent,
                Err((resp, meta)) => {
                    usage_log(state, &provider, "gemini", usage_started, &meta, false);
                    return resp;
                }
            };
            // 纵深防御:上游返回 HTML 页面(如 base_url 缺 /v1 命中中转站 Web UI)→ 不透传,人话错误
            if is_html_upstream(&upstream) {
                usage_log(state, &provider, "gemini", usage_started, &send_meta, false);
                return gemini_err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", HTML_UPSTREAM_ERR);
            }
            // 用量台账(仪表盘后端)
            usage_log(
                state,
                &provider,
                "gemini",
                usage_started,
                &send_meta,
                upstream.status().is_success(),
            );
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp = Response::builder().status(status);
            if let Some(ct) = upstream.headers().get(reqwest::header::CONTENT_TYPE) {
                if let Ok(hv) = HeaderValue::from_bytes(ct.as_bytes()) {
                    resp = resp.header(axum::http::header::CONTENT_TYPE, hv);
                }
            }
            if !stream {
                let bytes = match read_body_timed(upstream, timeout).await {
                    Ok(bytes) => bytes,
                    Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
                };
                usage_log_response(
                    state,
                    &provider,
                    "gemini",
                    usage_started,
                    &send_meta,
                    status.is_success(),
                    &bytes,
                );
                return resp
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"));
            }
            let body = stream_body_with_usage(
                upstream,
                state,
                &provider,
                "gemini",
                usage_started,
                status.is_success(),
            );
            resp.body(body)
                .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"))
        }
        // ── 转换:Gemini → ChatCompletions(M3b 同思路)──
        crate::providers::WireApi::ChatCompletions => {
            // 模型映射(2026-08-16 实测定案):gemini CLI 的 ModelRouter 恒发 gemini 系名
            // (GEMINI_MODEL/-m/settings 均拦不住),Chat 上游 Key 组通常没有 → 重写为供应商默认模型;
            // 供应商未配模型则原名透传。透传分支(wire=gemini)不重写:原生上游模型名原样。
            let upstream_model = if provider.model.is_empty() {
                model
            } else {
                &provider.model
            };
            let conv = match crate::gateway_gemini_conv::gemini_to_chat_request(
                upstream_model,
                stream,
                &body_bytes,
            ) {
                Ok(c) => c,
                Err(e) => {
                    return gemini_err(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", e.message())
                }
            };
            eprintln!("[GW] gemini conv→chat model={model}→{upstream_model} action={action} stream={stream} body={}B", conv.body.len());

            // /v1 双形态(与 dispatch_anthropic 同规则):裸域拼 /v1/chat/completions(OpenAI 惯例,
            // 2xapi.cc.cd 实测仅 /v1 形态);base 已带 /v1 则直接拼(DeepSeek 等挂载根的站两形态均可)
            let base = provider.base_url.trim_end_matches('/');
            let target = if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            let build_rb = |client: &reqwest::Client| -> reqwest::RequestBuilder {
                let mut rb = client
                    .post(&target)
                    .header(
                        reqwest::header::AUTHORIZATION,
                        format!("Bearer {}", provider.api_key),
                    )
                    .header(reqwest::header::CONTENT_TYPE, "application/json");
                if let Some(ua) = provider.user_agent.as_deref().filter(|s| !s.is_empty()) {
                    rb = rb.header(reqwest::header::USER_AGENT, ua);
                }
                if let Some(hs) = provider.custom_headers.as_ref() {
                    for (k, v) in hs {
                        rb = rb.header(k, v);
                    }
                }
                if !conv.body.is_empty() {
                    rb = rb.body(conv.body.clone());
                }
                rb
            };
            let (upstream, send_meta) = match send_with_accel(
                state,
                &provider.api_key,
                &line,
                &line_client,
                &direct_client,
                timeout,
                &build_rb,
            )
            .await
            {
                Ok(sent) => sent,
                Err((resp, meta)) => {
                    usage_log(state, &provider, "gemini", usage_started, &meta, false);
                    return resp;
                }
            };

            // 纵深防御:上游返回 HTML 页面(如 base_url 缺 /v1 命中中转站 Web UI)→ 不透传,人话错误
            if is_html_upstream(&upstream) {
                usage_log(state, &provider, "gemini", usage_started, &send_meta, false);
                return gemini_err(StatusCode::BAD_GATEWAY, "UNAVAILABLE", HTML_UPSTREAM_ERR);
            }
            usage_log(
                state,
                &provider,
                "gemini",
                usage_started,
                &send_meta,
                upstream.status().is_success(),
            );

            // 上游非成功:包装为 Gemini 错误形态(状态码透传)
            if !upstream.status().is_success() {
                let st = upstream.status().as_u16();
                let up_bytes = match read_body_timed(upstream, timeout).await {
                    Ok(b) => b,
                    Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
                };
                let wrapped = crate::gateway_gemini_conv::chat_error_to_gemini(st, &up_bytes);
                let status = StatusCode::from_u16(st).unwrap_or(StatusCode::BAD_GATEWAY);
                return Response::builder()
                    .status(status)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(wrapped))
                    .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build err body"));
            }

            if conv.stream {
                // Chat SSE → Gemini SSE 逐块转换(不缓冲,M3b 增量思路)
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(16);
                let up_stream = upstream.bytes_stream();
                let usage_home = state.codex_home.clone();
                let usage_provider = provider.clone();
                let usage_started_copy = usage_started;
                tokio::spawn(async move {
                    let mut conv_state = crate::gateway_gemini_conv::GeminiSseConvState::new();
                    let mut s = up_stream;
                    let mut stream_ok = true;
                    'stream: while let Some(chunk) = next_stream_chunk(&mut s).await {
                        match chunk {
                            Ok(bytes) => {
                                for out in conv_state.feed(&bytes) {
                                    if tx.send(Ok(out.into_bytes())).await.is_err() {
                                        stream_ok = false;
                                        break 'stream;
                                    }
                                }
                            }
                            Err(e) => {
                                stream_ok = false;
                                let _ = tx.send(Err(std::io::Error::other(e))).await;
                                break;
                            }
                        }
                    }
                    if stream_ok {
                        for out in conv_state.finish() {
                            if tx.send(Ok(out.into_bytes())).await.is_err() {
                                stream_ok = false;
                                break;
                            }
                        }
                    }
                    flush_stream_usage(
                        &usage_home,
                        &usage_provider,
                        "chat_completions",
                        usage_started_copy,
                        stream_ok,
                        conv_state.usage_snapshot(),
                        conv_state.model_snapshot(),
                        conv_state.request_id_snapshot(),
                    );
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(
                        tokio_stream::wrappers::ReceiverStream::new(rx),
                    ))
                    .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build stream body"))
            } else {
                let up_bytes = match read_body_timed(upstream, timeout).await {
                    Ok(b) => b,
                    Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
                };
                usage_log_response(
                    state,
                    &provider,
                    "gemini",
                    usage_started,
                    &send_meta,
                    true,
                    &up_bytes,
                );
                let converted = match crate::gateway_gemini_conv::chat_json_to_gemini_json(
                    upstream_model,
                    &up_bytes,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        return err_resp(StatusCode::BAD_GATEWAY, &format!("gemini conv: {e}"))
                    }
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(converted))
                    .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build conv body"))
            }
        }
        other => {
            let name = match other {
                crate::providers::WireApi::Responses => "responses",
                crate::providers::WireApi::Anthropic => "anthropic",
                _ => "unknown",
            };
            gemini_err(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                &format!("Gemini 入口暂不支持协议 {name} 的供应商:请把该供应商协议切为 ChatCompletions(网关自动转换)或 Gemini(原生透传)"),
            )
        }
    }
}

/// 官方通道超时：官方后端长思考场景宽限。
const OFFICIAL_TIMEOUT_SECS: u64 = 600;

/// base64url 解码（JWT payload 用；避免为单点引入依赖）。
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &ch in input.as_bytes() {
        if ch == b'=' {
            continue;
        }
        let val = TABLE.iter().position(|&t| t == ch)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// 从官方 Bearer JWT 解出 chatgpt_account_id（客户端未带 header 时的兜底）。
fn jwt_chatgpt_account_id(bearer: &str) -> Option<String> {
    let token = bearer.strip_prefix("Bearer ")?;
    let payload_b64 = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload_b64)?;
    let v: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    v.get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(String::from)
}

/// 官方通道专用 client：仅应用「官方通道代理」，与供应商代理/加速线路互不干扰。
/// `socks5://` 统一升级为 `socks5h://`（远端解析）：官方域名在本地 DNS 污染环境下
/// 本地解析会连到假 IP（真机实证 socks5=连接失败、socks5h=通）。
fn build_official_client(state: &AppState) -> Result<reqwest::Client, String> {
    let mut builder =
        reqwest::Client::builder().timeout(Duration::from_secs(OFFICIAL_TIMEOUT_SECS));
    let proxy_url = crate::server::load_official_proxy(&state.codex_home);
    if !proxy_url.is_empty() {
        let normalized = if let Some(rest) = proxy_url.strip_prefix("socks5://") {
            format!("socks5h://{rest}")
        } else {
            proxy_url.clone()
        };
        let proxy =
            reqwest::Proxy::all(&normalized).map_err(|e| format!("官方通道代理无效: {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("官方通道 client 构建失败: {e}"))
}

/// official-passthrough-gateway：把 CLI 的官方 Bearer 透传官方后端（SSE 流式回传）。
/// 网关不保存、不改写凭据；官方失败原样回传错误，不回退第三方。
async fn passthrough_official(state: &AppState, req: Request<Body>) -> Response<Body> {
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };
    let bearer = match parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if v.starts_with("Bearer ") => v.to_string(),
        _ => {
            return err_resp(
                StatusCode::UNAUTHORIZED,
                "官方通道需要 Codex 官方登录：请先完成 codex login",
            )
        }
    };
    let account_id = parts
        .headers
        .get("chatgpt-account-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| jwt_chatgpt_account_id(&bearer))
        .unwrap_or_default();
    let client = match build_official_client(state) {
        Ok(c) => c,
        Err(e) => return err_resp(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let started = std::time::Instant::now();
    let upstream = client
        .post("https://chatgpt.com/backend-api/codex/responses")
        .header(AUTHORIZATION, &bearer)
        .header("chatgpt-account-id", &account_id)
        .header("originator", "codex_cli_rs")
        .header(USER_AGENT, "codex_cli_rs")
        .header(CONTENT_TYPE, "application/json")
        .body(body_bytes.to_vec())
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[GW] official passthrough send fail: {e}");
            return err_resp(
                StatusCode::BAD_GATEWAY,
                &format!("官方通道请求失败（检查网络或配置官方通道代理）: {e}"),
            );
        }
    };
    let status = upstream.status();
    eprintln!(
        "[GW] official passthrough ← {} {}ms",
        status,
        started.elapsed().as_millis()
    );
    if !status.is_success() {
        let msg = upstream.text().await.unwrap_or_default();
        let brief: String = msg.chars().take(400).collect();
        let mapped = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return err_resp(mapped, &format!("官方通道上游 {status}: {brief}"));
    }
    let stream = upstream.bytes_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build official stream body"))
}

async fn dispatch(
    state: &AppState,
    req: Request<Body>,
    suffix: &str,
    agent: &str,
) -> Response<Body> {
    // 托盘「网关开/关」守卫:关闭时全部代理入口统一 503 人话(含 /v1/* 及 agent 通路)
    if !state
        .tray_gate_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return err_resp(StatusCode::SERVICE_UNAVAILABLE, "网关已由托盘关闭");
    }
    // FR-4.9 热切换：每次都重新读 active
    // 503 文案用注册表显示名(opencode/openclaw 等此前误报「Codex 供应商」,openclaw 真机验收发现)
    let agent_label = crate::agents::find(agent)
        .map(|m| m.name)
        .unwrap_or("Codex");
    let provider = match crate::providers::get_provider_for_agent(&state.providers_path, agent) {
        Some(p) => p,
        None => {
            return err_resp(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("请先选择 {agent_label} 供应商"),
            )
        }
    };
    // Key 资源池(A 线一期):多 Key 轮询选一替换 api_key;单 Key 原样返回(行为零变化)
    let provider = crate::keypool::apply(&state.keypool, provider);
    // official-passthrough-gateway：Codex 激活官方 ChatGPT 时，responses 请求透传官方后端
    //（官方账号功能在托管下保留）；其他 agent/通路维持 01-D1 防御性拒绝。
    if provider.access_mode == AccessMode::Official {
        if agent == "codex" && suffix == "responses" {
            return passthrough_official(state, req).await;
        }
        return err_resp(StatusCode::BAD_REQUEST, "Official 模式不走网关");
    }
    // hermes 通路(OpenAI Chat 形态)暂不支持 Responses 型上游(需 chat→responses 转换,未做):
    // 明确报错不静默(人话错误映射原则),提示换 Chat 兼容供应商
    if agent == "hermes" && provider.wire_api == crate::providers::WireApi::Responses {
        return err_resp(
            StatusCode::BAD_REQUEST,
            "该供应商为 Responses 协议,Hermes 通路暂不支持,请换 Chat 兼容协议的供应商",
        );
    }

    // ── 阶段 4 加速装配:决定走哪条线路,并备好直连兜底 ──
    // 星图 任务 B1/B2:per-Key 凭证覆盖/降级 + 凭证确保(缺失或超 12h → 同步签发)
    let line = accel_plan(state, &provider.base_url, &provider.api_key);
    let line = ensure_line_cred(state, line, &provider.base_url, &provider.api_key).await;
    let direct_client = match build_client(&provider) {
        Ok(c) => c,
        Err(e) => {
            return err_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("build client: {e}"),
            )
        }
    };
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let line_client = match &line {
        Some((l, _)) => match build_line_client(l, timeout) {
            Ok(c) => Some(c),
            Err(e) => {
                return err_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("build line client: {e}"),
                )
            }
        },
        None => None,
    };
    if let Some((l, pk)) = &line {
        eprintln!(
            "[GW] accel line={} endpoint={} per_key={} (直连兜底开启)",
            l.id, l.endpoint, pk
        );
    }

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };

    let _usage_started = std::time::Instant::now();

    // ★ 请求日志(排查用)
    let req_model = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_default();
    let req_stream = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| {
            v.get("stream")
                .and_then(|s| s.as_bool())
                .unwrap_or(false)
                .then_some(true)
        })
        .unwrap_or(false);
    eprintln!(
        "[GW] /{} | provider={} mode={:?} wire={:?} model={} stream={} body={}B",
        suffix,
        provider.id.get(..8).unwrap_or(&provider.id),
        provider.access_mode,
        provider.wire_api,
        req_model,
        req_stream,
        body_bytes.len()
    );

    // FR-5：wire_api=chat_completions 时，/responses 入口做 Responses→Chat 转换
    let (target_suffix, send_body, conv_stream): (String, Vec<u8>, Option<bool>) =
        if provider.wire_api == crate::providers::WireApi::ChatCompletions && suffix == "responses"
        {
            let conv = match crate::gateway_conv::responses_to_chat_request(&body_bytes) {
                Ok(c) => c,
                Err(e) => return err_resp(StatusCode::BAD_REQUEST, &format!("协议转换失败: {e}")),
            };
            ("chat/completions".to_string(), conv.body, Some(conv.stream))
        } else {
            (suffix.to_string(), body_bytes.to_vec(), None)
        };

    // chat 上游路径补 /v1(真机实证 2026-08-16:2xa.cc.cd 的 /chat/completions 404,/v1/chat/completions 通;
    // DeepSeek 等根路径站两写皆通——带 /v1 后缀续接、不带补齐,同 dispatch_anthropic 规则)。
    // responses 不动:2xa 的 /responses 根路径在役(codex 主链),改了会破坏现有流量。
    let base = provider.base_url.trim_end_matches('/');
    let url = if target_suffix == "chat/completions" && !base.ends_with("/v1") {
        format!("{base}/v1/{target_suffix}")
    } else {
        format!("{base}/{target_suffix}")
    };

    // 01-D3：注入 provider.api_key（覆盖任何来源的凭证）；请求构建抽为闭包以支持换线重试
    let build_rb = |client: &reqwest::Client| -> reqwest::RequestBuilder {
        let mut rb = client.request(method.clone(), url.clone()).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", provider.api_key),
        );
        if let Some(ua) = provider.user_agent.as_deref().filter(|s| !s.is_empty()) {
            rb = rb.header(reqwest::header::USER_AGENT, ua);
        }
        if let Some(hs) = provider.custom_headers.as_ref() {
            for (k, v) in hs {
                rb = rb.header(k, v);
            }
        }
        if let Some(ct) = parts.headers.get(axum::http::header::CONTENT_TYPE) {
            rb = rb.header(reqwest::header::CONTENT_TYPE, ct.clone());
        }
        if !send_body.is_empty() {
            rb = rb.body(send_body.clone());
        }
        rb
    };

    // ── 换线重试:首发带 line 的 client;连接层失败且 line 存在 → 用直连 client 重试一次。
    // send() 返回 Ok 前未向客户端写任何字节,故「已开始写响应(中途断流)」绝不重试是天然成立的。
    // 407 判别/换线重试整段抽为 send_with_accel(R1 /anthropic 接线共用,行为与原内联等价)。
    let (upstream, send_meta) = match send_with_accel(
        state,
        &provider.api_key,
        &line,
        &line_client,
        &direct_client,
        timeout,
        &build_rb,
    )
    .await
    {
        Ok(sent) => sent,
        Err((resp, meta)) => {
            usage_log(state, &provider, agent, _usage_started, &meta, false);
            // Key 池打点:发送层失败(含超时)冷却当前 Key(单 Key 无池,打点为 no-op)
            state.keypool.mark_failure(&provider.id, &provider.api_key);
            return resp;
        }
    };
    // 纵深防御:上游返回 HTML 页面(如 base_url 缺 /v1 命中中转站 Web UI)→ 不透传,人话错误
    if is_html_upstream(&upstream) {
        usage_log(state, &provider, agent, _usage_started, &send_meta, false);
        return err_resp(StatusCode::BAD_GATEWAY, HTML_UPSTREAM_ERR);
    }
    // 用量台账(仪表盘后端):落一行(尽力而为,不阻塞)
    usage_log(
        state,
        &provider,
        agent,
        _usage_started,
        &send_meta,
        upstream.status().is_success(),
    );
    eprintln!(
        "[GW] ← upstream {} conv={:?}",
        upstream.status(),
        conv_stream
    );
    // Key 池打点:429/5xx 冷却,2xx 清冷却(单 Key 无池,打点为 no-op)
    {
        let st = upstream.status().as_u16();
        if st == 429 || st >= 500 {
            state.keypool.mark_failure(&provider.id, &provider.api_key);
        } else if st < 400 {
            state.keypool.mark_success(&provider.id);
        }
    }

    // 协议转换响应（FR-5.2/5.3）
    if let Some(stream_flag) = conv_stream {
        let up_status = upstream.status();
        if !up_status.is_success() {
            let up_bytes = match read_body_timed(upstream, timeout).await {
                Ok(b) => b,
                Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
            };
            let st = StatusCode::from_u16(up_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            return Response::builder()
                .status(st)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(up_bytes))
                .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build err body"));
        }
        if stream_flag {
            // ★ 增量流式转换：逐块 Chat SSE → 即时 Responses SSE（不缓冲，防 Codex 超时断连）
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(16);
            let up_stream = upstream.bytes_stream();
            let usage_home = state.codex_home.clone();
            let usage_provider = provider.clone();
            let usage_started = _usage_started;
            tokio::spawn(async move {
                let mut conv = crate::gateway_conv::SseConvState::new();
                let mut s = up_stream;
                let mut stream_ok = true;
                'stream: while let Some(chunk) = next_stream_chunk(&mut s).await {
                    match chunk {
                        Ok(bytes) => {
                            for out in conv.feed(&bytes) {
                                if tx.send(Ok(out.into_bytes())).await.is_err() {
                                    stream_ok = false;
                                    break 'stream;
                                }
                            }
                        }
                        Err(e) => {
                            stream_ok = false;
                            let _ = tx.send(Err(std::io::Error::other(e))).await;
                            break;
                        }
                    }
                }
                if stream_ok {
                    for out in conv.finish() {
                        if tx.send(Ok(out.into_bytes())).await.is_err() {
                            stream_ok = false;
                            break;
                        }
                    }
                }
                flush_stream_usage(
                    &usage_home,
                    &usage_provider,
                    "chat_completions",
                    usage_started,
                    stream_ok,
                    conv.usage_snapshot(),
                    conv.model_snapshot(),
                    conv.request_id_snapshot(),
                );
            });
            return Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(
                    tokio_stream::wrappers::ReceiverStream::new(rx),
                ))
                .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build stream body"));
        } else {
            let up_bytes = match read_body_timed(upstream, timeout).await {
                Ok(b) => b,
                Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
            };
            usage_log_response(
                state,
                &provider,
                "responses",
                _usage_started,
                &send_meta,
                true,
                &up_bytes,
            );
            let converted = match crate::gateway_conv::chat_json_to_responses_json(&up_bytes) {
                Ok(v) => v,
                Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &format!("resp conv: {e}")),
            };
            return Response::builder()
                .status(StatusCode::OK)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(converted))
                .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build conv body"));
        }
    }

    // 否则：上游状态码 + body 原样流式透传（FR-4.11）
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = Response::builder().status(status);
    if let Some(ct) = upstream.headers().get(reqwest::header::CONTENT_TYPE) {
        if let Ok(hv) = HeaderValue::from_bytes(ct.as_bytes()) {
            resp = resp.header(axum::http::header::CONTENT_TYPE, hv);
        }
    }
    if !req_stream {
        let bytes = match read_body_timed(upstream, timeout).await {
            Ok(bytes) => bytes,
            Err(e) => return err_resp(StatusCode::BAD_GATEWAY, &e),
        };
        usage_log_response(
            state,
            &provider,
            &target_suffix,
            _usage_started,
            &send_meta,
            status.is_success(),
            &bytes,
        );
        return resp
            .body(Body::from(bytes))
            .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"));
    }
    let protocol = if target_suffix == "chat/completions" {
        "chat/completions"
    } else {
        "responses"
    };
    let body = stream_body_with_usage(
        upstream,
        state,
        &provider,
        protocol,
        _usage_started,
        status.is_success(),
    );
    resp.body(body)
        .unwrap_or_else(|_| err_resp(StatusCode::BAD_GATEWAY, "build response body"))
}

fn build_client(provider: &crate::providers::Provider) -> Result<reqwest::Client, String> {
    let timeout = Duration::from_secs(provider.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    // 不设总超时(SSE 长会话必被 120s 截断):连接与单次读各自限时,流式路径再包 per-chunk timeout;
    // 非流式路径由 read_body_timed 保留总超时语义
    let mut b = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        // 不跟随重定向:防止 https→http 降级时 Authorization 头被转发到异源
        .redirect(reqwest::redirect::Policy::none());
    if let Some(p) = provider.proxy_url.as_deref().filter(|s| !s.is_empty()) {
        match reqwest::Proxy::all(p) {
            Ok(px) => b = b.proxy(px),
            Err(e) => return Err(format!("proxy: {e}")),
        }
    }
    b.build().map_err(|e| format!("client: {e}"))
}

// ── 阶段 4 加速装配(任务书 §五)+ 星图 任务 B:per-Key 凭证────────

/// 每账号节点凭证签发超过该时长视为过期,凭证确保段将重签(12h)。
const CRED_STALE_SECS: i64 = 12 * 3600;

/// 判断当前请求应走哪条加速线路(返回线路 + 凭证是否为 per-Key,供 407 判别):
/// - mode=custom → 自定义节点(全量走代理,凭证从 accel-credentials.json 注入;恒非 per-Key);
/// - mode=official → 按供应商 base_url 命中的官方线路:
///   有 per-Key 项且未降级 → 覆盖为该账号凭证;已降级 → None(直连,不再打节点);
///   无项但有 legacy → 保留共享凭证(老用户平滑);无项无 legacy → None(由凭证确保段尝试签发);
/// - mode=off / 未命中 → 直连(None)。
fn accel_plan(state: &AppState, base_url: &str, api_key: &str) -> Option<(AccLine, bool)> {
    let cfg = state.accel.lock().unwrap_or_else(|p| p.into_inner());
    match cfg.mode.as_str() {
        "custom" => {
            let endpoint = cfg.custom_node.trim();
            if endpoint.is_empty() {
                None
            } else {
                Some((
                    AccLine {
                        id: "custom".into(),
                        name: "自定义节点".into(),
                        endpoint: endpoint.to_string(),
                        scope: Vec::new(),
                        priority: 0,
                        enabled: true,
                        credential: crate::acclines::load_credentials(&state.codex_home),
                    },
                    false,
                ))
            }
        }
        "official" => {
            let line = {
                let lines = state.health.lines.lock().unwrap_or_else(|p| p.into_inner());
                crate::acclines::match_line_healthy(base_url, &lines, &state.health).cloned()
            };
            let mut line = line?;
            let st = state.nodecreds.read().unwrap();
            match st.get_for_key(api_key) {
                Some(entry) if !entry.degraded_to_direct => {
                    // per-Key 覆盖:替换 acclines 注入的共享凭证
                    line.credential = Some(Cred {
                        user: entry.user.clone(),
                        pass: entry.pass.clone(),
                    });
                    Some((line, true))
                }
                Some(_) => None, // 已降级:本请求直接走直连
                None => {
                    if st.legacy_cred().is_some() {
                        Some((line, false)) // 老用户平滑:保留共享凭证兜底
                    } else {
                        None // 无凭证可用 → 直连(凭证确保段会尝试签发)
                    }
                }
            }
        }
        _ => None,
    }
}

/// 签发外呼统一限 5s(nodecreds 内建 10s,这里收紧为网关内联预算;超时视作不可达)。
async fn issue_timed(
    base: &str,
    api_key: &str,
) -> Result<crate::nodecreds::NodeCred, crate::nodecreds::IssueErr> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        crate::nodecreds::issue_node_cred(base, api_key),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(crate::nodecreds::IssueErr::Unreachable(
            "签发超时(5s)".into(),
        )),
    }
}

/// 记降级:store 该 key 项 degraded_to_direct=true(快照若带配额数字一并回写)+ 落盘。
/// 无该项(如 legacy 用户)则无项可记,no-op。pass 永不进日志。
fn mark_degraded(state: &AppState, api_key: &str, snap: Option<&crate::nodecreds::QuotaSnapshot>) {
    let mut st = state.nodecreds.write().unwrap();
    if let Some(e) = st.creds.get_mut(&crate::nodecreds::hash_key(api_key)) {
        e.degraded_to_direct = true;
        if let Some(s) = snap {
            if let Some(u) = s.quota_used_bytes {
                e.quota_used_bytes = u;
            }
            if let Some(t) = s.quota_total_bytes {
                e.quota_total_bytes = t;
            }
        }
        let _ = crate::nodecreds::save_store(&state.codex_home, &st);
    }
}

/// 凭证确保段(星图 任务 B2):official 命中线但 store 无该 key 项(或签发超 12h)→
/// 同步签发(5s 超时,no_proxy):
/// - Ok → set_for_key + save_store + 覆盖凭证(per-Key);
/// - Err(Unreachable) → 本请求跳线直连(不报错,日志);legacy 凭证线保留(老用户平滑);
/// - Err(QuotaFull/KeyInvalid) → 跳线直连 + 记 degraded。
async fn ensure_line_cred(
    state: &AppState,
    line: Option<(AccLine, bool)>,
    base_url: &str,
    api_key: &str,
) -> Option<(AccLine, bool)> {
    let mode = {
        let cfg = state.accel.lock().unwrap_or_else(|p| p.into_inner());
        cfg.mode.clone()
    };
    if mode != "official" || api_key.trim().is_empty() {
        return line;
    }
    // 快照判定:该 key 项是否降级 / 是否需要(重)签发
    let (degraded, needs_issue) = {
        let st = state.nodecreds.read().unwrap();
        match st.get_for_key(api_key) {
            Some(e) if e.degraded_to_direct => (true, false),
            Some(e) => (
                false,
                chrono::Utc::now().timestamp() - e.issued_at > CRED_STALE_SECS,
            ),
            None => (false, true),
        }
    };
    if degraded {
        return None; // 已降级:直连,不再签发
    }
    if !needs_issue {
        return line; // 新鲜项:accel_plan 已完成 per-Key 覆盖
    }
    // 无线路时再取一次命中线(accel_plan 的 None 含「无项无 legacy」可签发场景)
    let base_line = match &line {
        Some((l, _)) => Some(l.clone()),
        None => {
            let lines = state.health.lines.lock().unwrap_or_else(|p| p.into_inner());
            crate::acclines::match_line_healthy(base_url, &lines, &state.health).cloned()
        }
    };
    let Some(mut l) = base_line else {
        return None; // 未命中官方线路:直连,不签发
    };
    match issue_timed(&crate::server::issue_base(), api_key).await {
        Ok(cred) => {
            {
                let mut st = state.nodecreds.write().unwrap();
                st.set_for_key(api_key, cred.clone());
                let _ = crate::nodecreds::save_store(&state.codex_home, &st);
            }
            eprintln!("[GW] 每账号节点凭证已签发并落盘");
            l.credential = Some(Cred {
                user: cred.user,
                pass: cred.pass,
            });
            Some((l, true))
        }
        Err(crate::nodecreds::IssueErr::Unreachable(e)) => {
            eprintln!("[GW] 节点凭证签发不可达({e}),本请求跳线直连");
            match line {
                Some((l, pk)) if !pk => Some((l, pk)), // legacy 共享凭证线保留
                _ => None,
            }
        }
        Err(crate::nodecreds::IssueErr::QuotaFull(snap)) => {
            eprintln!("[GW] 节点凭证签发:配额满,该 Key 记降级并本请求直连");
            mark_degraded(state, api_key, snap.as_ref());
            None
        }
        Err(crate::nodecreds::IssueErr::KeyInvalid) => {
            eprintln!("[GW] 节点凭证签发:Key 无效,该 Key 记降级并本请求直连");
            mark_degraded(state, api_key, None);
            None
        }
    }
}

/// 407 判别的结果:重签成功(新凭证 line client)/本请求直连/凭证无效(维持 502)。
enum Resolve407 {
    NewClient(reqwest::Client),
    Direct,
    Invalid,
}

/// per-Key 凭证的 407 判别(星图 任务 B3;安全前提同换线重试:407 在隧道握手阶段,
/// 上游未收到任何字节,故重试/换直连都不会重复副作用):
/// - 重签 Ok → 新凭证重建 line_client,由调用方重试原请求一次;
/// - Err(QuotaFull) → store 该 key degraded_to_direct=true + 落盘,本请求直连;
/// - Err(KeyInvalid) → 维持 502「节点凭证无效」(不绕过用户指定线路);
/// - Err(Unreachable) → 本请求直连。
///
/// legacy/custom 凭证的 407 不进本函数(调用方维持原 502 行为)。
async fn resolve_407_perkey(
    state: &AppState,
    api_key: &str,
    line: &AccLine,
    timeout: Duration,
) -> Resolve407 {
    eprintln!("[GW] 407 判别:重签每账号凭证");
    match issue_timed(&crate::server::issue_base(), api_key).await {
        Ok(cred) => {
            {
                let mut st = state.nodecreds.write().unwrap();
                st.set_for_key(api_key, cred.clone());
                let _ = crate::nodecreds::save_store(&state.codex_home, &st);
            }
            let l = AccLine {
                credential: Some(Cred {
                    user: cred.user,
                    pass: cred.pass,
                }),
                ..line.clone()
            };
            match build_line_client(&l, timeout) {
                Ok(c) => Resolve407::NewClient(c),
                Err(e) => {
                    eprintln!("[GW] 重签后建线失败({e}),本请求直连");
                    Resolve407::Direct
                }
            }
        }
        Err(crate::nodecreds::IssueErr::QuotaFull(snap)) => {
            mark_degraded(state, api_key, snap.as_ref());
            eprintln!("[GW] 407 判别:配额满,该 Key 降级直连并落盘");
            Resolve407::Direct
        }
        Err(crate::nodecreds::IssueErr::KeyInvalid) => {
            eprintln!("[GW] 407 判别:Key 无效,维持 502(不绕过用户指定线路)");
            Resolve407::Invalid
        }
        Err(crate::nodecreds::IssueErr::Unreachable(e)) => {
            eprintln!("[GW] 407 判别:节点不可达({e}),本请求直连");
            Resolve407::Direct
        }
    }
}

/// 走线路的 HTTP 客户端:Proxy::all(line.endpoint) + basic auth(凭证来自线路)。
fn build_line_client(line: &AccLine, timeout: Duration) -> Result<reqwest::Client, String> {
    let proxy = reqwest::Proxy::all(&line.endpoint).map_err(|e| format!("proxy: {e}"))?;
    let proxy = if let Some(cred) = &line.credential {
        proxy.basic_auth(&cred.user, &cred.pass)
    } else {
        proxy
    };
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .proxy(proxy)
        .build()
        .map_err(|e| format!("client: {e}"))
}

#[derive(Clone)]
struct SendMeta {
    line: String,
    degraded_to_direct: bool,
}

type SendResult = Result<(reqwest::Response, SendMeta), (Response<Body>, SendMeta)>;

fn usage_log(
    state: &AppState,
    provider: &crate::providers::Provider,
    route: &str,
    started: std::time::Instant,
    meta: &SendMeta,
    ok: bool,
) {
    crate::usage_stats::log_request(
        &state.codex_home,
        &crate::usage_stats::ReqLog {
            ts: chrono::Utc::now().timestamp(),
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            key_masked: crate::usage_stats::mask_key(&provider.api_key),
            route: route.into(),
            line: meta.line.clone(),
            degraded_to_direct: meta.degraded_to_direct,
            latency_ms: started.elapsed().as_millis() as u64,
            ok,
        },
    );
}

fn usage_log_response(
    state: &AppState,
    provider: &crate::providers::Provider,
    protocol: &str,
    started: std::time::Instant,
    meta: &SendMeta,
    status_ok: bool,
    body: &[u8],
) {
    if provider.access_mode == crate::providers::AccessMode::Official || !status_ok {
        return;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return;
    };
    let usage = crate::usage_stats::normalize_usage(protocol, &value);
    if usage.input_tokens.is_none()
        && usage.output_tokens.is_none()
        && usage.total_tokens.is_none()
        && usage.cache_read_tokens.is_none()
    {
        return;
    }
    usage_log_value(
        &state.codex_home,
        provider,
        protocol,
        started,
        status_ok,
        value,
    );
    let _ = meta;
}

fn usage_log_value(
    codex_home: &std::path::Path,
    provider: &crate::providers::Provider,
    protocol: &str,
    started: std::time::Instant,
    status_ok: bool,
    value: serde_json::Value,
) {
    if provider.access_mode == crate::providers::AccessMode::Official {
        return;
    }
    let usage = crate::usage_stats::normalize_usage(protocol, &value);
    if usage.input_tokens.is_none()
        && usage.output_tokens.is_none()
        && usage.total_tokens.is_none()
        && usage.cache_read_tokens.is_none()
    {
        return;
    }
    crate::usage_stats::log_usage(
        codex_home,
        crate::usage_stats::UsageRecord {
            kind: "usage".into(),
            ts: chrono::Utc::now().timestamp(),
            provider_id: provider.id.clone(),
            provider_name: provider.name.clone(),
            model: usage_model(&value),
            route: protocol.to_string(),
            request_id: usage_request_id(&value),
            usage,
            cost: None,
            cost_source: None,
            latency_ms: started.elapsed().as_millis() as u64,
            ok: status_ok,
        },
    );
}

fn string_field(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(|field| field.as_str())
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(str::to_string)
    })
}

fn usage_model(value: &serde_json::Value) -> Option<String> {
    string_field(value, &["model", "modelVersion"])
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| string_field(response, &["model", "modelVersion"]))
        })
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| string_field(message, &["model", "modelVersion"]))
        })
}

fn usage_request_id(value: &serde_json::Value) -> Option<String> {
    string_field(value, &["id"])
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| string_field(response, &["id"]))
        })
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| string_field(message, &["id"]))
        })
}

fn merge_usage(target: &mut Option<serde_json::Value>, value: &serde_json::Value) {
    let Some(source) = value.as_object() else {
        return;
    };
    let object = target.get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(object) = object.as_object_mut() else {
        return;
    };
    for (key, value) in source {
        if !value.is_null() {
            object.insert(key.clone(), value.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_stream_usage(
    codex_home: &std::path::Path,
    provider: &crate::providers::Provider,
    protocol: &'static str,
    started: std::time::Instant,
    status_ok: bool,
    usage: Option<serde_json::Value>,
    model: Option<String>,
    request_id: Option<String>,
) {
    if let Some(usage) = usage {
        usage_log_value(
            codex_home,
            provider,
            protocol,
            started,
            status_ok,
            serde_json::json!({
                "id": request_id,
                "model": model,
                "usage": usage,
            }),
        );
    }
}

/// 非流式读完整响应体:保留总超时语义(客户端已不设总超时——流式路径按 chunk 限时,
/// 非流式这里对整体读取包一层 tokio::time::timeout)。
async fn read_body_timed(
    upstream: reqwest::Response,
    timeout: Duration,
) -> Result<axum::body::Bytes, String> {
    match tokio::time::timeout(timeout, upstream.bytes()).await {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(format!("read upstream: {e}")),
        Err(_) => Err("read upstream: 读响应体超时".into()),
    }
}

/// 流式单 chunk 读:包 tokio::time::timeout(每 chunk 60s)——SSE 长会话不被总超时截断,
/// 死连接也不会无限挂起;超时/上游错误统一 Err(String),由调用方断流收束。
async fn next_stream_chunk<S, E>(stream: &mut S) -> Option<Result<axum::body::Bytes, String>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    match tokio::time::timeout(
        Duration::from_secs(STREAM_CHUNK_TIMEOUT_SECS),
        stream.next(),
    )
    .await
    {
        Ok(Some(Ok(bytes))) => Some(Ok(bytes)),
        Ok(Some(Err(e))) => Some(Err(e.to_string())),
        Ok(None) => None,
        Err(_) => Some(Err(format!(
            "读取上游流超时({STREAM_CHUNK_TIMEOUT_SECS}s 无数据)"
        ))),
    }
}

fn stream_body_with_usage(
    upstream: reqwest::Response,
    state: &AppState,
    provider: &crate::providers::Provider,
    protocol: &'static str,
    started: std::time::Instant,
    status_ok: bool,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let usage_home = state.codex_home.clone();
    let usage_provider = provider.clone();
    tokio::spawn(async move {
        let mut stream = upstream.bytes_stream();
        let mut buffer = Vec::new();
        let mut usage = None;
        let mut model = None;
        let mut request_id = None;
        let mut stream_ok = true;
        'stream: while let Some(chunk) = next_stream_chunk(&mut stream).await {
            match chunk {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                        let line = String::from_utf8_lossy(&buffer[..position])
                            .trim()
                            .to_string();
                        buffer.drain(..=position);
                        collect_stream_usage(&line, &mut usage, &mut model, &mut request_id);
                    }
                    if tx.send(Ok(bytes)).await.is_err() {
                        stream_ok = false;
                        break 'stream;
                    }
                }
                Err(error) => {
                    stream_ok = false;
                    let _ = tx.send(Err(std::io::Error::other(error))).await;
                    break;
                }
            }
        }
        if !buffer.is_empty() {
            let line = String::from_utf8_lossy(&buffer).trim().to_string();
            collect_stream_usage(&line, &mut usage, &mut model, &mut request_id);
        }
        flush_stream_usage(
            &usage_home,
            &usage_provider,
            protocol,
            started,
            status_ok && stream_ok,
            usage,
            model,
            request_id,
        );
    });
    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
}

fn collect_stream_usage(
    line: &str,
    usage: &mut Option<serde_json::Value>,
    model: &mut Option<String>,
    request_id: &mut Option<String>,
) {
    let Some(data) = line.strip_prefix("data:").map(str::trim) else {
        return;
    };
    if data == "[DONE]" || data.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    if let Some(value) = value.get("usage") {
        merge_usage(usage, value);
    }
    if let Some(value) = value.get("usageMetadata") {
        merge_usage(usage, value);
    }
    if let Some(value) = value.get("response").and_then(|v| v.get("usage")) {
        merge_usage(usage, value);
    }
    if let Some(value) = value.get("response").and_then(|v| v.get("usageMetadata")) {
        merge_usage(usage, value);
    }
    if let Some(value) = value.get("message").and_then(|v| v.get("usage")) {
        merge_usage(usage, value);
    }
    if model.is_none() {
        *model = usage_model(&value);
    }
    if request_id.is_none() {
        *request_id = usage_request_id(&value);
    }
}

/// 加速发送核心(R1 抽共用,dispatch 与 dispatch_anthropic 共享):
/// 首发 = 命中线的 client(未命中/加速关 = 直连 client);Ok 但代理 407 或 Err 呈现代理
/// 认证失败(CONNECT 阶段 407)→ 非 per-Key(legacy/custom)人话化 502 不绕线,per-Key 走
/// resolve_407_and_retry 判别;其余连接层失败且线在用 → 换直连 client 重试一次;
/// 未用线时 timeout → 504、其余 → 502。终态响应以 Err(Response) 返回,调用方直接透传。
/// send() 返回 Ok 前未向客户端写任何字节,故重试/换线均无重复副作用。
async fn send_with_accel<F>(
    state: &AppState,
    api_key: &str,
    line: &Option<(AccLine, bool)>,
    line_client: &Option<reqwest::Client>,
    direct_client: &reqwest::Client,
    timeout: Duration,
    build_rb: &F,
) -> SendResult
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let used_line = line_client.is_some();
    let per_key = line.as_ref().map(|(_, pk)| *pk).unwrap_or(false);
    let line_meta = SendMeta {
        line: line
            .as_ref()
            .map(|(line, _)| line.id.clone())
            .unwrap_or_else(|| "direct".into()),
        degraded_to_direct: false,
    };
    let direct_meta = SendMeta {
        line: "direct".into(),
        degraded_to_direct: used_line,
    };
    let first = line_client.as_ref().unwrap_or(direct_client);
    // send 只到响应头,不设总超时(SSE 流在 send 之后);但慢读上游可能让 send 无限挂起,包 60s 超时
    match tokio::time::timeout(timeout, build_rb(first).send()).await {
        Ok(Ok(r)) => {
            // 代理 407 → 线路凭证无效。per-Key 凭证走重签判别(星图 resolve_407:重签/降级/直连);
            // legacy/custom 凭证人话化 502,不换直连(避免绕过用户指定的线路)。
            if used_line && r.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                if !per_key {
                    eprintln!("[GW] line 凭证无效(407)");
                    return Err((err_resp(StatusCode::BAD_GATEWAY, "节点凭证无效"), line_meta));
                }
                let line_ref = &line.as_ref().expect("used_line ⇒ line").0;
                resolve_407_and_retry(state, api_key, line_ref, timeout, direct_client, build_rb)
                    .await
            } else {
                Ok((r, line_meta))
            }
        }
        Ok(Err(e)) => {
            if used_line && proxy_auth_error(&e) {
                // CONNECT 阶段的 407 以 Err(hyper ProxyAuthRequired) 形态出现:per-Key 同判别
                if !per_key {
                    eprintln!("[GW] line 代理认证失败: {e}");
                    return Err((err_resp(StatusCode::BAD_GATEWAY, "节点凭证无效"), line_meta));
                }
                eprintln!("[GW] per-Key 代理认证失败,重签判别: {e}");
                let line_ref = &line.as_ref().expect("used_line ⇒ line").0;
                resolve_407_and_retry(state, api_key, line_ref, timeout, direct_client, build_rb)
                    .await
            } else if used_line {
                eprintln!("[GW] line 失败({e}),换直连重试一次");
                match tokio::time::timeout(timeout, build_rb(direct_client).send()).await {
                    Ok(Ok(r)) => Ok((r, direct_meta)),
                    Ok(Err(e2)) if e2.is_timeout() => Err((
                        err_resp(StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
                        direct_meta,
                    )),
                    Ok(Err(e2)) => {
                        eprintln!("[GW] ✗ upstream ERR: {e2}");
                        Err((
                            err_resp(StatusCode::BAD_GATEWAY, "upstream unreachable"),
                            direct_meta,
                        ))
                    }
                    Err(_) => {
                        eprintln!("[GW] send timeout (direct)");
                        Err((
                            err_resp(StatusCode::BAD_GATEWAY, "上游响应超时"),
                            direct_meta,
                        ))
                    }
                }
            } else if e.is_timeout() {
                Err((
                    err_resp(StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
                    direct_meta,
                ))
            } else {
                eprintln!("[GW] ✗ upstream ERR: {e}");
                Err((
                    err_resp(StatusCode::BAD_GATEWAY, "upstream unreachable"),
                    direct_meta,
                ))
            }
        }
        Err(_) => {
            eprintln!("[GW] send timeout (first)");
            Err((err_resp(StatusCode::BAD_GATEWAY, "上游响应超时"), line_meta))
        }
    }
}

/// 407 后的 per-Key 重签判别 + 单次重试(R1 抽共用):resolve_407_perkey 给出
/// NewClient(新凭证线 client 重试一次,仍 407 → 人话化 502 收束)/ Direct(直连重试)/
/// Invalid(502 人话化);重试的连接错误处理同首发(timeout → 504,其余 → 502)。
async fn resolve_407_and_retry<F>(
    state: &AppState,
    api_key: &str,
    line: &AccLine,
    timeout: Duration,
    direct_client: &reqwest::Client,
    build_rb: &F,
) -> SendResult
where
    F: Fn(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let retry_line = match resolve_407_perkey(state, api_key, line, timeout).await {
        Resolve407::Invalid => {
            return Err((
                err_resp(StatusCode::BAD_GATEWAY, "节点凭证无效"),
                SendMeta {
                    line: line.id.clone(),
                    degraded_to_direct: false,
                },
            ))
        }
        Resolve407::NewClient(c) => Some(c),
        Resolve407::Direct => None,
    };
    let meta = SendMeta {
        line: retry_line
            .as_ref()
            .map(|_| line.id.clone())
            .unwrap_or_else(|| "direct".into()),
        degraded_to_direct: retry_line.is_none(),
    };
    let client = retry_line.as_ref().unwrap_or(direct_client);
    match tokio::time::timeout(timeout, build_rb(client).send()).await {
        // 重签凭证重试仍 407 → 人话化收束(不无限重试)
        Ok(Ok(r2))
            if retry_line.is_some()
                && r2.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED =>
        {
            eprintln!("[GW] 重签后仍 407");
            Err((err_resp(StatusCode::BAD_GATEWAY, "节点凭证无效"), meta))
        }
        Ok(Ok(r2)) => Ok((r2, meta)),
        Ok(Err(e2)) if e2.is_timeout() => Err((
            err_resp(StatusCode::GATEWAY_TIMEOUT, "upstream timeout"),
            meta,
        )),
        Ok(Err(e2)) => {
            eprintln!("[GW] ✗ upstream ERR: {e2}");
            Err((
                err_resp(StatusCode::BAD_GATEWAY, "upstream unreachable"),
                meta,
            ))
        }
        Err(_) => {
            eprintln!("[GW] send timeout (client)");
            Err((err_resp(StatusCode::BAD_GATEWAY, "上游响应超时"), meta))
        }
    }
}

/// 请求错误是否指向代理认证失败(407/401):CONNECT 模式下代理拒绝会以 Err 形式出现
/// (hyper 的 ProxyAuthRequired),需据此区分「凭证错误(不重试直连)」与「线路不可达(可换直连)」。
fn proxy_auth_error(e: &reqwest::Error) -> bool {
    if let Some(st) = e.status() {
        return st == reqwest::StatusCode::UNAUTHORIZED
            || st == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED;
    }
    // 拼接错误及整条 source 链的 Display(hyper 的 ProxyAuthRequired 在链内,顶层 to_string 不含)
    let mut chain = String::new();
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        chain.push(' ');
        chain.push_str(&err.to_string());
        cur = err.source();
    }
    let msg = chain.to_ascii_lowercase();
    let has_code = |c: &str| {
        msg.split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|t| t == c)
    };
    has_code("407")
        || has_code("401")
        || msg.contains("proxy auth")
        || msg.contains("proxyauthenticationrequired")
}

/// test-node 探测结果(供 POST /api/accel/test-node 映射人话)。
#[derive(Debug)]
pub enum NodeTestOutcome {
    Ok { latency_ms: u64 },
    Timeout,
    Auth,
    Unavailable,
}

/// 经代理测试目标节点连通性:basic auth 来自凭证(可空);成功计时返回。
/// target 由装配方决定(契约固定为 https://api.2xa.cc.cd/models)。
pub async fn test_node_via(
    endpoint: &str,
    target: &str,
    cred: Option<&Cred>,
    timeout: Duration,
) -> NodeTestOutcome {
    let proxy = match reqwest::Proxy::all(endpoint) {
        Ok(p) => p,
        Err(_) => return NodeTestOutcome::Unavailable,
    };
    let proxy = if let Some(c) = cred {
        proxy.basic_auth(&c.user, &c.pass)
    } else {
        proxy
    };
    let client = match reqwest::Client::builder()
        .timeout(timeout)
        .proxy(proxy)
        .build()
    {
        Ok(c) => c,
        Err(_) => return NodeTestOutcome::Unavailable,
    };
    let started = std::time::Instant::now();
    match client.get(target).send().await {
        Ok(r) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let st = r.status();
            if st == reqwest::StatusCode::UNAUTHORIZED
                || st == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
            {
                NodeTestOutcome::Auth
            } else if st.is_success() {
                NodeTestOutcome::Ok { latency_ms }
            } else {
                NodeTestOutcome::Unavailable
            }
        }
        Err(e) => {
            if e.is_timeout() {
                NodeTestOutcome::Timeout
            } else if proxy_auth_error(&e) {
                NodeTestOutcome::Auth
            } else {
                NodeTestOutcome::Unavailable
            }
        }
    }
}

fn err_resp(status: StatusCode, msg: &str) -> Response<Body> {
    (status, msg.to_string()).into_response()
}

/// 上游返回 HTML 页面(真机实证:base_url 缺 /v1 时 2xa 中转站对裸域路径回 200 text/html Web UI,
/// 网关若透传 CLI 表现为空流/拿到一堆 HTML)→ 不透传,人话错误。
/// 只拦 text/html;正常模型响应(text/event-stream / application/json 等)零影响。
const HTML_UPSTREAM_ERR: &str = "上游返回了网页内容(HTTP 200 text/html),通常是 API 地址不对——请检查供应商 base_url 是否带 /v1(如 https://2xa.cc.cd/v1)";

/// Content-Type 判 HTML。不读响应体 → SSE 流式/透传路径同样能拦(真机案例均为 text/html 头)。
/// ponytail:不嗅探响应体——谎报头+HTML 体属未观测场景,需要时在缓冲路径加字节预检即可。
fn is_html_upstream(upstream: &reqwest::Response) -> bool {
    upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

// ── 单测（M3a Gate：mock 上游验证每跳）──────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{self, ProviderInput};
    use crate::server::AppState;
    use axum::{
        routing::{get, post},
        Router,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn make_state(label: &str) -> (AppState, PathBuf, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("2xapi-m3-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let providers_path = root.join("providers.json");
        let state = AppState {
            config_path: root.join("config.toml"),
            backup_dir: root.join("backups"),
            providers_path: providers_path.clone(),
            codex_home: root.join("codex"),
            wb_home: root.clone(),
            hermes_home: root.join("hermes"),
            gem_home: root.clone(),
            grok_home: root.join("grok"),
            oc_home: root.join("ochome"),
            oclaw_home: root.join("oclaw"),
            cd_home: root.join("cdsupport"),
            cursor_home: root.join("cursorhome"),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            launcher: Default::default(),
            health: std::sync::Arc::new(crate::acclines::HealthState::new(vec![])),
            accel: std::sync::Arc::new(std::sync::Mutex::new(crate::server::AccelCfg::default())),
            nodecreds: std::sync::Arc::new(
                std::sync::RwLock::new(crate::nodecreds::Store::empty()),
            ),
        };
        (state, providers_path, root)
    }

    fn add_provider(path: &std::path::Path, base_url: &str, api_key: &str) -> String {
        let input = ProviderInput {
            name: "T".into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: "gpt-test".into(),
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p = providers::create(path, input).unwrap();
        providers::set_active(path, &p.id);
        p.id
    }

    pub(crate) fn claude_desktop_mapping_provider() -> crate::providers::Provider {
        crate::providers::Provider {
            model: "fallback-model".into(),
            claude_desktop_model_routes: vec![
                crate::providers::ClaudeDesktopModelRoute {
                    role: "sonnet".into(),
                    model: "gpt-5.6".into(),
                    label_override: Some("GPT".into()),
                    supports_1m: true,
                },
                crate::providers::ClaudeDesktopModelRoute {
                    role: "opus".into(),
                    model: "gpt-5.6-sol".into(),
                    label_override: None,
                    supports_1m: true,
                },
                crate::providers::ClaudeDesktopModelRoute {
                    role: "fable".into(),
                    model: "gpt-5.5".into(),
                    label_override: None,
                    supports_1m: false,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn claude_desktop_request_models_are_mapped_before_forwarding() {
        let provider = claude_desktop_mapping_provider();
        let cases = [
            ("claude-sonnet-5", "gpt-5.6"),
            ("claude-opus-5", "gpt-5.6-sol"),
            ("claude-fable-5", "gpt-5.5"),
            ("claude-haiku-4-5", "gpt-5.6"),
        ];

        for (requested, expected) in cases {
            let body = Bytes::from(
                serde_json::to_vec(&json!({"model": requested, "messages": []})).unwrap(),
            );
            let rewritten = rewrite_anthropic_request_model("claude-desktop", &provider, body);
            let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
            assert_eq!(value["model"], expected);
        }
    }

    #[test]
    fn anthropic_model_rewrite_is_scoped_and_unknown_models_pass_through() {
        let provider = claude_desktop_mapping_provider();
        let body = Bytes::from_static(br#"{"model":"claude-sonnet-5","messages":[]}"#);
        assert_eq!(
            rewrite_anthropic_request_model("claude", &provider, body.clone()),
            body
        );

        let unknown = Bytes::from_static(br#"{"model":"custom-model","messages":[]}"#);
        assert_eq!(
            rewrite_anthropic_request_model("claude-desktop", &provider, unknown.clone()),
            unknown
        );
    }

    #[test]
    fn usage_stream_extracts_nested_response_identity_and_gemini_model_version() {
        let mut usage = None;
        let mut model = None;
        let mut request_id = None;
        collect_stream_usage(
            r#"data: {"response":{"id":"resp-1","model":"gpt-responses","usage":{"input_tokens":8,"output_tokens":3}}}"#,
            &mut usage,
            &mut model,
            &mut request_id,
        );
        assert_eq!(model.as_deref(), Some("gpt-responses"));
        assert_eq!(request_id.as_deref(), Some("resp-1"));
        assert_eq!(
            usage
                .as_ref()
                .and_then(|value| value["input_tokens"].as_u64()),
            Some(8)
        );

        let mut gemini_usage = None;
        let mut gemini_model = None;
        let mut gemini_request_id = None;
        collect_stream_usage(
            r#"data: {"modelVersion":"gemini-2.5-pro","usageMetadata":{"promptTokenCount":11}}"#,
            &mut gemini_usage,
            &mut gemini_model,
            &mut gemini_request_id,
        );
        assert_eq!(gemini_model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(
            gemini_usage
                .as_ref()
                .and_then(|value| value["promptTokenCount"].as_u64()),
            Some(11)
        );
    }

    fn usage_rows(codex_home: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(codex_home.join("usage-stats.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// 启动一个 mock 上游，返回 (base_url, 收到的 Authorization 列表)。
    /// 另加 GET /(供 test-node 探测 200 用),不记录 seen。
    async fn mock_upstream(resp_body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let app = Router::new()
            .route(
                "/responses",
                post(move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                    let seen = seen_clone.clone();
                    async move {
                        let auth = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push(auth);
                        (StatusCode::OK, resp_body)
                    }
                }),
            )
            .route("/", get(|| async { (StatusCode::OK, "UP_OK") }))
            // 其余 POST 路径(chat/images 探测用)固定回 resp_body 并记 Authorization
            .fallback(post({
                let seen_fallback = seen.clone();
                move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                    let seen = seen_fallback;
                    async move {
                        let auth = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push(auth);
                        (StatusCode::OK, resp_body)
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    async fn mock_status_upstream(
        status: StatusCode,
        resp_body: &'static str,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let app = Router::new().fallback(post(
            move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                let seen = seen_clone.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                        .unwrap_or_default();
                    seen.lock().unwrap().push(auth);
                    (status, resp_body)
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    async fn req_post_responses(body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    /// 用量仪表盘:网关请求后落盘台账,聚合含该 provider(P50/P90 由 usage_stats 单测覆盖)。
    #[tokio::test]
    async fn gateway_logs_usage_stats() {
        let (base, _seen) = mock_upstream("OK_BODY").await;
        let (state, providers_path, root) = make_state("usage-stats");
        add_provider(&providers_path, &base, "sk-usage");
        let resp = proxy_responses(
            State(Arc::new(state.clone())),
            req_post_responses(r#"{"input":"hi"}"#).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let raw = std::fs::read_to_string(state.codex_home.join("usage-stats.jsonl")).unwrap();
        assert!(
            raw.contains("\"route\":\"codex\""),
            "codex 通道应落台账: {raw}"
        );
        assert!(raw.contains("\"ok\":true"), "{raw}");
        assert!(!raw.contains("sk-usage"), "不落明文 Key: {raw}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 01-D3 / FR-4.3：上游收到的 Authorization = Bearer {provider.api_key}（非 Codex 传来值），且 body 透传。
    #[tokio::test]
    async fn injects_provider_key_and_passthrough() {
        let (base, seen) = mock_upstream("PASSTHROUGH_BODY").await;
        let (state, providers_path, root) = make_state("inject");
        // Codex 试图自带一个假 key，网关应忽略它、注入 provider key
        let _id = add_provider(&providers_path, &base, "sk-provider-secret");

        let resp = proxy_responses(
            State(Arc::new(state)),
            req_post_responses("{\"hello\":1}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"PASSTHROUGH_BODY");
        // 给 mock 一点写 seen 的时间
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            seen.lock().unwrap().first().map(|s| s.as_str()),
            Some("Bearer sk-provider-secret")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 01-D7 / FR-4.9：热切换——切 active 后下一请求走新 provider（新 key）。
    #[tokio::test]
    async fn hot_swap_next_request_uses_new_provider() {
        let (base_a, seen_a) = mock_upstream("FROM_A").await;
        let (base_b, seen_b) = mock_upstream("FROM_B").await;
        let (state, providers_path, root) = make_state("hotswap");

        let id_a = add_provider(&providers_path, &base_a, "sk-A");
        // 另建 B 并不激活
        let input_b = ProviderInput {
            name: "B".into(),
            base_url: base_b.clone(),
            api_key: "sk-B".into(),
            model: "m".into(),
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p_b = providers::create(&providers_path, input_b).unwrap();

        // 先走 A
        let r1 = proxy_responses(
            State(Arc::new(clone_state(&state))),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(r1.status(), StatusCode::OK);
        // 热切换到 B（仅改 active_provider_id，不重启）
        providers::set_active(&providers_path, &p_b.id);
        let r2 = proxy_responses(
            State(Arc::new(clone_state(&state))),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(r2.status(), StatusCode::OK);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            seen_a.lock().unwrap().first().map(|s| s.as_str()),
            Some("Bearer sk-A")
        );
        assert_eq!(
            seen_b.lock().unwrap().first().map(|s| s.as_str()),
            Some("Bearer sk-B")
        );
        let _ = id_a;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FR-4.8：上游超时 → 504。
    #[tokio::test]
    async fn upstream_timeout_returns_504() {
        let app = Router::new().route(
            "/responses",
            post(
                |_h: axum::http::HeaderMap, _b: axum::body::Bytes| async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    (StatusCode::OK, "slow")
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (state, providers_path, root) = make_state("timeout");
        // 直接写 providers.json（绕过 create 的校验，专测网关超时行为；timeout_secs=1）
        let pd = providers::ProviderData {
            schema_version: 1,
            active_provider_id: Some("p-to".into()),
            active_provider_ids: std::collections::HashMap::from([("codex".into(), "p-to".into())]),
            providers: vec![providers::Provider {
                id: "p-to".into(),
                name: "T".into(),
                base_url: format!("http://{}", addr),
                api_key: "sk".into(),
                keys: vec![],
                model: "m".into(),
                timeout_secs: Some(1),
                sub2api_multiplier: 1.0,
                ..Default::default()
            }],
        };
        std::fs::write(&providers_path, serde_json::to_string(&pd).unwrap()).unwrap();

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// FR-4.11：上游 5xx 原样透传。
    #[tokio::test]
    async fn upstream_error_passthrough() {
        let app = Router::new().route(
            "/responses",
            post(
                |_h: axum::http::HeaderMap, _b: axum::body::Bytes| async move {
                    (StatusCode::INSUFFICIENT_STORAGE, "upstream-broke")
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (state, providers_path, root) = make_state("err507");
        add_provider(&providers_path, &format!("http://{}", addr), "sk");

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE); // 507 透传
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"upstream-broke");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 无 active provider → 503。
    #[tokio::test]
    async fn no_active_provider_returns_503() {
        let (state, _providers_path, root) = make_state("noactive");
        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// UA 伪装:provider.user_agent 有值 → 上游收到该 UA(只对目标供应商生效);缺省 → 不设 UA 头(现状)。
    #[tokio::test]
    async fn provider_ua_overrides_upstream_user_agent() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let app = Router::new().route(
            "/responses",
            post(move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                let seen = seen_clone.clone();
                async move {
                    let ua = h
                        .get("user-agent")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                        .unwrap_or_default();
                    seen.lock().unwrap().push(ua);
                    (StatusCode::OK, "OK")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base = format!("http://{}", addr);
        let (state, providers_path, root) = make_state("ua-override");

        // 缺省(None):不伪装 → 上游不收到 UA 头(网关现状)
        add_provider(&providers_path, &base, "sk-default");
        let r1 = proxy_responses(
            State(Arc::new(clone_state(&state))),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(r1.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let default_ua = seen.lock().unwrap().pop().expect("缺省应收到 UA");
        assert!(default_ua.is_empty(), "缺省不应设 UA 头,实际: {default_ua}");

        // 伪装:user_agent 有值 → 上游收到该 UA
        let input = ProviderInput {
            name: "UAS".into(),
            base_url: base,
            api_key: "sk-ua".into(),
            model: "m".into(),
            sub2api_multiplier: 1.0,
            user_agent: Some("curl/8.6.0".into()),
            ..ProviderInput::default()
        };
        let p = providers::create(&providers_path, input).unwrap();
        providers::set_active(&providers_path, &p.id);
        let r2 = proxy_responses(
            State(Arc::new(clone_state(&state))),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(r2.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(seen.lock().unwrap().pop().as_deref(), Some("curl/8.6.0"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Claude 接入(批):/anthropic/* 路由────────────────────

    /// mock 上游:同时挂 /v1/messages 与 /messages,记录 (x-api-key, 命中的路径)。
    async fn mock_anthropic_upstream() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s_v1 = seen.clone();
        let s_m = seen.clone();
        let app = Router::new()
            .route(
                "/v1/messages",
                post(move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                    let seen = s_v1.clone();
                    async move {
                        let auth = h
                            .get("x-api-key")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push((auth, "/v1/messages".into()));
                        (StatusCode::OK, "ANTHROPIC_OK_V1")
                    }
                }),
            )
            .route(
                "/messages",
                post(move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                    let seen = s_m.clone();
                    async move {
                        let auth = h
                            .get("x-api-key")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push((auth, "/messages".into()));
                        (StatusCode::OK, "ANTHROPIC_OK_M")
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    async fn req_post_anthropic(body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/anthropic/v1/messages")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    fn add_claude_provider(path: &std::path::Path, base_url: &str, api_key: &str) -> String {
        let input = ProviderInput {
            name: "ClaudeT".into(),
            agent: "claude".into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: "claude-sonnet".into(),
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p = providers::create(path, input).unwrap();
        providers::set_active(path, &p.id);
        p.id
    }

    // ── Gemini 入口(多平台阶段 C 第一段)─────────────────────

    /// 建 agent=gemini 供应商(不设 active,由调用方控制全局 active 以测不串台)。
    fn add_gemini_provider(
        path: &std::path::Path,
        base_url: &str,
        api_key: &str,
        wire: crate::providers::WireApi,
    ) -> String {
        let input = ProviderInput {
            name: "GeminiT".into(),
            agent: "gemini".into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: "gemini-2.5-flash".into(),
            wire_api: wire,
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        providers::create(path, input).unwrap().id
    }

    /// mock Chat 上游:记录 (Authorization, 收到的请求 body),返回固定响应(支持 SSE)。
    async fn mock_chat_upstream(
        resp_body: &'static str,
        resp_ctype: &'static str,
    ) -> (String, Arc<Mutex<Vec<(String, Vec<u8>)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |h: axum::http::HeaderMap, b: axum::body::Bytes| {
                let seen = seen_c.clone();
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                        .unwrap_or_default();
                    seen.lock().unwrap().push((auth, b.to_vec()));
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, resp_ctype)],
                        resp_body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    /// mock 原生 Gemini 上游(透传分支):记录 (x-goog-api-key, 收到的请求 body, 路径段),固定响应。
    async fn mock_gemini_upstream() -> (String, Arc<Mutex<Vec<(String, Vec<u8>, String)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let app = Router::new().route(
            "/v1beta/models/:model_action",
            post(
                move |ma: axum::extract::Path<String>,
                      h: axum::http::HeaderMap,
                      b: axum::body::Bytes| {
                    let seen = seen_c.clone();
                    async move {
                        let ma = ma.0;
                        let key = h
                            .get("x-goog-api-key")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push((key, b.to_vec(), ma));
                        (StatusCode::OK, "GEMINI_PASSTHROUGH_OK")
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    fn req_gemini(model_action: &str, query: Option<&str>, body: String) -> Request<Body> {
        let uri = match query {
            Some(q) => format!("/v1beta/models/{model_action}?{q}"),
            None => format!("/v1beta/models/{model_action}"),
        };
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn gemini_route_converts_to_chat_and_injects_key() {
        let chat_resp = r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
        let (base, seen) = mock_chat_upstream(chat_resp, "application/json").await;
        let (state, providers_path, root) = make_state("gemini-conv");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem-secret",
            crate::providers::WireApi::ChatCompletions,
        );

        let body =
            serde_json::json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] })
                .to_string();
        let resp = proxy_gemini(
            State(Arc::new(state.clone())),
            axum::extract::Path("gemini-2.5-flash:generateContent".into()),
            req_gemini("gemini-2.5-flash:generateContent", None, body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["candidates"][0]["content"]["parts"][0]["text"], "收到",
            "响应应为 Gemini 形态:\n{v}"
        );
        assert_eq!(v["usageMetadata"]["totalTokenCount"], 7);

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = seen.lock().unwrap();
        let (auth, up_body) = &seen[0];
        assert_eq!(auth, "Bearer sk-gem-secret", "上游应收到注入的 Bearer Key");
        let up: serde_json::Value = serde_json::from_slice(up_body).unwrap();
        assert_eq!(
            up["model"], "gemini-2.5-flash",
            "URL 上的 model 应写入 chat body:\n{up}"
        );
        assert_eq!(up["messages"][0]["role"], "user");
        assert_eq!(up["messages"][0]["content"], "hi");
        assert_eq!(up["stream"], false);
        let summary = crate::usage_stats::summary(&state.codex_home);
        assert_eq!(summary["providers"][0]["count"], 1);
        assert_eq!(summary["providers"][0]["routes"][0], "gemini");
        let usage = usage_rows(&state.codex_home);
        assert_eq!(usage[0]["route"], "gemini");
        assert_eq!(usage[0]["line"], "direct");
        assert_eq!(usage[0]["ok"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn gemini_route_stream_sse_converted() {
        let sse = "data: {\"id\":\"1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"}}]}\n\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"}}]}\n\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n";
        let (base, _seen) = mock_chat_upstream(sse, "text/event-stream").await;
        let (state, providers_path, root) = make_state("gemini-sse");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem",
            crate::providers::WireApi::ChatCompletions,
        );

        let body =
            serde_json::json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] })
                .to_string();
        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:streamGenerateContent".into()),
            req_gemini("m:streamGenerateContent", Some("alt=sse"), body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains(r#""text":"你""#), "流式文本分块:\n{s}");
        assert!(s.contains(r#""text":"好""#));
        assert!(s.contains(r#""finishReason":"STOP""#));
        assert!(s.contains(r#""promptTokenCount":3"#), "usage 应转回:\n{s}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn gemini_route_no_provider_503() {
        let (state, _providers_path, root) = make_state("gemini-503");
        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:generateContent".into()),
            req_gemini("m:generateContent", None, r#"{"contents":[]}"#.into()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Gemini 供应商"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// D2:多模态请求明确报错,不静默降级。
    #[tokio::test]
    async fn gemini_route_multimodal_rejected_400() {
        let (base, seen) = mock_chat_upstream("{}", "application/json").await;
        let (state, providers_path, root) = make_state("gemini-mm");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem",
            crate::providers::WireApi::ChatCompletions,
        );

        let body = serde_json::json!({ "contents": [{ "role": "user", "parts": [{ "inlineData": { "mimeType": "image/png", "data": "iVBOR" } }] }] }).to_string();
        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:generateContent".into()),
            req_gemini("m:generateContent", None, body),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"]["message"].as_str().unwrap().contains("多模态"),
            "人话错误:\n{v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(seen.lock().unwrap().is_empty(), "多模态请求不得打到上游");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 不串台:全局 active 是 codex 供应商时,/v1beta 仍取 agent=gemini 供应商。
    #[tokio::test]
    async fn gemini_route_uses_gemini_provider_even_when_active_is_codex() {
        let (base_gem, seen_gem) = mock_chat_upstream(r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"G"},"finish_reason":"stop"}]}"#, "application/json").await;
        let (base_codex, _seen_codex) = mock_chat_upstream(r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"C"},"finish_reason":"stop"}}]}"#, "application/json").await;
        let (state, providers_path, root) = make_state("gemini-x");
        let _cid = add_provider(&providers_path, &base_codex, "sk-codex"); // 全局 active=codex
        let _gid = add_gemini_provider(
            &providers_path,
            &base_gem,
            "sk-gem",
            crate::providers::WireApi::ChatCompletions,
        );

        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:generateContent".into()),
            req_gemini(
                "m:generateContent",
                None,
                r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}]}"#.into(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["candidates"][0]["content"]["parts"][0]["text"], "G",
            "必须走 gemini 供应商:\n{v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = seen_gem.lock().unwrap();
        assert_eq!(seen[0].0, "Bearer sk-gem");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 协议不支持(Responses):400 人话,不打上游。
    #[tokio::test]
    async fn gemini_wire_responses_rejected_400() {
        let (base, seen) = mock_chat_upstream("{}", "application/json").await;
        let (state, providers_path, root) = make_state("gemini-wire");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem",
            crate::providers::WireApi::Responses,
        );

        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:generateContent".into()),
            req_gemini(
                "m:generateContent",
                None,
                r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}]}"#.into(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("responses"));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 透传分支:wire_api=gemini → 原样 body 打上游 /v1beta/models/…,Key 注入 x-goog-api-key,响应原样回。
    #[tokio::test]
    async fn gemini_wire_gemini_passthrough() {
        let (base, seen) = mock_gemini_upstream().await;
        let (state, providers_path, root) = make_state("gemini-pass");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem-native",
            crate::providers::WireApi::Gemini,
        );

        let body = r#"{"contents":[{"role":"user","parts":[{"text":"native"}]}]}"#;
        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("gemini-2.5-pro:generateContent".into()),
            req_gemini("gemini-2.5-pro:generateContent", None, body.to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"GEMINI_PASSTHROUGH_OK", "透传分支响应应原样");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = seen.lock().unwrap();
        let (key, up_body, ma) = &seen[0];
        assert_eq!(key, "sk-gem-native", "透传用 x-goog-api-key 注入");
        assert_eq!(up_body, body.as_bytes(), "body 必须原样透传");
        assert_eq!(
            ma, "gemini-2.5-pro:generateContent",
            "上游收到完整 model:action 路径"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 模型映射:CLI 路由恒发 gemini 系名 → 重写为供应商默认模型;未配模型则原名透传。
    #[tokio::test]
    async fn gemini_route_maps_model_to_provider_default() {
        let chat_resp = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
        let (base, seen) = mock_chat_upstream(chat_resp, "application/json").await;
        let (state, providers_path, root) = make_state("gem-model-map");
        let _gid = add_gemini_provider(
            &providers_path,
            &base,
            "sk-gem",
            crate::providers::WireApi::ChatCompletions,
        );

        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("gemini-3.5-flash:generateContent".into()),
            req_gemini(
                "gemini-3.5-flash:generateContent",
                None,
                r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}]}"#.into(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let up: serde_json::Value = {
            let seen = seen.lock().unwrap();
            serde_json::from_slice(&seen[0].1).unwrap()
        };
        assert_eq!(
            up["model"], "gemini-2.5-flash",
            "应重写为供应商默认模型:{up}"
        );

        // 供应商未配模型 → 原名透传(create 校验要求 model 非空,故直接写文件模拟历史数据)
        let (state2, providers_path2, root2) = make_state("gem-model-keep");
        std::fs::write(
            &providers_path2,
            serde_json::json!({ "providers": [{ "id": "gnm", "name": "无模型", "agent": "gemini",
                "base_url": base, "api_key": "sk-gem", "model": "", "wire_api": "chat_completions" }] }).to_string(),
        )
        .unwrap();
        let resp2 = proxy_gemini(
            State(Arc::new(state2)),
            axum::extract::Path("gemini-3.5-flash:generateContent".into()),
            req_gemini(
                "gemini-3.5-flash:generateContent",
                None,
                r#"{"contents":[{"role":"user","parts":[{"text":"x"}]}]}"#.into(),
            ),
        )
        .await;
        assert_eq!(resp2.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let up2: serde_json::Value = {
            let seen = seen.lock().unwrap();
            serde_json::from_slice(&seen[1].1).unwrap()
        };
        assert_eq!(up2["model"], "gemini-3.5-flash", "未配模型应原名透传:{up2}");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    /// streamGenerateContent 缺 ?alt=sse → 400(gemini CLI 固定 alt=sse)。
    #[tokio::test]
    async fn gemini_stream_missing_alt_400() {
        let (state, _providers_path, root) = make_state("gemini-alt");
        let resp = proxy_gemini(
            State(Arc::new(state)),
            axum::extract::Path("m:streamGenerateContent".into()),
            req_gemini("m:streamGenerateContent", None, r#"{"contents":[]}"#.into()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"]["message"].as_str().unwrap().contains("alt=sse"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 真机端到端(代替用户,惯例;手动触发:cargo test gemini_e2e_real_cli -- --ignored --nocapture):
    /// 真实 gemini CLI → 本网关(转换分支 Gemini→Chat)→ 真实 2xa.cc.cd chat 上游(OpenAI 平台 Key)→ 转回。
    /// Key 只读自 ~/.codex/providers.json 的 2xapi.cc.cd 条目;占位 Key 走 CLI→网关段,真 Key 只进网关→上游段。
    #[tokio::test]
    #[ignore]
    async fn gemini_e2e_real_cli() {
        // 1. 只读取真实 2xapi Key 与模型名
        let home = std::env::var("HOME").unwrap();
        let raw = std::fs::read_to_string(format!("{home}/.codex/providers.json"))
            .expect("读 ~/.codex/providers.json");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("解析 providers.json");
        let provs = v
            .get("providers")
            .and_then(|p| p.as_array())
            .expect("providers 数组");
        let entry = provs
            .iter()
            .find(|p| {
                let base_2xa = p
                    .get("base_url")
                    .and_then(|b| b.as_str())
                    .map(|s| s.contains("2xa"))
                    .unwrap_or(false);
                base_2xa
                    && p.get("api_key")
                        .and_then(|k| k.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
            })
            .expect("未找到带 Key 的 2xa 系供应商");
        let real_key = entry["api_key"].as_str().unwrap().to_string();
        let model = entry["model"]
            .as_str()
            .unwrap_or("deepseek-chat")
            .to_string();
        let upstream_base = entry["base_url"]
            .as_str()
            .unwrap_or("https://2xa.cc.cd")
            .to_string();
        eprintln!("[E2E] 上游 = {upstream_base} 模型 = {model}");

        // 2. 隔离环境:临时 providers.json(仅 1 个 gemini 供应商,模型=条目真实默认)+ 临时 HOME
        let (state, providers_path, root) = make_state("gemini-e2e");
        std::fs::write(
            &providers_path,
            serde_json::json!({ "providers": [{ "id": "ge2e", "name": "2xa真实站", "agent": "gemini",
                "base_url": upstream_base, "api_key": real_key, "model": model, "wire_api": "chat_completions" }] }).to_string(),
        )
        .unwrap();
        let home_e2e = root.join("cli-home");
        std::fs::create_dir_all(home_e2e.join(".gemini")).unwrap();
        std::fs::write(
            home_e2e.join(".gemini/settings.json"),
            r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#,
        )
        .unwrap();

        // 3. 起真实网关(随机端口,不与在跑 app 的 8787 冲突)
        let router = crate::server::build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(
            home_e2e.join(".gemini/.env"),
            format!("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:{port}\nGEMINI_API_KEY=sk-gateway-placeholder\nGEMINI_MODEL={model}\n"),
        )
        .unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // 4. 真实 gemini CLI:注入式启动(Key=占位,真 Key 只在网关)
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(150),
            tokio::process::Command::new("gemini")
                .arg("-p")
                .arg("请只回复两个字:收到")
                .env("HOME", &home_e2e)
                .env("GEMINI_CLI_TRUST_WORKSPACE", "true")
                .output(),
        )
        .await
        .expect("CLI 150s 超时")
        .expect("启动 gemini CLI 失败(已装?)");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!(
            "[E2E] exit={:?} stdout尾={:?} stderr尾={:?}",
            out.status.code(),
            stdout.chars().rev().take(120).collect::<String>(),
            &stderr[stderr.len().saturating_sub(200)..]
        );
        assert!(out.status.success(), "CLI 应正常退出");
        assert!(
            !stdout.trim().is_empty(),
            "应收到真实回复,stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        eprintln!("[E2E] 全链走通:gemini CLI → 网关(Gemini→Chat 转换)→ 2xa 真实上游");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// C 段文生图:200+data 透传并实证标 image_out;上游不可达 502 不标。
    #[tokio::test]
    async fn images_passthrough_marks_image_out() {
        let (base, seen) = mock_upstream(r#"{"created":1,"data":[{"b64_json":"aGk="}]}"#).await;
        let (state, providers_path, root) = make_state("img-ok");
        add_provider(&providers_path, &base, "sk-img");
        let resp = proxy_images(
            State(Arc::new(state.clone())),
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-test","prompt":"a cat"}"#.to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"][0]["b64_json"], "aGk=", "响应原样透传");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            seen.lock().unwrap().first().cloned(),
            Some("Bearer sk-img".into())
        );
        // 单维实证已停用：成功请求只记录用量，不写入能力标签。
        let summary = crate::usage_stats::summary(&state.codex_home);
        assert_eq!(summary["providers"][0]["count"], 1);
        assert_eq!(summary["providers"][0]["routes"][0], "images");
        let usage = usage_rows(&state.codex_home);
        assert_eq!(usage[0]["route"], "images");
        assert_eq!(usage[0]["line"], "direct");
        assert_eq!(usage[0]["ok"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn images_accel_uses_per_key_cred_and_logs_line() {
        let (base, upstream_seen) =
            mock_upstream(r#"{"created":1,"data":[{"url":"https://example.test/i.png"}]}"#).await;
        let (proxy_url, proxy_seen) = mock_proxy(Some(("img-user", "img-pass"))).await;
        let (state, providers_path, root) = make_state("img-accel-usage");
        add_provider(&providers_path, &base, "sk-img-accel-0001");
        put_cred(&state, "sk-img-accel-0001", "img-user", "img-pass", false);
        set_accel(
            &state,
            "official",
            vec![test_line(
                "img-line",
                &proxy_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "shared".into(),
                    pass: "shared-wrong".into(),
                }),
            )],
            "",
        );

        let resp = proxy_images(
            State(Arc::new(state.clone())),
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"gpt-test","prompt":"cat"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !proxy_seen.lock().unwrap().is_empty(),
            "图片请求应经加速代理"
        );
        assert_eq!(
            upstream_seen.lock().unwrap().first().map(String::as_str),
            Some("Bearer sk-img-accel-0001")
        );
        let summary = crate::usage_stats::summary(&state.codex_home);
        assert_eq!(summary["providers"][0]["count"], 1);
        assert_eq!(summary["providers"][0]["routes"][0], "images");
        let usage = usage_rows(&state.codex_home);
        assert_eq!(usage[0]["route"], "images");
        assert_eq!(usage[0]["line"], "img-line");
        assert_eq!(usage[0]["ok"], true);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn images_upstream_error_logs_failed_usage() {
        let (base, _seen) = mock_status_upstream(
            StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"upstream failed"}"#,
        )
        .await;
        let (state, providers_path, root) = make_state("img-error-usage");
        add_provider(&providers_path, &base, "sk-img-error");
        let resp = proxy_images(
            State(Arc::new(state.clone())),
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"gpt-test","prompt":"x"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let summary = crate::usage_stats::summary(&state.codex_home);
        assert_eq!(summary["providers"][0]["count"], 1);
        assert_eq!(summary["providers"][0]["routes"][0], "images");
        assert_eq!(summary["providers"][0]["okRate"], 0.0);
        let usage = usage_rows(&state.codex_home);
        assert_eq!(usage[0]["route"], "images");
        assert_eq!(usage[0]["line"], "direct");
        assert_eq!(usage[0]["ok"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn images_unreachable_502_no_mark() {
        let (state, providers_path, root) = make_state("img-dead");
        add_provider(&providers_path, "http://127.0.0.1:9", "sk-img2");
        let resp = proxy_images(
            State(Arc::new(state.clone())),
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-test","prompt":"x"}"#.to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Claude 接入:base_url 无 /v1 前缀 → 拼 /v1/messages;注入 x-api-key {claude api_key};body 透传。
    #[tokio::test]
    async fn anthropic_route_injects_claude_key_and_hits_v1_messages() {
        let (base, seen) = mock_anthropic_upstream().await;
        let (state, providers_path, root) = make_state("anthropic");
        let _id = add_claude_provider(&providers_path, &base, "sk-claude-secret");

        let resp = proxy_anthropic(
            State(Arc::new(state)),
            req_post_anthropic("{\"model\":\"claude-sonnet\"}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.first().map(|(a, _)| a.as_str()),
            Some("sk-claude-secret")
        );
        assert_eq!(seen.first().map(|(_, p)| p.as_str()), Some("/v1/messages"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Claude 接入:base_url 已带 /v1 后缀 → 拼 /messages(不产生 /v1/v1/messages)。
    #[tokio::test]
    async fn anthropic_route_base_url_v1_suffix_still_hits_v1_messages() {
        let (base, seen) = mock_anthropic_upstream().await;
        let (state, providers_path, root) = make_state("anthropic-v1");
        let _id = add_claude_provider(&providers_path, &format!("{}/v1", base), "sk-claude-v1");

        let resp = proxy_anthropic(State(Arc::new(state)), req_post_anthropic("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = seen.lock().unwrap();
        // 命中 /v1/messages(而非 /v1/v1/messages 或 /messages)
        assert_eq!(
            seen.first().map(|(a, p)| (a.as_str(), p.as_str())),
            Some(("sk-claude-v1", "/v1/messages"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Claude 接入:全局 active 是 codex,但存在 claude 供应商 → /anthropic/* 仍取 claude(不串台)。
    #[tokio::test]
    async fn anthropic_route_uses_claude_provider_even_when_active_is_codex() {
        let (base, seen) = mock_anthropic_upstream().await;
        let (state, providers_path, root) = make_state("anthropic-isolate");
        // codex 供应商(active)+ claude 供应商(不 active)
        let input_cx = ProviderInput {
            name: "Cx".into(),
            agent: "codex".into(),
            base_url: "http://127.0.0.1:9".into(), // 若误发到 codex 会立刻失败
            api_key: "sk-codex-key".into(),
            model: "gpt-test".into(),
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p_cx = providers::create(&providers_path, input_cx).unwrap();
        let _p_cl = add_claude_provider(&providers_path, &base, "sk-claude-secret");
        // 全局 active 保持 codex
        providers::set_active(&providers_path, &p_cx.id);

        let resp = proxy_anthropic(State(Arc::new(state)), req_post_anthropic("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            seen.lock().unwrap().first().map(|(a, _)| a.as_str()),
            Some("sk-claude-secret")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Claude 接入:无 claude 供应商 → 503。
    #[tokio::test]
    async fn anthropic_route_no_claude_provider_returns_503() {
        let (state, _providers_path, root) = make_state("anthropic-none");
        let resp = proxy_anthropic(State(Arc::new(state)), req_post_anthropic("{}").await).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Hermes 接入(/hermes/chat/completions 专属入口,共用 dispatch)──

    async fn mock_hermes_chat_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        // 双路径监听:dispatch 对 base 不带 /v1 的 chat 上游补 /v1(2026-08-16 修正),两写都接
        let make_handler = {
            let seen = seen_clone.clone();
            move || {
                let seen = seen.clone();
                move |h: axum::http::HeaderMap, _b: axum::body::Bytes| {
                    let seen = seen.clone();
                    async move {
                        let auth = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from)
                            .unwrap_or_default();
                        seen.lock().unwrap().push(auth);
                        (StatusCode::OK, "{\"id\":\"chatcmpl-ok\"}")
                    }
                }
            }
        };
        let app = Router::new()
            .route("/chat/completions", post(make_handler()))
            .route("/v1/chat/completions", post(make_handler()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{}", addr), seen)
    }

    async fn req_post_hermes(body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/hermes/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    fn add_hermes_provider(path: &std::path::Path, base_url: &str, api_key: &str) -> String {
        let input = ProviderInput {
            name: "HermesT".into(),
            agent: "hermes".into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: "glm-5".into(),
            wire_api: crate::providers::WireApi::ChatCompletions,
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p = providers::create(path, input).unwrap();
        providers::set_active(path, &p.id);
        p.id
    }

    /// Hermes 接入:全局 active 是 codex,但存在 hermes 供应商 → /hermes/* 仍取 hermes(不串台)。
    #[tokio::test]
    async fn hermes_route_uses_hermes_provider_even_when_active_is_codex() {
        let (base, seen) = mock_hermes_chat_upstream().await;
        let (state, providers_path, root) = make_state("hermes-isolate");
        let input_cx = ProviderInput {
            name: "Cx".into(),
            agent: "codex".into(),
            base_url: "http://127.0.0.1:9".into(), // 若误发到 codex 会立刻失败
            api_key: "sk-codex-key".into(),
            model: "gpt-test".into(),
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p_cx = providers::create(&providers_path, input_cx).unwrap();
        let _p_hm = add_hermes_provider(&providers_path, &base, "sk-hermes-secret");
        providers::set_active(&providers_path, &p_cx.id);

        let resp = proxy_hermes_chat(
            State(Arc::new(state)),
            req_post_hermes("{\"model\":\"glm-5\"}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"{\"id\":\"chatcmpl-ok\"}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            seen.lock().unwrap().first().map(|a| a.as_str()),
            Some("Bearer sk-hermes-secret")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hermes 接入:Responses 型供应商 → 400 人话错误(不静默)。
    #[tokio::test]
    async fn hermes_route_rejects_responses_wire_provider() {
        let (state, providers_path, root) = make_state("hermes-resp");
        let input = ProviderInput {
            name: "HmR".into(),
            agent: "hermes".into(),
            base_url: "http://127.0.0.1:9".into(),
            api_key: "sk-x".into(),
            model: "m".into(),
            wire_api: crate::providers::WireApi::Responses, // 默认值,但显式标注意图
            sub2api_multiplier: 1.0,
            ..ProviderInput::default()
        };
        let p = providers::create(&providers_path, input).unwrap();
        providers::set_active(&providers_path, &p.id);

        let resp = proxy_hermes_chat(State(Arc::new(state)), req_post_hermes("{}").await).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("Hermes 通路暂不支持"), "人话错误: {body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// agent_label 注册表化(opencode 真机验收发现:此前误报「请先选择 Codex 供应商」)。
    #[tokio::test]
    async fn dispatch_503_uses_agent_registry_label() {
        let (state, _providers_path, root) = make_state("label-opencode");
        let req = Request::builder()
            .method("POST")
            .uri("/opencode/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = proxy_opencode_chat(State(Arc::new(state)), req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("请先选择 OpenCode 供应商"),
            "应报平台名: {body}"
        );
        assert!(!body.contains("Codex 供应商"), "不得误报 Codex: {body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn req_post_chat(body: &'static str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    /// 纵深防御(真机实证两轮:base_url 缺 /v1 时上游回 200 text/html Web UI)→ 不透传,
    /// 客户端拿到人话错误而非 HTML。
    #[tokio::test]
    async fn chat_html_upstream_returns_human_error_not_html() {
        let html =
            "<!DOCTYPE html><html><head><title>2xa</title></head><body>welcome</body></html>";
        let (base, _seen) = mock_chat_upstream(html, "text/html").await;
        let (state, providers_path, root) = make_state("html-def");
        add_provider(&providers_path, &base, "sk-html");

        let resp = proxy_chat(
            State(Arc::new(state)),
            req_post_chat(r#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#)
                .await,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "HTML 上游应 502 人话错误,不得透传 200 HTML"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("上游返回了网页内容") && body.contains("/v1"),
            "应为人话错误: {body}"
        );
        assert!(!body.contains("<!DOCTYPE"), "不得透传 HTML: {body}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 回归:正常 application/json 上游不受 HTML 拦截影响,原样透传。
    #[tokio::test]
    async fn chat_json_upstream_still_passthrough() {
        let chat_resp = r#"{"id":"c1","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}"#;
        let (base, _seen) = mock_chat_upstream(chat_resp, "application/json").await;
        let (state, providers_path, root) = make_state("html-ok");
        add_provider(&providers_path, &base, "sk-json");

        let resp = proxy_chat(
            State(Arc::new(state)),
            req_post_chat(r#"{"model":"gpt-test","messages":[]}"#).await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], chat_resp.as_bytes(), "JSON 响应应原样透传");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hermes 接入:无 hermes 供应商 → 503。
    #[tokio::test]
    async fn hermes_route_no_hermes_provider_returns_503() {
        let (state, _providers_path, root) = make_state("hermes-none");
        let resp = proxy_hermes_chat(State(Arc::new(state)), req_post_hermes("{}").await).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── R1 /anthropic 加速接线(与 Codex 同一体系,四个必测场景)──

    // ⑴ anthropic + official 命中 → 请求经代理转发到上游。mock 代理校验 Basic auth
    // (Proxy-Authorization 不符即 407);legacy 非 per-Key,407 即 502——故 200 + 上游命中
    // 即证明线路凭证 Basic auth 已正确送达代理。
    #[tokio::test]
    async fn anthropic_accel_hit_routes_through_proxy_with_basic_auth() {
        let _g = crate::server::set_issue_base_for_tests(crate::server::DEAD_ISSUE_BASE);
        let (up_base, up_seen) = mock_anthropic_upstream().await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "p"))).await;
        let (state, providers_path, root) = make_state("anth-accel-hit");
        add_claude_provider(&providers_path, &up_base, "sk-claude-line");
        state.nodecreds.write().unwrap().legacy = Some(Cred {
            user: "u".into(),
            pass: "p".into(),
        });
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_anthropic(
            State(Arc::new(state)),
            req_post_anthropic("{\"model\":\"claude-sonnet\"}").await,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "命中线应经代理(Basic auth 校验通过)返回 200"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !px_seen.lock().unwrap().is_empty(),
            "anthropic 命中线:代理应看到经其转发的请求"
        );
        assert_eq!(
            up_seen
                .lock()
                .unwrap()
                .first()
                .map(|(a, p)| (a.as_str(), p.as_str())),
            Some(("sk-claude-line", "/v1/messages")),
            "上游应经代理收到请求并保留 x-api-key 注入"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑵ 未命中 scope → 直连,代理零请求。
    #[tokio::test]
    async fn anthropic_accel_no_match_falls_back_direct() {
        let (up_base, up_seen) = mock_anthropic_upstream().await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "p"))).await;
        let (state, providers_path, root) = make_state("anth-accel-nomatch");
        add_claude_provider(&providers_path, &up_base, "sk-claude-direct");
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["not-this-host.com"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_anthropic(State(Arc::new(state)), req_post_anthropic("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(px_seen.lock().unwrap().is_empty(), "未命中不应经代理");
        assert_eq!(
            up_seen.lock().unwrap().first().map(|(a, _)| a.as_str()),
            Some("sk-claude-direct"),
            "直连应命中上游"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑶ 线路坏(连接即断)→ 换直连重试一次,响应完整。legacy 在册 → accel_plan 保留
    // 共享凭证线,首发真打坏代理失败(非 407)→ 直连兜底。
    #[tokio::test]
    async fn anthropic_accel_bad_line_retries_direct_and_response_complete() {
        let bad = broken_proxy().await;
        let (up_base, up_seen) = mock_anthropic_upstream().await;
        let (state, providers_path, root) = make_state("anth-accel-badline");
        add_claude_provider(&providers_path, &up_base, "sk-claude-retry");
        state.nodecreds.write().unwrap().legacy = Some(Cred {
            user: "u".into(),
            pass: "p".into(),
        });
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &bad,
                &["127.0.0.1"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_anthropic(State(Arc::new(state)), req_post_anthropic("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1", "坏线换直连后响应应完整");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            up_seen.lock().unwrap().len(),
            1,
            "直连重试应恰好命中上游一次"
        );
        let usage = usage_rows(&root.join("codex"));
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0]["line"], "direct");
        assert_eq!(usage[0]["degraded_to_direct"], true);
        assert_eq!(usage[0]["ok"], true);
        let summary = crate::usage_stats::summary(&root.join("codex"));
        assert_eq!(summary["providers"][0]["directFallbackCount"], 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑷ per-Key 407 → 重签判得配额满 → store 记降级,本请求降级直连且响应完整。
    #[tokio::test]
    async fn anthropic_per_key_407_quota_full_degrades_direct() {
        let issue = crate::server::spawn_issue_mock(
            "403 Forbidden",
            r#"{"error":"该账号本月已用满 10G","quotaUsedBytes":777,"quotaTotalBytes":888}"#,
        )
        .await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let (up_base, up_seen) = mock_anthropic_upstream().await;
        let (px_url, px_seen) = mock_proxy(Some(("right", "right"))).await;
        let (state, providers_path, root) = make_state("anth-pk-403");
        add_claude_provider(&providers_path, &up_base, "sk-claude-full-0006");
        put_cred(
            &state,
            "sk-claude-full-0006",
            "stale-user",
            "stale-pass",
            false,
        ); // 代理侧为错 → 407
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "x".into(),
                    pass: "y".into(),
                }),
            )],
            "",
        );

        let resp = proxy_anthropic(
            State(Arc::new(state.clone())),
            req_post_anthropic("{}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "配额满降级直连应 200");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ANTHROPIC_OK_V1", "降级直连响应应完整");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(up_seen.lock().unwrap().len(), 1, "直连重试恰好命中上游一次");
        assert!(
            !px_seen.lock().unwrap().is_empty(),
            "首发应打到代理(收到 407)"
        );
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key("sk-claude-full-0006")
            .cloned()
            .unwrap();
        assert!(
            entry.degraded_to_direct,
            "QuotaFull 应记 degraded_to_direct"
        );
        assert_eq!(entry.quota_used_bytes, 777, "快照 used 应回写");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// gemini per-Key 407/配额满:降级直连且响应转换回 Gemini 形态(对齐 anthropic 同款测试)。
    #[tokio::test]
    async fn gemini_per_key_407_quota_full_degrades_direct() {
        let issue = crate::server::spawn_issue_mock(
            "403 Forbidden",
            r#"{"error":"该账号本月已用满 10G","quotaUsedBytes":777,"quotaTotalBytes":888}"#,
        )
        .await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let chat_resp = r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"收到"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#;
        let (up_base, up_seen) = mock_chat_upstream(chat_resp, "application/json").await;
        let (px_url, px_seen) = mock_proxy(Some(("right", "right"))).await;
        let (state, providers_path, root) = make_state("gem-pk-403");
        add_gemini_provider(
            &providers_path,
            &up_base,
            "sk-gem-full-0006",
            crate::providers::WireApi::ChatCompletions,
        );
        put_cred(
            &state,
            "sk-gem-full-0006",
            "stale-user",
            "stale-pass",
            false,
        ); // 代理侧为错 → 407
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "x".into(),
                    pass: "y".into(),
                }),
            )],
            "",
        );

        let resp = proxy_gemini(
            State(Arc::new(state.clone())),
            axum::extract::Path("m:generateContent".into()),
            req_gemini(
                "m:generateContent",
                None,
                r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#.into(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "配额满降级直连应 200");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["candidates"][0]["content"]["parts"][0]["text"], "收到",
            "降级直连响应应完整转换回 Gemini 形态:\n{v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(up_seen.lock().unwrap().len(), 1, "直连重试恰好命中上游一次");
        assert!(
            !px_seen.lock().unwrap().is_empty(),
            "首发应打到代理(收到 407)"
        );
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key("sk-gem-full-0006")
            .cloned()
            .unwrap();
        assert!(
            entry.degraded_to_direct,
            "QuotaFull 应记 degraded_to_direct"
        );
        assert_eq!(entry.quota_used_bytes, 777, "快照 used 应回写");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn clone_state(s: &AppState) -> AppState {
        AppState {
            config_path: s.config_path.clone(),
            backup_dir: s.backup_dir.clone(),
            providers_path: s.providers_path.clone(),
            codex_home: s.codex_home.clone(),
            wb_home: s.wb_home.clone(),
            hermes_home: s.hermes_home.clone(),
            gem_home: s.gem_home.clone(),
            grok_home: s.grok_home.clone(),
            oc_home: s.oc_home.clone(),
            oclaw_home: s.oclaw_home.clone(),
            cd_home: s.cd_home.clone(),
            cursor_home: s.cursor_home.clone(),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: s.tray_gate_enabled.clone(),
            launcher: s.launcher.clone(),
            health: s.health.clone(),
            accel: s.accel.clone(),
            nodecreds: s.nodecreds.clone(),
        }
    }

    // ── 阶段 4 加速装配:mock 代理集成测试(任务书 §五 必测)──

    fn test_line(id: &str, endpoint: &str, scope: &[&str], cred: Option<Cred>) -> AccLine {
        AccLine {
            id: id.into(),
            name: id.into(),
            endpoint: endpoint.into(),
            scope: scope.iter().map(|s| s.to_string()).collect(),
            priority: 1,
            enabled: true,
            credential: cred,
        }
    }

    fn set_accel(state: &AppState, mode: &str, lines: Vec<AccLine>, custom_node: &str) {
        *state.accel.lock().unwrap() = crate::server::AccelCfg {
            mode: mode.into(),
            custom_node: custom_node.into(),
        };
        state.health.set_lines(lines);
    }

    fn b64(data: &str) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = data.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.len();
            let mut v = [0u8; 3];
            v[..n].copy_from_slice(chunk);
            let x = ((v[0] as u32) << 16) | ((v[1] as u32) << 8) | (v[2] as u32);
            out.push(T[((x >> 18) & 63) as usize] as char);
            out.push(T[((x >> 12) & 63) as usize] as char);
            out.push(if n > 1 {
                T[((x >> 6) & 63) as usize] as char
            } else {
                '='
            });
            out.push(if n > 2 {
                T[(x & 63) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    fn auth_ok(head: &str, required: &Option<(String, String)>) -> bool {
        let Some((u, p)) = required else { return true };
        let expected = format!("Basic {}", b64(&format!("{}:{}", u, p)));
        head.lines().any(|l| {
            let low = l.to_ascii_lowercase();
            if !(low.starts_with("proxy-authorization:") || low.starts_with("authorization:")) {
                return false;
            }
            l.split_once(':').map(|x| x.1).map(str::trim) == Some(expected.as_str())
        })
    }

    fn content_length(head: &str) -> Option<usize> {
        head.lines().find_map(|l| {
            if l.to_ascii_lowercase().starts_with("content-length:") {
                l.split_once(':')
                    .map(|x| x.1)
                    .and_then(|s| s.trim().parse().ok())
            } else {
                None
            }
        })
    }

    fn split_host_port(s: &str, def: u16) -> (String, u16) {
        if let Some(i) = s.rfind(':') {
            if let Ok(port) = s[i + 1..].parse::<u16>() {
                return (s[..i].to_string(), port);
            }
        }
        (s.to_string(), def)
    }

    fn find_head_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
    }

    /// 读取 HTTP 头(到 \r\n\r\n 止),返回 (head 字符串, 该次读取中头之后的剩余字节)。
    /// 剩余字节必须回传——否则头读取会吞掉紧跟其后的 body 字节。
    async fn read_http_head<R: tokio::io::AsyncBufRead + Unpin>(
        r: &mut R,
    ) -> std::io::Result<(String, Vec<u8>)> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let end;
        loop {
            let n = r.read(&mut tmp).await?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof in head",
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_head_end(&buf) {
                end = pos;
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let rest = buf[end..].to_vec();
        Ok((head, rest))
    }

    /// 按 Content-Length 补齐请求/响应体;rest 为头读取时已缓冲的剩余字节。
    async fn read_body<R: tokio::io::AsyncBufRead + Unpin>(
        r: &mut R,
        head: &str,
        mut rest: Vec<u8>,
    ) -> Vec<u8> {
        let clen = content_length(head).unwrap_or(0);
        if clen > rest.len() {
            let mut extra = vec![0u8; clen - rest.len()];
            let _ = r.read_exact(&mut extra).await;
            rest.extend_from_slice(&extra);
        }
        rest.truncate(clen);
        rest
    }

    /// mock 代理:支持 CONNECT 隧道 + HTTP 绝对式转发;需 basic-auth(auth=Some 时校验
    /// Proxy-Authorization/Authorization 头,不符返回 407)。seen 记录收到的方法与目标。
    async fn handle_proxy_conn(
        sock: tokio::net::TcpStream,
        auth: Option<(String, String)>,
        seen: Arc<Mutex<Vec<String>>>,
    ) {
        let mut br = tokio::io::BufReader::new(sock);
        let (head, rest) = match read_http_head(&mut br).await {
            Ok(h) => h,
            Err(_) => return,
        };
        let lines: Vec<&str> = head.split("\r\n").collect();
        let first_line = lines.first().map(|s| s.to_string()).unwrap_or_default();
        let mut it = first_line.split_whitespace();
        let method = it.next().unwrap_or("").to_string();
        let target = it.next().unwrap_or("").to_string();
        if method.is_empty() {
            return;
        }

        if method == "CONNECT" {
            seen.lock().unwrap().push(format!("CONNECT {target}"));
            if !auth_ok(&head, &auth) {
                let _ = br.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                return;
            }
            let _ = br
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await;
            let (host, port) = split_host_port(&target, 443);
            if let Ok(up) = tokio::net::TcpStream::connect((host.as_str(), port)).await {
                let sock = br.into_inner();
                let (mut cr, mut cw) = sock.into_split();
                let (mut ur, mut uw) = up.into_split();
                let _ = tokio::join!(
                    tokio::io::copy(&mut cr, &mut uw),
                    tokio::io::copy(&mut ur, &mut cw)
                );
            }
            return;
        }

        // HTTP 绝对式转发
        seen.lock().unwrap().push(format!("{method} {target}"));
        if !auth_ok(&head, &auth) {
            let body = b"proxy auth required";
            let resp = format!(
                "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            );
            let _ = br.write_all(resp.as_bytes()).await;
            return;
        }
        let body = read_body(&mut br, &head, rest).await;

        let after = target
            .find("://")
            .map(|i| &target[i + 3..])
            .unwrap_or(&target);
        let host_port = after.split('/').next().unwrap_or("").to_string();
        let (host, port) = split_host_port(&host_port, 80);
        let mut up = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
            Ok(u) => u,
            Err(_) => {
                let _ = br.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                return;
            }
        };
        // 重建请求行为 origin-form,去掉 Proxy-Authorization(代理自身的鉴权头不下发上游)
        let path = if after.len() >= host_port.len() {
            &after[host_port.len()..]
        } else {
            ""
        };
        let path = if path.is_empty() { "/" } else { path };
        let mut out = String::new();
        out.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
        for l in lines.iter().skip(1) {
            if l.is_empty() {
                break;
            }
            if l.to_ascii_lowercase().starts_with("proxy-authorization") {
                continue;
            }
            out.push_str(l);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        let _ = up.write_all(out.as_bytes()).await;
        if !body.is_empty() {
            let _ = up.write_all(&body).await;
        }
        // 回传上游响应
        let mut up_br = tokio::io::BufReader::new(up);
        let (resp_head, rest) = match read_http_head(&mut up_br).await {
            Ok(h) => h,
            Err(_) => return,
        };
        let rbody = read_body(&mut up_br, &resp_head, rest).await;
        let _ = br.write_all(resp_head.as_bytes()).await;
        let _ = br.write_all(&rbody).await;
    }

    async fn mock_proxy(auth: Option<(&str, &str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let required = auth.map(|(u, p)| (u.to_string(), p.to_string()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let seen2 = seen2.clone();
                let req = required.clone();
                tokio::spawn(async move {
                    handle_proxy_conn(sock, req, seen2).await;
                });
            }
        });
        (format!("http://{}", addr), seen)
    }

    /// 坏代理:接受连接后立即关闭(连接层失败 → 应触发换直连重试;确定性,无端口竞争)。
    async fn broken_proxy() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                drop(sock);
            }
        });
        format!("http://{}", addr)
    }

    /// 挂起代理:读掉请求后保持连接但不回应(触发超时)。
    async fn hang_proxy() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await; // 消费请求,确保写完成
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let _ = sock;
                });
            }
        });
        format!("http://{}", addr)
    }

    // ① 命中走代理(legacy 共享凭证场景):上游应经代理转发收到请求。
    // 星图后:无 per-Key 项但有 legacy → 保留共享凭证;签发外呼指向死端口(Unreachable)不干扰。
    #[tokio::test]
    async fn accel_hit_routes_through_proxy() {
        let _g = crate::server::set_issue_base_for_tests(crate::server::DEAD_ISSUE_BASE);
        let (up_base, up_seen) = mock_upstream("PROXIED_BODY").await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "p"))).await;
        let (state, providers_path, root) = make_state("accel-hit");
        add_provider(&providers_path, &up_base, "sk-line");
        state.nodecreds.write().unwrap().legacy = Some(Cred {
            user: "u".into(),
            pass: "p".into(),
        });
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(
            State(Arc::new(state)),
            req_post_responses("{\"hello\":1}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"PROXIED_BODY");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !px_seen.lock().unwrap().is_empty(),
            "代理应看到经其转发的请求"
        );
        assert_eq!(
            up_seen.lock().unwrap().first().map(|s| s.as_str()),
            Some("Bearer sk-line")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ② 不命中 → 回落直连:代理不应看到任何请求。
    #[tokio::test]
    async fn accel_no_match_falls_back_direct() {
        let (up_base, up_seen) = mock_upstream("DIRECT_BODY").await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "p"))).await;
        let (state, providers_path, root) = make_state("accel-nomatch");
        add_provider(&providers_path, &up_base, "sk-direct");
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["not-this-host.com"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"DIRECT_BODY");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(px_seen.lock().unwrap().is_empty(), "未命中不应经代理");
        assert_eq!(
            up_seen.lock().unwrap().first().map(|s| s.as_str()),
            Some("Bearer sk-direct")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ③ 坏线(代理连不上)→ 自动换直连重试,响应完整不断流。
    // 星图后:无凭证项 → 确保段向死端口签发(Unreachable)跳线,坏线本就连不上 → 直连兜底不变。
    #[tokio::test]
    async fn accel_bad_line_retries_direct_and_stream_complete() {
        let _g = crate::server::set_issue_base_for_tests(crate::server::DEAD_ISSUE_BASE);
        let (up_base, up_seen) = mock_upstream("FULL_STREAM_BODY_1234567890").await;
        let bad = broken_proxy().await;
        let (state, providers_path, root) = make_state("accel-badline");
        add_provider(&providers_path, &up_base, "sk-retry");
        set_accel(
            &state,
            "official",
            vec![test_line("l1", &bad, &["127.0.0.1"], None)],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            &bytes[..],
            b"FULL_STREAM_BODY_1234567890",
            "坏线换直连后响应应完整"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            up_seen.lock().unwrap().len(),
            1,
            "直连重试应恰好命中上游一次"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ④ legacy 凭证错误 → 代理 407 → 错误人话化(且不换直连绕过线路)。星图后 legacy 407 行为不变。
    #[tokio::test]
    async fn accel_wrong_cred_proxy_407_humanized() {
        let _g = crate::server::set_issue_base_for_tests(crate::server::DEAD_ISSUE_BASE);
        let (up_base, up_seen) = mock_upstream("SHOULD_NOT_REACH").await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "right"))).await;
        let (state, providers_path, root) = make_state("accel-407");
        add_provider(&providers_path, &up_base, "sk-wrong");
        // legacy 在册(老用户)→ 线路保留共享凭证;该凭证在代理侧为错
        state.nodecreds.write().unwrap().legacy = Some(Cred {
            user: "u".into(),
            pass: "wrong".into(),
        });
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "u".into(),
                    pass: "wrong".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("节点凭证无效"),
            "407 应人话化为节点凭证无效, got {s}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            up_seen.lock().unwrap().is_empty(),
            "凭证错误不应换直连命中上游"
        );
        assert!(!px_seen.lock().unwrap().is_empty(), "代理应看到请求");
        let usage = usage_rows(&root.join("codex"));
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0]["line"], "l1");
        assert_eq!(usage[0]["degraded_to_direct"], false);
        assert_eq!(usage[0]["ok"], false);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 星图 任务 B:per-Key 凭证覆盖 + 407 判别 + 降级 ──

    /// 往 store 放该 key 的 per-Key 项(issued_at=now,即「新鲜」)。
    fn put_cred(state: &AppState, api_key: &str, user: &str, pass: &str, degraded: bool) {
        let mut c = crate::server::test_node_cred(user, pass);
        c.degraded_to_direct = degraded;
        state.nodecreds.write().unwrap().set_for_key(api_key, c);
    }

    // ⑤ per-Key 覆盖:store 有新鲜项 → 代理请求带该凭证(而非线路共享凭证)。
    // 共享凭证在代理侧为错;若覆盖失效会 407→判别→mock 签发 401→502,断言 200 即证明覆盖生效。
    #[tokio::test]
    async fn per_key_cred_overrides_shared_line_cred() {
        let issue = crate::server::spawn_issue_mock("401 Unauthorized", r#"{"error":"x"}"#).await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let (up_base, up_seen) = mock_upstream("PK_BODY").await;
        let (px_url, px_seen) = mock_proxy(Some(("pk-user", "pk-pass"))).await;
        let (state, providers_path, root) = make_state("pk-override");
        add_provider(&providers_path, &up_base, "sk-pk-override-0001");
        put_cred(&state, "sk-pk-override-0001", "pk-user", "pk-pass", false);
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "shared".into(),
                    pass: "shared-wrong".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK, "per-Key 凭证应被代理接受");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"PK_BODY");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!px_seen.lock().unwrap().is_empty(), "应经代理转发");
        assert_eq!(up_seen.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑥ per-Key 407 → 重签判得配额满 → store 记降级,本请求改直连且响应完整。
    #[tokio::test]
    async fn per_key_407_quota_full_degrades_direct() {
        let issue = crate::server::spawn_issue_mock(
            "403 Forbidden",
            r#"{"error":"该账号本月已用满 10G","quotaUsedBytes":777,"quotaTotalBytes":888}"#,
        )
        .await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let (up_base, up_seen) = mock_upstream("DIRECT_FULL_BODY_9876543210").await;
        let (px_url, px_seen) = mock_proxy(Some(("right", "right"))).await;
        let (state, providers_path, root) = make_state("pk-403");
        add_provider(&providers_path, &up_base, "sk-pk-full-0002");
        put_cred(&state, "sk-pk-full-0002", "stale-user", "stale-pass", false); // 代理侧为错 → 407
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "x".into(),
                    pass: "y".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(
            State(Arc::new(state.clone())),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            &bytes[..],
            b"DIRECT_FULL_BODY_9876543210",
            "配额满应降级直连且响应完整"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(up_seen.lock().unwrap().len(), 1, "直连重试恰好命中上游一次");
        assert!(
            !px_seen.lock().unwrap().is_empty(),
            "首发应打到代理(收到 407)"
        );
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key("sk-pk-full-0002")
            .cloned()
            .unwrap();
        assert!(
            entry.degraded_to_direct,
            "QuotaFull 应记 degraded_to_direct"
        );
        assert_eq!(entry.quota_used_bytes, 777, "快照 used 应回写");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑦ per-Key 407 → 重签 Ok → 新凭证重建线客户端,原请求重试一次成功。
    #[tokio::test]
    async fn per_key_407_reissue_retries_with_new_cred() {
        let issue = crate::server::spawn_issue_mock(
            "200 OK",
            r#"{"user":"fresh-user","pass":"fresh-pass","quotaTotalBytes":50,"quotaUsedBytes":10,"proxyEndpoint":"http://n"}"#,
        )
        .await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let (up_base, up_seen) = mock_upstream("REISSUE_OK").await;
        let (px_url, px_seen) = mock_proxy(Some(("fresh-user", "fresh-pass"))).await;
        let (state, providers_path, root) = make_state("pk-reissue");
        add_provider(&providers_path, &up_base, "sk-pk-reissue-0003");
        put_cred(&state, "sk-pk-reissue-0003", "old-user", "old-pass", false); // 代理侧为错 → 407
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "x".into(),
                    pass: "y".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(
            State(Arc::new(state.clone())),
            req_post_responses("{}").await,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "重签新凭证重试应成功");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"REISSUE_OK");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(up_seen.lock().unwrap().len(), 1);
        assert!(
            px_seen.lock().unwrap().len() >= 2,
            "首发 407 + 新凭证重试,代理至少见两次"
        );
        let entry = state
            .nodecreds
            .read()
            .unwrap()
            .get_for_key("sk-pk-reissue-0003")
            .cloned()
            .unwrap();
        assert_eq!(
            entry.quota_total_bytes, 50,
            "重签后 store 应更新为新凭证配额"
        );
        assert!(!entry.degraded_to_direct);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑧ per-Key 407 → 重签判得 Key 无效 → 维持 502 人话化(不绕过线路)。
    #[tokio::test]
    async fn per_key_407_key_invalid_keeps_502() {
        let issue =
            crate::server::spawn_issue_mock("401 Unauthorized", r#"{"error":"Key 无效或未充值"}"#)
                .await;
        let _g = crate::server::set_issue_base_for_tests(&issue);
        let (up_base, up_seen) = mock_upstream("SHOULD_NOT_REACH").await;
        let (px_url, px_seen) = mock_proxy(Some(("right", "right"))).await;
        let (state, providers_path, root) = make_state("pk-401");
        add_provider(&providers_path, &up_base, "sk-pk-invalid-0004");
        put_cred(
            &state,
            "sk-pk-invalid-0004",
            "stale-user",
            "stale-pass",
            false,
        );
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "x".into(),
                    pass: "y".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("节点凭证无效"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            up_seen.lock().unwrap().is_empty(),
            "KeyInvalid 不应绕线直连命中上游"
        );
        assert!(!px_seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ⑨ degraded_to_direct → 该请求直接走直连,代理零请求。
    #[tokio::test]
    async fn degraded_entry_goes_direct_zero_proxy_hits() {
        let _g = crate::server::set_issue_base_for_tests(crate::server::DEAD_ISSUE_BASE);
        let (up_base, up_seen) = mock_upstream("DEGRADED_DIRECT_OK").await;
        let (px_url, px_seen) = mock_proxy(Some(("u", "p"))).await;
        let (state, providers_path, root) = make_state("pk-degraded");
        add_provider(&providers_path, &up_base, "sk-pk-degraded-0005");
        put_cred(&state, "sk-pk-degraded-0005", "u", "p", true); // 已降级
        set_accel(
            &state,
            "official",
            vec![test_line(
                "l1",
                &px_url,
                &["127.0.0.1"],
                Some(Cred {
                    user: "u".into(),
                    pass: "p".into(),
                }),
            )],
            "",
        );

        let resp = proxy_responses(State(Arc::new(state)), req_post_responses("{}").await).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"DEGRADED_DIRECT_OK");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(px_seen.lock().unwrap().is_empty(), "已降级:代理应零请求");
        assert_eq!(up_seen.lock().unwrap().len(), 1, "直连恰好命中上游一次");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── test-node 探测(核心函数,路由 /api/accel/test-node 复用)──

    #[tokio::test]
    async fn test_node_via_ok_through_proxy() {
        let (up_base, _) = mock_upstream("OK").await;
        let (px_url, _) = mock_proxy(Some(("u", "p"))).await;
        let cred = Cred {
            user: "u".into(),
            pass: "p".into(),
        };
        let out = test_node_via(&px_url, &up_base, Some(&cred), Duration::from_secs(5)).await;
        match out {
            NodeTestOutcome::Ok { .. } => {} // 本地 mock 可能 0ms,不苛求 latency 具体值
            other => panic!("应成功, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_node_via_wrong_cred_407_auth() {
        let (px_url, _) = mock_proxy(Some(("u", "right"))).await;
        let cred = Cred {
            user: "u".into(),
            pass: "wrong".into(),
        };
        let out = test_node_via(
            &px_url,
            "https://api.2xa.cc.cd/models",
            Some(&cred),
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(out, NodeTestOutcome::Auth),
            "凭证错误应判 Auth, got {out:?}"
        );
    }

    #[tokio::test]
    async fn test_node_via_timeout() {
        let hang = hang_proxy().await;
        let (up_base, _) = mock_upstream("OK").await;
        let out = test_node_via(&hang, &up_base, None, Duration::from_millis(400)).await;
        assert!(
            matches!(out, NodeTestOutcome::Timeout),
            "代理挂起应超时, got {out:?}"
        );
    }
}

#[cfg(test)]
mod verify_wb_path {
    use super::*;
    use crate::gateway::tests::{claude_desktop_mapping_provider, make_state};
    use axum::routing::post;
    use axum::Router;
    use std::sync::{Arc, Mutex};

    // 实证:workbuddy chat 入口对 base 不带 /v1 的上游应发 /v1/chat/completions(0c89f3a 修复)
    #[tokio::test]
    async fn workbuddy_chat_upstream_gets_v1_path() {
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hits_c = hits.clone();
        let hits_bare = hits.clone();
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(move |uri: axum::http::Uri, _b: axum::body::Bytes| {
                    let h = hits_c.clone();
                    async move {
                        h.lock().unwrap().push(format!("V1:{}", uri.path()));
                        (StatusCode::OK, "{\"ok\":1}")
                    }
                }),
            )
            .route(
                "/chat/completions",
                post(move |uri: axum::http::Uri, _b: axum::body::Bytes| {
                    let h = hits_bare.clone();
                    async move {
                        h.lock().unwrap().push(format!("BARE:{}", uri.path()));
                        (StatusCode::OK, "{\"ok\":1}")
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let root = std::env::temp_dir().join(format!("wb-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("providers.json"),
            serde_json::json!({
                "providers": [{"id": "wpx", "name": "x", "agent": "workbuddy",
                    "base_url": format!("http://{addr}"), "api_key": "sk-x", "model": "m1",
                    "wire_api": "chat_completions"}]
            })
            .to_string(),
        )
        .unwrap();
        let state = AppState {
            config_path: root.join("c.toml"),
            backup_dir: root.join("bk"),
            providers_path: root.join("providers.json"),
            codex_home: root.join("codex"),
            wb_home: root.clone(),
            hermes_home: root.clone(),
            gem_home: root.clone(),
            grok_home: root.clone(),
            oc_home: root.clone(),
            oclaw_home: root.clone(),
            cd_home: root.clone(),
            cursor_home: root.clone(),
            launcher: Default::default(),
            health: Arc::new(crate::acclines::HealthState::new(vec![])),
            accel: Arc::new(Mutex::new(crate::server::AccelCfg::default())),
            nodecreds: Arc::new(std::sync::RwLock::new(crate::nodecreds::Store::empty())),
            keypool: std::sync::Arc::new(crate::keypool::KeyPool::new()),
            tray_gate_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app2 = crate::server::build_router(state);
        use tower::ServiceExt;
        let resp = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/workbuddy/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"m1","stream":false,"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = hits.lock().unwrap().clone();
        assert_eq!(
            got,
            vec!["V1:/v1/chat/completions"],
            "应只命中 /v1 路径: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 用量台账 route 应透传调用方 agent 参数,而非硬编码 "codex"(多 agent 通路回归)。
    #[test]
    fn usage_log_route_comes_from_caller() {
        let (state, _providers, root) = make_state("route");
        let provider = claude_desktop_mapping_provider();
        let meta = SendMeta {
            line: "direct".into(),
            degraded_to_direct: false,
        };
        usage_log(
            &state,
            &provider,
            "hermes",
            std::time::Instant::now(),
            &meta,
            true,
        );
        let raw = std::fs::read_to_string(state.codex_home.join("usage-stats.jsonl")).unwrap();
        let line: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(
            line["route"], "hermes",
            "台账 route 应为调用方 agent:\n{line}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// official-passthrough：从 Bearer JWT 解 chatgpt_account_id（真实 claim 形态）。
    #[test]
    fn jwt_account_id_decoded_from_real_shape() {
        // header.payload.sig；payload 用 base64url 编码最小真实 claim 集
        let payload = br#"{"iss":"https://auth.openai.com","https://api.openai.com/auth":{"chatgpt_account_id":"6e72f571-1718-4122-b367-f89e5a4712ab","chatgpt_plan_type":"plus"}}"#;
        let b64 = {
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            let (mut buf, mut bits) = (0u32, 0u32);
            for &byte in payload {
                buf = (buf << 8) | byte as u32;
                bits += 8;
                while bits >= 6 {
                    bits -= 6;
                    out.push(TABLE[((buf >> bits) & 0x3F) as usize] as char);
                }
            }
            if bits > 0 {
                out.push(TABLE[((buf << (6 - bits)) & 0x3F) as usize] as char);
            }
            out
        };
        let token = format!("x.{b64}.y");
        let id = jwt_chatgpt_account_id(&format!("Bearer {token}")).unwrap();
        assert_eq!(id, "6e72f571-1718-4122-b367-f89e5a4712ab");
        assert!(jwt_chatgpt_account_id("Bearer not-a-jwt").is_none());
        assert!(jwt_chatgpt_account_id("garbage").is_none());
    }
}
