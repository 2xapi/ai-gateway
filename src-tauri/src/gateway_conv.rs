//! Responses ↔ ChatCompletions 协议转换（M3b，FR-5）。
//!
//! 背景（01-D5）：Codex 恒发 **Responses** 格式。当 `provider.wire_api = chat_completions` 时，
//! 网关在 `/responses` 入口做：
//! - 请求：Responses → ChatCompletions（`input`/`instructions` → `messages`）。
//! - 非流式响应：Chat `choices[0].message` → Responses `output`。
//! - 流式响应：Chat SSE `delta` → Responses SSE（`response.created` / `response.output_text.delta` / `.done` / `response.completed`）。
//!
//! 实现策略：上游响应整体缓冲后转换（保证事件序列正确；增量逐 token 投递为后续优化）。

use serde_json::{json, Value};

/// 转换后的 Chat 请求体 + 是否流式。
pub struct ConvertedRequest {
    pub body: Vec<u8>,
    pub stream: bool,
}

/// Responses 请求体 → ChatCompletions 请求体（FR-5.1）。
pub fn responses_to_chat_request(body: &[u8]) -> Result<ConvertedRequest, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("非法 responses body: {e}"))?;
    let obj = v.as_object().ok_or("responses body 不是 object")?;

    let mut messages: Vec<Value> = Vec::new();
    // instructions → system 消息
    if let Some(ins) = obj.get("instructions").and_then(|x| x.as_str()) {
        if !ins.is_empty() {
            messages.push(json!({ "role": "system", "content": ins }));
        }
    }
    // input → messages
    match obj.get("input") {
        Some(Value::String(s)) => {
            messages.push(json!({ "role": "user", "content": s }));
        }
        Some(Value::Array(arr)) => {
            for item in arr {
                if let Some(role) = item.get("role").and_then(|x| x.as_str()) {
                    // 真机(DeepSeek)实测:codex 以 developer role 传系统指令,多数 chat 上游
                    // 只认 system/user/assistant/tool —— developer 一律映射为 system
                    let role = match role {
                        "developer" => "system",
                        r => r,
                    };
                    let content = extract_text(item.get("content"));
                    messages.push(json!({ "role": role, "content": content }));
                } else if let Some(s) = item.as_str() {
                    messages.push(json!({ "role": "user", "content": s }));
                }
                // 无法映射的 input 条目类型（如 reasoning）丢弃
            }
        }
        _ => {}
    }

    let mut chat = serde_json::Map::new();
    chat.insert(
        "model".into(),
        obj.get("model").cloned().unwrap_or(json!("")),
    );
    chat.insert("messages".into(), Value::Array(messages));
    if let Some(s) = obj.get("stream") {
        chat.insert("stream".into(), s.clone());
    }
    for src in ["temperature", "top_p"] {
        if let Some(x) = obj.get(src) {
            chat.insert(src.into(), x.clone());
        }
    }
    if let Some(x) = obj.get("max_output_tokens") {
        chat.insert("max_tokens".into(), x.clone());
    }

    let stream = obj.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
    let body =
        serde_json::to_vec(&Value::Object(chat)).map_err(|e| format!("编码 chat body: {e}"))?;
    Ok(ConvertedRequest { body, stream })
}

/// 非流式：ChatCompletions 响应 → Responses 响应（FR-5.2）。
pub fn chat_json_to_responses_json(chat: &[u8]) -> Result<Vec<u8>, String> {
    let v: Value = serde_json::from_slice(chat).map_err(|e| format!("非法 chat body: {e}"))?;
    let text = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let model = v
        .get("model")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let finish = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|x| x.as_str())
        .unwrap_or("stop");
    let status = if finish == "length" {
        "incomplete"
    } else {
        "completed"
    };
    let created_at = chrono::Utc::now().timestamp();

    let mut resp = serde_json::Map::new();
    // Responses 标准字段（Codex 客户端依赖）：object / created_at / model / error / incomplete_details
    resp.insert("id".into(), json!(id));
    resp.insert("object".into(), json!("response"));
    resp.insert("created_at".into(), json!(created_at));
    resp.insert("status".into(), json!(status));
    resp.insert("model".into(), json!(model));
    resp.insert("error".into(), Value::Null);
    resp.insert("incomplete_details".into(), Value::Null);
    resp.insert(
        "output".into(),
        json!([{ "type": "message", "id": format!("msg_{}", &id), "role": "assistant", "content": [{ "type": "output_text", "text": text }] }]),
    );
    if let Some(u) = v.get("usage") {
        resp.insert("usage".into(), convert_usage(u));
    }
    serde_json::to_vec(&Value::Object(resp)).map_err(|e| format!("编码 responses body: {e}"))
}

fn convert_usage(u: &Value) -> Value {
    json!({
        "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
    })
}

/// usage 缺失时的兜底:全零而非空对象——Codex 解析 ResponseCompleted 要求字段存在。
fn zero_usage() -> Value {
    json!({ "input_tokens": 0, "output_tokens": 0, "total_tokens": 0 })
}

/// 从 Responses 的 content（字符串 或 [{type:..,text:..}]）抽出文本。
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .map(String::from)
                    .or_else(|| p.as_str().map(String::from))
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

pub struct SseConvState {
    buffer: String,
    created: bool,
    item_added: bool,
    text: String,
    id: String,
    model: String,
    usage: Option<Value>,
}

impl SseConvState {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            created: false,
            item_added: false,
            text: String::new(),
            id: "resp-conv".into(),
            model: String::new(),
            usage: None,
        }
    }
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim().to_string();
            self.buffer = self.buffer[pos + 1..].to_string();
            self.proc(&line, &mut out);
        }
        out
    }
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = self.feed(b"\n");
        let now = chrono::Utc::now().timestamp();
        out.push(fmt(
            "response.output_text.done",
            &json!({"type":"response.output_text.done","text":self.text.clone()}),
        ));
        // 真机(codex 0.148)实测修复:delta 必须归属 active item,completed 的 usage 必须含
        // input_tokens 等字段(空对象会被 Codex 判 parse 失败断流重试)
        out.push(fmt("response.output_item.done", &json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":format!("msg_{}",self.id),"role":"assistant","status":"completed","content":[{"type":"output_text","text":self.text.clone()}]}})));
        out.push(fmt("response.completed", &json!({"type":"response.completed","response":{
            "id":self.id.clone(),"object":"response","created_at":now,"model":self.model.clone(),"status":"completed",
            "output":[{"type":"message","id":format!("msg_{}",self.id),"role":"assistant","content":[{"type":"output_text","text":self.text.clone()}]}],
            "usage":self.usage.as_ref().map(convert_usage).unwrap_or_else(zero_usage),
            "incomplete_details":null
        }})));
        out
    }
    fn proc(&mut self, line: &str, out: &mut Vec<String>) {
        let p = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return,
        };
        if p == "[DONE]" {
            return;
        }
        let v: Value = match serde_json::from_str(p) {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(u) = v.get("usage") {
            self.usage = Some(u.clone());
        }
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            self.model = m.to_string();
        }
        if !self.created {
            if let Some(i) = v.get("id").and_then(|x| x.as_str()) {
                self.id = i.to_string();
            }
            out.push(fmt("response.created", &json!({"type":"response.created","response":{"id":self.id.clone(),"object":"response","status":"in_progress","output":[]}})));
            self.created = true;
        }
        if let Some(ch) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            if !self.item_added {
                out.push(fmt("response.output_item.added", &json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":format!("msg_{}",self.id),"role":"assistant","status":"in_progress","content":[]}})));
                self.item_added = true;
            }
            self.text.push_str(ch);
            out.push(fmt(
                "response.output_text.delta",
                &json!({"type":"response.output_text.delta","delta":ch}),
            ));
        }
    }
}

fn fmt(event: &str, data: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, data)
}

// ── 单测（FR-5.1~5.4）─────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_request_messages() {
        // Responses：instructions + input(数组消息)
        let body = br#"{"model":"gpt-x","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hello"}]}],"stream":false,"temperature":0.5,"max_output_tokens":100}"#;
        let conv = responses_to_chat_request(body).unwrap();
        assert!(!conv.stream);
        let v: Value = serde_json::from_slice(&conv.body).unwrap();
        let msgs = v.get("messages").unwrap().as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be brief");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello");
        assert_eq!(v["model"], "gpt-x");
        assert_eq!(v["max_tokens"], 100);
        assert_eq!(v["temperature"], 0.5);
        // 未知 Responses 字段（max_output_tokens 之外）不应泄漏为 chat 字段
        assert!(v.get("input").is_none());
    }

    /// 真机(DeepSeek)实测回归:codex 的 developer role 消息须映射为 system,
    /// 否则上游报 unknown variant `developer`。
    #[test]
    fn maps_developer_role_to_system() {
        let body = br#"{"model":"m","input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"You are Codex"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let conv = responses_to_chat_request(body).unwrap();
        let v: Value = serde_json::from_slice(&conv.body).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["role"], "system",
            "developer 应映射为 system:\n{msgs:?}"
        );
        assert_eq!(msgs[0]["content"], "You are Codex");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn converts_request_string_input() {
        let body = br#"{"model":"m","input":"Hi there","stream":true}"#;
        let conv = responses_to_chat_request(body).unwrap();
        assert!(conv.stream);
        let v: Value = serde_json::from_slice(&conv.body).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "Hi there");
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn converts_nonstream_response() {
        let chat = br#"{"id":"chat-1","choices":[{"index":0,"message":{"role":"assistant","content":"Hi back"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
        let out = chat_json_to_responses_json(chat).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], "chat-1");
        assert_eq!(v["object"], "response"); // 标准字段：Codex 客户端依赖
        assert!(v["created_at"].is_i64());
        assert_eq!(v["status"], "completed");
        assert_eq!(v["output"][0]["content"][0]["text"], "Hi back");
        assert_eq!(v["usage"]["input_tokens"], 3);
        assert_eq!(v["usage"]["output_tokens"], 2);
    }

    /// FR-5.4：round-trip——用户文本经请求转换后能被 chat 侧读到，chat 响应文本能转回 responses。
    #[test]
    fn round_trip_text_preserved() {
        let req = br#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"ping"}]}]}"#;
        let conv = responses_to_chat_request(req).unwrap();
        let chat_req: Value = serde_json::from_slice(&conv.body).unwrap();
        assert_eq!(chat_req["messages"][0]["content"], "ping"); // 请求侧文本一致
                                                                // 模拟 chat 上游回复
        let chat_resp = br#"{"id":"c","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}]}"#;
        let resp = chat_json_to_responses_json(chat_resp).unwrap();
        let v: Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(v["output"][0]["content"][0]["text"], "pong"); // 响应侧文本一致
    }

    /// 真机(codex 0.148)暴露的增量转换器缺陷回归:①delta 前须有 output_item.added(active item)
    /// ②completed 的 usage 须含 input_tokens(空对象被 Codex 判 parse 失败断流)③标准字段 object/created_at/model。
    #[test]
    fn sse_conv_state_emits_item_events_and_full_usage() {
        let mut c = SseConvState::new();
        let events = c.feed("data: {\"id\":\"chatcmpl-9\",\"model\":\"deepseek-chat\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"}}]}\n\n".as_bytes());
        assert_eq!(
            events.len(),
            3,
            "首块应出 created + item.added + delta:\n{events:?}"
        );
        assert!(events[0].starts_with("event: response.created"));
        assert!(
            events[1].starts_with("event: response.output_item.added"),
            "delta 前必须有 active item:\n{:?}",
            events
        );
        assert!(events[2].starts_with("event: response.output_text.delta"));

        let tail = c.feed("data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"}}]}\n\n".as_bytes());
        assert_eq!(tail.len(), 1, "后续块只出 delta");

        let done = c.finish();
        let joined = done.join("");
        assert!(joined.contains("event: response.output_text.done"));
        assert!(
            joined.contains("event: response.output_item.done"),
            "缺 output_item.done:\n{joined}"
        );
        assert!(joined.contains("event: response.completed"));
        assert!(
            joined.contains(r#""object":"response""#),
            "completed 缺 object:\n{joined}"
        );
        assert!(
            joined.contains(r#""created_at""#),
            "completed 缺 created_at:\n{joined}"
        );
        assert!(
            joined.contains(r#""model":"deepseek-chat""#),
            "completed 缺 model:\n{joined}"
        );
        assert!(
            joined.contains(r#""input_tokens":0"#),
            "无上游 usage 时须全零兜底而非空对象:\n{joined}"
        );
        assert!(joined.contains(r#""text":"你好""#));

        // 带上游 usage:converted 数值
        let mut c2 = SseConvState::new();
        c2.feed("data: {\"id\":\"c2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n".as_bytes());
        let f2 = c2.finish().join("");
        assert!(
            f2.contains(r#""input_tokens":7"#) && f2.contains(r#""total_tokens":10"#),
            "上游 usage 应转换:\n{f2}"
        );
    }
}
