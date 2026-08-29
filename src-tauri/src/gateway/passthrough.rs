//! 官方通道透传：Codex 官方 Bearer 直转 chatgpt.com 后端（SSE 流式回传），
//! 官方专用代理 client 与 JWT 账号解析。自 gateway.rs 按职责拆出（行为零变化）。

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::time::Duration;

use crate::server::AppState;

use super::{err_resp, MAX_BODY_BYTES};

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
pub(super) fn jwt_chatgpt_account_id(bearer: &str) -> Option<String> {
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
pub(super) async fn passthrough_official(state: &AppState, req: Request<Body>) -> Response<Body> {
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
