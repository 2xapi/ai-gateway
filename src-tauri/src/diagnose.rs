//! Provider Doctor 三步诊断（M5，FR-6 / 04 §1.3）。
//!
//! 顺序执行，任一步失败仍继续后续步骤并汇总 errors：
//! 1. 配置校验（base_url / api_key / timeout）
//! 2. 连接测试：`GET {base_url}/models`，**404 自动重试 `{base_url}/v1/models`**
//! 3. 真实请求：按 wire_api 发 `max_tokens=16` 的最小请求

use crate::providers::{AccessMode, ModelConfig, Provider, WireApi};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Clone)]
pub struct DiagError {
    pub step: String,
    pub code: String,
    pub message: String,
    pub category: String,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: String,
    pub stage: String,
    pub status: String,
    pub latency_ms: Option<u64>,
    pub code: Option<String>,
    pub http_status: Option<u16>,
    pub error_class: String,
    pub message: String,
}

#[derive(Debug, Clone)]
struct HealthState {
    consecutive_failures: u32,
    last_success_at: Option<i64>,
    last_failure_at: Option<i64>,
    circuit: String,
    next_probe_at: Option<i64>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HealthSnapshot {
    pub status: String,
    pub state: String,
    pub consecutive_failures: u32,
    pub last_success_at: Option<i64>,
    pub last_failure_at: Option<i64>,
    pub circuit: String,
    pub next_probe_at: Option<i64>,
    pub cooldown_until: Option<i64>,
}

static HEALTH: LazyLock<Mutex<HashMap<String, HealthState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const CIRCUIT_COOLDOWN_SECS: i64 = 60;

#[derive(Serialize)]
#[allow(non_snake_case)] // JSON 字段按 04 §1.3 契约用 camelCase（configValid/latencyMs/testOk）
pub struct DiagnoseResult {
    pub configValid: bool,
    pub reachable: bool,
    pub latencyMs: Option<u64>,
    pub models: Vec<ModelConfig>,
    pub testOk: bool,
    pub errors: Vec<DiagError>,
    pub checks: Vec<DoctorCheck>,
    pub health: HealthSnapshot,
    pub suggestions: Vec<String>,
}

pub async fn diagnose(provider: &Provider) -> DiagnoseResult {
    let mut errors: Vec<DiagError> = Vec::new();

    // Step1 配置校验
    let config_valid = validate_config(provider, &mut errors);

    // Step2 连接测试（Official 无上游，跳过）
    let health_key = if provider.id.trim().is_empty() {
        provider.name.as_str()
    } else {
        provider.id.as_str()
    };
    let probe_allowed =
        provider.access_mode == AccessMode::Official || health_allows_probe(health_key);
    let (reachable, latency_ms, models) = if provider.access_mode == AccessMode::Official {
        (true, None, Vec::new())
    } else if !probe_allowed {
        errors.push(err(
            "health",
            "circuit_open",
            "供应商连续失败，熔断冷却中；到期后将自动半开探测",
        ));
        (false, None, Vec::new())
    } else {
        connect_test(provider, &mut errors).await
    };

    // Step3 真实请求
    let test_ok = if reachable && provider.access_mode != AccessMode::Official {
        real_request(provider, &mut errors).await
    } else {
        false
    };

    let checks = build_checks(
        provider,
        config_valid,
        reachable,
        latency_ms,
        test_ok,
        &errors,
    );
    let health = record_health(
        provider,
        provider.access_mode == AccessMode::Official || test_ok,
    );
    let suggestions = suggestions(&errors, provider);

    DiagnoseResult {
        configValid: config_valid,
        reachable,
        latencyMs: latency_ms,
        models,
        testOk: test_ok,
        errors,
        checks,
        health,
        suggestions,
    }
}

/// 熔断器探测门：open 冷却期内禁止普通探测，到期只放行一次半开探测。
pub fn health_allows_probe(provider_id: &str) -> bool {
    let now = epoch_now();
    let mut states = HEALTH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(state) = states.get_mut(provider_id) else {
        return true;
    };
    if state.circuit != "open" {
        return true;
    }
    if state.next_probe_at.is_some_and(|next| next > now) {
        return false;
    }
    state.circuit = "half_open".into();
    state.next_probe_at = Some(now);
    true
}

fn build_checks(
    provider: &Provider,
    config_valid: bool,
    reachable: bool,
    latency_ms: Option<u64>,
    test_ok: bool,
    errors: &[DiagError],
) -> Vec<DoctorCheck> {
    let error_for = |stage: &str| errors.iter().find(|error| error.step == stage).cloned();
    let mut checks = Vec::with_capacity(7);
    let push = |checks: &mut Vec<DoctorCheck>,
                id: &str,
                stage: &str,
                ok: bool,
                latency: Option<u64>,
                error: Option<DiagError>,
                message: &str| {
        let http_status = error.as_ref().and_then(|item| {
            item.code
                .strip_prefix("http_")
                .and_then(|value| value.parse::<u16>().ok())
        });
        checks.push(DoctorCheck {
            id: id.into(),
            stage: stage.into(),
            status: if ok { "passed" } else { "failed" }.into(),
            latency_ms: latency,
            code: error.as_ref().map(|item| item.code.clone()),
            http_status,
            error_class: error
                .as_ref()
                .map(|item| item.category.clone())
                .unwrap_or_else(|| "none".into()),
            message: error
                .map(|item| item.message)
                .unwrap_or_else(|| message.into()),
        });
    };
    push(
        &mut checks,
        "config",
        "config",
        config_valid,
        None,
        error_for("config"),
        "配置校验通过",
    );
    let proxy_ok = provider
        .proxy_url
        .as_deref()
        .is_none_or(|proxy| reqwest::Url::parse(proxy).is_ok());
    push(
        &mut checks,
        "proxy",
        "proxy",
        proxy_ok,
        None,
        error_for("config").filter(|error| error.code == "proxy_url"),
        if proxy_ok {
            "代理配置通过"
        } else {
            "代理配置无效"
        },
    );
    if provider.access_mode == AccessMode::Official {
        checks.push(DoctorCheck {
            id: "auth".into(),
            stage: "auth".into(),
            status: "skipped".into(),
            latency_ms: None,
            code: None,
            http_status: None,
            error_class: "none".into(),
            message: "官方 OAuth 由 Codex 客户端管理，Doctor 不读取 auth.json".into(),
        });
        checks.push(DoctorCheck {
            id: "models".into(),
            stage: "models".into(),
            status: "skipped".into(),
            latency_ms: None,
            code: None,
            http_status: None,
            error_class: "none".into(),
            message: "官方模式不探测第三方模型目录".into(),
        });
        checks.push(DoctorCheck {
            id: "request".into(),
            stage: "request".into(),
            status: "skipped".into(),
            latency_ms: None,
            code: None,
            http_status: None,
            error_class: "none".into(),
            message: "官方模式不发送第三方探测请求".into(),
        });
    } else {
        let auth_error = errors
            .iter()
            .find(|error| error.step == "connect" || error.step == "request")
            .filter(|error| {
                error.code.starts_with("http_401") || error.code.starts_with("http_403")
            })
            .cloned();
        push(
            &mut checks,
            "auth",
            "auth",
            auth_error.is_none() && reachable,
            latency_ms,
            auth_error,
            "凭据接受",
        );
        push(
            &mut checks,
            "models",
            "models",
            reachable,
            latency_ms,
            error_for("connect"),
            "模型目录可访问",
        );
        push(
            &mut checks,
            "request",
            "request",
            test_ok,
            None,
            error_for("request"),
            "最小请求通过",
        );
    }
    checks.push(DoctorCheck {
        id: "stream".into(),
        stage: "stream".into(),
        status: "skipped".into(),
        latency_ms: None,
        code: None,
        http_status: None,
        error_class: "none".into(),
        message: "P0 仅做最小请求；流式探测将在显式开启时执行".into(),
    });
    checks.push(DoctorCheck {
        id: "tools".into(),
        stage: "tools".into(),
        status: "skipped".into(),
        latency_ms: None,
        code: None,
        http_status: None,
        error_class: "none".into(),
        message: "供应商未声明工具探测契约，未发送额外请求".into(),
    });
    checks
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn record_health(provider: &Provider, ok: bool) -> HealthSnapshot {
    if provider.access_mode == AccessMode::Official {
        return HealthSnapshot {
            status: "official".into(),
            state: "official".into(),
            consecutive_failures: 0,
            last_success_at: None,
            last_failure_at: None,
            circuit: "bypass".into(),
            next_probe_at: None,
            cooldown_until: None,
        };
    }
    let key = if provider.id.trim().is_empty() {
        provider.name.clone()
    } else {
        provider.id.clone()
    };
    let now = epoch_now();
    let mut states = HEALTH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let state = states.entry(key).or_insert(HealthState {
        consecutive_failures: 0,
        last_success_at: None,
        last_failure_at: None,
        circuit: "closed".into(),
        next_probe_at: None,
    });
    if ok {
        state.consecutive_failures = 0;
        state.last_success_at = Some(now);
        state.circuit = "closed".into();
        state.next_probe_at = None;
    } else {
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.last_failure_at = Some(now);
        if state.consecutive_failures >= CIRCUIT_FAILURE_THRESHOLD {
            state.circuit = "open".into();
            state.next_probe_at = Some(now + CIRCUIT_COOLDOWN_SECS);
        }
    }
    HealthSnapshot {
        status: if ok {
            "healthy"
        } else if state.circuit == "open" {
            "degraded"
        } else {
            "unhealthy"
        }
        .into(),
        state: state.circuit.clone(),
        consecutive_failures: state.consecutive_failures,
        last_success_at: state.last_success_at,
        last_failure_at: state.last_failure_at,
        circuit: state.circuit.clone(),
        next_probe_at: state.next_probe_at,
        cooldown_until: state.next_probe_at,
    }
}

fn suggestions(errors: &[DiagError], provider: &Provider) -> Vec<String> {
    let mut out = Vec::new();
    for error in errors {
        if error.code.contains("401") {
            out.push(
                "凭据被拒绝：请重新生成该供应商的 API key，并避免把官方 OAuth 凭据填入第三方入口。"
                    .into(),
            );
        } else if error.code.contains("403") {
            out.push("供应商返回 403：优先检查模型可用区域、账户权限和代理出口，不要自动切换官方 OAuth。".into());
        } else if error.code.contains("429") {
            out.push("请求受限：稍后重试或启用同一接入模式下的备用供应商。".into());
        } else if error.code.contains("404") {
            out.push(
                "接口路径不匹配：检查 Responses、Chat Completions、Anthropic 或 Gemini 协议选择。"
                    .into(),
            );
        } else if error.code == "unreachable" {
            out.push("网络不可达：检查该供应商独立代理、网关监听端口和 TLS 证书。".into());
        } else if error.code == "circuit_open" {
            out.push(
                "该供应商已进入短暂熔断冷却；请等待半开探测，或在同一接入模式下选择备用供应商。"
                    .into(),
            );
        }
    }
    if provider.access_mode == AccessMode::Official {
        out.push("官方模式保持 auth.json 和官方配置边界不变；第三方失败不会接管官方登录。".into());
    }
    out.sort();
    out.dedup();
    out
}

fn validate_config(p: &Provider, errors: &mut Vec<DiagError>) -> bool {
    let mut ok = true;
    if p.access_mode != AccessMode::Official {
        if p.base_url.trim().is_empty() {
            errors.push(err("config", "base_url", "base_url 为空"));
            ok = false;
        } else {
            match reqwest::Url::parse(p.base_url.trim()) {
                Ok(url) if matches!(url.scheme(), "http" | "https") => {
                    if !url.username().is_empty() || url.password().is_some() {
                        errors.push(err("config", "base_url", "base_url 不得包含用户名或密码"));
                        ok = false;
                    }
                }
                _ => {
                    errors.push(err(
                        "config",
                        "base_url",
                        "base_url 必须是合法 http(s) 地址",
                    ));
                    ok = false;
                }
            }
        }
        if p.api_key.trim().is_empty() {
            errors.push(err("config", "api_key", "api_key 为空"));
            ok = false;
        }
    }
    if let Some(t) = p.timeout_secs {
        if !(5..=3600).contains(&t) {
            errors.push(err("config", "timeout_secs", "timeout 越界(5~3600)"));
            ok = false;
        }
    }
    if let Some(proxy) = p
        .proxy_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        match reqwest::Url::parse(proxy) {
            Ok(url) if matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") => {}
            _ => {
                errors.push(err(
                    "config",
                    "proxy_url",
                    "proxy_url 必须是 http(s) 或 socks5 地址",
                ));
                ok = false;
            }
        }
    }
    ok
}

async fn connect_test(
    p: &Provider,
    errors: &mut Vec<DiagError>,
) -> (bool, Option<u64>, Vec<ModelConfig>) {
    let base = p.base_url.trim_end_matches('/');
    let client = build_client(p);
    // FR-6.2：先 /models，404 再 /v1/models
    for path in ["/models", "/v1/models"] {
        let url = format!("{base}{path}");
        let start = Instant::now();
        match add_provider_headers(client.get(&url).bearer_auth(&p.api_key), p)
            .send()
            .await
        {
            Ok(r) => {
                let status = r.status();
                let latency = start.elapsed().as_millis() as u64;
                if status == reqwest::StatusCode::NOT_FOUND {
                    continue; // 重试下一个路径
                }
                if status.is_success() {
                    let models = parse_models(r).await;
                    return (true, Some(latency), models);
                } else {
                    errors.push(err(
                        "connect",
                        &format!("http_{}", status.as_u16()),
                        &format!("{path} {}", response_error_detail(r).await),
                    ));
                }
            }
            Err(e) => {
                errors.push(err("connect", "unreachable", &format!("{path}: {e}")));
            }
        }
    }
    (false, None, Vec::new())
}

async fn real_request(p: &Provider, errors: &mut Vec<DiagError>) -> bool {
    let base = p.base_url.trim_end_matches('/');
    let client = build_client(p);
    let (url, body) = match p.wire_api {
        WireApi::Responses => (
            format!("{base}/responses"),
            json!({ "model": p.model, "input": "ping", "max_output_tokens": 16 }),
        ),
        WireApi::ChatCompletions => (
            format!("{base}/chat/completions"),
            json!({ "model": p.model, "messages": [{ "role": "user", "content": "ping" }], "max_tokens": 16 }),
        ),
        WireApi::Anthropic => (
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            },
            json!({ "model": p.model, "max_tokens": 16, "messages": [{ "role": "user", "content": "ping" }] }),
        ),
        // 多平台阶段 C:原生 generateContent ping(2xa 实测 Bearer 头亦过认证)
        WireApi::Gemini => (
            if base.ends_with("/v1beta") {
                format!("{base}/models/{}:generateContent", p.model)
            } else {
                format!("{base}/v1beta/models/{}:generateContent", p.model)
            },
            json!({ "contents": [{ "role": "user", "parts": [{ "text": "ping" }] }] }),
        ),
    };
    let request = match p.wire_api {
        WireApi::Anthropic => client
            .post(&url)
            .header("x-api-key", &p.api_key)
            .header("anthropic-version", "2023-06-01"),
        WireApi::Gemini => client.post(&url).header("x-goog-api-key", &p.api_key),
        _ => client.post(&url).bearer_auth(&p.api_key),
    };
    match add_provider_headers(request, p).json(&body).send().await {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => {
            let status = r.status();
            errors.push(err(
                "request",
                &format!("http_{}", status.as_u16()),
                &format!("真实请求 {}", response_error_detail(r).await),
            ));
            false
        }
        Err(e) => {
            errors.push(err("request", "unreachable", &format!("{e}")));
            false
        }
    }
}

fn add_provider_headers(
    mut request: reqwest::RequestBuilder,
    provider: &Provider,
) -> reqwest::RequestBuilder {
    if let Some(user_agent) = provider
        .user_agent
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        request = request.header(reqwest::header::USER_AGENT, user_agent);
    }
    if let Some(headers) = provider.custom_headers.as_ref() {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }
    request
}

fn build_client(p: &Provider) -> reqwest::Client {
    let mut b =
        reqwest::Client::builder().timeout(Duration::from_secs(p.timeout_secs.unwrap_or(30)));
    if let Some(px) = p.proxy_url.as_deref().filter(|s| !s.is_empty()) {
        if let Ok(proxy) = reqwest::Proxy::all(px) {
            b = b.proxy(proxy);
        }
    }
    b.build().unwrap_or_else(|_| reqwest::Client::new())
}

async fn response_error_detail(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .unwrap_or_default()
        .into_iter()
        .take(16 * 1024)
        .collect::<Vec<_>>();
    let detail = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            let text = String::from_utf8_lossy(&body).trim().to_string();
            (!text.is_empty()).then_some(text)
        })
        .unwrap_or_default();
    let detail = detail
        .chars()
        .take(300)
        .collect::<String>()
        .replace("\r", " ")
        .replace('\n', " ");
    // 诊断结果会返回前端，绝不回显可能包含凭据的上游错误正文。
    let lower = detail.to_ascii_lowercase();
    if lower.contains("bearer ")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("authorization")
        || lower.contains("sk-")
        || lower.contains("token=")
        || lower.contains("password")
    {
        format!("HTTP {}（上游错误详情已隐藏）", status.as_u16())
    } else if detail.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}: {detail}", status.as_u16())
    }
}

async fn parse_models(r: reqwest::Response) -> Vec<ModelConfig> {
    let v: Value = r.json().await.unwrap_or(json!({}));
    let arr = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(|x| x.as_array());
    arr.map(|a| {
        a.iter()
            .filter_map(|m| {
                let name = m
                    .get("id")
                    .or_else(|| m.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if name.is_empty() {
                    None
                } else {
                    Some(ModelConfig {
                        name: name.into(),
                        ..Default::default()
                    })
                }
            })
            .collect()
    })
    .unwrap_or_default()
}

fn err(step: &str, code: &str, message: &str) -> DiagError {
    DiagError {
        step: step.into(),
        code: code.into(),
        message: message.into(),
        category: classify_error(step, code, message).into(),
    }
}

fn classify_error(step: &str, code: &str, message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if code == "unreachable" || code.contains("timeout") {
        "timeout"
    } else if code.contains("401") {
        "auth"
    } else if code.contains("403") || lower.contains("region") || lower.contains("country") {
        "region"
    } else if code.contains("429") {
        "rate_limit"
    } else if code.contains("404") {
        "protocol"
    } else if code == "circuit_open" {
        "circuit"
    } else if code.starts_with("http_") {
        "upstream"
    } else if step == "config" {
        "config"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn provider_official() -> Provider {
        Provider {
            name: "O".into(),
            access_mode: AccessMode::Official,
            model: "gpt".into(),
            ..Default::default()
        }
    }

    /// Official：configValid=true、无上游错误、testOk=false（无上游可测）。
    #[tokio::test]
    async fn official_no_upstream_errors() {
        let r = diagnose(&provider_official()).await;
        assert!(r.configValid);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(!r.testOk);
        assert!(r
            .checks
            .iter()
            .any(|check| check.id == "auth" && check.status == "skipped"));
        assert_eq!(r.health.circuit, "bypass");
    }

    /// 配置错误（非法 base_url）：configValid=false，errors 含 config 步。
    #[tokio::test]
    async fn bad_base_url_flagged_in_config() {
        let mut p = provider_official();
        p.access_mode = AccessMode::PureApi;
        p.base_url = "not-a-url".into();
        p.api_key = "sk".into();
        p.model = "m".into();
        let r = diagnose(&p).await;
        assert!(!r.configValid);
        assert!(r
            .errors
            .iter()
            .any(|e| e.step == "config" && e.code == "base_url"));
        assert!(!r.reachable);
    }

    /// 好 provider + mock 上游（/models 返回模型，/responses 200）：全绿。
    #[tokio::test]
    async fn good_provider_all_green() {
        let app = Router::new()
            .route(
                "/models",
                get(|| async { Json(json!({ "data": [{ "id": "gpt-x" }] })) }),
            )
            .route(
                "/responses",
                axum::routing::post(|| async { (reqwest::StatusCode::OK, "ok") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = Provider {
            name: "G".into(),
            base_url: format!("http://{addr}"),
            api_key: "sk".into(),
            model: "gpt-x".into(),
            access_mode: AccessMode::PureApi,
            wire_api: WireApi::Responses,
            sub2api_multiplier: 1.0,
            ..Default::default()
        };
        let _ = n;
        let r = diagnose(&p).await;
        assert!(r.configValid);
        assert!(r.reachable, "{:?}", r.errors);
        assert!(r.testOk, "{:?}", r.errors);
        assert_eq!(r.models.len(), 1);
        assert_eq!(r.models[0].name, "gpt-x");
        assert!(r
            .checks
            .iter()
            .any(|check| check.id == "stream" && check.status == "skipped"));
        assert_eq!(r.health.status, "healthy");
    }

    #[tokio::test]
    async fn auth_errors_are_classified_and_redacted() {
        let app = Router::new().route(
            "/models",
            get(|| async {
                (
                    reqwest::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "token=sk-secret-value"}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let p = Provider {
            id: "redaction-test".into(),
            name: "Redaction".into(),
            base_url: format!("http://{addr}"),
            api_key: "sk-local-secret".into(),
            model: "gpt-x".into(),
            access_mode: AccessMode::PureApi,
            ..Default::default()
        };
        let r = diagnose(&p).await;
        assert!(r.errors.iter().any(|e| e.category == "auth"));
        let serialized = serde_json::to_string(&r.errors).unwrap();
        assert!(!serialized.contains("sk-secret-value"));
    }
}
