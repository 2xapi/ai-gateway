//! Gemini generateContent ↔ OpenAI ChatCompletions 协议转换(多平台阶段 C 第一段,D2 拍板)。
//!
//! 入口(路由/Key 注入在 gateway::dispatch_gemini,server.rs 注册 `/v1beta/models/:model_action`):
//! - 请求:Gemini `contents`/`systemInstruction`/`tools` → Chat `messages`/`tools`
//!   (role user/model ↔ user/assistant,functionCall ↔ tool_calls,functionResponse ↔ tool 消息);
//! - 非流式响应:Chat `choices[0].message` → Gemini `candidates[0].content` + `usageMetadata`;
//! - 流式响应:Chat SSE `delta` → 逐条 GenerateContentResponse SSE 分块(`?alt=sse` 语义);
//! - 多模态(D2):请求含 inlineData/fileData **明确报错**,不静默降级;
//! - 上游错误:包装为 Gemini 标准错误形态 `{"error":{code,message,status}}`。
//!
//! 透传分支(provider.wire_api=gemini,2xa 原生支持已实测)不做转换,在 dispatch 层直发。

use serde_json::{json, Map, Value};

/// 转换后的 Chat 请求体(stream 由入口 action 决定:generateContent=false / streamGenerateContent=true)。
#[derive(Debug)]
pub struct ConvertedChatRequest {
    pub body: Vec<u8>,
    pub stream: bool,
}

/// 请求转换失败:多模态(400 人话)与非法 body 分开,dispatch 层同返回 400 但文案不同。
#[derive(Debug)]
pub enum GeminiConvError {
    Multimodal(String),
    Invalid(String),
}

impl GeminiConvError {
    pub fn message(&self) -> &str {
        match self {
            GeminiConvError::Multimodal(m) | GeminiConvError::Invalid(m) => m,
        }
    }
}

/// Gemini 请求体(不含 URL 上的 model)→ ChatCompletions 请求体。
pub fn gemini_to_chat_request(
    model: &str,
    stream: bool,
    body: &[u8],
) -> Result<ConvertedChatRequest, GeminiConvError> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| GeminiConvError::Invalid(format!("非法 generateContent body: {e}")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| GeminiConvError::Invalid("generateContent body 不是 object".into()))?;

    let mut messages: Vec<Value> = Vec::new();

    // systemInstruction → system 消息(支持字符串或 {parts:[{text}]} 两种形态)
    if let Some(text) = extract_system_text(obj.get("systemInstruction")) {
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    // contents → messages;functionResponse 的 tool_call_id 与最近生成的 assistant tool_call 按函数名顺序关联
    let contents = obj
        .get("contents")
        .and_then(|c| c.as_array())
        .ok_or_else(|| GeminiConvError::Invalid("缺少 contents 数组".into()))?;
    // 函数名 → 尚未消费的 tool_call_id 队列(gemini 无显式 id,按顺序匹配同名调用)
    let mut pending_calls: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for (ci, content) in contents.iter().enumerate() {
        let role = content
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("user");
        let parts = content
            .get("parts")
            .and_then(|p| p.as_array())
            .ok_or_else(|| GeminiConvError::Invalid(format!("contents[{ci}] 缺少 parts 数组")))?;

        let mut text_buf = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut func_responses: Vec<(String, String)> = Vec::new(); // (name, response JSON 字符串)

        for (pi, part) in parts.iter().enumerate() {
            if part.get("text").is_some() {
                text_buf.push_str(part.get("text").and_then(|t| t.as_str()).unwrap_or(""));
            } else if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let args = fc.get("args").cloned().unwrap_or(json!({}));
                let id = format!("call_gem_{ci}_{pi}");
                pending_calls
                    .entry(name.clone())
                    .or_default()
                    .push(id.clone());
                tool_calls.push(json!({
                    "id": id, "type": "function",
                    "function": { "name": name, "arguments": args.to_string() }
                }));
            } else if let Some(fr) = part.get("functionResponse") {
                let name = fr
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                let resp = fr.get("response").cloned().unwrap_or(json!({}));
                func_responses.push((name, resp.to_string()));
            } else if part.get("inlineData").is_some() || part.get("fileData").is_some() {
                return Err(GeminiConvError::Multimodal(
                    "多模态内容(inlineData/fileData)暂不支持:本入口当前仅支持文本与工具调用,请在 Gemini CLI 中避免发送图片/文件".into(),
                ));
            } else if part.get("thought") == Some(&Value::Bool(true))
                || part.get("thoughtSignature").is_some()
            {
                // 思考 part:Chat 上游不消费,丢弃(透传分支不受影响,原生 gemini 上游原样到达)
            }
            // 其余未知 part 类型丢弃(与 M3b「无法映射的条目丢弃」同策略)
        }

        match role {
            "model" => {
                // assistant:文本(可空) + tool_calls
                if !text_buf.is_empty() || tool_calls.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": text_buf }));
                } else {
                    messages.push(json!({ "role": "assistant", "content": null }));
                }
                if !tool_calls.is_empty() {
                    let m = messages.last_mut().unwrap();
                    m["tool_calls"] = Value::Array(tool_calls);
                }
            }
            _ => {
                // user / tool(functionResponse 归 user content 时拆为 tool 消息)
                if !text_buf.is_empty() {
                    messages.push(json!({ "role": "user", "content": text_buf }));
                }
                for (name, resp) in func_responses {
                    let id = pending_calls
                        .get_mut(&name)
                        .and_then(|q| {
                            if q.is_empty() {
                                None
                            } else {
                                Some(q.remove(0))
                            }
                        })
                        .unwrap_or_else(|| format!("call_gem_orphan_{name}"));
                    messages.push(json!({ "role": "tool", "tool_call_id": id, "content": resp }));
                }
            }
        }
    }

    let mut chat = Map::new();
    chat.insert("model".into(), json!(model));
    chat.insert("messages".into(), Value::Array(messages));

    // generationConfig → Chat 采样参数(克制映射,与 M3b 同策略)
    if let Some(gc) = obj.get("generationConfig").and_then(|g| g.as_object()) {
        for (src, dst) in [
            ("temperature", "temperature"),
            ("topP", "top_p"),
            ("maxOutputTokens", "max_tokens"),
        ] {
            if let Some(x) = gc.get(src) {
                if !x.is_null() {
                    chat.insert(dst.into(), x.clone());
                }
            }
        }
        if let Some(ss) = gc.get("stopSequences").and_then(|s| s.as_array()) {
            let stops: Vec<&str> = ss.iter().filter_map(|s| s.as_str()).collect();
            if !stops.is_empty() {
                chat.insert("stop".into(), json!(stops));
            }
        }
        // 思考档位:thinkingLevel ↔ reasoning_effort(直接小写,取值集合一致)
        if let Some(level) = gc
            .get("thinkingConfig")
            .and_then(|t| t.get("thinkingLevel"))
            .and_then(|l| l.as_str())
            .filter(|s| !s.is_empty())
        {
            chat.insert("reasoning_effort".into(), json!(level.to_ascii_lowercase()));
        }
    }

    // tools.functionDeclarations → Chat tools;toolConfig → tool_choice
    let mut chat_tools: Vec<Value> = Vec::new();
    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            if let Some(decls) = tool.get("functionDeclarations").and_then(|d| d.as_array()) {
                for d in decls {
                    chat_tools.push(json!({
                        "type": "function",
                        "function": {
                            "name": d.get("name").and_then(|n| n.as_str()).unwrap_or_default(),
                            "description": d.get("description").cloned().unwrap_or(Value::Null),
                            "parameters": d.get("parameters").cloned().unwrap_or(json!({ "type": "object" })),
                        }
                    }));
                }
            }
            // googleSearch/codeExecution 等内建工具:Chat 上游无对应,丢弃
        }
    }
    if !chat_tools.is_empty() {
        chat.insert("tools".into(), Value::Array(chat_tools));
        if let Some(mode) = obj
            .get("toolConfig")
            .and_then(|t| t.get("functionCallingConfig"))
            .and_then(|c| c.get("mode"))
            .and_then(|m| m.as_str())
        {
            let choice = match mode {
                "ANY" => "required",
                "NONE" => "none",
                _ => "auto", // AUTO
            };
            chat.insert("tool_choice".into(), json!(choice));
        }
    }

    chat.insert("stream".into(), json!(stream));
    let body = serde_json::to_vec(&Value::Object(chat))
        .map_err(|e| GeminiConvError::Invalid(format!("编码 chat body: {e}")))?;
    Ok(ConvertedChatRequest { body, stream })
}

/// 非流式:ChatCompletions 响应 → Gemini GenerateContentResponse。
pub fn chat_json_to_gemini_json(model: &str, chat: &[u8]) -> Result<Vec<u8>, String> {
    let v: Value = serde_json::from_slice(chat).map_err(|e| format!("非法 chat 响应: {e}"))?;
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));

    let mut parts: Vec<Value> = Vec::new();
    if let Some(t) = msg.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            parts.push(json!({ "text": t }));
        }
    }
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let f = tc.get("function").cloned().unwrap_or(json!({}));
            let args_str = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            parts.push(json!({ "functionCall": { "name": f.get("name").and_then(|n| n.as_str()).unwrap_or_default(), "args": args } }));
        }
    }
    if parts.is_empty() {
        parts.push(json!({ "text": "" }));
    }

    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    let finish_reason = match finish {
        "length" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "STOP", // stop / tool_calls / 未知
    };

    let mut resp = Map::new();
    resp.insert("candidates".into(), json!([{ "content": { "role": "model", "parts": parts }, "finishReason": finish_reason, "index": 0 }]));
    if !model.is_empty() {
        resp.insert("modelVersion".into(), json!(model));
    }
    resp.insert("usageMetadata".into(), convert_usage(v.get("usage")));
    serde_json::to_vec(&Value::Object(resp)).map_err(|e| format!("编码 gemini body: {e}"))
}

/// Chat SSE `delta` → Gemini SSE 分块(逐块即时投递,不缓冲;M3b 增量转换器同思路)。
/// functionCall 的 args 跨块累积,完整后在 finish() 的终块一次性输出(与 Google 真实流式一致)。
pub struct GeminiSseConvState {
    buffer: Vec<u8>,
    tools: Vec<(String, String, String)>, // (id, name, args 增量拼接)
    finish_reason: Option<String>,
    usage: Option<Value>,
    model: String,
    request_id: String,
    saw_text: bool,
}

impl GeminiSseConvState {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            tools: Vec::new(),
            finish_reason: None,
            usage: None,
            model: String::new(),
            request_id: String::new(),
            saw_text: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        // 字节缓冲按 \n 取整行后再解码:多字节 UTF-8 被 TCP 分块切开时不会产生 U+FFFD
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let line = String::from_utf8_lossy(&self.buffer[..pos])
                .trim()
                .to_string();
            self.buffer.drain(..=pos);
            if let Some(ev) = self.proc_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// 上游流结束:输出终块(functionCall parts 若有 + finishReason + usageMetadata 兜底全零)。
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = self.feed(b"\n");

        let mut parts: Vec<Value> = Vec::new();
        for (id, name, args) in std::mem::take(&mut self.tools) {
            let _ = id; // gemini functionCall 无 id 字段
            let args_v: Value = serde_json::from_str(&args).unwrap_or(json!({}));
            parts.push(json!({ "functionCall": { "name": name, "args": args_v } }));
        }

        let finish = self.finish_reason.clone().unwrap_or_else(|| "STOP".into());
        let mut cand = Map::new();
        if !parts.is_empty() {
            cand.insert("content".into(), json!({ "role": "model", "parts": parts }));
        } else {
            cand.insert("content".into(), json!({}));
        }
        cand.insert("finishReason".into(), json!(finish));
        cand.insert("index".into(), json!(0));
        let mut last = Map::new();
        last.insert("candidates".into(), json!([Value::Object(cand)]));
        if !self.model.is_empty() {
            last.insert("modelVersion".into(), json!(self.model.clone()));
        }
        last.insert("usageMetadata".into(), convert_usage(self.usage.as_ref()));
        out.push(format!("data: {}\n\n", Value::Object(last)));
        out
    }

    pub fn usage_snapshot(&self) -> Option<Value> {
        self.usage.clone()
    }

    pub fn model_snapshot(&self) -> Option<String> {
        (!self.model.is_empty()).then(|| self.model.clone())
    }

    pub fn request_id_snapshot(&self) -> Option<String> {
        (!self.request_id.is_empty()).then(|| self.request_id.clone())
    }

    fn proc_line(&mut self, line: &str) -> Option<String> {
        let p = line.strip_prefix("data:")?.trim();
        if p == "[DONE]" {
            return None;
        }
        let v: Value = serde_json::from_str(p).ok()?;
        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
            self.request_id = id.to_string();
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(u.clone());
        }
        if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
            self.model = m.to_string();
        }
        let choice = v.get("choices")?.get(0)?;

        // 文本增量 → 立即出 gemini 分块
        if let Some(t) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str())
        {
            if !t.is_empty() {
                self.saw_text = true;
                return Some(format!(
                    "data: {}\n\n",
                    json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": t }] }, "index": 0 }] })
                ));
            }
        }
        // tool_calls 增量按 index 累积
        if let Some(tcs) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let f = tc.get("function").cloned().unwrap_or(json!({}));
                let name = f
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                while self.tools.len() <= idx {
                    self.tools
                        .push((String::new(), String::new(), String::new()));
                }
                let slot = &mut self.tools[idx];
                if !name.is_empty() {
                    slot.1 = name;
                }
                slot.2.push_str(args);
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    slot.0 = id.to_string();
                }
            }
        }
        if let Some(f) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.finish_reason = Some(match f {
                "length" => "MAX_TOKENS".into(),
                "content_filter" => "SAFETY".into(),
                _ => "STOP".into(),
            });
        }
        None
    }
}

/// usage 兜底:字段全零而非缺失/空对象——M3b 教训(客户端按字段存在性解析)。
fn convert_usage(u: Option<&Value>) -> Value {
    let u = u.unwrap_or(&Value::Null);
    json!({
        "promptTokenCount": u.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
        "candidatesTokenCount": u.get("completion_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
        "totalTokenCount": u.get("total_tokens").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}

/// 上游 Chat 错误 body → Gemini 标准错误形态(CLI 端解析 friendly)。
pub fn chat_error_to_gemini(status: u16, body: &[u8]) -> Vec<u8> {
    let raw = String::from_utf8_lossy(body);
    let msg = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(String::from))
        })
        .unwrap_or_else(|| raw.chars().take(500).collect());
    let status_str = match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        500 => "INTERNAL",
        503 => "UNAVAILABLE",
        _ => "UNKNOWN",
    };
    json!({ "error": { "code": status, "message": msg, "status": status_str } })
        .to_string()
        .into_bytes()
}

fn extract_system_text(si: Option<&Value>) -> Option<String> {
    match si {
        Some(Value::String(s)) => Some(s.clone()),
        Some(obj) => {
            let parts = obj.get("parts")?.as_array()?;
            Some(
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
            )
        }
        None => None,
    }
}

// ── 单测 ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_text_system_tools_request() {
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "你好" }] }],
            "systemInstruction": { "parts": [{ "text": "be brief" }] },
            "generationConfig": {
                "temperature": 0.7, "maxOutputTokens": 256, "topP": 0.9,
                "stopSequences": ["END"],
                "thinkingConfig": { "thinkingLevel": "HIGH" }
            },
            "tools": [{ "functionDeclarations": [{
                "name": "get_weather", "description": "查天气",
                "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
            }] }],
            "toolConfig": { "functionCallingConfig": { "mode": "ANY" } }
        });
        let conv =
            gemini_to_chat_request("gemini-2.5-flash", false, body.to_string().as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&conv.body).unwrap();
        assert_eq!(v["model"], "gemini-2.5-flash");
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be brief");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "你好");
        assert_eq!(v["temperature"], 0.7);
        assert_eq!(v["max_tokens"], 256);
        assert_eq!(v["top_p"], 0.9);
        assert_eq!(v["stop"], json!(["END"]));
        assert_eq!(v["reasoning_effort"], "high");
        assert_eq!(v["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(v["tool_choice"], "required");
    }

    #[test]
    fn converts_function_call_roundtrip() {
        // 会话:model functionCall → user functionResponse → user 追问
        let body = r#"{"contents":[
            {"role":"user","parts":[{"text":"北京天气"}]},
            {"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city":"北京"}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"get_weather","response":{"temp":30}}}]}
        ]}"#;
        let conv = gemini_to_chat_request("m", false, body.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&conv.body).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        let tc = &msgs[1]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], r#"{"city":"北京"}"#);
        assert_eq!(msgs[2]["role"], "tool");
        // tool 消息的 tool_call_id 必须关联到 assistant tool_calls 的 id
        assert_eq!(msgs[2]["tool_call_id"], tc["id"]);
        assert_eq!(msgs[2]["content"], r#"{"temp":30}"#);
    }

    #[test]
    fn multimodal_request_rejected() {
        let body = r#"{"contents":[{"role":"user","parts":[{"text":"看图"},{"inlineData":{"mimeType":"image/png","data":"iVBOR"}}]}]}"#;
        match gemini_to_chat_request("m", false, body.as_bytes()) {
            Err(GeminiConvError::Multimodal(m)) => assert!(m.contains("多模态")),
            other => panic!("应报多模态错误,实际 {other:?}"),
        }
    }

    #[test]
    fn converts_nonstream_response() {
        let chat = r#"{"id":"c1","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"你好","tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":11,"completion_tokens":22,"total_tokens":33}}"#;
        let out = chat_json_to_gemini_json("m", chat.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let cand = &v["candidates"][0];
        assert_eq!(cand["content"]["role"], "model");
        let parts = cand["content"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "你好");
        assert_eq!(parts[1]["functionCall"]["name"], "f");
        assert_eq!(parts[1]["functionCall"]["args"]["a"], 1);
        assert_eq!(cand["finishReason"], "MAX_TOKENS");
        assert_eq!(v["usageMetadata"]["promptTokenCount"], 11);
        assert_eq!(v["usageMetadata"]["candidatesTokenCount"], 22);
        assert_eq!(v["usageMetadata"]["totalTokenCount"], 33);
    }

    #[test]
    fn sse_text_stream_and_zero_usage_fallback() {
        let mk = |v: Value| format!("data: {}\n\n", v);
        let mut c = GeminiSseConvState::new();
        let e1 = c.feed(mk(json!({ "id": "1", "model": "m", "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "你" } }] })).as_bytes());
        assert_eq!(e1.len(), 1, "文本增量应立即出一块:\n{e1:?}");
        let v1: Value = serde_json::from_str(e1[0].trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(v1["candidates"][0]["content"]["parts"][0]["text"], "你");

        let e2 = c.feed(
            mk(json!({ "id": "1", "choices": [{ "index": 0, "delta": { "content": "好" } }] }))
                .as_bytes(),
        );
        assert_eq!(e2.len(), 1);
        let e3 = c.feed(mk(json!({ "id": "1", "choices": [{ "index": 0, "finish_reason": "stop" }], "usage": { "prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4 } })).as_bytes());
        assert_eq!(e3.len(), 0, "finish/usage 不应即时出块");

        let fin = c.finish();
        let joined = fin.join("");
        assert!(joined.contains(r#""finishReason":"STOP""#));
        assert!(
            joined.contains(r#""promptTokenCount":2"#),
            "上游 usage 应转换:\n{joined}"
        );
        assert!(joined.contains(r#""candidatesTokenCount":2"#));
    }

    /// M3b 教训专项:上游无 usage 时终块必须带全零 usageMetadata(空缺会被客户端判解析失败)。
    #[test]
    fn sse_usage_missing_falls_back_zero() {
        let mut c = GeminiSseConvState::new();
        c.feed(b"data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\ndata: [DONE]\n\n");
        let fin = c.finish().join("");
        assert!(
            fin.contains(r#""promptTokenCount":0"#),
            "usage 缺失须全零兜底:\n{fin}"
        );
        assert!(fin.contains(r#""candidatesTokenCount":0"#));
        assert!(fin.contains(r#""totalTokenCount":0"#));
    }

    #[test]
    fn sse_tool_calls_accumulate_across_chunks() {
        // 用 json! 宏构造 SSE 行,避免手写转义错漏
        let mk = |d: Value| {
            format!(
                "data: {}\n\n",
                json!({ "id": "1", "choices": [{ "index": 0, "delta": d }] })
            )
        };
        let mut c = GeminiSseConvState::new();
        let _ = c.feed(mk(json!({ "tool_calls": [{ "index": 0, "id": "call_9", "function": { "name": "get_w", "arguments": "{\"ci" } }] })).as_bytes());
        let _ = c.feed(mk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "ty\":\"BJ\"}" } }] })).as_bytes());
        let _ = c.feed(
            format!(
                "data: {}\n\n",
                json!({ "id": "1", "choices": [{ "index": 0, "finish_reason": "tool_calls" }] })
            )
            .as_bytes(),
        );
        let fin = c.finish().join("");
        assert!(
            fin.contains(r#""functionCall""#),
            "终块应含 functionCall:\n{fin}"
        );
        assert!(fin.contains(r#""name":"get_w""#));
        assert!(
            fin.contains(r#""city":"BJ""#),
            "args 跨块拼接应完整:\n{fin}"
        );
        assert!(
            fin.contains(r#""finishReason":"STOP""#),
            "tool_calls 完成 → STOP(gemini 语义):\n{fin}"
        );
    }

    #[test]
    fn chat_error_wrapped_to_gemini_shape() {
        let out = chat_error_to_gemini(401, br#"{"error":{"message":"bad key","type":"auth"}}"#);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["error"]["code"], 401);
        assert_eq!(v["error"]["message"], "bad key");
        assert_eq!(v["error"]["status"], "UNAUTHENTICATED");
        // 非 JSON body 兜底
        let out2 = chat_error_to_gemini(500, b"upstream boom");
        let v2: Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["error"]["message"], "upstream boom");
        assert_eq!(v2["error"]["status"], "INTERNAL");
    }

    /// 多字节 UTF-8 被 TCP 分块切开时,字节缓冲按整行解码,不得出现 U+FFFD。
    #[test]
    fn sse_feed_preserves_utf8_across_chunk_boundaries() {
        let mut c = GeminiSseConvState::new();
        // 「你好」= E4 BD A0 E5 A5 BD,在 E4 BD 之后切开喂入
        let head =
            b"data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\xe4\xbd";
        let tail = b"\xa0\xe5\xa5\xbd\"}}]}\n\n";
        assert!(c.feed(head).is_empty(), "半行不应出块");
        let evs = c.feed(tail);
        assert_eq!(evs.len(), 1, "整行到齐应出一块:\n{evs:?}");
        assert!(evs[0].contains("你好"), "UTF-8 拼接应无损:\n{:?}", evs);
        assert!(!evs[0].contains('\u{fffd}'), "不得出现 U+FFFD:\n{:?}", evs);
    }
}
