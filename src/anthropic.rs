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

fn stop_reason(value: Option<&str>) -> &'static str {
    match value {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

fn is_incomplete_stop(value: Option<&str>) -> bool {
    matches!(value, Some("max_tokens"))
}

fn merge_usage(current: &Value, incoming: Option<&Value>) -> Value {
    let prompt = incoming
        .and_then(|value| value.get("input_tokens").or_else(|| value.get("prompt_tokens")))
        .and_then(Value::as_u64)
        .or_else(|| current.get("input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let completion = incoming
        .and_then(|value| value.get("output_tokens").or_else(|| value.get("completion_tokens")))
        .and_then(Value::as_u64)
        .or_else(|| current.get("output_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    json!({
        "input_tokens": prompt,
        "output_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens": prompt,
        "completion_tokens": completion,
    })
}

fn usage_from_anthropic(usage: Option<&Value>) -> Value {
    merge_usage(&json!({"input_tokens": 0, "output_tokens": 0}), usage)
}

fn responses_status(stop_reason: Option<&str>) -> &'static str {
    if is_incomplete_stop(stop_reason) {
        "incomplete"
    } else {
        "completed"
    }
}

fn completed_event_type(stop_reason: Option<&str>) -> &'static str {
    if is_incomplete_stop(stop_reason) {
        "response.incomplete"
    } else {
        "response.completed"
    }
}

fn with_status_fields(mut payload: Value, stop_reason: Option<&str>) -> Value {
    let status = responses_status(stop_reason);
    payload["status"] = Value::String(status.into());
    if status == "incomplete" {
        payload["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    payload
}

pub fn to_responses_json(data: &Value, requested_model: &str) -> Value {
    let mut output = Vec::new();
    let mut text = String::new();
    if let Some(Value::Array(blocks)) = data.get("content") {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking") | Some("redacted_thinking") => {
                    let thinking = block
                        .get("thinking")
                        .or_else(|| block.get("data"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if thinking.is_empty() && block.get("type").and_then(Value::as_str) != Some("redacted_thinking") {
                        continue;
                    }
                    let summary = if thinking.is_empty() { "[redacted]" } else { thinking };
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("rs_{}", Uuid::new_v4()),
                        "summary": [{"type": "summary_text", "text": summary}],
                    }));
                }
                Some("text") => {
                    let chunk = block.get("text").and_then(Value::as_str).unwrap_or("");
                    text.push_str(chunk);
                    output.push(json!({
                        "type": "message",
                        "id": format!("msg_{}", Uuid::new_v4()),
                        "role": "assistant",
                        "status": "completed",
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
                        "status": "completed",
                    }));
                }
                _ => {}
            }
        }
    }
    with_status_fields(
        json!({
            "id": data.get("id").and_then(Value::as_str).unwrap_or(""),
            "object": "response",
            "created_at": now(),
            "model": requested_model,
            "output": output,
            "output_text": text,
            "usage": usage_from_anthropic(data.get("usage")),
        }),
        data.get("stop_reason").and_then(Value::as_str),
    )
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

enum CurrentBlock {
    None,
    Text { index: usize, id: String },
    Reasoning { index: usize, id: String },
    Tool { index: usize, id: String, name: String, arguments: String },
}

struct StreamState {
    requested_model: String,
    response_id: String,
    text: String,
    output: Vec<Value>,
    usage: Value,
    stop_reason: Option<String>,
    current: CurrentBlock,
    error: Option<Value>,
}

impl StreamState {
    fn new(requested_model: String) -> Self {
        Self {
            requested_model,
            response_id: format!("resp_{}", Uuid::new_v4()),
            text: String::new(),
            output: Vec::new(),
            usage: json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0, "prompt_tokens": 0, "completion_tokens": 0}),
            stop_reason: None,
            current: CurrentBlock::None,
            error: None,
        }
    }

    fn response_body(&self) -> Value {
        if let Some(error) = &self.error {
            return json!({
                "id": self.response_id,
                "object": "response",
                "created_at": now(),
                "status": "failed",
                "model": self.requested_model,
                "output": self.output,
                "output_text": self.text,
                "usage": self.usage,
                "error": error,
            });
        }
        with_status_fields(
            json!({
                "id": self.response_id,
                "object": "response",
                "created_at": now(),
                "model": self.requested_model,
                "output": self.output,
                "output_text": self.text,
                "usage": self.usage,
            }),
            self.stop_reason.as_deref(),
        )
    }

    fn apply_event(&mut self, event: &Value, sequence: &mut u64) -> Vec<Value> {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                // Keep the Responses id from response.created so Codex can
                // correlate the whole stream. Anthropic's message id is only
                // used when we have not emitted a local id yet.
                if self.response_id.is_empty() {
                    if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                        self.response_id = id.to_string();
                    }
                }
                if event.pointer("/message/usage").is_some() {
                    self.usage = merge_usage(&self.usage, event.pointer("/message/usage"));
                }
                Vec::new()
            }
            Some("content_block_start") => self.start_block(event, sequence),
            Some("content_block_delta") => self.delta_block(event, sequence),
            Some("content_block_stop") => self.close_current(sequence),
            Some("message_delta") => {
                if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_string());
                }
                if event.get("usage").is_some() {
                    self.usage = merge_usage(&self.usage, event.get("usage"));
                }
                Vec::new()
            }
            Some("error") => {
                self.error = Some(event.get("error").cloned().unwrap_or_else(|| json!({
                    "message": event.get("message").and_then(Value::as_str).unwrap_or("upstream stream failed"),
                    "type": "upstream_error",
                })));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn start_block(&mut self, event: &Value, sequence: &mut u64) -> Vec<Value> {
        match event.pointer("/content_block/type").and_then(Value::as_str) {
            Some("text") => {
                let id = event
                    .pointer("/content_block/id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4()));
                let index = self.output.len();
                let item = json!({
                    "type": "message",
                    "id": id,
                    "role": "assistant",
                    "status": "in_progress",
                    "content": [{"type": "output_text", "text": ""}],
                });
                self.output.push(item.clone());
                self.current = CurrentBlock::Text { index, id: id.clone() };
                vec![
                    responses_event("response.output_item.added", json!({"output_index": index, "item": item}), sequence),
                    responses_event(
                        "response.content_part.added",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": ""}
                        }),
                        sequence,
                    ),
                ]
            }
            Some("thinking") | Some("redacted_thinking") => {
                let id = event
                    .pointer("/content_block/id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("rs_{}", Uuid::new_v4()));
                let initial = event
                    .pointer("/content_block/thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let index = self.output.len();
                let item = json!({
                    "type": "reasoning",
                    "id": id,
                    "summary": [{"type": "summary_text", "text": initial}],
                });
                self.output.push(item.clone());
                self.current = CurrentBlock::Reasoning { index, id: id.clone() };
                let mut events = vec![responses_event("response.output_item.added", json!({"output_index": index, "item": item}), sequence)];
                if !initial.is_empty() {
                    events.push(responses_event(
                        "response.reasoning_summary_text.delta",
                        json!({"item_id": id, "output_index": index, "summary_index": 0, "delta": initial}),
                        sequence,
                    ));
                }
                events
            }
            Some("tool_use") => {
                let tool_id = event.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").to_string();
                let name = event.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").to_string();
                let index = self.output.len();
                let item = json!({
                    "type": "function_call",
                    "id": tool_id,
                    "call_id": tool_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                });
                self.output.push(item.clone());
                self.current = CurrentBlock::Tool {
                    index,
                    id: tool_id,
                    name,
                    arguments: String::new(),
                };
                vec![responses_event("response.output_item.added", json!({"output_index": index, "item": item}), sequence)]
            }
            _ => Vec::new(),
        }
    }

    fn delta_block(&mut self, event: &Value, sequence: &mut u64) -> Vec<Value> {
        if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
            if !text.is_empty() {
                self.text.push_str(text);
                if let CurrentBlock::Text { index, id } = &self.current {
                    let index = *index;
                    let id = id.clone();
                    if let Some(item) = self.output.get_mut(index) {
                        append_output_text(item, text);
                    }
                    return vec![responses_event(
                        "response.output_text.delta",
                        json!({"item_id": id, "output_index": index, "content_index": 0, "delta": text}),
                        sequence,
                    )];
                }
            }
        }
        if let Some(thinking) = event.pointer("/delta/thinking").and_then(Value::as_str) {
            if !thinking.is_empty() {
                if let CurrentBlock::Reasoning { index, id } = &self.current {
                    let index = *index;
                    let id = id.clone();
                    if let Some(item) = self.output.get_mut(index) {
                        append_reasoning_text(item, thinking);
                    }
                    return vec![responses_event(
                        "response.reasoning_summary_text.delta",
                        json!({"item_id": id, "output_index": index, "summary_index": 0, "delta": thinking}),
                        sequence,
                    )];
                }
            }
        }
        if let Some(partial) = event.pointer("/delta/partial_json").and_then(Value::as_str) {
            if let CurrentBlock::Tool { index, id, arguments, .. } = &mut self.current {
                arguments.push_str(partial);
                let index = *index;
                let id = id.clone();
                return vec![responses_event(
                    "response.function_call_arguments.delta",
                    json!({"item_id": id, "output_index": index, "delta": partial}),
                    sequence,
                )];
            }
        }
        Vec::new()
    }

    fn close_current(&mut self, sequence: &mut u64) -> Vec<Value> {
        match std::mem::replace(&mut self.current, CurrentBlock::None) {
            CurrentBlock::None => Vec::new(),
            CurrentBlock::Text { index, id } => {
                if let Some(item) = self.output.get_mut(index) {
                    item["status"] = json!("completed");
                }
                let item = self.output.get(index).cloned().unwrap_or(json!({}));
                let text = item
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                vec![
                    responses_event(
                        "response.output_text.done",
                        json!({"item_id": id, "output_index": index, "content_index": 0, "text": text}),
                        sequence,
                    ),
                    responses_event("response.output_item.done", json!({"output_index": index, "item": item}), sequence),
                ]
            }
            CurrentBlock::Reasoning { index, id } => {
                let item = self.output.get(index).cloned().unwrap_or(json!({}));
                let text = item
                    .pointer("/summary/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                vec![
                    responses_event(
                        "response.reasoning_summary_text.done",
                        json!({"item_id": id, "output_index": index, "summary_index": 0, "text": text}),
                        sequence,
                    ),
                    responses_event("response.output_item.done", json!({"output_index": index, "item": item}), sequence),
                ]
            }
            CurrentBlock::Tool { index, id, name, arguments } => {
                let item = json!({
                    "type": "function_call",
                    "id": id,
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                });
                if let Some(slot) = self.output.get_mut(index) {
                    *slot = item.clone();
                }
                vec![
                    responses_event(
                        "response.function_call_arguments.done",
                        json!({"item_id": id, "output_index": index, "arguments": arguments}),
                        sequence,
                    ),
                    responses_event("response.output_item.done", json!({"output_index": index, "item": item}), sequence),
                ]
            }
        }
    }

    fn finalize(&mut self, sequence: &mut u64) -> Vec<Value> {
        let mut events = self.close_current(sequence);
        let response = self.response_body();
        let event_type = if self.error.is_some() {
            "response.failed"
        } else {
            completed_event_type(self.stop_reason.as_deref())
        };
        let extra = if self.error.is_some() {
            json!({"response": response, "error": response.get("error").cloned().unwrap_or(json!({"message": "upstream stream failed"}))})
        } else {
            json!({"response": response})
        };
        events.push(responses_event(event_type, extra, sequence));
        events
    }
}

fn append_output_text(item: &mut Value, text: &str) {
    if let Some(Value::Array(content)) = item.get_mut("content") {
        if let Some(Value::Object(part)) = content.get_mut(0) {
            let current = part.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            part.insert("text".into(), Value::String(current + text));
        }
    }
}

fn append_reasoning_text(item: &mut Value, text: &str) {
    if let Some(Value::Array(summary)) = item.get_mut("summary") {
        if let Some(Value::Object(part)) = summary.get_mut(0) {
            let current = part.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            part.insert("text".into(), Value::String(current + text));
        }
    }
}

async fn emit(tx: &Sender<Result<Bytes, Infallible>>, payload: &Value) -> bool {
    send_sse(tx, None, &payload.to_string()).await
}

async fn emit_all(tx: &Sender<Result<Bytes, Infallible>>, events: &[Value]) -> bool {
    for event in events {
        if !emit(tx, event).await {
            return false;
        }
    }
    true
}

fn parse_sse_event(block: &str) -> Option<Value> {
    let (_, data) = sse_block(block);
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    serde_json::from_str(&data).ok()
}

fn drain_sse_blocks(buffer: &mut String) -> Vec<Value> {
    *buffer = buffer.replace("\r\n", "\n");
    let mut events = Vec::new();
    while let Some(index) = buffer.find("\n\n") {
        let block = buffer[..index].to_string();
        *buffer = buffer[index + 2..].to_string();
        if let Some(event) = parse_sse_event(&block) {
            events.push(event);
        }
    }
    events
}

fn drain_sse_tail(buffer: &str) -> Option<Value> {
    if buffer.trim().is_empty() {
        None
    } else {
        parse_sse_event(buffer)
    }
}

#[cfg(test)]
fn replay_anthropic_events(events: &[Value], requested_model: &str) -> (Vec<Value>, Value) {
    let mut state = StreamState::new(requested_model.to_string());
    let mut sequence = 0u64;
    let created = json!({
        "id": state.response_id,
        "object": "response",
        "created_at": now(),
        "status": "in_progress",
        "model": state.requested_model,
        "output": [],
    });
    let mut outgoing = vec![responses_event("response.created", json!({"response": created}), &mut sequence)];
    for event in events {
        outgoing.extend(state.apply_event(event, &mut sequence));
    }
    outgoing.extend(state.finalize(&mut sequence));
    let response = outgoing
        .iter()
        .rev()
        .find_map(|event| event.get("response").cloned())
        .unwrap_or_else(|| state.response_body());
    (outgoing, response)
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
        for event in drain_sse_blocks(&mut buffer) {
            if !emit_all(&tx, &state.apply_event(&event, &mut sequence)).await {
                return None;
            }
        }
    }
    if let Some(event) = drain_sse_tail(&buffer) {
        if !emit_all(&tx, &state.apply_event(&event, &mut sequence)).await {
            return None;
        }
    }
    let events = state.finalize(&mut sequence);
    let response = events
        .iter()
        .rev()
        .find_map(|event| event.get("response").cloned())
        .unwrap_or_else(|| state.response_body());
    if !emit_all(&tx, &events).await {
        return None;
    }
    Some(response)
}

fn chat_chunk(id: &str, created: u64, model: &str, delta: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
    let mut value = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

pub async fn stream_as_chat(
    upstream: reqwest::Response,
    tx: Sender<Result<Bytes, Infallible>>,
    requested_model: String,
) -> Option<Value> {
    let id = format!("chatcmpl-{}", Uuid::new_v4());
    let created = now();
    if !emit(&tx, &chat_chunk(&id, created, &requested_model, json!({"role": "assistant", "content": ""}), None, None)).await {
        return None;
    }
    let mut state = StreamState::new(requested_model.clone());
    let mut sequence = 0u64;
    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else { break; };
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        for event in drain_sse_blocks(&mut buffer) {
            if !apply_chat_event(&mut state, &event, &tx, &id, created, &requested_model).await {
                return None;
            }
        }
    }
    if let Some(event) = drain_sse_tail(&buffer) {
        if !apply_chat_event(&mut state, &event, &tx, &id, created, &requested_model).await {
            return None;
        }
    }
    let _ = state.close_current(&mut sequence);
    if let Some(error) = &state.error {
        let _ = emit(&tx, &json!({"error": error})).await;
        let _ = send_sse(&tx, None, "[DONE]").await;
        return Some(state.response_body());
    }
    let finish = stop_reason(state.stop_reason.as_deref());
    let _ = emit(&tx, &chat_chunk(&id, created, &requested_model, json!({}), Some(finish), normalized_usage(Some(&state.usage)))).await;
    let _ = send_sse(&tx, None, "[DONE]").await;
    Some(state.response_body())
}

async fn apply_chat_event(
    state: &mut StreamState,
    event: &Value,
    tx: &Sender<Result<Bytes, Infallible>>,
    id: &str,
    created: u64,
    model: &str,
) -> bool {
    match event.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => {
            if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                if !text.is_empty() {
                    state.text.push_str(text);
                    if !emit(tx, &chat_chunk(id, created, model, json!({"content": text}), None, None)).await {
                        return false;
                    }
                }
            }
            if let Some(partial) = event.pointer("/delta/partial_json").and_then(Value::as_str) {
                if let CurrentBlock::Tool { arguments, .. } = &mut state.current {
                    arguments.push_str(partial);
                }
            }
            true
        }
        Some("content_block_start") => {
            if event.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                let tool_id = event.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("").to_string();
                let name = event.pointer("/content_block/name").and_then(Value::as_str).unwrap_or("").to_string();
                if !emit(
                    tx,
                    &chat_chunk(
                        id,
                        created,
                        model,
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
                .await
                {
                    return false;
                }
                state.current = CurrentBlock::Tool {
                    index: state.output.len(),
                    id: tool_id,
                    name,
                    arguments: String::new(),
                };
            }
            true
        }
        Some("content_block_stop") => {
            if let CurrentBlock::Tool { id, name, arguments, .. } = std::mem::replace(&mut state.current, CurrentBlock::None) {
                state.output.push(json!({
                    "type": "function_call",
                    "id": id,
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                    "status": "completed",
                }));
            } else {
                state.current = CurrentBlock::None;
            }
            true
        }
        Some("message_delta") => {
            if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
                state.stop_reason = Some(reason.to_string());
            }
            if event.get("usage").is_some() {
                state.usage = merge_usage(&state.usage, event.get("usage"));
            }
            true
        }
        Some("message_start") => {
            if event.pointer("/message/usage").is_some() {
                state.usage = merge_usage(&state.usage, event.pointer("/message/usage"));
            }
            true
        }
        Some("error") => {
            state.error = Some(event.get("error").cloned().unwrap_or_else(|| json!({
                "message": event.get("message").and_then(Value::as_str).unwrap_or("upstream stream failed"),
                "type": "upstream_error",
            })));
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
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["usage"]["input_tokens"], 10);
    }

    #[test]
    fn max_tokens_is_incomplete() {
        let data = json!({
            "id": "msg_1",
            "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "partial"}],
            "usage": {"input_tokens": 10, "output_tokens": 2}
        });
        let response = to_responses_json(&data, "deepseek-v4-flash");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(response["output_text"], "partial");
    }

    fn event_types(events: &[Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.get("type").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    #[test]
    fn stream_thinking_only_end_turn_is_not_silent() {
        // Repro: DeepSeek xhigh 最后一轮只吐 thinking，然后 end_turn。
        // 旧转换会生成 completed + 空 output，Codex 显示任务已结束但没有正文。
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_ds", "usage": {"input_tokens": 226, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "I should summarize the work now."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1519}}),
            json!({"type": "message_stop"}),
        ];
        let (outgoing, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let types = event_types(&outgoing);
        assert!(types.contains(&"response.output_item.added".into()), "{types:?}");
        assert!(types.contains(&"response.reasoning_summary_text.delta".into()), "{types:?}");
        assert!(types.contains(&"response.output_item.done".into()), "{types:?}");
        assert!(types.contains(&"response.completed".into()), "{types:?}");
        assert!(response["id"].as_str().unwrap_or("").starts_with("resp_"), "{}", response["id"]);
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"].as_array().map(Vec::len).unwrap_or(0), 1);
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][0]["summary"][0]["text"], "I should summarize the work now.");
        assert_eq!(response["usage"]["input_tokens"], 226);
        assert_eq!(response["usage"]["output_tokens"], 1519);
        assert_eq!(response["output_text"], "");
    }

    #[test]
    fn stream_text_emits_output_item_events() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_text"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "id": "msg_out", "text": ""}}),
            json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "hello "}}),
            json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "world"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        ];
        let (outgoing, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let types = event_types(&outgoing);
        assert_eq!(
            types.iter().filter(|name| *name == "response.output_item.added").count(),
            1
        );
        assert!(types.contains(&"response.content_part.added".into()), "{types:?}");
        assert!(types.contains(&"response.output_text.delta".into()), "{types:?}");
        assert!(types.contains(&"response.output_text.done".into()), "{types:?}");
        assert!(types.contains(&"response.output_item.done".into()), "{types:?}");
        let delta = outgoing
            .iter()
            .find(|event| event.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
            .unwrap();
        assert_eq!(delta["item_id"], "msg_out");
        assert_eq!(delta["output_index"], 0);
        assert_eq!(delta["content_index"], 0);
        assert_eq!(response["output_text"], "hello world");
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello world");
        assert_eq!(response["status"], "completed");
    }

    #[test]
    fn stream_max_tokens_marks_incomplete() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_cut"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "id": "msg_out"}}),
            json!({"type": "content_block_delta", "delta": {"text": "partial"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 32}}),
        ];
        let (outgoing, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let types = event_types(&outgoing);
        assert!(types.contains(&"response.incomplete".into()), "{types:?}");
        assert!(!types.contains(&"response.completed".into()), "{types:?}");
        assert_eq!(response["status"], "incomplete");
        assert_eq!(response["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(response["output_text"], "partial");
    }

    #[test]
    fn stream_tool_then_text_keeps_both_and_closes_args() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_mix"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_1", "name": "exec_command"}}),
            json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{\"cmd\":\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "id": "msg_out"}}),
            json!({"type": "content_block_delta", "delta": {"text": "ran ls"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ];
        let (outgoing, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let types = event_types(&outgoing);
        assert!(types.contains(&"response.function_call_arguments.done".into()), "{types:?}");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(response["output"][1]["type"], "message");
        assert_eq!(response["output_text"], "ran ls");
    }

    #[test]
    fn stream_usage_keeps_input_tokens_from_message_start() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_usage", "usage": {"input_tokens": 100, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "id": "msg_out"}}),
            json!({"type": "content_block_delta", "delta": {"text": "ok"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 8}}),
        ];
        let (_, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        assert_eq!(response["usage"]["input_tokens"], 100);
        assert_eq!(response["usage"]["output_tokens"], 8);
    }

    fn assemble_codex_view(events: &[Value]) -> (Vec<String>, String, Option<String>, Option<String>) {
        // Mimic Codex: only materialize items from output_item.added/done,
        // and assistant text from output_text.delta that carries item_id.
        let mut items: Vec<Value> = Vec::new();
        let mut assistant = String::new();
        let mut status = None;
        let mut created_id = None;
        let mut completed_id = None;
        for event in events {
            match event.get("type").and_then(Value::as_str) {
                Some("response.created") => {
                    created_id = event.pointer("/response/id").and_then(Value::as_str).map(str::to_string);
                }
                Some("response.output_item.added") => {
                    if let Some(item) = event.get("item") {
                        let index = event.get("output_index").and_then(Value::as_u64).unwrap_or(items.len() as u64) as usize;
                        if index >= items.len() {
                            items.resize(index + 1, json!(null));
                        }
                        items[index] = item.clone();
                    }
                }
                Some("response.output_text.delta") => {
                    if event.get("item_id").and_then(Value::as_str).is_some() {
                        assistant.push_str(event.get("delta").and_then(Value::as_str).unwrap_or(""));
                    }
                }
                Some("response.completed") | Some("response.incomplete") | Some("response.failed") => {
                    status = event.get("type").and_then(Value::as_str).map(str::to_string);
                    completed_id = event.pointer("/response/id").and_then(Value::as_str).map(str::to_string);
                }
                _ => {}
            }
        }
        let kinds = items
            .iter()
            .filter_map(|item| item.get("type").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(created_id, completed_id, "created/completed response ids must match");
        (kinds, assistant, status, created_id)
    }

    #[test]
    fn codex_view_of_thinking_only_turn_is_not_empty_complete() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_ds", "usage": {"input_tokens": 226, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "I should summarize the work now."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1519}}),
        ];
        let (outgoing, _) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let (kinds, assistant, status, _) = assemble_codex_view(&outgoing);
        assert_eq!(kinds, vec!["reasoning".to_string()]);
        assert_eq!(assistant, "");
        assert_eq!(status.as_deref(), Some("response.completed"));
    }

    #[test]
    fn stream_error_is_failed_not_empty_completed() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_err"}}),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ];
        let (outgoing, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        let types = event_types(&outgoing);
        assert!(types.contains(&"response.failed".into()), "{types:?}");
        assert!(!types.contains(&"response.completed".into()), "{types:?}");
        assert_eq!(response["status"], "failed");
        assert_eq!(response["error"]["message"], "Overloaded");
        let (_, _, status, _) = assemble_codex_view(&outgoing);
        assert_eq!(status.as_deref(), Some("response.failed"));
    }

    #[test]
    fn stream_flushes_open_block_without_stop() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_open"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_open", "name": "exec_command"}}),
            json!({"type": "content_block_delta", "delta": {"partial_json": "{\"cmd\":\"pwd\"}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ];
        let (_, response) = replay_anthropic_events(&events, "deepseek-v4-flash");
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["arguments"], "{\"cmd\":\"pwd\"}");
    }
}
