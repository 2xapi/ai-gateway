use serde::Deserialize;
use std::time::Duration;

/// 已知模型的上下文窗口（token）。上游不返回时用此兜底（命中即填）。
fn known_context(name: &str) -> Option<u64> {
    let n = name.to_ascii_lowercase();
    let has = |sub: &str| n.contains(sub);
    // OpenAI
    if has("gpt-4o")
        || has("gpt-4-turbo")
        || has("gpt-4-1106")
        || has("gpt-4-0125")
        || has("gpt-4-vision")
    {
        return Some(128_000);
    }
    if has("o1-mini") || has("o3-mini") {
        return Some(128_000);
    }
    if has("o1") || has("o3") {
        return Some(200_000);
    }
    if has("gpt-4") {
        return Some(8_192);
    }
    if has("gpt-3.5") {
        return Some(16_385);
    }
    // Anthropic
    if has("claude") {
        return Some(200_000);
    }
    // DeepSeek（含中转的自定义名如 deepseek-v4-flash）
    if has("deepseek") {
        return Some(64_000);
    }
    // Google
    if has("gemini-1.5") {
        return Some(1_000_000);
    }
    if has("gemini-2") {
        return Some(2_000_000);
    }
    if has("gemini") {
        return Some(32_000);
    }
    // MiniMax
    if has("minimax") {
        return Some(1_000_000);
    }
    // 通义
    if has("qwen") || has("qwq") {
        return Some(128_000);
    }
    // 智谱 / 月之暗面 / xAI
    if has("glm") {
        return Some(128_000);
    }
    if has("kimi") {
        return Some(256_000);
    }
    if has("grok") {
        return Some(131_072);
    }
    None
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy() // 绕过系统代理
        .build()
        .expect("failed to build HTTP client")
}

/// 探测模型列表。返回 (模型名, 上下文窗口)；上下文优先取上游字段，其次已知表，都没有则 None。
/// 兼容 base_url 是否已含 /v1：先试 /models，再试 /v1/models。
pub async fn probe_endpoint(base_url: &str, api_key: &str) -> Vec<(String, Option<u64>)> {
    let base = base_url.trim_end_matches('/');
    for suffix in ["/models", "/v1/models"] {
        let got = try_probe(&format!("{}{}", base, suffix), api_key).await;
        if !got.is_empty() {
            return got;
        }
    }
    Vec::new()
}

async fn try_probe(url: &str, api_key: &str) -> Vec<(String, Option<u64>)> {
    let resp = match client()
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    #[derive(Deserialize)]
    struct ModelsResp {
        #[serde(default)]
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        // 常见上下文字段（不同上游命名不同）；max_tokens 语义歧义（常为输出上限），不采用
        #[serde(default)]
        context_length: Option<u64>,
        #[serde(default)]
        context_window: Option<u64>,
        #[serde(default)]
        max_context_tokens: Option<u64>,
        #[serde(default)]
        max_context_window: Option<u64>,
    }

    if let Ok(parsed) = resp.json::<ModelsResp>().await {
        return parsed
            .data
            .into_iter()
            .filter_map(|m| {
                let name = if !m.id.is_empty() { m.id } else { m.name };
                if name.is_empty() {
                    return None;
                }
                let ctx = m
                    .context_length
                    .or(m.context_window)
                    .or(m.max_context_tokens)
                    .or(m.max_context_window)
                    .or_else(|| known_context(&name));
                Some((name, ctx))
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 启动一个内存 mock 上游，返回 base_url。同时响应 /models 与 /v1/models。
    async fn spawn_mock() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if sock.read(&mut buf).await.is_err() {
                        return;
                    }
                    let body = r#"{"object":"list","data":[
                        {"id":"gpt-4o","object":"model","owned_by":"openai","context_length":128000},
                        {"id":"deepseek-chat","object":"model","owned_by":"deepseek","context_window":64000},
                        {"id":"plain","object":"model","owned_by":"x"}
                    ]}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn probe_local_mock_parses_models_and_ctx() {
        let base = spawn_mock().await;
        let r = probe_endpoint(&base, "sk-tmp").await;
        assert_eq!(r.len(), 3, "should find 3 models, got {r:?}");
        assert_eq!(r[0], ("gpt-4o".to_string(), Some(128_000)));
        assert_eq!(r[1], ("deepseek-chat".to_string(), Some(64_000)));
        // 无上下文字段的上游字段 → 已知表兜底；plain 不在表 → None
        assert_eq!(r[2], ("plain".to_string(), None));
    }

    #[tokio::test]
    async fn probe_handles_base_url_with_v1_suffix() {
        let base = spawn_mock().await;
        // base 自带 /v1（opencode 场景）：probe 应先试 {base}/models 成功，不再拼 /v1/v1/models
        let r = probe_endpoint(&format!("{base}/v1"), "sk-tmp").await;
        assert_eq!(
            r.len(),
            3,
            "base with /v1 should still find models, got {r:?}"
        );
    }

    #[tokio::test]
    async fn probe_returns_empty_for_unreachable() {
        let r = probe_endpoint("http://127.0.0.1:9", "sk-tmp").await;
        assert!(r.is_empty());
    }

    // ── 阶段 2 preflight 测试(任务书 §三):路由感知 mock 覆盖四类结果 ──

    /// 路由感知 mock:/models→JSON;/responses→SSE;/chat/completions→JSON;其余 404。
    /// mode 控制形态,覆盖 preflight 的四类结果。
    async fn spawn_mock_router(mode: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let n = match sock.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .split(' ')
                        .nth(1)
                        .unwrap_or("")
                        .to_string();

                    let auth_ok = req.lines().any(|l| {
                        l.to_ascii_lowercase()
                            .starts_with("authorization: bearer good")
                    });
                    let (status, ctype, body) = if !auth_ok {
                        (
                            "401 Unauthorized",
                            "application/json",
                            r#"{"error":"bad key"}"#.to_string(),
                        )
                    } else if path.ends_with("/models") {
                        let models =
                            r#"{"object":"list","data":[{"id":"m-x","context_window":64000}]}"#;
                        ("200 OK", "application/json", models.to_string())
                    } else if path.ends_with("/responses") {
                        if mode == "chat_only" {
                            ("404 Not Found", "application/json", "{}".to_string())
                        } else {
                            (
                                "200 OK",
                                "text/event-stream",
                                "data: {\"type\":\"response.created\"}\n\n".to_string(),
                            )
                        }
                    } else if path.ends_with("/chat/completions") {
                        if mode == "responses_only" {
                            ("404 Not Found", "application/json", "{}".to_string())
                        } else {
                            (
                                "200 OK",
                                "application/json",
                                r#"{"choices":[{"message":{"role":"assistant","content":"x"}}]}"#
                                    .to_string(),
                            )
                        }
                    } else {
                        ("404 Not Found", "application/json", "{}".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status, ctype, body.len(), body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn preflight_all_ok() {
        let base = spawn_mock_router("all").await;
        let r = preflight(&base, "good", "", crate::providers::WireApi::Responses).await;
        assert!(r.key_ok && r.models.len() == 1);
        assert!(r.responses_compat, "responses 应读到 SSE");
        assert!(r.chat_ok);
        assert_eq!(r.suggest, "gateway");
        assert!(r.error.is_none());
    }

    #[tokio::test]
    async fn preflight_chat_only_suggests_gateway_conversion() {
        let base = spawn_mock_router("chat_only").await;
        let r = preflight(&base, "good", "m-x", crate::providers::WireApi::Responses).await;
        assert!(!r.responses_compat);
        assert!(r.chat_ok);
        assert_eq!(r.suggest, "gateway", "仅 chat 也建议走网关(由网关转换)");
    }

    #[tokio::test]
    async fn preflight_responses_only() {
        let base = spawn_mock_router("responses_only").await;
        let r = preflight(&base, "good", "m-x", crate::providers::WireApi::Responses).await;
        assert!(r.responses_compat);
        assert!(!r.chat_ok);
        assert_eq!(r.suggest, "gateway");
    }

    #[tokio::test]
    async fn preflight_bad_key_classifies_auth() {
        let base = spawn_mock_router("all").await;
        let r = preflight(
            &base,
            "wrong-key",
            "m-x",
            crate::providers::WireApi::Responses,
        )
        .await;
        assert!(!r.key_ok);
        assert_eq!(r.error, Some("auth"), "401 应分类为 auth");
    }

    #[tokio::test]
    async fn preflight_unreachable_classifies_timeout() {
        let r = preflight(
            "http://127.0.0.1:9",
            "sk",
            "m",
            crate::providers::WireApi::Responses,
        )
        .await;
        assert!(!r.key_ok && !r.responses_compat && !r.chat_ok);
        assert_eq!(r.error, Some("timeout"));
    }

    /// 真机暴露(DeepSeek 坏 key 无 model 时误报 timeout):model 为空须用 /models 状态分类 auth。
    #[tokio::test]
    async fn preflight_bad_key_no_model_classifies_auth() {
        let base = spawn_mock_router("all").await; // 无 model → 流探测跳过 → 走 /models 状态辅助
        let r = preflight(&base, "wrong-key", "", crate::providers::WireApi::Responses).await;
        assert!(!r.key_ok);
        assert_eq!(
            r.error,
            Some("auth"),
            "无 model 时坏 key 应分类为 auth,而非 timeout"
        );
    }
}

// ── 阶段 2:preflight 测试连接(任务书 §三)──────────────────────

/// 单项流探测结果:HTTP 状态码(None = 连接层失败/超时)。
#[derive(Debug, Clone, Copy)]
pub struct StreamProbe {
    pub status: Option<u16>,
    pub got_sse: bool, // 是否读到首个 SSE data: 事件(Responses 兼容判定)
}

/// 探测 /responses 流式兼容:POST {model,input:"hi",stream:true,max_output_tokens:16},
/// 10s 超时;读到首个 SSE `data:` 事件即算兼容(不必读完)。兼容 base 带/不带 /v1。
pub async fn probe_responses_stream(base_url: &str, api_key: &str, model: &str) -> StreamProbe {
    let base = base_url.trim_end_matches('/');
    let body = serde_json::json!({
        "model": model, "input": "hi", "stream": true, "max_output_tokens": 16
    });
    for suffix in ["/responses", "/v1/responses"] {
        if let Some(r) =
            try_stream_probe(&format!("{}{}", base, suffix), api_key, &body, true).await
        {
            return r;
        }
    }
    StreamProbe {
        status: None,
        got_sse: false,
    }
}

/// 探测 /chat/completions:非流式 POST {messages,max_tokens:1}。
pub async fn probe_chat(base_url: &str, api_key: &str, model: &str) -> StreamProbe {
    let base = base_url.trim_end_matches('/');
    let body = serde_json::json!({
        "model": model, "messages": [{"role":"user","content":"hi"}], "max_tokens": 1
    });
    for suffix in ["/chat/completions", "/v1/chat/completions"] {
        if let Some(r) =
            try_stream_probe(&format!("{}{}", base, suffix), api_key, &body, false).await
        {
            return r;
        }
    }
    StreamProbe {
        status: None,
        got_sse: false,
    }
}

pub async fn probe_anthropic(base_url: &str, api_key: &str, model: &str) -> StreamProbe {
    let base = base_url.trim_end_matches('/');
    let body = serde_json::json!({
        "model": model, "messages": [{"role":"user","content":"hi"}], "max_tokens": 1
    });
    let urls = if base.ends_with("/v1") {
        vec![format!("{base}/messages")]
    } else {
        vec![format!("{base}/v1/messages"), format!("{base}/messages")]
    };
    for url in urls {
        if let Some(result) = try_native_probe(
            client()
                .post(url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body),
        )
        .await
        {
            return result;
        }
    }
    StreamProbe {
        status: None,
        got_sse: false,
    }
}

pub async fn probe_gemini(base_url: &str, api_key: &str, model: &str) -> StreamProbe {
    let base = base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1beta") {
        format!("{base}/models/{model}:generateContent")
    } else {
        format!("{base}/v1beta/models/{model}:generateContent")
    };
    let body = serde_json::json!({
        "contents": [{"role":"user","parts":[{"text":"hi"}]}]
    });
    try_native_probe(
        client()
            .post(url)
            .header("x-goog-api-key", api_key)
            .json(&body),
    )
    .await
    .unwrap_or(StreamProbe {
        status: None,
        got_sse: false,
    })
}

async fn try_native_probe(request: reqwest::RequestBuilder) -> Option<StreamProbe> {
    let response = request.send().await.ok()?;
    Some(StreamProbe {
        status: Some(response.status().as_u16()),
        got_sse: false,
    })
}

/// 仅探测 /models 的 HTTP 状态(用于 model 为空时区分 timeout/auth/notfound;
/// 返回 (base 带 /models, /v1/models) 各自状态,None=连接失败)。
async fn probe_models_http_status(base_url: &str, api_key: &str) -> (Option<u16>, Option<u16>) {
    let base = base_url.trim_end_matches('/');
    let mut s1 = None;
    let mut s2 = None;
    if let Ok(r) = client()
        .get(format!("{base}/models"))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
    {
        s1 = Some(r.status().as_u16());
    }
    if let Ok(r) = client()
        .get(format!("{base}/v1/models"))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
    {
        s2 = Some(r.status().as_u16());
    }
    (s1, s2)
}

/// 单次探测:返回 Some(结果) 表示拿到了 HTTP 响应(不再换后缀);None = 连接失败(换下一后缀)。
async fn try_stream_probe(
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    want_sse: bool,
) -> Option<StreamProbe> {
    let resp = client()
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(body)
        .send()
        .await
        .ok()?;
    let status = resp.status().as_u16();
    if want_sse && status == 200 {
        // 逐块读 body,出现首个 "data:" 行即兼容(上限 8KB / 读满即止,不等待流结束)
        let mut resp = resp;
        let mut buf: Vec<u8> = Vec::with_capacity(1024);
        while let Some(chunk) = resp.chunk().await.ok()? {
            buf.extend_from_slice(&chunk);
            if buf.windows(5).any(|w| w == b"data:") || buf.len() > 8192 {
                break;
            }
        }
        let got_sse = buf.windows(5).any(|w| w == b"data:");
        return Some(StreamProbe {
            status: Some(status),
            got_sse,
        });
    }
    Some(StreamProbe {
        status: Some(status),
        got_sse: false,
    })
}

/// preflight 组装结果。
#[derive(Debug)]
pub struct PreflightResult {
    pub key_ok: bool,
    pub models: Vec<(String, Option<u64>)>,
    pub responses_compat: bool,
    pub chat_ok: bool,
    pub anthropic_ok: bool,
    pub gemini_ok: bool,
    pub wire_api: crate::providers::WireApi,
    pub latency_ms: u64,
    pub suggest: String,             // "gateway" | ""
    pub error: Option<&'static str>, // "timeout" | "auth" | "notfound"
}

/// 完整 preflight:models 探测(key 有效性+延迟) + 当前协议真实请求 + 建议。
pub async fn preflight(
    base_url: &str,
    api_key: &str,
    model_hint: &str,
    wire_api: crate::providers::WireApi,
) -> PreflightResult {
    let started = std::time::Instant::now();
    let models = probe_endpoint(base_url, api_key).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    let model = if !model_hint.is_empty() {
        model_hint.to_string()
    } else {
        models.first().map(|(n, _)| n.clone()).unwrap_or_default()
    };

    let empty_probe = StreamProbe {
        status: None,
        got_sse: false,
    };
    let (resp_probe, chat_probe, anthropic_probe, gemini_probe) = if model.is_empty() {
        // 连模型名都没有:流探测无从发起
        (empty_probe, empty_probe, empty_probe, empty_probe)
    } else {
        match wire_api {
            crate::providers::WireApi::Responses | crate::providers::WireApi::ChatCompletions => {
                let (responses, chat) = futures_util::join!(
                    probe_responses_stream(base_url, api_key, &model),
                    probe_chat(base_url, api_key, &model)
                );
                (responses, chat, empty_probe, empty_probe)
            }
            crate::providers::WireApi::Anthropic => (
                empty_probe,
                empty_probe,
                probe_anthropic(base_url, api_key, &model).await,
                empty_probe,
            ),
            crate::providers::WireApi::Gemini => (
                empty_probe,
                empty_probe,
                empty_probe,
                probe_gemini(base_url, api_key, &model).await,
            ),
        }
    };

    let responses_compat = resp_probe.got_sse;
    let chat_ok = chat_probe.status == Some(200);
    let anthropic_ok = anthropic_probe.status == Some(200);
    let gemini_ok = gemini_probe.status == Some(200);
    let native_ok = responses_compat || chat_ok || anthropic_ok || gemini_ok;
    let key_ok = !models.is_empty() || native_ok;

    // 错误分类(优先级:连不上 > 认证 > 地址/协议)。
    // 注意:model 为空时流探测全部跳过(status=None),此时须依据 /models 的 HTTP 状态
    // 辅助分类(实测:DeepSeek 坏 key 时 /models 返回 401,先前误报 timeout——真机暴露修复)。
    let mut statuses = [
        resp_probe.status,
        chat_probe.status,
        anthropic_probe.status,
        gemini_probe.status,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if statuses.is_empty() && !key_ok {
        let (first, second) = probe_models_http_status(base_url, api_key).await;
        statuses.extend(first);
        statuses.extend(second);
    }
    let error = if statuses.is_empty() && !key_ok {
        Some("timeout")
    } else if statuses.iter().any(|status| matches!(status, 401 | 403)) {
        Some("auth")
    } else if !key_ok && !native_ok {
        Some("notfound")
    } else {
        None
    };

    let suggest = if native_ok {
        "gateway".to_string()
    } else {
        String::new()
    };

    PreflightResult {
        key_ok,
        models,
        responses_compat,
        chat_ok,
        anthropic_ok,
        gemini_ok,
        wire_api,
        latency_ms,
        suggest,
        error,
    }
}
