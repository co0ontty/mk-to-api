//! 上游渠道：在 MonkeyCode 之外接入任意 OpenAI / Anthropic 兼容上游。
//!
//! 中文说明：渠道定义保存在 `~/.mk2api/channels.json`，网关对外暴露的模型 ID 统一带渠道前缀
//! （`<slug>/<上游模型 ID>`）。这样不同渠道下的同名模型（比如两家都有 `gpt-5.6-sol`）不会
//! 互相覆盖，客户端模型列表里也能一眼看出模型来自哪个渠道。
//!
//! MonkeyCode 内置上游仍然沿用 `monkeycode-basic|pro|ultra/` 前缀与 HMAC 签名链路，
//! 渠道走的是普通的 `Authorization: Bearer`，两条链路互不影响。

use reqwest::header;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::time::Duration;

use crate::now;

/// 内置 MonkeyCode 上游占用的前缀，渠道 slug 不允许与之冲突。
pub const RESERVED_SLUGS: [&str; 3] = ["monkeycode-basic", "monkeycode-pro", "monkeycode-ultra"];
/// 模型发现请求的超时时间。
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(20);
/// 模型发现失败时最多回显多少字符的上游响应，避免把整页 HTML 塞进配置。
const ERROR_EXCERPT: usize = 400;

/// 上游协议。决定请求打到哪个路径、以及客户端协议不一致时怎么转换。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    Responses,
    Chat,
    Anthropic,
}

impl Wire {
    /// 解析配置里的 `wire_api`。缺省与无法识别的取值都按 Responses 处理。
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
            Some("chat") | Some("openai-chat") | Some("openai-completions") | Some("chat-completions") | Some("completions") => Wire::Chat,
            Some("anthropic") | Some("anthropic-messages") | Some("claude") => Wire::Anthropic,
            _ => Wire::Responses,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Wire::Responses => "responses",
            Wire::Chat => "chat",
            Wire::Anthropic => "anthropic",
        }
    }

    /// 相对 `base_url` 的请求路径。
    pub fn path(self) -> &'static str {
        match self {
            Wire::Responses => "responses",
            Wire::Chat => "chat/completions",
            Wire::Anthropic => "messages",
        }
    }

    pub fn is_anthropic(self) -> bool {
        matches!(self, Wire::Anthropic)
    }
}

/// 一个上游渠道。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// 显示名，作为模型名前缀（`[名字] GPT-5.6 Sol`）。
    pub name: String,
    /// 模型 ID 前缀，全局唯一。
    pub slug: String,
    pub base_url: String,
    pub api_key: String,
    pub wire: Wire,
    pub enabled: bool,
    /// 额外请求头，例如某些聚合站要求的 `x-openai-actor-authorization`。
    pub headers: Vec<(String, String)>,
    /// 上游模型 ID 列表。来自 `/models` 自动发现，也可以在配置里手写。
    pub models: Vec<String>,
    pub updated_at: u64,
    /// 最近一次模型发现的错误，便于在管理台定位「为什么这个渠道没有模型」。
    pub last_error: Option<String>,
    /// 手动指定上下文窗口；不填时按 200k 起步，再按上游报错自动修正。
    pub context_window: Option<u64>,
    /// 手动指定单次最大输出 token。
    pub max_output: Option<u64>,
}

impl Channel {
    /// 对外暴露的模型 ID：`<slug>/<上游模型 ID>`；上游 ID 已经带该前缀时不重复拼。
    pub fn gateway_id(&self, upstream: &str) -> String {
        let prefix = format!("{}/", self.slug);
        if upstream.starts_with(&prefix) {
            upstream.to_string()
        } else {
            format!("{prefix}{upstream}")
        }
    }

    /// 请求里的模型 ID 反解出上游模型 ID。不属于该渠道时返回 `None`。
    pub fn upstream_model(&self, requested: &str) -> Option<String> {
        requested.strip_prefix(&format!("{}/", self.slug)).filter(|rest| !rest.is_empty()).map(str::to_string)
    }

    fn to_json(&self) -> Value {
        let headers = self
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect::<Map<String, Value>>();
        json!({
            "name": self.name,
            "slug": self.slug,
            "base_url": self.base_url,
            "api_key": self.api_key,
            "wire_api": self.wire.as_str(),
            "enabled": self.enabled,
            "headers": Value::Object(headers),
            "models": self.models,
            "updated_at": self.updated_at,
            "last_error": self.last_error,
            "context_window": self.context_window,
            "max_output": self.max_output,
        })
    }
}

/// 把渠道名字转成模型 ID 前缀：非字母数字一律折叠成 `-`。
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in name.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '.' || lower == '_' {
            out.push(lower);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches(['-', '.']).to_string();
    if out.len() <= 48 {
        out
    } else {
        out[..48].trim_end_matches(['-', '.']).to_string()
    }
}

/// slug 是否可用：非空、未占用、且不撞 MonkeyCode 内置前缀。
pub fn slug_available(slug: &str, existing: &[Channel], keep_slug: Option<&str>) -> bool {
    if slug.is_empty() || RESERVED_SLUGS.contains(&slug) {
        return false;
    }
    !existing.iter().any(|channel| channel.slug == slug && Some(channel.slug.as_str()) != keep_slug)
}

/// 从名字派生一个未被占用的 slug（`huniu`、`huniu-2`、……）。
pub fn unique_slug(name: &str, existing: &[Channel], keep_slug: Option<&str>) -> String {
    let base = {
        let derived = slugify(name);
        if derived.is_empty() {
            "channel".to_string()
        } else {
            derived
        }
    };
    if slug_available(&base, existing, keep_slug) {
        return base;
    }
    for index in 2..1000 {
        let candidate = format!("{base}-{index}");
        if slug_available(&candidate, existing, keep_slug) {
            return candidate;
        }
    }
    format!("{base}-x")
}

/// 规范化 base_url：去掉尾部斜杠；粘贴了完整端点时回退到 base；只给了域名时补 `/v1`。
pub fn normalize_base_url(raw: &str) -> String {
    let mut url = raw.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/responses", "/messages", "/models"] {
        if let Some(rest) = url.strip_suffix(suffix) {
            url = rest.trim_end_matches('/').to_string();
            break;
        }
    }
    if url.is_empty() {
        return url;
    }
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url.as_str());
    if !after_scheme.contains('/') {
        url.push_str("/v1");
    }
    url
}

/// 从管理台 / CLI 提交的 JSON 构造渠道。`existing` 用于保留未提交的字段与 `updated_at`。
pub fn channel_from_body(body: &Value, existing: Option<&Channel>, others: &[Channel]) -> Result<Channel, String> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| existing.map(|channel| channel.name.clone()))
        .ok_or_else(|| "name is required".to_string())?;
    let base_url = body
        .get("base_url")
        .or_else(|| body.get("baseUrl"))
        .and_then(Value::as_str)
        .map(normalize_base_url)
        .filter(|value| !value.is_empty())
        .or_else(|| existing.map(|channel| channel.base_url.clone()))
        .ok_or_else(|| "base_url is required".to_string())?;
    let api_key = body
        .get("api_key")
        .or_else(|| body.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| existing.map(|channel| channel.api_key.clone()))
        .ok_or_else(|| "api_key is required".to_string())?;

    let keep_slug = existing.map(|channel| channel.slug.as_str());
    let slug = body
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(slugify)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| unique_slug(&name, others, keep_slug));
    if !slug_available(&slug, others, keep_slug) {
        return Err(format!("slug is reserved or already used: {slug}"));
    }

    let wire = match body.get("wire_api") {
        Some(value) => Wire::parse(value.as_str()),
        None => existing.map(|channel| channel.wire).unwrap_or(Wire::Responses),
    };
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| existing.map(|channel| channel.enabled).unwrap_or(true));
    let headers = match body.get("headers") {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
            .collect(),
        _ => existing.map(|channel| channel.headers.clone()).unwrap_or_default(),
    };
    let models = match body.get("models").and_then(Value::as_array) {
        Some(items) => items.iter().filter_map(Value::as_str).map(str::trim).filter(|value| !value.is_empty()).map(str::to_string).collect(),
        None => existing.map(|channel| channel.models.clone()).unwrap_or_default(),
    };

    Ok(Channel {
        name,
        slug,
        base_url,
        api_key,
        wire,
        enabled,
        headers,
        models,
        updated_at: existing.map(|channel| channel.updated_at).unwrap_or_else(now),
        last_error: existing.and_then(|channel| channel.last_error.clone()),
        context_window: body
            .get("context_window")
            .and_then(Value::as_u64)
            .or_else(|| existing.and_then(|channel| channel.context_window)),
        max_output: body.get("max_output").and_then(Value::as_u64).or_else(|| existing.and_then(|channel| channel.max_output)),
    })
}

/// 读取渠道列表。文件不存在时返回空列表。
pub fn load(path: &Path) -> Vec<Channel> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    channels_from_json(&value)
}

/// 从 `{ "channels": [...] }`（或裸数组）解析渠道列表。
pub fn channels_from_json(value: &Value) -> Vec<Channel> {
    let items = match value {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map.get("channels").and_then(Value::as_array).cloned().unwrap_or_default(),
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str)?.trim().to_string();
            let slug = item.get("slug").and_then(Value::as_str).unwrap_or(&name).trim().to_string();
            let base_url = normalize_base_url(item.get("base_url").and_then(Value::as_str)?);
            if name.is_empty() || slug.is_empty() || base_url.is_empty() {
                return None;
            }
            let headers = item
                .get("headers")
                .and_then(Value::as_object)
                .map(|map| map.iter().filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string()))).collect())
                .unwrap_or_default();
            Some(Channel {
                name,
                slug,
                base_url,
                api_key: item.get("api_key").and_then(Value::as_str).unwrap_or_default().to_string(),
                wire: Wire::parse(item.get("wire_api").and_then(Value::as_str)),
                enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                headers,
                models: item
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default(),
                updated_at: item.get("updated_at").and_then(Value::as_u64).unwrap_or(0),
                last_error: item.get("last_error").and_then(Value::as_str).map(str::to_string),
                context_window: item.get("context_window").and_then(Value::as_u64),
                max_output: item.get("max_output").and_then(Value::as_u64),
            })
        })
        .collect()
}

pub fn to_json(channels: &[Channel]) -> Value {
    json!({
        "channels": channels.iter().map(Channel::to_json).collect::<Vec<_>>(),
        "updated_at": now(),
    })
}

/// 对外（管理台）展示用：不直接回显 api_key，只标记是否存在。
pub fn public_json(channel: &Channel) -> Value {
    json!({
        "name": channel.name,
        "slug": channel.slug,
        "base_url": channel.base_url,
        "wire_api": channel.wire.as_str(),
        "enabled": channel.enabled,
        "models": channel.models,
        "model_count": channel.models.len(),
        "headers": channel.headers.iter().map(|(key, value)| (key.clone(), Value::String(value.clone()))).collect::<Map<String, Value>>(),
        "has_key": !channel.api_key.is_empty(),
        "updated_at": channel.updated_at,
        "last_error": channel.last_error,
        "context_window": channel.context_window,
        "max_output": channel.max_output,
    })
}

/// 拉取上游 `/models` 列表。支持 `{"data":[...]}`、`{"models":[...]}` 与裸数组三种形态。
pub async fn discover_models(client: &reqwest::Client, channel: &Channel) -> Result<Vec<String>, String> {
    let url = format!("{}/models", channel.base_url.trim_end_matches('/'));
    let mut request = client
        .get(&url)
        .timeout(DISCOVER_TIMEOUT)
        .header(header::ACCEPT, "application/json");
    request = authorize(request, channel);
    let response = request.send().await.map_err(|error| format!("GET {url} failed: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("GET {url} -> HTTP {status}: {}", excerpt(&text)));
    }
    let value: Value = serde_json::from_str(&text).map_err(|error| format!("GET {url} returned invalid JSON: {error}"))?;
    let mut models: Vec<String> = Vec::new();
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array());
    for entry in entries.into_iter().flatten() {
        let id = match entry {
            Value::String(value) => Some(value.clone()),
            Value::Object(map) => map
                .get("id")
                .or_else(|| map.get("name"))
                .or_else(|| map.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        if let Some(id) = id {
            let id = id.trim().to_string();
            if !id.is_empty() && !models.contains(&id) {
                models.push(id);
            }
        }
    }
    if models.is_empty() {
        return Err(format!("GET {url} returned no model ids: {}", excerpt(&text)));
    }
    Ok(models)
}

/// 给请求加上渠道鉴权与自定义请求头。
pub fn authorize(request: reqwest::RequestBuilder, channel: &Channel) -> reqwest::RequestBuilder {
    let mut request = request.header(header::AUTHORIZATION, format!("Bearer {}", channel.api_key));
    if channel.wire.is_anthropic() {
        request = request.header("x-api-key", channel.api_key.clone()).header("anthropic-version", crate::anthropic::anthropic_version());
    }
    for (key, value) in &channel.headers {
        request = request.header(key.as_str(), value.as_str());
    }
    request
}

fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ERROR_EXCERPT {
        return trimmed.to_string();
    }
    trimmed.chars().take(ERROR_EXCERPT).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(name: &str, slug: &str) -> Channel {
        Channel {
            name: name.into(),
            slug: slug.into(),
            base_url: "https://example.test/v1".into(),
            api_key: "sk-test".into(),
            wire: Wire::Responses,
            enabled: true,
            headers: Vec::new(),
            models: vec!["gpt-5.6-sol".into()],
            updated_at: 0,
            last_error: None,
            context_window: None,
            max_output: None,
        }
    }

    #[test]
    fn slugify_folds_punctuation() {
        assert_eq!(slugify("Huniu.Fun"), "huniu.fun");
        assert_eq!(slugify("  My  Channel!! "), "my-channel");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn unique_slug_avoids_collisions_and_reserved_names() {
        let existing = vec![channel("huniu", "huniu"), channel("basic", "monkeycode-basic")];
        assert_eq!(unique_slug("huniu", &existing, None), "huniu-2");
        assert_eq!(unique_slug("火牛", &existing, None), "channel");
        assert_eq!(unique_slug("channel", &existing, None), "channel");
        // 更新自己时不该给自己让路。
        assert_eq!(unique_slug("huniu", &existing, Some("huniu")), "huniu");
    }

    #[test]
    fn normalize_base_url_strips_endpoints_and_adds_v1() {
        assert_eq!(normalize_base_url("https://api.test/v1/"), "https://api.test/v1");
        assert_eq!(normalize_base_url("https://api.test/v1/chat/completions"), "https://api.test/v1");
        assert_eq!(normalize_base_url("https://api.test"), "https://api.test/v1");
    }

    #[test]
    fn gateway_id_and_upstream_model_round_trip() {
        let channel = channel("huniu", "huniu");
        assert_eq!(channel.gateway_id("gpt-5.6-sol"), "huniu/gpt-5.6-sol");
        assert_eq!(channel.gateway_id("huniu/gpt-5.6-sol"), "huniu/gpt-5.6-sol");
        assert_eq!(channel.upstream_model("huniu/gpt-5.6-sol").as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(channel.upstream_model("huniu/"), None);
        assert_eq!(channel.upstream_model("gpt-5.6-sol"), None);
        // 上游 ID 自带斜杠（如 openai/gpt-4o）时整段保留。
        assert_eq!(channel.upstream_model("huniu/openai/gpt-4o").as_deref(), Some("openai/gpt-4o"));
    }

    #[test]
    fn channel_round_trips_through_json() {
        let mut channel = channel("火牛", "huniu");
        channel.headers.push(("x-test".into(), "1".into()));
        let parsed = channels_from_json(&to_json(&[channel.clone()]));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "火牛");
        assert_eq!(parsed[0].headers, vec![("x-test".to_string(), "1".to_string())]);
        assert_eq!(parsed[0].wire, Wire::Responses);
    }

    #[test]
    fn wire_parse_accepts_aliases() {
        assert_eq!(Wire::parse(Some("openai-completions")), Wire::Chat);
        assert_eq!(Wire::parse(Some("anthropic-messages")), Wire::Anthropic);
        assert_eq!(Wire::parse(Some("nonsense")), Wire::Responses);
        assert_eq!(Wire::parse(None), Wire::Responses);
    }

    #[test]
    fn channel_from_body_requires_credentials() {
        let body = json!({"name": "huniu"});
        assert!(channel_from_body(&body, None, &[]).is_err());
        let body = json!({"name": "huniu", "base_url": "https://api.test", "api_key": "sk-1"});
        let channel = channel_from_body(&body, None, &[]).unwrap();
        assert_eq!(channel.slug, "huniu");
        assert_eq!(channel.base_url, "https://api.test/v1");
    }

    #[test]
    fn public_json_hides_the_key() {
        let value = public_json(&channel("huniu", "huniu"));
        assert_eq!(value["has_key"], true);
        assert!(value.get("api_key").is_none());
        assert_eq!(value["model_count"], 1);
    }
}
