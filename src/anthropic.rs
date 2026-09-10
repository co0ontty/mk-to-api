use axum::body::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::convert::Infallible;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use crate::{normalized_usage, now, send_sse, sse_block};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn is_anthropic_type(value: Option<&str>) -> bool {
    matches!(value, Some("anthropic") | Some("anthropic-messages"))
}

pub fn anthropic_version() -> &'static str {
    ANTHROPIC_VERSION
}

pub fn system_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.as_str()),
                Value::Object(map) => map.get("text").and_then(Value::as_str),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn text_from_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(value) => Some(value.clone()),
                Value::Object(map) => map
                    .get("text")
                    .or_else(|| map.get("output"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                _ => None,
            })
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn content_blocks(content: Option<&Value>, for_assistant: bool) -> Vec<Value> {
    match content {
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(json!({"type": "text", "text": text})),
                Value::Object(map) => {
                    let kind = map.get("type").and_then(Value::as_str).unwrap_or("text");
                    if matches!(kind, "text" | "input_text" | "output_text") {
                        Some(json!({"type": "text", "text": map.get("text").and_then(Value::as_str).unwrap_or("")}))
                    } else if !for_assistant && kind == "input_image" {
                        if let Some(url) = map.get("image_url").and_then(Value::as_str) {
                            Some(json!({"type": "image", "source": {"type": "url", "url": url}}))
                        } else {
                            None
                        }
                    } else if !for_assistant && kind == "image_url" {
                        map.get("image_url")
                            .and_then(|value| value.get("url"))
                            .and_then(Value::as_str)
                            .map(|url| json!({"type": "image", "source": {"type": "url", "url": url}}))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

fn convert_tools(tools: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(tools)) = tools else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let map = tool.as_object()?;
            let function = map.get("function").and_then(Value::as_object).unwrap_or(map);
            let name = function.get("name").or_else(|| map.get("name")).and_then(Value::as_str)?;
            let description = function
                .get("description")
                .or_else(|| map.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let parameters = function
                .get("parameters")
                .or_else(|| function.get("input_schema"))
                .or_else(|| map.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            }))
        })
        .collect()
}

fn thinking_from_body(body: &Value) -> Option<Value> {
    let effort = body
        .pointer("/reasoning/effort")
        .or_else(|| body.get("reasoning_effort"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if effort.is_empty() || effort == "off" || effort == "none" {
        return None;
    }
    let budget = match effort {
        "minimal" | "low" => 1024,
        "medium" => 4096,
        "high" => 8192,
        "xhigh" | "max" => 16384,
        _ => 4096,
    };
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

fn push_message(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(Value::Array(existing)) = last.get_mut("content") {
                existing.extend(content);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": content}));
}

fn collect_input_items(body: &Value) -> Vec<Value> {
    if let Some(Value::Array(items)) = body.get("input") {
        if !items.is_empty() {
            return items.clone();
        }
    }
    if let Some(Value::Array(items)) = body.get("messages") {
        return items.clone();
    }
    Vec::new()
}

pub fn to_anthropic_request(body: &Value, model: &str, default_max_tokens: u64) -> Value {
    let mut system_parts = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            system_parts.push(instructions.to_string());
        }
    }
    let mut messages = Vec::new();
    for item in collect_input_items(body) {
        let map = match item.as_object() {
            Some(map) => map,
            None => {
                if let Some(text) = item.as_str() {
                    push_message(&mut messages, "user", vec![json!({"type": "text", "text": text})]);
                }
                continue;
            }
        };
        let item_type = map.get("type").and_then(Value::as_str).unwrap_or("");
        let role = map.get("role").and_then(Value::as_str).unwrap_or("");
        if matches!(role, "system" | "developer") || item_type == "developer" {
            let text = text_from_content(map.get("content").or_else(|| map.get("text")));
            if !text.is_empty() {
                system_parts.push(text);
            }
            continue;
        }
        if item_type == "function_call" || item_type == "tool_use" {
            let id = map
                .get("call_id")
                .or_else(|| map.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = map.get("name").and_then(Value::as_str).unwrap_or("");
            push_message(
                &mut messages,
                "assistant",
                vec![json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": parse_arguments(map.get("arguments").or_else(|| map.get("input"))),
                })],
            );
            continue;
        }
        if item_type == "function_call_output" || item_type == "tool_result" {
            let id = map
                .get("call_id")
                .or_else(|| map.get("tool_use_id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let output = map
                .get("output")
                .or_else(|| map.get("content"))
                .map(|value| match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            push_message(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                })],
            );
            continue;
        }
        if item_type == "reasoning" {
            continue;
        }
        let normalized_role = if role == "assistant" { "assistant" } else { "user" };
        let blocks = content_blocks(map.get("content").or_else(|| map.get("text")), normalized_role == "assistant");
        if !blocks.is_empty() {
            push_message(&mut messages, normalized_role, blocks);
        }
    }
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }
    let mut max_tokens = body
        .get("max_output_tokens")
        .or_else(|| body.get("max_tokens"))
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(default_max_tokens)
        .max(16);
    let mut outgoing = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    let system = system_parts.into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n");
    outgoing["system"] = Value::String(if system.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        system
    });
    let tools = convert_tools(body.get("tools"));
    if !tools.is_empty() {
        outgoing["tools"] = Value::Array(tools);
    }
    if let Some(tool_choice) = body.get("tool_choice") {
        outgoing["tool_choice"] = match tool_choice {
            Value::String(value) if value == "auto" => json!({"type": "auto"}),
            Value::String(value) if value == "none" => json!({"type": "none"}),
            Value::String(value) if value == "required" => json!({"type": "any"}),
            Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("function") => {
                json!({"type": "tool", "name": map.get("name").or_else(|| map.get("function").and_then(|v| v.get("name"))).and_then(Value::as_str).unwrap_or("")})
            }
            other => other.clone(),
        };
    }
    if let Some(thinking) = thinking_from_body(body) {
        let budget = thinking.get("budget_tokens").and_then(Value::as_u64).unwrap_or(1024);
        if max_tokens <= budget {
            max_tokens = budget + 1024;
            outgoing["max_tokens"] = json!(max_tokens);
        }
        outgoing["thinking"] = thinking;
    }
    outgoing
}

fn usage_from_anthropic(usage: Option<&Value>) -> Value {
    let prompt = usage.and_then(|value| value.get("input_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let completion = usage.and_then(|value| value.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0);
    json!({
        "input_tokens": prompt,
        "output_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens": prompt,
        "completion_tokens": completion,
    })
}

fn stop_reason(value: Option<&str>) -> &'static str {
    match value {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

fn responses_stop_reason(value: Option<&str>) -> &'static str {
    match value {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "incomplete",
        _ => "completed",
    }
}

pub fn to_responses_json(data: &Value, requested_model: &str) -> Value {
    let mut output = Vec::new();
    let mut text = String::new();
    if let Some(Value::Array(blocks)) = data.get("content") {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    let thinking = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                    if !thinking.is_empty() {
                        output.push(json!({
                            "type": "reasoning",
                            "id": format!("rs_{}", Uuid::new_v4()),
                            "summary": [{"type": "summary_text", "text": thinking}],
                        }));
                    }
                }
                Some("text") => {
                    let chunk = block.get("text").and_then(Value::as_str).unwrap_or("");
                    text.push_str(chunk);
                    output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", Uuid::new_v4()),
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": chunk}],
                    }));
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let arguments = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    output.push(json!({
                        "type": "function_call",
                        "id": id,
                        "call_id": id,
                        "name": name,
                        "arguments": arguments.to_string(),
                    }));
                }
                _ => {}
            }
        }
    }
    let status = responses_stop_reason(data.get("stop_reason").and_then(Value::as_str));
    json!({
        "id": data.get("id").and_then(Value::as_str).unwrap_or(""),
        "object": "response",
        "created_at": now(),
        "status": if status == "incomplete" { "incomplete" } else { "completed" },
        "model": requested_model,
        "output": output,
        "output_text": text,
        "usage": usage_from_anthropic(data.get("usage")),
    })
}

pub fn to_chat_json(data: &Value, requested_model: &str) -> Value {
    let responses = to_responses_json(data, requested_model);
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    if let Some(Value::Array(items)) = responses.get("output") {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(Value::Array(parts)) = item.get("content") {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                content.push_str(text);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    tool_calls.push(json!({
                        "id": item.get("call_id").and_then(Value::as_str).unwrap_or(""),
                        "type": "function",
                        "function": {
                            "name": item.get("name").and_then(Value::as_str).unwrap_or(""),
                            "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                        }
                    }));
                }
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant", "content": if content.is_empty() { Value::Null } else { Value::String(content) }});
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    json!({
        "id": format!("chatcmpl-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": now(),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": stop_reason(data.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": normalized_usage(responses.get("usage")),
    })
}

fn responses_event(event_type: &str, extra: Value, sequence: &mut u64) -> Value {
    let mut map = extra.as_object().cloned().unwrap_or_default();
    map.insert("type".into(), Value::String(event_type.into()));
    map.insert("sequence_number".into(), json!(*sequence));
    *sequence += 1;
    Value::Object(map)
}

struct StreamState {
    requested_model: String,
    response_id: String,
    text: String,
    output: Vec<Value>,
    usage: Value,
    stop_reason: Option<String>,
    current_tool: Option<(usize, String, String, String)>,
    current_text_index: Option<usize>,
}

impl StreamState {
    fn new(requested_model: String) -> Self {
        Self {
            requested_model,
            response_id: format!("resp_{}", Uuid::new_v4()),
            text: String::new(),
            output: Vec::new(),
            usage: json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
            stop_reason: None,
            current_tool: None,
            current_text_index: None,
        }
    }

    fn completed(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": now(),
            "status": "completed",
            "model": self.requested_model,
            "output": self.output,
            "output_text": self.text,
            "usage": self.usage,
        })
    }
}

async fn emit(tx: &Sender<Result<Bytes, Infallible>>, payload: &Value) -> bool {
    send_sse(tx, None, &payload.to_string()).await
}

pub async fn stream_as_responses(
    upstream: reqwest::Response,
    tx: Sender<Result<Bytes, Infallible>>,
    requested_model: String,
) -> Option<Value> {
    let mut state = StreamState::new(requested_model);
    let mut sequence = 0u64;
    let created = json!({
        "id": state.response_id,
        "object": "response",
        "created_at": now(),
        "status": "in_progress",
        "model": state.requested_model,
        "output": [],
    });
    if !emit(&tx, &responses_event("response.created", json!({"response": created}), &mut sequence)).await {
        return None;
    }
    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else { break; };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string();
            buffer = buffer[index + 2..].to_string();
            let (_, data) = sse_block(&block);
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&data) else { continue; };
            if !handle_anthropic_event(&event, &mut state, &mut sequence, &tx, false).await {
                return None;
            }
        }
    }
    let completed = state.completed();
    let _ = emit(&tx, &responses_event("response.completed", json!({"response": completed}), &mut sequence)).await;
    Some(state.completed())
}

pub async fn stream_as_chat(
    upstream: reqwest::Response,
    tx: Sender<Result<Bytes, Infallible>>,
    requested_model: String,
) -> Option<Value> {
    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let created = now();
    let model_name = requested_model.clone();
    let chunk = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
        let mut value = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model_name,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(usage) = usage {
            value["usage"] = usage;
        }
        value
    };
    if !emit(&tx, &chunk(json!({"role": "assistant", "content": ""}), None, None)).await {
        return None;
    }
    let mut state = StreamState::new(requested_model);
    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else { break; };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        buffer = buffer.replace("\r\n", "\n");
        while let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string();
            buffer = buffer[index + 2..].to_string();
            let (_, data) = sse_block(&block);
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&data) else { continue; };
            match event.get("type").and_then(Value::as_str) {
                Some("content_block_delta") => {
                    if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                        if !text.is_empty() && !emit(&tx, &chunk(json!({"content": text}), None, None)).await {
                            return None;
                        }
                        state.text.push_str(text);
                    }
                    if let Some(partial) = event.pointer("/delta/partial_json").and_then(Value::as_str) {
                        if let Some((_, _, _, arguments)) = state.current_tool.as_mut() {
                            arguments.push_str(partial);
                        }
                    }
                }
                Some("content_block_start") => {
                    if event.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                        let tool_id = event.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").to_string();
                        let name = event.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").to_string();
                        let _ = emit(
                            &tx,
                            &chunk(
                                json!({
                                    "tool_calls": [{
                                        "index": state.output.len(),
                                        "id": tool_id,
                                        "type": "function",
                                        "function": {"name": name, "arguments": ""}
                                    }]
                                }),
                                None,
                                None,
                            ),
                        )
                        .await;
                        state.current_tool = Some((state.output.len(), tool_id, name, String::new()));
                    }
                }
                Some("content_block_stop") => {
                    if let Some((_, tool_id, name, arguments)) = state.current_tool.take() {
                        state.output.push(json!({
                            "type": "function_call",
                            "id": tool_id,
                            "call_id": tool_id,
                            "name": name,
                            "arguments": arguments,
                        }));
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        state.stop_reason = Some(reason.to_string());
                    }
                    if let Some(usage) = event.get("usage") {
                        state.usage = usage_from_anthropic(Some(usage));
                    }
                }
                Some("message_start") => {
                    if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                        state.response_id = id.to_string();
                    }
                    if let Some(usage) = event.pointer("/message/usage") {
                        state.usage = usage_from_anthropic(Some(usage));
                    }
                }
                _ => {}
            }
        }
    }
    let finish = stop_reason(state.stop_reason.as_deref());
    let _ = emit(&tx, &chunk(json!({}), Some(finish), normalized_usage(Some(&state.usage)))).await;
    let _ = send_sse(&tx, None, "[DONE]").await;
    Some(state.completed())
}

async fn handle_anthropic_event(
    event: &Value,
    state: &mut StreamState,
    sequence: &mut u64,
    tx: &Sender<Result<Bytes, Infallible>>,
    _unused: bool,
) -> bool {
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                state.response_id = id.to_string();
            }
            if let Some(usage) = event.pointer("/message/usage") {
                state.usage = usage_from_anthropic(Some(usage));
            }
            true
        }
        Some("content_block_start") => {
            match event.pointer("/content_block/type").and_then(Value::as_str) {
                Some("text") => {
                    let index = state.output.len();
                    state.current_text_index = Some(index);
                    state.output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", Uuid::new_v4()),
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": ""}],
                    }));
                    true
                }
                Some("tool_use") => {
                    let tool_id = event.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = event.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").to_string();
                    let index = state.output.len();
                    let item = json!({
                        "type": "function_call",
                        "id": tool_id,
                        "call_id": tool_id,
                        "name": name,
                        "arguments": "",
                    });
                    state.output.push(item.clone());
                    state.current_tool = Some((index, tool_id, name, String::new()));
                    emit(tx, &responses_event("response.output_item.added", json!({"output_index": index, "item": item}), sequence)).await
                }
                _ => true,
            }
        }
        Some("content_block_delta") => {
            if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                if !text.is_empty() {
                    state.text.push_str(text);
                    if let Some(index) = state.current_text_index {
                        if let Some(item) = state.output.get_mut(index) {
                            if let Some(Value::Array(content)) = item.get_mut("content") {
                                if let Some(Value::Object(part)) = content.get_mut(0) {
                                    let current = part.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                                    part.insert("text".into(), Value::String(current + text));
                                }
                            }
                        }
                    }
                    return emit(tx, &responses_event("response.output_text.delta", json!({"delta": text}), sequence)).await;
                }
            }
            if let Some(partial) = event.pointer("/delta/partial_json").and_then(Value::as_str) {
                if let Some((index, _, _, arguments)) = state.current_tool.as_mut() {
                    arguments.push_str(partial);
                    let index = *index;
                    return emit(
                        tx,
                        &responses_event(
                            "response.function_call_arguments.delta",
                            json!({"output_index": index, "delta": partial}),
                            sequence,
                        ),
                    )
                    .await;
                }
            }
            true
        }
        Some("content_block_stop") => {
            if let Some((index, tool_id, name, arguments)) = state.current_tool.take() {
                let item = json!({
                    "type": "function_call",
                    "id": tool_id,
                    "call_id": tool_id,
                    "name": name,
                    "arguments": arguments,
                });
                if let Some(slot) = state.output.get_mut(index) {
                    *slot = item.clone();
                }
                return emit(tx, &responses_event("response.output_item.done", json!({"output_index": index, "item": item}), sequence)).await;
            }
            state.current_text_index = None;
            true
        }
        Some("message_delta") => {
            if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(reason.to_string());
            }
            if let Some(usage) = event.get("usage") {
                state.usage = usage_from_anthropic(Some(usage));
            }
            true
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_responses_history_and_tools() {
        let body = json!({
            "instructions": "be brief",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "tools": [{"type": "function", "name": "bash", "description": "run", "parameters": {"type": "object"}}],
            "max_output_tokens": 128
        });
        let outgoing = to_anthropic_request(&body, "monkeycode-basic/deepseek-v4-flash", 32000);
        assert_eq!(outgoing["model"], "monkeycode-basic/deepseek-v4-flash");
        assert_eq!(outgoing["system"], "be brief");
        assert_eq!(outgoing["max_tokens"], 128);
        assert_eq!(outgoing["messages"][0]["role"], "user");
        assert_eq!(outgoing["messages"][1]["role"], "assistant");
        assert_eq!(outgoing["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(outgoing["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(outgoing["tools"][0]["name"], "bash");
    }

    #[test]
    fn converts_anthropic_message_to_responses() {
        let data = json!({
            "id": "msg_1",
            "stop_reason": "end_turn",
            "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "text", "text": "OK"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 2}
        });
        let response = to_responses_json(&data, "monkeycode-basic/deepseek-v4-flash");
        assert_eq!(response["output_text"], "OK");
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["usage"]["input_tokens"], 10);
    }
}
