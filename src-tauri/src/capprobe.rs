//! 能力探测(超融合 A 线一期 §2,方案 v1.0 + 侦察报告 §2 设计输入):
//! 六维探测的前四维——文本 / 工具调用 / 推理(reasoning_levels 产物,不发请求)/ 图像输入。
//!
//! 铁律(侦察报告 §2.2 核心发现):**禁止以 HTTP 状态码判别能力**——2xa 对 Claude 系
//! chat 通道会吞图(200+「没看到图」)。图像输入=内容验证型:固定色块图提问,回答含
//! 期望色词=支持;含否认词表=不支持;其余=unknown。
//! 标签按「供应商×模型」粒度持久化 `<codex_home>/capability-tags.json`;
//! source=manual 的条目探测不自动覆盖(手动覆盖兜底误判,重探按钮强制回 auto)。

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// 图像输入探测的否认词表(中英,侦察报告 §三.2 固化)。
pub const IMAGE_DENY_WORDS: &[&str] = &[
    "没有图片",
    "看不到",
    "无法看到",
    "没有看到",
    "未看到",
    "无法识别",
    "无法查看",
    "请提供图片",
    "请上传图片",
    "no image",
    "cannot see",
    "can't see",
    "could not see",
    "no picture",
    "didn't see",
    "did not see",
    "unable to see",
    "unable to view",
    "not able to see",
    "no image attached",
    "cannot view",
    "can't view",
    "was not attached",
    "attachment",
];

/// 64x64 纯红 PNG(base64;探测载荷)。真机实证:1x1 会因上游压缩失真(红→棕),64x64 稳定答「红色」。
const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAb0lEQVR4nO3PAQkAAAyEwO9feoshgnABdLep8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3I8QUNyPEFDcjxBQ3IPanc8OLDQitxAAAAAElFTkSuQmCC";

#[derive(Debug, Clone, PartialEq)]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    fn as_str(&self) -> &'static str {
        match self {
            Tri::Yes => "yes",
            Tri::No => "no",
            Tri::Unknown => "unknown",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "yes" => Tri::Yes,
            "no" => Tri::No,
            _ => Tri::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Caps {
    pub text: Tri,
    pub tools: Tri,
    pub reasoning: Tri,
    pub image_in: Tri,
}

impl Caps {
    pub fn unknown() -> Self {
        Self {
            text: Tri::Unknown,
            tools: Tri::Unknown,
            reasoning: Tri::Unknown,
            image_in: Tri::Unknown,
        }
    }
    fn to_json(&self) -> Value {
        json!({
            "text": self.text.as_str(),
            "tools": self.tools.as_str(),
            "reasoning": self.reasoning.as_str(),
            "image_in": self.image_in.as_str(),
        })
    }
    fn from_json(v: &Value) -> Self {
        let g = |k: &str| {
            v.get(k)
                .and_then(|x| x.as_str())
                .map(Tri::from_str)
                .unwrap_or(Tri::Unknown)
        };
        Self {
            text: g("text"),
            tools: g("tools"),
            reasoning: g("reasoning"),
            image_in: g("image_in"),
        }
    }
}

fn tags_path(codex_home: &Path) -> PathBuf {
    codex_home.join("capability-tags.json")
}

/// 标签键:供应商×模型粒度(侦察 §2.2:同一供应商 gpt 过 claude 吞,不可一刀切)。
pub fn tag_key(provider_id: &str, model: &str) -> String {
    format!("{provider_id}::{model}")
}

fn load_all(codex_home: &Path) -> Map<String, Value> {
    let raw = std::fs::read_to_string(tags_path(codex_home)).unwrap_or_default();
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| v.get("tags").and_then(|t| t.as_object().cloned()))
        .unwrap_or_default()
}

fn save_all(codex_home: &Path, tags: &Map<String, Value>) {
    let path = tags_path(codex_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = json!({ "version": 1, "tags": tags });
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )
    .is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 查单条单维标签(媒体关卡用);未探测 → Unknown(不拦,探测后自然拦截)。
pub fn tri_of(codex_home: &Path, provider_id: &str, model: &str, dim: &str) -> Tri {
    let tags = load_all(codex_home);
    tags.get(&tag_key(provider_id, model))
        .and_then(|v| v.get("caps")) // 存储形态:{source,probed_at,caps:{四维}}
        .and_then(|c| c.get(dim))
        .and_then(|x| x.as_str())
        .map(Tri::from_str)
        .unwrap_or(Tri::Unknown)
}

/// 单维实证标记(C 段图出:首次成功调用即标 yes,免探测成本;失败不标——参数错≠能力无)。
/// manual 条目不覆盖(与 store_probe 同保护);caps 缺则建(未探测过的模型可直接标单维)。
pub fn mark_dim(codex_home: &Path, provider_id: &str, model: &str, dim: &str, tri: Tri) {
    let mut all = load_all(codex_home);
    let entry = all
        .entry(tag_key(provider_id, model))
        .or_insert_with(|| json!({ "source": "auto" }));
    if entry.get("source").and_then(|s| s.as_str()) == Some("manual") {
        return;
    }
    if let Some(obj) = entry.as_object_mut() {
        let caps = obj.entry("caps").or_insert_with(|| json!({}));
        if let Some(c) = caps.as_object_mut() {
            c.insert(dim.to_string(), json!(tri.as_str()));
        }
        obj.insert("updated_at".into(), json!(chrono::Utc::now().timestamp()));
    }
    save_all(codex_home, &all);
}

/// 读全部标签(GET /api/capability-tags 响应体)。
pub fn all_json(codex_home: &Path) -> Value {
    json!({ "tags": load_all(codex_home) })
}

/// 写入探测结果(source=auto):manual 条目不覆盖。
pub fn store_probe(codex_home: &Path, provider_id: &str, model: &str, caps: &Caps) -> Value {
    let mut all = load_all(codex_home);
    let key = tag_key(provider_id, model);
    let entry = all
        .entry(key.clone())
        .or_insert_with(|| json!({ "source": "auto" }));
    if entry.get("source").and_then(|s| s.as_str()) == Some("manual") {
        // 手动覆盖兜底:自动探测不碰(重探按钮走 force 参数回 auto)
        return entry.clone();
    }
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("source".into(), json!("auto"));
        obj.insert("probed_at".into(), json!(chrono::Utc::now().timestamp()));
        obj.insert("caps".into(), caps.to_json());
    }
    let out = entry.clone();
    save_all(codex_home, &all);
    out
}

/// 手动覆盖(PUT):单维或整组 on/off(source=manual);auto=清覆盖(回 source=auto)。
pub fn set_manual(
    codex_home: &Path,
    provider_id: &str,
    model: &str,
    dim: &str,
    val: &str,
) -> Result<Value, String> {
    let dims = ["text", "tools", "reasoning", "image_in"];
    if !dims.contains(&dim) {
        return Err(format!("维度仅支持 {dims:?}"));
    }
    let v = match val {
        "on" => Tri::Yes,
        "off" => Tri::No,
        "auto" => Tri::Unknown,
        _ => return Err("值仅支持 on / off / auto".into()),
    };
    let mut all = load_all(codex_home);
    let key = tag_key(provider_id, model);
    if val == "auto" {
        // 清覆盖(注释即语义):条目回 source=auto(解除 manual 对探测/实证的「不覆盖」保护),
        // 该维置 Unknown(=未标记、放行语义,下次探测/实证重填真实值);条目不存在 → 无覆盖可清,no-op。
        if let Some(entry) = all.get_mut(&key) {
            if let Some(obj) = entry.as_object_mut() {
                let mut caps = Caps::from_json(obj.get("caps").unwrap_or(&Value::Null));
                match dim {
                    "text" => caps.text = Tri::Unknown,
                    "tools" => caps.tools = Tri::Unknown,
                    "reasoning" => caps.reasoning = Tri::Unknown,
                    _ => caps.image_in = Tri::Unknown,
                }
                obj.insert("source".into(), json!("auto"));
                obj.insert("caps".into(), caps.to_json());
                obj.insert("updated_at".into(), json!(chrono::Utc::now().timestamp()));
            }
        }
        let out = all.get(&key).cloned().unwrap_or(Value::Null);
        save_all(codex_home, &all);
        return Ok(out);
    }
    let entry = all
        .entry(key.clone())
        .or_insert_with(|| json!({ "source": "manual", "caps": Caps::unknown().to_json() }));
    let mut caps = Caps::from_json(entry.get("caps").unwrap_or(&Value::Null));
    match dim {
        "text" => caps.text = v,
        "tools" => caps.tools = v,
        "reasoning" => caps.reasoning = v,
        _ => caps.image_in = v,
    }
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("source".into(), json!("manual"));
        obj.insert("caps".into(), caps.to_json());
        obj.insert("updated_at".into(), json!(chrono::Utc::now().timestamp()));
    }
    let out = entry.clone();
    save_all(codex_home, &all);
    Ok(out)
}

/// 强制重探测入口(store_probe 的 manual 穿透版,重探按钮)。
pub fn store_probe_force(codex_home: &Path, provider_id: &str, model: &str, caps: &Caps) -> Value {
    let mut all = load_all(codex_home);
    let key = tag_key(provider_id, model);
    let entry = all.entry(key).or_insert_with(|| json!({}));
    if let Some(obj) = entry.as_object_mut() {
        obj.insert("source".into(), json!("auto"));
        obj.insert("probed_at".into(), json!(chrono::Utc::now().timestamp()));
        obj.insert("caps".into(), caps.to_json());
    }
    let out = entry.clone();
    save_all(codex_home, &all);
    out
}

// ── 探测执行 ──────────────────────────────────────────────

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(75))
        .build()
        .unwrap_or_default()
}

fn wire_url(base_url: &str, wire_api: crate::providers::WireApi, model: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match wire_api {
        crate::providers::WireApi::Responses => {
            if base.ends_with("/v1") {
                format!("{base}/responses")
            } else {
                format!("{base}/v1/responses")
            }
        }
        crate::providers::WireApi::ChatCompletions => {
            if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            }
        }
        crate::providers::WireApi::Anthropic => {
            if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            }
        }
        crate::providers::WireApi::Gemini => {
            if base.ends_with("/v1beta") {
                format!("{base}/models/{model}:generateContent")
            } else {
                format!("{base}/v1beta/models/{model}:generateContent")
            }
        }
    }
}

async fn post_wire(
    c: &reqwest::Client,
    url: &str,
    api_key: &str,
    wire_api: crate::providers::WireApi,
    body: Value,
) -> Result<Value, String> {
    let request = match wire_api {
        crate::providers::WireApi::Anthropic => c
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        crate::providers::WireApi::Gemini => c.post(url).header("x-goog-api-key", api_key),
        _ => c.post(url).bearer_auth(api_key),
    };
    let resp = request
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    if status >= 400 {
        return Err(format!("upstream {status}"));
    }
    Ok(v)
}

fn content_text(wire_api: crate::providers::WireApi, value: &Value) -> String {
    let parts = match wire_api {
        crate::providers::WireApi::ChatCompletions => value
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .map(|text| vec![text.to_string()])
            .unwrap_or_default(),
        crate::providers::WireApi::Responses => {
            if let Some(text) = value.get("output_text").and_then(Value::as_str) {
                vec![text.to_string()]
            } else {
                value
                    .get("output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .flat_map(|item| {
                        item.get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .map(String::from)
                    .collect()
            }
        }
        crate::providers::WireApi::Anthropic => value
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(String::from)
            .collect(),
        crate::providers::WireApi::Gemini => value
            .get("candidates")
            .and_then(|candidates| candidates.get(0))
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(String::from)
            .collect(),
    };
    parts.join(" ").to_lowercase()
}

fn has_tool_call(wire_api: crate::providers::WireApi, value: &Value) -> bool {
    match wire_api {
        crate::providers::WireApi::ChatCompletions => value
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("tool_calls"))
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty()),
        crate::providers::WireApi::Responses => value
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
            }),
        crate::providers::WireApi::Anthropic => value
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"))
            }),
        crate::providers::WireApi::Gemini => value
            .get("candidates")
            .and_then(|candidates| candidates.get(0))
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some())),
    }
}

#[derive(Clone, Copy)]
enum ProbeKind {
    Text,
    Tools,
    Image,
}

fn probe_body(wire_api: crate::providers::WireApi, model: &str, kind: ProbeKind) -> Value {
    let text = match kind {
        ProbeKind::Text => "回复:OK",
        ProbeKind::Tools => "北京今天天气怎么样?请调用工具查询。",
        ProbeKind::Image => "这张图是什么颜色?只回答颜色词。",
    };
    match wire_api {
        crate::providers::WireApi::ChatCompletions => {
            let content = match kind {
                ProbeKind::Image => json!([
                    { "type": "text", "text": text },
                    { "type": "image_url", "image_url": { "url": format!("data:image/png;base64,{RED_PNG_B64}") } }
                ]),
                _ => json!(text),
            };
            let mut body = json!({
                "model": model, "max_tokens": if matches!(kind, ProbeKind::Tools) { 200 } else { 60 },
                "stream": false, "messages": [{ "role": "user", "content": content }]
            });
            if matches!(kind, ProbeKind::Tools) {
                body["tools"] = json!([{ "type": "function", "function": {
                    "name": "get_weather", "description": "查询城市天气",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }
                }}]);
            }
            body
        }
        crate::providers::WireApi::Responses => {
            let input = match kind {
                ProbeKind::Image => json!([{ "role": "user", "content": [
                    { "type": "input_text", "text": text },
                    { "type": "input_image", "image_url": format!("data:image/png;base64,{RED_PNG_B64}") }
                ] }]),
                _ => json!(text),
            };
            let mut body = json!({
                "model": model, "max_output_tokens": if matches!(kind, ProbeKind::Tools) { 200 } else { 60 },
                "stream": false, "input": input
            });
            if matches!(kind, ProbeKind::Tools) {
                body["tools"] = json!([{ "type": "function", "name": "get_weather",
                    "description": "查询城市天气",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }
                }]);
            }
            body
        }
        crate::providers::WireApi::Anthropic => {
            let content = match kind {
                ProbeKind::Image => json!([
                    { "type": "text", "text": text },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": RED_PNG_B64 } }
                ]),
                _ => json!(text),
            };
            let mut body = json!({
                "model": model, "max_tokens": if matches!(kind, ProbeKind::Tools) { 200 } else { 60 },
                "messages": [{ "role": "user", "content": content }]
            });
            if matches!(kind, ProbeKind::Tools) {
                body["tools"] = json!([{ "name": "get_weather", "description": "查询城市天气",
                    "input_schema": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }
                }]);
            }
            body
        }
        crate::providers::WireApi::Gemini => {
            let parts = match kind {
                ProbeKind::Image => json!([
                    { "text": text },
                    { "inlineData": { "mimeType": "image/png", "data": RED_PNG_B64 } }
                ]),
                _ => json!([{ "text": text }]),
            };
            let mut body = json!({ "contents": [{ "role": "user", "parts": parts }] });
            if matches!(kind, ProbeKind::Tools) {
                body["tools"] = json!([{ "functionDeclarations": [{
                    "name": "get_weather", "description": "查询城市天气",
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } }, "required": ["city"] }
                }] }]);
            }
            body
        }
    }
}

/// 前四维探测(text/tools/image_in 发请求;reasoning 由调用方以 reasoning_levels 产物合入)。
pub async fn probe_caps(
    base_url: &str,
    api_key: &str,
    model: &str,
    wire_api: crate::providers::WireApi,
    reasoning_levels: Option<&[String]>,
) -> Caps {
    let mut caps = Caps {
        text: Tri::Unknown,
        tools: Tri::Unknown,
        reasoning: match reasoning_levels {
            Some(levels) if !levels.is_empty() => Tri::Yes,
            _ => Tri::Unknown,
        },
        image_in: Tri::Unknown,
    };
    let c = client();
    let url = wire_url(base_url, wire_api, model);

    if let Ok(v) = post_wire(
        &c,
        &url,
        api_key,
        wire_api,
        probe_body(wire_api, model, ProbeKind::Text),
    )
    .await
    {
        if !content_text(wire_api, &v).is_empty() {
            caps.text = Tri::Yes;
        }
    }

    if let Ok(v) = post_wire(
        &c,
        &url,
        api_key,
        wire_api,
        probe_body(wire_api, model, ProbeKind::Tools),
    )
    .await
    {
        if has_tool_call(wire_api, &v) {
            caps.tools = Tri::Yes;
        } else if !content_text(wire_api, &v).is_empty() {
            caps.tools = Tri::No;
        }
    }

    if let Ok(v) = post_wire(
        &c,
        &url,
        api_key,
        wire_api,
        probe_body(wire_api, model, ProbeKind::Image),
    )
    .await
    {
        let t = content_text(wire_api, &v);
        if t.is_empty() {
            // 无文本内容(可能纯 reasoning 输出)→ unknown
        } else if IMAGE_DENY_WORDS.iter().any(|w| t.contains(w)) {
            caps.image_in = Tri::No; // 200 假阳性:模型否认看到图
        } else if t.contains('红') || t.contains("red") {
            caps.image_in = Tri::Yes;
        }
        // 其余回答(如色盲式错误)→ 保持 unknown
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("2xapi-caps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(&r).unwrap();
        r
    }

    #[test]
    fn store_and_manual_override() {
        let r = root("store");
        let caps = Caps {
            text: Tri::Yes,
            tools: Tri::No,
            reasoning: Tri::Yes,
            image_in: Tri::No,
        };
        let e = store_probe(&r, "p1", "m1", &caps);
        assert_eq!(e["source"], "auto");
        assert_eq!(e["caps"]["image_in"], "no");
        // 再次探测更新 auto 条目
        let caps2 = Caps {
            text: Tri::Yes,
            tools: Tri::Yes,
            reasoning: Tri::Yes,
            image_in: Tri::No,
        };
        store_probe(&r, "p1", "m1", &caps2);
        let all = load_all(&r);
        assert_eq!(
            all[&tag_key("p1", "m1")]["caps"]["tools"],
            "yes",
            "auto 探测应更新"
        );
        // 手动覆盖(on=yes 归一) → auto 不再碰(caps2.image_in=No 不应冲掉 yes)
        set_manual(&r, "p1", "m1", "image_in", "on").unwrap();
        store_probe(&r, "p1", "m1", &caps2);
        let all = load_all(&r);
        assert_eq!(all[&tag_key("p1", "m1")]["source"], "manual");
        assert_eq!(
            all[&tag_key("p1", "m1")]["caps"]["image_in"],
            "yes",
            "manual 覆盖不被 auto 冲掉"
        );
        // force(重探按钮)回 auto
        store_probe_force(&r, "p1", "m1", &caps2);
        let all = load_all(&r);
        assert_eq!(all[&tag_key("p1", "m1")]["source"], "auto");
        // 非法参数
        assert!(set_manual(&r, "p1", "m1", "bogus", "on").is_err());
        assert!(set_manual(&r, "p1", "m1", "image_in", "bogus").is_err());
        // 供应商×模型粒度隔离
        store_probe(&r, "p1", "m2", &caps);
        let all = load_all(&r);
        assert!(all.contains_key(&tag_key("p1", "m2")));
    }

    #[test]
    fn deny_words_and_keys() {
        assert!(IMAGE_DENY_WORDS.contains(&"no image attached"));
        assert_eq!(tag_key("a", "b"), "a::b");
    }

    /// set_manual val="auto" = 真正清覆盖:回 source=auto + 该维 Unknown(放行语义),
    /// manual 值不再生效、后续探测可重填;条目不存在 → no-op 不建条目。
    #[test]
    fn manual_auto_clears_override() {
        let r = root("auto");
        // 探测 → 手动覆盖 on → auto 清覆盖
        let caps = Caps {
            text: Tri::Yes,
            tools: Tri::No,
            reasoning: Tri::Yes,
            image_in: Tri::No,
        };
        store_probe(&r, "p1", "m1", &caps);
        set_manual(&r, "p1", "m1", "image_in", "on").unwrap();
        let all = load_all(&r);
        assert_eq!(all[&tag_key("p1", "m1")]["source"], "manual");
        assert_eq!(all[&tag_key("p1", "m1")]["caps"]["image_in"], "yes");

        set_manual(&r, "p1", "m1", "image_in", "auto").unwrap();
        let all = load_all(&r);
        assert_eq!(
            all[&tag_key("p1", "m1")]["source"],
            "auto",
            "auto 应移除 manual 标记"
        );
        assert_eq!(
            all[&tag_key("p1", "m1")]["caps"]["image_in"],
            "unknown",
            "清覆盖后该维应回未标记(放行),待探测重填"
        );
        assert_eq!(
            all[&tag_key("p1", "m1")]["caps"]["tools"],
            "no",
            "其余维探测值保留"
        );
        // 清覆盖后探测可再次覆盖(manual 保护已解除)
        let caps2 = Caps {
            text: Tri::Yes,
            tools: Tri::No,
            reasoning: Tri::Yes,
            image_in: Tri::Yes,
        };
        store_probe(&r, "p1", "m1", &caps2);
        assert_eq!(
            load_all(&r)[&tag_key("p1", "m1")]["caps"]["image_in"],
            "yes"
        );
        // 不存在的条目:auto no-op(不建 manual 条目)
        assert_eq!(
            set_manual(&r, "p2", "m9", "text", "auto").unwrap(),
            Value::Null
        );
        assert!(!load_all(&r).contains_key(&tag_key("p2", "m9")));
    }
    #[test]
    fn mark_dim_creates_caps_and_respects_manual() {
        let home = std::env::temp_dir().join(format!("2xapi-md-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // 未探测过的模型:直接标单维,caps 缺则建
        mark_dim(&home, "p", "m", "image_out", Tri::Yes);
        assert_eq!(tri_of(&home, "p", "m", "image_out"), Tri::Yes);
        assert_eq!(
            tri_of(&home, "p", "m", "image_in"),
            Tri::Unknown,
            "其余维不受影响"
        );
        // manual 条目不覆盖
        let mut all = load_all(&home);
        all.insert(
            tag_key("p", "m"),
            json!({"source": "manual", "caps": {"image_out": "no"}}),
        );
        save_all(&home, &all);
        mark_dim(&home, "p", "m", "image_out", Tri::Yes);
        assert_eq!(tri_of(&home, "p", "m", "image_out"), Tri::No, "manual 优先");
        let _ = std::fs::remove_dir_all(&home);
    }
}

/// 真机探测 e2e(#[ignore] 手动驱动;A 线一期验收):
/// 真实 providers.json 的 2xa 供应商,gpt 系+claude 系各一,复验侦察报告 §2 分化结论
/// (gpt-5.6 识图 yes;claude-fable-5 通道吞图 → image_in=no,HTTP 200 假阳性被内容验证识破)。
/// 标签落 tempdir,真实 capability-tags.json 零触碰;Key 内存直读不上日志。
#[cfg(test)]
mod real {
    use super::*;

    #[tokio::test]
    #[ignore = "真机探测:2xa 小成本请求×2 供应商(每维 1 发,16-200 token),手动驱动"]
    async fn caps_real_probe_2xa() {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let data = crate::providers::load(&home.join(".codex").join("providers.json"));
        let pick = |model_pat: &str| {
            data.providers
                .iter()
                .find(|p| {
                    p.base_url.contains("2xa")
                        && p.model.contains(model_pat)
                        && p.model != "gpt-image-2"
                })
                .cloned()
        };
        let Some(gpt) = pick("gpt-5.6") else {
            eprintln!("[caps] 无 gpt-5.6 供应商,跳过");
            return;
        };
        let tmp = std::env::temp_dir().join(format!("2xapi-caps-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let levels = gpt.reasoning_levels.clone().unwrap_or_default();
        let caps = probe_caps(
            &gpt.base_url,
            &gpt.api_key,
            &gpt.model,
            gpt.wire_api,
            Some(&levels),
        )
        .await;
        println!(
            "[gpt-5.6] text={:?} tools={:?} reasoning={:?} image_in={:?}",
            caps.text, caps.tools, caps.reasoning, caps.image_in
        );
        assert_eq!(caps.text, Tri::Yes, "gpt 文本应可用");
        assert_eq!(caps.image_in, Tri::Yes, "gpt-5.6 识图应支持(侦察实证)");
        let e = store_probe(&tmp, &gpt.id, &gpt.model, &caps);
        assert_eq!(e["source"], "auto");

        // claude 系对照:不预设分化(侦察报告「吞图」为时点性结论;2026-08-17 复验已通过,
        // 上游行为变化/Key 组不同——这正是能力标签需持续重探的产品论据)。仅断言二值产出。
        if let Some(cl) = pick("claude-fable-5") {
            let levels = cl.reasoning_levels.clone().unwrap_or_default();
            let caps = probe_caps(
                &cl.base_url,
                &cl.api_key,
                &cl.model,
                cl.wire_api,
                Some(&levels),
            )
            .await;
            println!(
                "[claude-fable-5] text={:?} tools={:?} reasoning={:?} image_in={:?}",
                caps.text, caps.tools, caps.reasoning, caps.image_in
            );
            assert!(
                caps.image_in != Tri::Unknown,
                "内容验证应产出 yes/no 二值(unknown=判别词表缺口)"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
        println!("[caps 真机] 分化结论复验通过");
    }
}

#[cfg(test)]
mod real_debug {
    /// 最小复现:client() 直打 2xa models 端点(无 Key,期望 401 快速返回)。
    #[tokio::test]
    #[ignore = "网络诊断"]
    async fn dbg_2xa_models() {
        let c = super::client();
        let t0 = std::time::Instant::now();
        let r = c.get("https://2xa.cc.cd/v1/models").send().await;
        match r {
            Ok(resp) => println!("models: {} in {:?}", resp.status(), t0.elapsed()),
            Err(e) => println!("models ERR: {e} in {:?}", t0.elapsed()),
        }
        let t1 = std::time::Instant::now();
        let r2 = reqwest::Client::builder()
            .user_agent("2xapi-console")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap()
            .post("https://2xa.cc.cd/v1/chat/completions")
            .header("content-type", "application/json")
            .body(
                r#"{"model":"gpt-5.6","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
            )
            .send()
            .await;
        match r2 {
            Ok(resp) => println!("chat+UA: {} in {:?}", resp.status(), t1.elapsed()),
            Err(e) => println!("chat+UA ERR: {e} in {:?}", t1.elapsed()),
        }
    }
}
