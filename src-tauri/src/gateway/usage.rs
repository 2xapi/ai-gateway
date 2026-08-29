//! 网关用量台账：请求行日志（usage_log）、非流式响应体与 SSE 流式 usage 归一落盘。
//! 自 gateway.rs 按职责拆出（行为零变化）。

use crate::server::AppState;

use super::forward::SendMeta;

pub(super) fn usage_log(
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

pub(super) fn usage_log_response(
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
pub(super) fn flush_stream_usage(
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
pub(super) fn collect_stream_usage(
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
