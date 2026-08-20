//! Provider Doctor 三步诊断（M5，FR-6 / 04 §1.3）。
//!
//! 顺序执行，任一步失败仍继续后续步骤并汇总 errors：
//! 1. 配置校验（base_url / api_key / timeout）
//! 2. 连接测试：`GET {base_url}/models`，**404 自动重试 `{base_url}/v1/models`**
//! 3. 真实请求：按 wire_api 发 `max_tokens=16` 的最小请求

use crate::providers::{AccessMode, ModelConfig, Provider, WireApi};
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize)]
pub struct DiagError {
    pub step: String,
    pub code: String,
    pub message: String,
}

#[derive(Serialize)]
#[allow(non_snake_case)] // JSON 字段按 04 §1.3 契约用 camelCase（configValid/latencyMs/testOk）
pub struct DiagnoseResult {
    pub configValid: bool,
    pub reachable: bool,
    pub latencyMs: Option<u64>,
    pub models: Vec<ModelConfig>,
    pub testOk: bool,
    pub errors: Vec<DiagError>,
}

pub async fn diagnose(provider: &Provider) -> DiagnoseResult {
    let mut errors: Vec<DiagError> = Vec::new();

    // Step1 配置校验
    let config_valid = validate_config(provider, &mut errors);

    // Step2 连接测试（Official 无上游，跳过）
    let (reachable, latency_ms, models) = if provider.access_mode == AccessMode::Official {
        (true, None, Vec::new())
    } else {
        connect_test(provider, &mut errors).await
    };

    // Step3 真实请求
    let test_ok = if reachable && provider.access_mode != AccessMode::Official {
        real_request(provider, &mut errors).await
    } else {
        false
    };

    DiagnoseResult {
        configValid: config_valid,
        reachable,
        latencyMs: latency_ms,
        models,
        testOk: test_ok,
        errors,
    }
}

fn validate_config(p: &Provider, errors: &mut Vec<DiagError>) -> bool {
    let mut ok = true;
    if p.access_mode != AccessMode::Official {
        if p.base_url.trim().is_empty() {
            errors.push(err("config", "base_url", "base_url 为空"));
            ok = false;
        } else if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
            errors.push(err("config", "base_url", "base_url 非 http(s)://"));
            ok = false;
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
        match client.get(&url).bearer_auth(&p.api_key).send().await {
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
                        &format!("{path} 返回 {}", status.as_u16()),
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
    match request.json(&body).send().await {
        Ok(r) if r.status().is_success() => true,
        Ok(r) => {
            errors.push(err(
                "request",
                &format!("http_{}", r.status().as_u16()),
                &format!("真实请求返回 {}", r.status().as_u16()),
            ));
            false
        }
        Err(e) => {
            errors.push(err("request", "unreachable", &format!("{e}")));
            false
        }
    }
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
    }
}
