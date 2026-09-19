//! Chat Completions ↔ Responses 双向转换。
//!
//! 中文说明：渠道上游只支持 `/v1/chat/completions` 时，pi / codex 仍然按 Responses 协议访问
//! 本机网关，所以这里负责把 Responses 请求体翻成 chat 请求体，再把 chat 响应（含 SSE 流）
//! 翻回 Responses 事件流。这样「只支持 chat 协议」的聚合站也能直接接进 codex。

use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::convert::Infallible;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{now, normalized_usage, send_sse, sse_block, AppState};

/// 可以直接透传给上游的采样参数。Responses 专属字段（`input` / `instructions` / `store` 等）
/// 不在这里，避免上游因为不认识而报错。
const PARAM_KEYS: [&str; 17] = [
    "stream",
    "temperature",
    "top_p",
    "top_k",
    "max_tokens",
    "max_completion_tokens",
    "stop",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "response_format",
    "tool_choice",
    "parallel_tool_calls",
    "user",
];

/// 请求侧：把 Responses（或已经是 chat）的请求体翻成 chat completions 请求体。
pub fn to_chat_request(body: &Value, model: &str) -> Value {
    let mut outgoing = Map::new();
    outgoing.insert("model".into(), Value::String(model.to_string()));

    let mut messages = match body.get("messages").and_then(Value::as_array) {
        Some(messages) => messages.clone(),
        None => messages_from_input(body),
    };
    // `instructions` / `system` 是 Responses 的顶层系统提示，chat 协议里要落到消息里。
    let instructions = body
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            let system = crate::anthropic::system_text(body.get("system"));
            if system.is_empty() { None } else { Some(system) }
        })
        .unwrap_or_default();
    if !instructions.trim().is_empty() && !has_system_message(&messages) {
        messages.insert(0, json!({"role": "system", "content": instructions}));
    }
    outgoing.insert("messages".into(), Value::Array(messages));

    copy_params(body, &mut outgoing);
    Value::Object(outgoing)
}

fn has_system_message(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|message| matches!(message.get("role").and_then(Value::as_str), Some("system") | Some("developer")))
}

fn copy_params(body: &Value, outgoing: &mut Map<String, Value>) {
    for key in PARAM_KEYS {
        if let Some(value) = body.get(key).filter(|value| !value.is_null()) {
            outgoing.insert(key.into(), value.clone());
        }
    }
    if !outgoing.contains_key("max_tokens") {
        if let Some(value) = body.get("max_output_tokens").filter(|value| !value.is_null()) {
            outgoing.insert("max_tokens".into(), value.clone());
        }
    }
    // 不少聚合站不认 max_completion_tokens，统一成 max_tokens。
    outgoing.remove("max_completion_tokens");
    if let Some(effort) = body
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .or_else(|| body.get("reasoning_effort"))
        .and_then(Value::as_str)
        .filter(|effort| !matches!(effort.to_ascii_lowercase().as_str(), "none" | "off"))
    {
        outgoing.insert("reasoning_effort".into(), Value::String(effort.to_string()));
    }
    if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
        outgoing.insert("tools".into(), chat_tools(tools));
    }
    if outgoing.get("stream").and_then(Value::as_bool).unwrap_or(false) && !outgoing.contains_key("stream_options") {
        outgoing.insert("stream_options".into(), json!({"include_usage": true}));
    }
}

/// Responses 的 function 工具是平铺的，chat 协议要求嵌在 `function` 里。
fn chat_tools(tools: &Value) -> Value {
    let Some(items) = tools.as_array() else {
        return tools.clone();
    };
    Value::Array(
        items
            .iter()
            .map(|tool| {
                let Some(map) = tool.as_object() else {
                    return tool.clone();
                };
                if map.get("function").is_some() || map.get("type").and_then(Value::as_str) != Some("function") {
                    return tool.clone();
                }
                let name = map.get("name").cloned().unwrap_or(Value::String(String::new()));
                let mut function = Map::new();
                function.insert("name".into(), name);
                for key in ["description", "parameters", "strict"] {
                    if let Some(value) = map.get(key) {
                        function.insert(key.into(), value.clone());
                    }
                }
                json!({"type": "function", "function": Value::Object(function)})
            })
            .collect(),
    )
}

fn messages_from_input(body: &Value) -> Vec<Value> {
    match body.get("input") {
        Some(Value::String(text)) => return vec![json!({"role": "user", "content": text})],
        Some(Value::Array(items)) => messages_from_items(items),
        _ => Vec::new(),
    }
}

fn messages_from_items(items: &[Value]) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    let mut pending_calls: Vec<Value> = Vec::new();
    for item in items {
        if let Some(text) = item.as_str() {
            flush_calls(&mut messages, &mut pending_calls);
            messages.push(json!({"role": "user", "content": text}));
            continue;
        }
        let Some(map) = item.as_object() else {
            continue;
        };
        match map.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                pending_calls.push(json!({
                    "id": map.get("call_id").or_else(|| map.get("id")).and_then(Value::as_str).unwrap_or(""),
                    "type": "function",
                    "function": {
                        "name": map.get("name").and_then(Value::as_str).unwrap_or(""),
                        "arguments": arguments_text(map.get("arguments")),
                    }
                }));
                continue;
            }
            Some("function_call_output") => {
                flush_calls(&mut messages, &mut pending_calls);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": map.get("call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": output_text(map.get("output")),
                }));
                continue;
            }
            // 思考内容在 chat 协议里没有对应位置，直接丢弃。
            Some("reasoning") => continue,
            _ => {}
        }
        let role = map.get("role").and_then(Value::as_str).unwrap_or("user");
        let role = if role == "developer" { "system" } else { role };
        if role != "assistant" {
            flush_calls(&mut messages, &mut pending_calls);
        }
        let content = content_to_chat(map.get("content").or_else(|| map.get("text")), role);
        let mut message = json!({"role": role, "content": content});
        if let Some(calls) = map.get("tool_calls") {
            message["tool_calls"] = calls.clone();
        }
        messages.push(message);
    }
    flush_calls(&mut messages, &mut pending_calls);
    messages
}

fn flush_calls(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let calls = std::mem::take(pending);
    messages.push(json!({"role": "assistant", "content": Value::Null, "tool_calls": calls}));
}

fn arguments_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn output_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str).or_else(|| part.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Responses 的内容块 → chat 的 `content`。全文本时收敛成字符串，避免上游不认数组。
fn content_to_chat(content: Option<&Value>, role: &str) -> Value {
    let text_type = if role == "assistant" { "output_text" } else { "input_text" };
    let parts: Vec<Value> = match content {
        Some(Value::String(text)) => vec![json!({"type": text_type, "text": text})],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                if let Some(text) = item.as_str() {
                    return Some(json!({"type": text_type, "text": text}));
                }
                let map = item.as_object()?;
                match map.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        Some(json!({"type": text_type, "text": map.get("text").and_then(Value::as_str).unwrap_or("")}))
                    }
                    Some("refusal") => Some(json!({
                        "type": text_type,
                        "text": map.get("refusal").or_else(|| map.get("text")).and_then(Value::as_str).unwrap_or("")
                    })),
                    Some("image_url") => Some(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": item.pointer("/image_url/url").and_then(Value::as_str).unwrap_or(""),
                            "detail": item.pointer("/image_url/detail").and_then(Value::as_str).unwrap_or("auto"),
                        }
                    })),
                    Some("input_image") => Some(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": map.get("image_url").and_then(Value::as_str).unwrap_or(""),
                            "detail": map.get("detail").and_then(Value::as_str).unwrap_or("auto"),
                        }
                    })),
                    Some("image") => {
                        let data = map.get("data").and_then(Value::as_str)?;
                        let mime = map.get("mimeType").or_else(|| map.get("media_type")).and_then(Value::as_str).unwrap_or("image/png");
                        Some(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}"), "detail": "auto"}}))
                    }
                    _ => None,
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    if parts.iter().all(|part| part.get("type").and_then(Value::as_str) != Some("image_url")) {
        let text = parts.iter().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("");
        return Value::String(text);
    }
    Value::Array(parts)
}

/// chat 的 usage 字段名（prompt/completion）→ Responses 的（input/output）。
fn responses_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = normalized_usage(usage)?;
    let cached = usage.get("prompt_tokens_details").and_then(|details| details.get("cached_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let reasoning = usage.get("completion_tokens_details").and_then(|details| details.get("reasoning_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Some(json!({
        "input_tokens": usage["prompt_tokens"],
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": usage["completion_tokens"],
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": usage["total_tokens"],
    }))
}

/// 响应侧：补齐 chat 响应里下游需要的字段（id / object / model / usage）。
pub fn normalize_chat_response(mut data: Value, requested_model: &str) -> Value {
    let usage = normalized_usage(data.get("usage"));
    if let Some(map) = data.as_object_mut() {
        map.insert("model".into(), Value::String(requested_model.to_string()));
        map.insert("object".into(), Value::String("chat.completion".into()));
        if !map.get("id").and_then(Value::as_str).is_some_and(|value| !value.is_empty()) {
            map.insert("id".into(), Value::String(format!("chatcmpl-{}", Uuid::new_v4().simple())));
        }
        map.insert("created".into(), json!(map.get("created").and_then(Value::as_u64).unwrap_or_else(now)));
        if let Some(usage) = usage {
            map.insert("usage".into(), usage);
        }
    }
    data
}

fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.get("content").and_then(Value::as_str))
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// chat 响应体 → Responses 响应体。
pub fn to_responses_json(data: &Value, requested_model: &str) -> Value {
    let choice = data.pointer("/choices/0");
    let message = choice.and_then(|value| value.get("message"));
    let finish = choice.and_then(|value| value.get("finish_reason")).and_then(Value::as_str);
    let text = message.map(message_text).unwrap_or_default();
    let calls = message.and_then(|value| value.get("tool_calls")).and_then(Value::as_array).cloned().unwrap_or_default();
    let reasoning = message.and_then(|value| value.get("reasoning_content")).and_then(Value::as_str).unwrap_or("");

    let mut output: Vec<Value> = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({
            "type": "reasoning",
            "id": format!("rs_{}", Uuid::new_v4().simple()),
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    if !text.is_empty() || calls.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    output.extend(calls.iter().map(function_call_item));

    let mut response = json!({
        "id": data.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple())),
        "object": "response",
        "created_at": data.get("created").and_then(Value::as_u64).unwrap_or_else(now),
        "status": if finish == Some("length") { "incomplete" } else { "completed" },
        "model": requested_model,
        "output": output,
        "output_text": text,
    });
    if let Some(usage) = responses_usage(data.get("usage")) {
        response["usage"] = usage;
    }
    if finish == Some("length") {
        response["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    response
}

fn function_call_item(call: &Value) -> Value {
    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
    json!({
        "type": "function_call",
        "id": format!("fc_{}", Uuid::new_v4().simple()),
        "call_id": id,
        "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
        "arguments": call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or(""),
        "status": "completed",
    })
}

/// 流式状态机：把 chat SSE 分片拼成 Responses 事件流。
struct ChatStream {
    requested_model: String,
    response_id: String,
    message_id: String,
    text: String,
    sequence: u64,
    /// 是否已经为文本发过 output_item.added（决定输出索引与收尾事件）。
    text_item_open: bool,
    tools: Vec<StreamTool>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    created_at: u64,
}

struct StreamTool {
    index: usize,
    call_id: String,
    item_id: String,
    name: String,
    arguments: String,
    open: bool,
}

impl ChatStream {
    fn new(requested_model: String) -> Self {
        Self {
            requested_model,
            response_id: format!("resp_{}", Uuid::new_v4().simple()),
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            text: String::new(),
            sequence: 0,
            text_item_open: false,
            tools: Vec::new(),
            usage: None,
            finish_reason: None,
            created_at: now(),
        }
    }

    fn event(&mut self, event_type: &str, extra: Value) -> Value {
        let mut map = extra.as_object().cloned().unwrap_or_default();
        map.insert("type".into(), Value::String(event_type.into()));
        map.insert("sequence_number".into(), json!(self.sequence));
        self.sequence += 1;
        Value::Object(map)
    }

    fn response_body(&self, status: &str) -> Value {
        // 消息固定占输出索引 0，工具调用顺延，保证事件里的 output_index 与最终 output 对齐。
        let mut output: Vec<Value> = vec![json!({
            "type": "message",
            "id": &self.message_id,
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": &self.text, "annotations": []}],
        })];
        output.extend(self.tools.iter().map(|tool| {
            json!({
                "type": "function_call",
                "id": tool.item_id,
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": tool.arguments,
                "status": "completed",
            })
        }));
        let mut response = json!({
            "id": &self.response_id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "model": &self.requested_model,
            "output": output,
            "output_text": &self.text,
        });
        if let Some(usage) = &self.usage {
            response["usage"] = usage.clone();
        }
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        response
    }

    /// 文本增量：必要时先补上 item / content_part 的 added 事件。
    fn text_delta(&mut self, delta: &str) -> Vec<Value> {
        if delta.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        if !self.text_item_open {
            self.text_item_open = true;
            events.push(self.event(
                "response.output_item.added",
                json!({
                    "output_index": 0,
                    "item": {"id": self.message_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
                }),
            ));
            events.push(self.event(
                "response.content_part.added",
                json!({
                    "item_id": self.message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            ));
        }
        self.text.push_str(delta);
        events.push(self.event(
            "response.output_text.delta",
            json!({
                "item_id": self.message_id,
                "output_index": 0,
                "content_index": 0,
                "delta": delta,
            }),
        ));
        events
    }

    /// 工具调用增量。`start` 为真表示这是一个新的工具调用。
    fn tool_delta(&mut self, index: i64, id: Option<&str>, name: Option<&str>, arguments: Option<&str>) -> Vec<Value> {
        let mut events = Vec::new();
        let index = index.max(0) as usize;
        if !self.tools.iter().any(|tool| tool.index == index) {
            let call_id = id.map(str::to_string).unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
            self.tools.push(StreamTool {
                index,
                call_id,
                item_id: format!("fc_{}", Uuid::new_v4().simple()),
                name: name.unwrap_or("").to_string(),
                arguments: String::new(),
                open: false,
            });
        }
        let position = self.tools.iter().position(|tool| tool.index == index).unwrap_or(0);
        let mut added = None;
        {
            let tool = &mut self.tools[position];
            if let Some(name) = name.filter(|name| !name.is_empty()) {
                tool.name = name.to_string();
            }
            if !tool.open {
                tool.open = true;
                added = Some((tool.item_id.clone(), tool.call_id.clone(), tool.name.clone()));
            }
            if let Some(arguments) = arguments.filter(|arguments| !arguments.is_empty()) {
                tool.arguments.push_str(arguments);
            }
        }
        if let Some((item_id, call_id, name)) = added {
            let item = json!({
                "type": "function_call",
                "id": item_id,
                "call_id": call_id,
                "name": name,
                "arguments": "",
                "status": "in_progress",
            });
            events.push(self.event("response.output_item.added", json!({"output_index": position + 1, "item": item})));
        }
        if let Some(arguments) = arguments.filter(|arguments| !arguments.is_empty()) {
            let item_id = self.tools[position].item_id.clone();
            events.push(self.event(
                "response.function_call_arguments.delta",
                json!({
                    "item_id": item_id,
                    "output_index": position + 1,
                    "delta": arguments,
                }),
            ));
        }
        events
    }

    fn finish(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.text_item_open {
            events.push(self.event(
                "response.output_text.done",
                json!({
                    "item_id": self.message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "text": self.text,
                }),
            ));
            events.push(self.event(
                "response.content_part.done",
                json!({
                    "item_id": self.message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": self.text, "annotations": []},
                }),
            ));
        }
        for position in 0..self.tools.len() {
            let tool = &self.tools[position];
            let item_id = tool.item_id.clone();
            let call_id = tool.call_id.clone();
            let name = tool.name.clone();
            let arguments = tool.arguments.clone();
            events.push(self.event(
                "response.function_call_arguments.done",
                json!({"item_id": item_id, "output_index": position + 1, "arguments": arguments}),
            ));
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "output_index": position + 1,
                    "item": {
                        "type": "function_call",
                        "id": item_id,
                        "call_id": call_id,
                        "name": name,
                        "arguments": arguments,
                        "status": "completed",
                    },
                }),
            ));
        }
        if self.text_item_open {
            events.push(self.event(
                "response.output_item.done",
                json!({
                    "output_index": 0,
                    "item": {
                        "type": "message",
                        "id": self.message_id,
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": self.text, "annotations": []}],
                    },
                }),
            ));
        }
        let status = if self.finish_reason.as_deref() == Some("length") { "incomplete" } else { "completed" };
        let body = self.response_body(status);
        let event_type = if status == "incomplete" { "response.incomplete" } else { "response.completed" };
        events.push(self.event(event_type, json!({"response": body})));
        events
    }

    /// 处理一个 chat SSE 分片（已解析成 JSON）。
    fn apply_chunk(&mut self, chunk: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        if let Some(usage) = responses_usage(chunk.get("usage")) {
            self.usage = Some(usage);
        }
        if let Some(finish) = chunk.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(finish.to_string());
        }
        let Some(delta) = chunk.pointer("/choices/0/delta") else {
            return events;
        };
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            // chat 没有思考通道，但内容仍属于答案的一部分时按文本转发。
            if delta.get("content").and_then(Value::as_str).unwrap_or("").is_empty() {
                events.extend(self.text_delta(reasoning));
            }
        }
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            events.extend(self.text_delta(content));
        } else if let Some(Value::Array(parts)) = delta.get("content") {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    events.extend(self.text_delta(text));
                }
            }
        }
        for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let index = call.get("index").and_then(Value::as_i64).unwrap_or(0);
            events.extend(self.tool_delta(
                index,
                call.get("id").and_then(Value::as_str),
                call.pointer("/function/name").and_then(Value::as_str),
                call.pointer("/function/arguments").and_then(Value::as_str),
            ));
        }
        events
    }
}

/// chat SSE → Responses SSE。返回最终 response 体，供调用方记用量。
pub async fn stream_as_responses(
    upstream: reqwest::Response,
    tx: mpsc::Sender<Result<Bytes, Infallible>>,
    requested_model: String,
) -> Option<Value> {
    let mut state = ChatStream::new(requested_model);
    let created = state.response_body("in_progress");
    let event = state.event("response.created", json!({"response": created}));
    if !send_sse(&tx, None, &event.to_string()).await {
        return None;
    }
    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string();
            buffer = buffer[index + 2..].to_string();
            let (_, data) = sse_block(&block);
            if let Some(chunk) = parse_chunk(&data) {
                for event in state.apply_chunk(&chunk) {
                    if !send_sse(&tx, None, &event.to_string()).await {
                        return None;
                    }
                }
            }
        }
    }
    let events = state.finish();
    let response = events
        .iter()
        .rev()
        .find_map(|event| event.get("response").cloned())
        .unwrap_or_else(|| state.response_body("completed"));
    for event in events {
        if !send_sse(&tx, None, &event.to_string()).await {
            return None;
        }
    }
    let _ = send_sse(&tx, None, "[DONE]").await;
    Some(response)
}

fn parse_chunk(data: &str) -> Option<Value> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(data).ok()
}

/// chat SSE 直通：改写 `model` 字段后原样转发给客户端，并记录用量。
pub async fn stream_chat_passthrough(
    upstream: reqwest::Response,
    tx: mpsc::Sender<Result<Bytes, Infallible>>,
    requested_model: String,
    state: AppState,
    key_id: String,
    started: std::time::Instant,
) {
    let is_sse = upstream
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .contains("text/event-stream");
    let mut usage = None;
    if !is_sse {
        // 上游忽略了 stream=true，返回一次性 JSON：补成一个分片，避免客户端等不到内容。
        if let Ok(data) = upstream.json::<Value>().await {
            let data = normalize_chat_response(data, &requested_model);
            usage = data.get("usage").cloned();
            let mut chunk = data;
            chunk["object"] = json!("chat.completion.chunk");
            if let Some(choice) = chunk.pointer("/choices/0").cloned() {
                chunk["choices"] = json!([{"index": 0, "delta": choice.get("message").cloned().unwrap_or(json!({})), "finish_reason": choice.get("finish_reason").cloned().unwrap_or(json!("stop"))}]);
            }
            let _ = send_sse(&tx, None, &chunk.to_string()).await;
        }
        let _ = send_sse(&tx, None, "[DONE]").await;
        crate::record_usage(&state, &key_id, &requested_model, "chat.completions", axum::http::StatusCode::OK, started, usage.as_ref()).await;
        return;
    }
    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else {
            break;
        };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string();
            buffer = buffer[index + 2..].to_string();
            let (_, data) = sse_block(&block);
            if data == "[DONE]" {
                if !send_sse(&tx, None, "[DONE]").await {
                    crate::record_usage(&state, &key_id, &requested_model, "chat.completions", axum::http::StatusCode::OK, started, usage.as_ref()).await;
                    return;
                }
                continue;
            }
            let Some(mut chunk) = parse_chunk(&data) else {
                continue;
            };
            if let Some(found) = normalized_usage(chunk.get("usage")) {
                usage = Some(found);
            }
            if let Some(map) = chunk.as_object_mut() {
                map.insert("model".into(), Value::String(requested_model.clone()));
            }
            if !send_sse(&tx, None, &chunk.to_string()).await {
                break;
            }
        }
    }
    crate::record_usage(&state, &key_id, &requested_model, "chat.completions", axum::http::StatusCode::OK, started, usage.as_ref()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_request_becomes_chat_request() {
        let body = json!({
            "instructions": "be brief",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "42"}
            ],
            "tools": [{"type": "function", "name": "lookup", "description": "d", "parameters": {"type": "object"}}],
            "max_output_tokens": 128,
            "reasoning": {"effort": "high"},
            "stream": true
        });
        let chat = to_chat_request(&body, "huniu/gpt-5.6-sol");
        assert_eq!(chat["model"], "huniu/gpt-5.6-sol");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "be brief");
        assert_eq!(chat["messages"][1]["content"], "hi");
        assert_eq!(chat["messages"][2]["role"], "assistant");
        assert_eq!(chat["messages"][2]["tool_calls"][0]["function"]["name"], "lookup");
        assert_eq!(chat["messages"][3]["role"], "tool");
        assert_eq!(chat["messages"][3]["tool_call_id"], "call_1");
        assert_eq!(chat["messages"][3]["content"], "42");
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
        assert_eq!(chat["max_tokens"], 128);
        assert_eq!(chat["reasoning_effort"], "high");
        assert_eq!(chat["stream_options"]["include_usage"], true);
        assert!(chat.get("input").is_none());
        assert!(chat.get("store").is_none());
    }

    #[test]
    fn chat_request_passes_through_but_normalizes_model() {
        let body = json!({
            "model": "whatever",
            "messages": [{"role": "user", "content": "hi"}],
            "temperature": 0.2,
            "store": false
        });
        let chat = to_chat_request(&body, "huniu/gpt-5.6-sol");
        assert_eq!(chat["model"], "huniu/gpt-5.6-sol");
        assert_eq!(chat["messages"][0]["content"], "hi");
        assert_eq!(chat["temperature"], 0.2);
        assert!(chat.get("store").is_none());
    }

    #[test]
    fn drops_disabled_reasoning_effort() {
        let body = json!({"input": "hi", "reasoning": {"effort": "none"}});
        let chat = to_chat_request(&body, "m");
        assert!(chat.get("reasoning_effort").is_none());
    }

    #[test]
    fn chat_response_becomes_responses_object() {
        let data = json!({
            "id": "chatcmpl-1",
            "created": 1700000000u64,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hello"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        });
        let response = to_responses_json(&data, "huniu/gpt-5.6-sol");
        assert_eq!(response["model"], "huniu/gpt-5.6-sol");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello");
        assert_eq!(response["output_text"], "hello");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["usage"]["input_tokens"], 3);
        assert_eq!(response["usage"]["output_tokens"], 4);
    }

    #[test]
    fn chat_tool_calls_become_function_call_items() {
        let data = json!({
            "choices": [{"message": {"role": "assistant", "content": Value::Null, "tool_calls": [
                {"id": "call_9", "type": "function", "function": {"name": "shell", "arguments": "{}"}}
            ]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let response = to_responses_json(&data, "huniu/m");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["call_id"], "call_9");
        assert_eq!(response["output"][0]["name"], "shell");
    }

    #[test]
    fn truncation_maps_to_incomplete() {
        let data = json!({"choices": [{"message": {"content": "x"}, "finish_reason": "length"}]});
        let response = to_responses_json(&data, "m");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn normalize_chat_response_fills_usage_and_model() {
        let data = json!({"choices": [{"message": {"content": "x"}}], "usage": {"prompt_tokens": 1, "completion_tokens": 2}});
        let chat = normalize_chat_response(data, "huniu/m");
        assert_eq!(chat["model"], "huniu/m");
        assert_eq!(chat["object"], "chat.completion");
        assert_eq!(chat["usage"]["total_tokens"], 3);
        assert!(chat["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }

    #[test]
    fn stream_emits_text_events_and_final_response() {
        let mut state = ChatStream::new("huniu/m".to_string());
        let first = state.apply_chunk(&json!({"choices": [{"delta": {"content": "he"}}]}));
        let types = first.iter().filter_map(|event| event.get("type").and_then(Value::as_str)).collect::<Vec<_>>();
        assert_eq!(types, vec!["response.output_item.added", "response.content_part.added", "response.output_text.delta"]);
        state.apply_chunk(&json!({"choices": [{"delta": {"content": "llo"}, "finish_reason": "stop"}]}));
        state.apply_chunk(&json!({"choices": [], "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}}));
        let events = state.finish();
        let completed = events.last().unwrap();
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(completed["response"]["output_text"], "hello");
        assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
    }

    #[test]
    fn stream_accumulates_tool_arguments() {
        let mut state = ChatStream::new("m".to_string());
        state.apply_chunk(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "shell", "arguments": "{\"a\""}}]}}]}));
        let events = state.apply_chunk(&json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":1}"}}]}}]}));
        assert_eq!(events[0]["type"], "response.function_call_arguments.delta");
        let finished = state.finish();
        let done = finished.iter().find(|event| event["type"] == "response.output_item.done").unwrap();
        assert_eq!(done["item"]["arguments"], "{\"a\":1}");
        assert_eq!(done["item"]["call_id"], "call_1");
    }
}
