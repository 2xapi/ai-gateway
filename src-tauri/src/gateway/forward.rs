//! 网关转发辅助：client 构建（直连/线路）、加速发送核心（send_with_accel + 407 换线重试）、
//! 非流式/流式响应读取、HTML 上游判别。自 gateway.rs 按职责拆出（行为零变化）。

use axum::body::{Body, Bytes};
use axum::http::{Response, StatusCode};
use futures_util::StreamExt;
use std::time::Duration;

use crate::acclines::AccLine;
use crate::server::AppState;

use super::accel::{resolve_407_perkey, Resolve407};
use super::usage::{collect_stream_usage, flush_stream_usage};
use super::{err_resp, DEFAULT_TIMEOUT_SECS, STREAM_CHUNK_TIMEOUT_SECS};

pub(super) fn build_client(
    provider: &crate::providers::Provider,
) -> Result<reqwest::Client, String> {
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

/// 走线路的 HTTP 客户端:Proxy::all(line.endpoint) + basic auth(凭证来自线路)。
pub(super) fn build_line_client(
    line: &AccLine,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
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
pub(super) struct SendMeta {
    pub(super) line: String,
    pub(super) degraded_to_direct: bool,
}

pub(super) type SendResult = Result<(reqwest::Response, SendMeta), (Response<Body>, SendMeta)>;
/// 非流式读完整响应体:保留总超时语义(客户端已不设总超时——流式路径按 chunk 限时,
/// 非流式这里对整体读取包一层 tokio::time::timeout)。
pub(super) async fn read_body_timed(
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
pub(super) async fn next_stream_chunk<S, E>(
    stream: &mut S,
) -> Option<Result<axum::body::Bytes, String>>
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
pub(super) fn stream_body_with_usage(
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
/// 加速发送核心(R1 抽共用,dispatch 与 dispatch_anthropic 共享):
/// 首发 = 命中线的 client(未命中/加速关 = 直连 client);Ok 但代理 407 或 Err 呈现代理
/// 认证失败(CONNECT 阶段 407)→ 非 per-Key(legacy/custom)人话化 502 不绕线,per-Key 走
/// resolve_407_and_retry 判别;其余连接层失败且线在用 → 换直连 client 重试一次;
/// 未用线时 timeout → 504、其余 → 502。终态响应以 Err(Response) 返回,调用方直接透传。
/// send() 返回 Ok 前未向客户端写任何字节,故重试/换线均无重复副作用。
pub(super) async fn send_with_accel<F>(
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
pub(super) fn proxy_auth_error(e: &reqwest::Error) -> bool {
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
/// Content-Type 判 HTML。不读响应体 → SSE 流式/透传路径同样能拦(真机案例均为 text/html 头)。
/// ponytail:不嗅探响应体——谎报头+HTML 体属未观测场景,需要时在缓冲路径加字节预检即可。
pub(super) fn is_html_upstream(upstream: &reqwest::Response) -> bool {
    upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}
