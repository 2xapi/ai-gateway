//! 第三方中转站用量统计核心。
//!
//! `ReqLog` 是已有的请求性能台账；`UsageRecord` 是只保存计数的 Token 台账。
//! 两者共存于 JSONL，但通过 `kind` 隔离。UsageRecord 不保存请求正文、响应正文、
//! Authorization、Cookie 或 API Key，查询按本机时区的自然日聚合。
#![allow(dead_code)]

use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

type RequestIndex = HashMap<PathBuf, HashMap<(String, String), (u8, u64)>>;

#[derive(Default)]
struct UsageState {
    request_index: RequestIndex,
}

static USAGE_LOCK: OnceLock<Mutex<UsageState>> = OnceLock::new();
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

fn usage_lock() -> &'static Mutex<UsageState> {
    USAGE_LOCK.get_or_init(|| Mutex::new(UsageState::default()))
}

fn log_path(home: &Path) -> PathBuf {
    home.join("usage-stats.jsonl")
}

fn rotated_log_path(path: &Path) -> PathBuf {
    path.with_extension("1.jsonl")
}

fn rotate_if_needed(path: &Path, incoming: u64, max: u64) -> std::io::Result<bool> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size.saturating_add(incoming) <= max {
        return Ok(false);
    }
    let rotated = rotated_log_path(path);
    match std::fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if size > 0 {
        std::fs::rename(path, &rotated)?;
        // 旋转文件同样收紧为 0600:rename 保留源 mode,此处覆盖旧版本可能留下的 0644
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&rotated, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(size > 0)
}

fn read_recent(path: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(max);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.take(max).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    if start > 0 {
        let Some(end) = bytes.iter().position(|b| *b == b'\n') else {
            return String::new();
        };
        bytes.drain(..=end);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        0
    } else {
        sorted[((sorted.len() as f64 * q).ceil() as usize).clamp(1, sorted.len()) - 1]
    }
}

/// 只保留前 3 个和后 4 个字符。
pub fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n < 8 {
        return "…".into();
    }
    format!(
        "{}…{}",
        key.chars().take(3).collect::<String>(),
        key.chars().skip(n - 4).collect::<String>()
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReqLog {
    pub ts: i64,
    pub provider_id: String,
    pub provider_name: String,
    pub key_masked: String,
    pub route: String,
    pub line: String,
    pub degraded_to_direct: bool,
    pub latency_ms: u64,
    pub ok: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    #[serde(default = "usage_kind")]
    pub kind: String,
    pub ts: i64,
    pub provider_id: String,
    pub provider_name: String,
    #[serde(default)]
    pub model: Option<String>,
    pub route: String,
    #[serde(default)]
    pub request_id: Option<String>,
    pub usage: NormalizedUsage,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub cost_source: Option<String>,
    #[serde(default)]
    pub latency_ms: u64,
    pub ok: bool,
}

fn usage_kind() -> String {
    "usage".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageAggregate {
    pub date: String,
    pub provider_id: Option<String>,
    pub request_count: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_hit_rate: Option<f64>,
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageModelAggregate {
    model: String,
    request_count: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_hit_rate: Option<f64>,
    cost: Option<f64>,
    share: Option<f64>,
}

fn number(v: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        v.get(*name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        })
    })
}

fn nested_number(v: &Value, parents: &[&str], names: &[&str]) -> Option<u64> {
    parents
        .iter()
        .find_map(|parent| v.get(*parent).and_then(|value| number(value, names)))
}

/// 将 Responses、Chat Completions、Anthropic、Gemini usage 统一为内部口径。
/// 也接受完整响应对象，会自动读取其中的 `usage`/`usageMetadata`。
pub fn normalize_usage(protocol: &str, input: &Value) -> NormalizedUsage {
    let value = input
        .get("usage")
        .or_else(|| input.get("usageMetadata"))
        .or_else(|| input.get("message").and_then(|v| v.get("usage")))
        .or_else(|| input.get("response").and_then(|v| v.get("usage")))
        .or_else(|| input.get("response").and_then(|v| v.get("usageMetadata")))
        .unwrap_or(input);
    let protocol = protocol.trim().to_ascii_lowercase();
    let (input_tokens, output_tokens, total_tokens, cache_read_tokens) =
        if protocol.contains("gemini") {
            (
                number(value, &["promptTokenCount", "prompt_tokens"]),
                number(value, &["candidatesTokenCount", "completion_tokens"]),
                number(value, &["totalTokenCount", "total_tokens"]),
                number(
                    value,
                    &[
                        "cachedContentTokenCount",
                        "cache_read_input_tokens",
                        "cached_tokens",
                    ],
                ),
            )
        } else if protocol.contains("anthropic") || protocol.contains("message") {
            (
                number(value, &["input_tokens", "prompt_tokens"]),
                number(value, &["output_tokens", "completion_tokens"]),
                number(value, &["total_tokens"]),
                number(
                    value,
                    &[
                        "cache_read_input_tokens",
                        "cache_read_tokens",
                        "cached_tokens",
                    ],
                ),
            )
        } else {
            (
                number(value, &["input_tokens", "prompt_tokens"]),
                number(value, &["output_tokens", "completion_tokens"]),
                number(value, &["total_tokens"]),
                number(
                    value,
                    &[
                        "cache_read_input_tokens",
                        "cache_read_tokens",
                        "cached_tokens",
                    ],
                )
                .or_else(|| {
                    nested_number(
                        value,
                        &[
                            "input_tokens_details",
                            "prompt_tokens_details",
                            "inputTokenDetails",
                        ],
                        &[
                            "cached_tokens",
                            "cache_read_tokens",
                            "cachedContentTokenCount",
                        ],
                    )
                }),
            )
        };
    let total_tokens = total_tokens.or_else(|| {
        input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output))
    });
    NormalizedUsage {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
    }
}

fn local_date(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn add(aggregate: &mut UsageAggregate, record: &UsageRecord) {
    aggregate.request_count = aggregate.request_count.saturating_add(1);
    add_optional(&mut aggregate.input_tokens, record.usage.input_tokens);
    add_optional(&mut aggregate.output_tokens, record.usage.output_tokens);
    add_optional(&mut aggregate.total_tokens, record.usage.total_tokens);
    add_optional(
        &mut aggregate.cache_read_tokens,
        record.usage.cache_read_tokens,
    );
    if let Some(cost) = record.cost {
        aggregate.cost = Some(aggregate.cost.unwrap_or(0.0) + cost);
    }
}

fn add_optional(target: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *target = Some(target.unwrap_or(0).saturating_add(value));
    }
}

fn finish(aggregate: &mut UsageAggregate) {
    aggregate.cache_hit_rate = match (aggregate.input_tokens, aggregate.cache_read_tokens) {
        (Some(input_tokens), Some(cache_read_tokens)) if input_tokens > 0 => {
            Some(cache_read_tokens as f64 / input_tokens as f64)
        }
        _ => None,
    };
}

fn usage_completeness(record: &UsageRecord) -> (u8, u64) {
    let usage = &record.usage;
    (
        usage.input_tokens.is_some() as u8
            + usage.output_tokens.is_some() as u8
            + usage.total_tokens.is_some() as u8
            + usage.cache_read_tokens.is_some() as u8,
        usage.total_tokens.unwrap_or(0),
    )
}

/// 去重键:优先真实 request_id;缺失时(Gemini generateContent 响应无 id,request_id
/// 恒 None)兜底为 provider_id+route+ts 60s 窗口+model,避免客户端重试双计。
/// 正常协议(responses/chat_completions/anthropic)恒带 id,不落入兜底分支;
/// route 为空时放弃兜底(无协议信息,不强行合并)。
fn dedup_key(record: &UsageRecord) -> Option<(String, String)> {
    if let Some(request_id) = record
        .request_id
        .as_deref()
        .filter(|request_id| !request_id.trim().is_empty())
    {
        return Some((record.provider_id.clone(), request_id.to_string()));
    }
    if record.route.trim().is_empty() {
        return None;
    }
    let model = record.model.as_deref().map(str::trim).unwrap_or("");
    let window = record.ts.div_euclid(60);
    // 无 request_id(如 Gemini generateContent)时,60s 窗口 + route + model 不足区分并行请求,
    // 会系统性误合并(欠计 N-1 个请求);叠加 usage 计数指纹:同一请求的重试记录计数相同仍可去重,
    // 不同请求计数相同的概率极低,把「系统性欠计」降为「罕见双计」。
    let usage = &record.usage;
    let fingerprint = format!(
        "{}-{}-{}",
        usage.input_tokens.unwrap_or(0),
        usage.output_tokens.unwrap_or(0),
        usage.cache_read_tokens.unwrap_or(0),
    );
    Some((
        record.provider_id.clone(),
        format!(
            "~no-id~{}~{}~{}~{}",
            record.route, model, window, fingerprint
        ),
    ))
}

fn dedup(records: &[UsageRecord]) -> Vec<&UsageRecord> {
    let mut by_request: BTreeMap<(String, String), &UsageRecord> = BTreeMap::new();
    let mut without_request = Vec::new();
    for record in records {
        let Some(key) = dedup_key(record) else {
            without_request.push(record);
            continue;
        };
        if by_request
            .get(&key)
            .is_none_or(|saved| usage_completeness(record) > usage_completeness(saved))
        {
            by_request.insert(key, record);
        }
    }
    by_request.into_values().chain(without_request).collect()
}

fn matches_provider(record: &UsageRecord, provider_id: Option<&str>) -> bool {
    provider_id.is_none_or(|provider_id| record.provider_id == provider_id)
}

fn normalized_filter(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn matches_date(record: &UsageRecord, date: &str) -> bool {
    local_date(record.ts) == date
}

/// 纯函数：按本机自然日返回摘要。无缓存或费用字段时返回 null。
pub fn summary_records(records: &[UsageRecord], now_ts: i64) -> Value {
    summary_records_filtered(records, now_ts, None, None)
}

pub fn summary_records_filtered(
    records: &[UsageRecord],
    now_ts: i64,
    provider_id: Option<&str>,
    date: Option<&str>,
) -> Value {
    let today = local_date(now_ts);
    let provider_id = normalized_filter(provider_id);
    let target_date = normalized_filter(date).unwrap_or(&today);
    let mut aggregate = UsageAggregate {
        date: target_date.to_string(),
        provider_id: provider_id.map(str::to_string),
        ..Default::default()
    };
    for record in dedup(records)
        .into_iter()
        .filter(|record| matches_provider(record, provider_id) && matches_date(record, target_date))
    {
        add(&mut aggregate, record);
    }
    finish(&mut aggregate);
    serde_json::to_value(aggregate).unwrap_or_else(|_| serde_json::json!({}))
}

/// 纯函数：返回最近 days 天的每日聚合，默认按本机时区切日。
pub fn history_records(records: &[UsageRecord], now_ts: i64, days: u32) -> Vec<UsageAggregate> {
    history_records_filtered(records, now_ts, days, None)
}

pub fn history_records_filtered(
    records: &[UsageRecord],
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<UsageAggregate> {
    let provider_id = normalized_filter(provider_id);
    let today = Local
        .timestamp_opt(now_ts, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().expect("epoch is valid"));
    let mut by_day: BTreeMap<String, UsageAggregate> = BTreeMap::new();
    for offset in 0..days {
        let day = today.date_naive() - chrono::Days::new(offset as u64);
        let date = day.format("%Y-%m-%d").to_string();
        by_day.insert(
            date.clone(),
            UsageAggregate {
                date,
                provider_id: provider_id.map(str::to_string),
                ..Default::default()
            },
        );
    }
    for record in dedup(records)
        .into_iter()
        .filter(|record| matches_provider(record, provider_id))
    {
        let date = local_date(record.ts);
        if let Some(aggregate) = by_day.get_mut(&date) {
            add(aggregate, record);
        }
    }
    for aggregate in by_day.values_mut() {
        finish(aggregate);
    }
    by_day.into_values().collect()
}

/// 纯函数：按模型聚合最近 days 天数据。
pub fn models_records(records: &[UsageRecord], now_ts: i64, days: u32) -> Vec<Value> {
    models_records_filtered(records, now_ts, days, None)
}

pub fn models_records_filtered(
    records: &[UsageRecord],
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<Value> {
    let provider_id = normalized_filter(provider_id);
    let dates: BTreeSet<String> = history_records_filtered(records, now_ts, days, provider_id)
        .into_iter()
        .map(|aggregate| aggregate.date)
        .collect();
    let mut by_model: BTreeMap<String, UsageAggregate> = BTreeMap::new();
    for record in dedup(records).into_iter().filter(|record| {
        matches_provider(record, provider_id) && dates.contains(&local_date(record.ts))
    }) {
        let model = record
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let aggregate = by_model
            .entry(model.clone())
            .or_insert_with(|| UsageAggregate {
                date: model,
                ..Default::default()
            });
        add(aggregate, record);
    }
    let mut ranked: Vec<UsageModelAggregate> = by_model
        .into_values()
        .map(|mut aggregate| {
            finish(&mut aggregate);
            let model = aggregate.date.clone();
            UsageModelAggregate {
                model,
                request_count: aggregate.request_count,
                input_tokens: aggregate.input_tokens,
                output_tokens: aggregate.output_tokens,
                total_tokens: aggregate.total_tokens,
                cache_read_tokens: aggregate.cache_read_tokens,
                cache_hit_rate: aggregate.cache_hit_rate,
                cost: aggregate.cost,
                share: None,
            }
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .total_tokens
            .unwrap_or(0)
            .cmp(&left.total_tokens.unwrap_or(0))
            .then_with(|| left.model.cmp(&right.model))
    });

    let total_tokens = ranked
        .iter()
        .map(|summary| summary.total_tokens.unwrap_or(0))
        .fold(0u64, u64::saturating_add);
    for summary in &mut ranked {
        summary.share = (total_tokens > 0)
            .then(|| summary.total_tokens.unwrap_or(0) as f64 / total_tokens as f64);
    }
    ranked
        .into_iter()
        .map(|summary| serde_json::to_value(summary).unwrap_or_default())
        .collect()
}

/// 纯函数：按「日期×模型」聚合最近 days 天数据，供按模型拆分的每日趋势图使用。
pub fn models_history_records(records: &[UsageRecord], now_ts: i64, days: u32) -> Vec<Value> {
    models_history_records_filtered(records, now_ts, days, None)
}

pub fn models_history_records_filtered(
    records: &[UsageRecord],
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<Value> {
    let provider_id = normalized_filter(provider_id);
    let dates: BTreeSet<String> = history_records_filtered(records, now_ts, days, provider_id)
        .into_iter()
        .map(|aggregate| aggregate.date)
        .collect();
    let mut by_model_date: BTreeMap<String, BTreeMap<String, UsageAggregate>> = BTreeMap::new();
    for record in dedup(records)
        .into_iter()
        .filter(|record| matches_provider(record, provider_id))
    {
        let date = local_date(record.ts);
        if !dates.contains(&date) {
            continue;
        }
        let model = record
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let aggregate = by_model_date
            .entry(model.clone())
            .or_default()
            .entry(date.clone())
            .or_insert_with(|| UsageAggregate {
                date: date.clone(),
                ..Default::default()
            });
        add(aggregate, record);
    }
    let mut series: Vec<Value> = by_model_date
        .into_iter()
        .map(|(model, mut by_date)| {
            let total: u64 = by_date
                .values()
                .map(|aggregate| aggregate.total_tokens.unwrap_or(0))
                .fold(0u64, u64::saturating_add);
            let points: Vec<Value> = dates
                .iter()
                .map(|date| match by_date.get_mut(date) {
                    Some(aggregate) => {
                        finish(aggregate);
                        json!({
                            "date": date,
                            "totalTokens": aggregate.total_tokens,
                            "inputTokens": aggregate.input_tokens,
                            "outputTokens": aggregate.output_tokens,
                            "cacheReadTokens": aggregate.cache_read_tokens,
                            "requests": aggregate.request_count,
                        })
                    }
                    None => json!({
                        "date": date,
                        "totalTokens": null,
                        "inputTokens": null,
                        "outputTokens": null,
                        "cacheReadTokens": null,
                        "requests": 0,
                    }),
                })
                .collect();
            json!({ "model": model, "totalTokens": total, "points": points })
        })
        .collect();
    series.sort_by(|left, right| {
        let left_total = left["totalTokens"].as_u64().unwrap_or(0);
        let right_total = right["totalTokens"].as_u64().unwrap_or(0);
        right_total.cmp(&left_total).then_with(|| {
            left["model"]
                .as_str()
                .unwrap_or("")
                .cmp(right["model"].as_str().unwrap_or(""))
        })
    });
    series
}

fn load_usage_records(home: &Path) -> Vec<UsageRecord> {
    let path = log_path(home);
    [rotated_log_path(&path), path]
        .into_iter()
        .flat_map(|path| {
            read_recent(&path, MAX_LOG_BYTES)
                .lines()
                .filter_map(|line| serde_json::from_str::<UsageRecord>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn update_request_index(index: &mut HashMap<(String, String), (u8, u64)>, record: &UsageRecord) {
    let Some(key) = dedup_key(record) else {
        return;
    };
    let completeness = usage_completeness(record);
    index
        .entry(key)
        .and_modify(|saved| *saved = (*saved).max(completeness))
        .or_insert(completeness);
}

fn request_index_from_logs(home: &Path) -> HashMap<(String, String), (u8, u64)> {
    let mut index = HashMap::new();
    for record in load_usage_records(home) {
        update_request_index(&mut index, &record);
    }
    index
}

fn ensure_request_index(state: &mut UsageState, home: &Path) {
    if !state.request_index.contains_key(home) {
        state
            .request_index
            .insert(home.to_path_buf(), request_index_from_logs(home));
    }
}

/// 兼容原有的性能日志查询接口。
pub fn summary(home: &Path) -> Value {
    let Ok(_guard) = usage_lock().lock() else {
        return serde_json::json!({ "providers": [] });
    };
    let path = log_path(home);
    let raw = [rotated_log_path(&path), path]
        .into_iter()
        .flat_map(|path| {
            read_recent(&path, MAX_LOG_BYTES)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut by_provider: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for value in raw {
        if value.get("kind").and_then(Value::as_str) == Some("usage")
            || value.get("usage").is_some()
        {
            continue;
        }
        if let Some(provider_id) = value.get("provider_id").and_then(Value::as_str) {
            by_provider
                .entry(provider_id.to_string())
                .or_default()
                .push(value);
        }
    }
    let providers = by_provider
        .into_iter()
        .map(|(provider_id, rows)| {
            let provider_name = rows
                .iter()
                .find_map(|row| row.get("provider_name").and_then(Value::as_str))
                .unwrap_or(&provider_id);
            let mut latencies: Vec<u64> = rows
                .iter()
                .filter_map(|row| row.get("latency_ms").and_then(Value::as_u64))
                .collect();
            latencies.sort_unstable();
            let ok = rows
                .iter()
                .filter(|row| row.get("ok").and_then(Value::as_bool).unwrap_or(false))
                .count();
            let routes: BTreeSet<&str> = rows
                .iter()
                .filter_map(|row| row.get("route").and_then(Value::as_str))
                .collect();
            serde_json::json!({
                "providerId": provider_id,
                "providerName": provider_name,
                "count": rows.len(),
                "p50Ms": percentile(&latencies, 0.50),
                "p90Ms": percentile(&latencies, 0.90),
                "okRate": if rows.is_empty() { 0.0 } else { ok as f64 / rows.len() as f64 },
                "directFallbackCount": rows.iter().filter(|row| row.get("degraded_to_direct").and_then(Value::as_bool) == Some(true)).count(),
                "lastTs": rows.iter().filter_map(|row| row.get("ts").and_then(Value::as_i64)).max().unwrap_or(0),
                "routes": routes,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "providers": providers })
}

#[allow(dead_code)]
pub fn usage_summary(home: &Path, now_ts: i64) -> Value {
    usage_summary_filtered(home, now_ts, None, None)
}

#[allow(dead_code)]
pub fn usage_summary_filtered(
    home: &Path,
    now_ts: i64,
    provider_id: Option<&str>,
    date: Option<&str>,
) -> Value {
    let Ok(_guard) = usage_lock().lock() else {
        return serde_json::json!({});
    };
    summary_records_filtered(&load_usage_records(home), now_ts, provider_id, date)
}

#[allow(dead_code)]
pub fn usage_history(home: &Path, now_ts: i64, days: u32) -> Vec<UsageAggregate> {
    usage_history_filtered(home, now_ts, days, None)
}

#[allow(dead_code)]
pub fn usage_history_filtered(
    home: &Path,
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<UsageAggregate> {
    let Ok(_guard) = usage_lock().lock() else {
        return vec![];
    };
    history_records_filtered(&load_usage_records(home), now_ts, days, provider_id)
}

#[allow(dead_code)]
pub fn usage_models(home: &Path, now_ts: i64, days: u32) -> Vec<Value> {
    usage_models_filtered(home, now_ts, days, None)
}

#[allow(dead_code)]
pub fn usage_models_filtered(
    home: &Path,
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<Value> {
    let Ok(_guard) = usage_lock().lock() else {
        return vec![];
    };
    models_records_filtered(&load_usage_records(home), now_ts, days, provider_id)
}

#[allow(dead_code)]
pub fn usage_models_history_filtered(
    home: &Path,
    now_ts: i64,
    days: u32,
    provider_id: Option<&str>,
) -> Vec<Value> {
    let Ok(_guard) = usage_lock().lock() else {
        return vec![];
    };
    models_history_records_filtered(&load_usage_records(home), now_ts, days, provider_id)
}

/// 记录一个已完成请求。调用方只传规范化后的计数，不传正文或凭证。
pub fn log_usage(home: &Path, mut record: UsageRecord) {
    use std::io::Write;

    record.kind = usage_kind();
    let Ok(mut state) = usage_lock().lock() else {
        return;
    };
    ensure_request_index(&mut state, home);
    if let Some(key) = dedup_key(&record) {
        if state
            .request_index
            .get(home)
            .and_then(|index| index.get(&key))
            .is_some_and(|saved| usage_completeness(&record) <= *saved)
        {
            return;
        }
    }
    let Ok(raw) = serde_json::to_string(&record) else {
        return;
    };
    let path = log_path(home);
    if raw.len() as u64 + 1 > MAX_LOG_BYTES || std::fs::create_dir_all(home).is_err() {
        return;
    }
    let Ok(rotated) = rotate_if_needed(&path, raw.len() as u64 + 1, MAX_LOG_BYTES) else {
        return;
    };
    if rotated {
        state
            .request_index
            .insert(home.to_path_buf(), request_index_from_logs(home));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    // 用量日志含 Key 摘要:创建时显式 0600(参考 usage_overlay 设置文件),Windows 走默认 ACL
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&path) {
        // 升级既有 0644 旧日志:mode 仅在创建时生效,已存在的文件补一次收紧
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        if writeln!(file, "{raw}").is_ok() {
            update_request_index(
                state
                    .request_index
                    .get_mut(home)
                    .expect("index is initialized"),
                &record,
            );
        }
    }
}

/// 追加一行已有请求性能日志。
pub fn log_request(home: &Path, record: &ReqLog) {
    use std::io::Write;

    let Ok(mut state) = usage_lock().lock() else {
        return;
    };
    let Ok(raw) = serde_json::to_string(record) else {
        return;
    };
    let path = log_path(home);
    let had_request_index = state.request_index.contains_key(home);
    if raw.len() as u64 + 1 > MAX_LOG_BYTES || std::fs::create_dir_all(home).is_err() {
        return;
    }
    let Ok(rotated) = rotate_if_needed(&path, raw.len() as u64 + 1, MAX_LOG_BYTES) else {
        return;
    };
    if rotated && had_request_index {
        state
            .request_index
            .insert(home.to_path_buf(), request_index_from_logs(home));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    // 与 log_usage 一致:创建时显式 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&path) {
        // 升级既有 0644 旧日志(mode 仅创建时生效)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = writeln!(file, "{raw}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_home(prefix: &str) -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn record(
        ts: i64,
        protocol: &str,
        usage: Value,
        model: &str,
        request_id: Option<&str>,
    ) -> UsageRecord {
        UsageRecord {
            kind: "usage".into(),
            ts,
            provider_id: "p".into(),
            provider_name: "P".into(),
            model: Some(model.into()),
            route: protocol.into(),
            request_id: request_id.map(str::to_string),
            usage: normalize_usage(protocol, &usage),
            cost: None,
            cost_source: None,
            latency_ms: 0,
            ok: true,
        }
    }

    #[test]
    fn normalizes_protocol_usage_and_cache_aliases() {
        assert_eq!(
            normalize_usage(
                "responses",
                &serde_json::json!({"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":3}}}),
            ),
            NormalizedUsage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                total_tokens: Some(14),
                cache_read_tokens: Some(3),
            }
        );
        assert_eq!(
            normalize_usage(
                "anthropic",
                &serde_json::json!({"input_tokens":7,"output_tokens":2,"cache_read_input_tokens":5}),
            )
            .cache_read_tokens,
            Some(5)
        );
        assert_eq!(
            normalize_usage(
                "gemini",
                &serde_json::json!({"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":2,"totalTokenCount":10,"cachedContentTokenCount":4}}),
            )
            .cache_read_tokens,
            Some(4)
        );
    }

    #[test]
    fn aggregates_local_days_and_deduplicates_stream() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .unwrap()
            .timestamp();
        let records = vec![
            record(
                now,
                "chat",
                serde_json::json!({"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}),
                "m",
                Some("r1"),
            ),
            record(
                now,
                "chat",
                serde_json::json!({"prompt_tokens":10,"completion_tokens":2}),
                "m",
                Some("r1"),
            ),
            record(
                now - 86_400,
                "chat",
                serde_json::json!({"prompt_tokens":3,"completion_tokens":1}),
                "n",
                None,
            ),
        ];
        let summary = summary_records(&records, now);
        assert_eq!(summary["requestCount"], 1);
        assert_eq!(summary["totalTokens"], 12);
        assert_eq!(summary["cacheReadTokens"], 4);
        assert_eq!(summary["cacheHitRate"], 0.4);
        assert_eq!(history_records(&records, now, 2).len(), 2);
        let models = models_records(&records, now, 2);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["model"], "m");
        assert!(models[0]["date"].is_null());
        assert_eq!(models[0]["share"], 0.75);
    }

    #[test]
    fn filters_provider_and_date_and_keeps_missing_tokens_null() {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .unwrap()
            .timestamp();
        let today = local_date(now);
        let mut selected = record(
            now,
            "responses",
            serde_json::json!({"input_tokens":8,"output_tokens":12}),
            "large",
            Some("selected"),
        );
        selected.provider_id = "p2".into();
        let mut second = record(
            now,
            "responses",
            serde_json::json!({"input_tokens":1,"output_tokens":1}),
            "small",
            Some("second"),
        );
        second.provider_id = "p2".into();
        let mut other = record(
            now,
            "responses",
            serde_json::json!({"input_tokens":100,"output_tokens":100}),
            "other",
            Some("other"),
        );
        other.provider_id = "p3".into();

        let records = vec![selected, second, other];
        let summary = summary_records_filtered(&records, now, Some("p2"), Some(&today));
        assert_eq!(summary["providerId"], "p2");
        assert_eq!(summary["requestCount"], 2);
        assert_eq!(summary["totalTokens"], 22);
        assert!(summary["cacheReadTokens"].is_null());
        assert!(summary["cacheHitRate"].is_null());

        let history = history_records_filtered(&records, now, 1, Some("p2"));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].provider_id.as_deref(), Some("p2"));
        assert_eq!(history[0].input_tokens, Some(9));

        let models = models_records_filtered(&records, now, 1, Some("p2"));
        assert_eq!(models[0]["model"], "large");
        assert_eq!(models[0]["totalTokens"], 20);
        assert_eq!(models[0]["share"], 20.0 / 22.0);
        assert!(models[0]["date"].is_null());

        let empty = summary_records(&[], now);
        assert_eq!(empty["requestCount"], 0);
        assert!(empty["inputTokens"].is_null());
        assert!(empty["outputTokens"].is_null());
        assert!(empty["totalTokens"].is_null());
        assert!(empty["cacheReadTokens"].is_null());
    }

    #[test]
    fn usage_log_does_not_persist_sensitive_fields_or_duplicate_id() {
        let home = std::env::temp_dir().join(format!("usage-core-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let record = record(
            1,
            "anthropic",
            serde_json::json!({"input_tokens":1,"output_tokens":2,"authorization":"Bearer secret","prompt":"secret"}),
            "m",
            Some("id"),
        );
        log_usage(&home, record.clone());
        log_usage(&home, record);
        let raw = std::fs::read_to_string(log_path(&home)).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("prompt"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn usage_log_initializes_index_from_current_and_rotated_logs() {
        let home = test_home("usage-index-init");
        let current = record(
            1,
            "responses",
            serde_json::json!({"input_tokens": 1, "output_tokens": 2}),
            "m",
            Some("current"),
        );
        let rotated = record(
            2,
            "responses",
            serde_json::json!({"input_tokens": 3, "output_tokens": 4}),
            "m",
            Some("rotated"),
        );
        std::fs::write(
            log_path(&home),
            format!("{}\n", serde_json::to_string(&current).unwrap()),
        )
        .unwrap();
        std::fs::write(
            rotated_log_path(&log_path(&home)),
            format!("{}\n", serde_json::to_string(&rotated).unwrap()),
        )
        .unwrap();

        log_usage(&home, current);
        log_usage(&home, rotated);

        let rows = [log_path(&home), rotated_log_path(&log_path(&home))]
            .into_iter()
            .flat_map(|path| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn usage_log_deduplicates_and_keeps_more_complete_record() {
        let home = test_home("usage-index-complete");
        let partial = record(
            1,
            "responses",
            serde_json::json!({"input_tokens": 10}),
            "m",
            Some("same"),
        );
        let complete = record(
            1,
            "responses",
            serde_json::json!({"input_tokens": 10, "output_tokens": 2}),
            "m",
            Some("same"),
        );
        log_usage(&home, partial.clone());
        log_usage(&home, partial);
        log_usage(&home, complete.clone());
        log_usage(&home, complete);

        assert_eq!(
            std::fs::read_to_string(log_path(&home))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let summary = usage_summary(&home, 1);
        assert_eq!(summary["requestCount"], 1);
        assert_eq!(summary["totalTokens"], 12);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Gemini generateContent 无 id:同 provider+route+model+60s 窗口的重试按兜底 key 去重,
    /// 跨窗口 / 跨 model 不误合并。
    #[test]
    fn id_missing_records_dedupe_within_ts_window_only() {
        let home = test_home("usage-no-id-window");
        // 1000/1015 同属 60s 桶(60*16=960..1020),重试 → 视为重复不落盘
        let first = record(
            1000,
            "gemini",
            serde_json::json!({"promptTokenCount":10,"candidatesTokenCount":2}),
            "g",
            None,
        );
        let mut retry = first.clone();
        retry.ts = 1015;
        log_usage(&home, first.clone());
        log_usage(&home, retry.clone());
        assert_eq!(
            std::fs::read_to_string(log_path(&home))
                .unwrap()
                .lines()
                .count(),
            1,
            "60s 窗口内同键重试应去重"
        );
        // 跨窗口(1120 → 桶 18)→ 独立请求,落盘
        let mut later = first.clone();
        later.ts = 1120;
        log_usage(&home, later);
        // 同窗口但 model 不同 → 独立请求,落盘
        let mut other_model = retry.clone();
        other_model.model = Some("g2".into());
        other_model.ts = 1015;
        log_usage(&home, other_model);
        assert_eq!(
            std::fs::read_to_string(log_path(&home))
                .unwrap()
                .lines()
                .count(),
            3
        );
        // 汇总口径一致:3 条独立请求
        let summary = usage_summary(&home, 1000);
        assert_eq!(summary["requestCount"], 3);
        // 读侧 dedup:同窗口重试对聚合只计一次
        let dedup_input = [
            record(
                1000,
                "gemini",
                serde_json::json!({"promptTokenCount":10,"candidatesTokenCount":2}),
                "g",
                None,
            ),
            record(
                1015,
                "gemini",
                serde_json::json!({"promptTokenCount":10,"candidatesTokenCount":2}),
                "g",
                None,
            ),
            record(
                1120,
                "gemini",
                serde_json::json!({"promptTokenCount":5,"candidatesTokenCount":1}),
                "g",
                None,
            ),
        ];
        let deduped = dedup(&dedup_input);
        assert_eq!(deduped.len(), 2, "同窗口重试去重、跨窗口保留");
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn usage_log_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let home = test_home("usage-perms");
        let r = record(
            1,
            "responses",
            serde_json::json!({"input_tokens": 1, "output_tokens": 2}),
            "m",
            Some("perm-id"),
        );
        log_usage(&home, r);
        assert_eq!(
            std::fs::metadata(log_path(&home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "usage-stats.jsonl 应为 0600"
        );
        // 旋转文件同样 0600(rename 保留源 mode + 显式收紧)
        std::fs::write(rotated_log_path(&log_path(&home)), vec![b'x'; 8]).unwrap();
        std::fs::write(log_path(&home), vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        let r2 = record(
            2,
            "responses",
            serde_json::json!({"input_tokens": 2, "output_tokens": 2}),
            "m",
            Some("perm-rotated"),
        );
        log_usage(&home, r2);
        assert_eq!(
            std::fs::metadata(rotated_log_path(&log_path(&home)))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "旋转文件 .1.jsonl 应为 0600"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn rotation_rebuilds_index_without_dropped_rotated_records() {
        let home = test_home("usage-index-rotation");
        let dropped = record(
            1,
            "responses",
            serde_json::json!({"input_tokens": 1, "output_tokens": 1}),
            "m",
            Some("dropped"),
        );
        std::fs::write(
            rotated_log_path(&log_path(&home)),
            format!("{}\n", serde_json::to_string(&dropped).unwrap()),
        )
        .unwrap();
        std::fs::write(log_path(&home), vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();

        let incoming = record(
            2,
            "responses",
            serde_json::json!({"input_tokens": 2, "output_tokens": 2}),
            "m",
            Some("incoming"),
        );
        log_usage(&home, incoming);
        log_usage(&home, dropped);

        assert_eq!(
            std::fs::read_to_string(log_path(&home))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask_key("sk-abcdefghijkl"), "sk-…ijkl");
        assert_eq!(mask_key("short"), "…");
    }

    #[test]
    fn legacy_summary_ignores_usage_ledger_rows() {
        let home =
            std::env::temp_dir().join(format!("usage-summary-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            log_path(&home),
            concat!(
                "{\"kind\":\"usage\",\"provider_id\":\"token\",\"latency_ms\":999,\"ok\":true}\n",
                "{\"provider_id\":\"p\",\"provider_name\":\"P\",\"latency_ms\":25,\"ok\":true}\n"
            ),
        )
        .unwrap();
        let providers = summary(&home)["providers"].as_array().unwrap().clone();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["providerId"], "p");
        assert_eq!(providers[0]["count"], 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn models_history_groups_by_day_and_model() {
        let day = chrono::Local::now().date_naive();
        let ts = |offset_days: i64, hour: i64| {
            day.and_hms_opt(hour as u32, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp()
                + offset_days * 86400
        };
        let records = vec![
            record(
                ts(0, 10),
                "responses",
                json!({"usage":{"input_tokens":100,"total_tokens":100}}),
                "gpt-5",
                Some("r1"),
            ),
            record(
                ts(0, 11),
                "responses",
                json!({"usage":{"input_tokens":50,"total_tokens":50}}),
                "gpt-5",
                Some("r2"),
            ),
            record(
                ts(0, 12),
                "responses",
                json!({"usage":{"input_tokens":30,"total_tokens":30}}),
                "claude-4",
                Some("r3"),
            ),
            record(
                ts(-1, 12),
                "responses",
                json!({"usage":{"input_tokens":10,"total_tokens":10}}),
                "gpt-5",
                Some("r4"),
            ),
        ];
        let series = models_history_records_filtered(&records, ts(0, 12), 2, None);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["model"], "gpt-5");
        assert_eq!(series[0]["totalTokens"], 160);
        let points = series[0]["points"].as_array().unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0]["totalTokens"], 10);
        assert_eq!(points[1]["totalTokens"], 150);
        assert_eq!(series[1]["model"], "claude-4");
        assert_eq!(series[1]["points"][1]["totalTokens"], 30);
    }
}
