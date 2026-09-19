use serde_json::{json, Map, Value};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PROVIDER: &str = "mk2api";
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_MAX_OUTPUT: u64 = 32_000;

/// 网关目录里的一条模型：id、协议、以及从 OhMyAgent / 上游错误动态得到的窗口。
///
/// 渠道模型与内置模型共用一个结构：
/// - 内置（MonkeyCode）：`id` 就是上游模型名（可带 `monkeycode-*/` 前缀），`channel` 为空；
/// - 渠道：`id` 是带渠道前缀的对外名（`huniu/gpt-5.6-sol`），`channel` 是渠道显示名，
///   `upstream` 是真正发给上游的模型名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub anthropic: bool,
    pub context_window: u64,
    pub max_output: u64,
    pub channel: Option<String>,
    pub upstream: Option<String>,
}

impl ModelInfo {
    /// MonkeyCode 内置上游模型。
    pub fn builtin(id: &str, anthropic: bool, context_window: u64, max_output: u64) -> Self {
        Self {
            id: id.to_string(),
            anthropic,
            context_window,
            max_output,
            channel: None,
            upstream: None,
        }
    }

    /// 渠道模型：`id` 为对外 ID，`channel` 为显示名前缀，`upstream` 为上游模型名。
    pub fn channel(id: &str, channel: &str, upstream: &str, anthropic: bool, context_window: u64, max_output: u64) -> Self {
        Self {
            id: id.to_string(),
            anthropic,
            context_window,
            max_output,
            channel: Some(channel.to_string()),
            upstream: Some(upstream.to_string()),
        }
    }

    pub fn advertised_context(&self) -> u64 {
        let reserve = self.max_output.min(self.context_window / 5);
        self.context_window.saturating_sub(reserve).max(16_384)
    }

    /// 上游真实模型名：渠道模型取 `upstream`，内置模型取 `id`。
    pub fn upstream_model(&self) -> &str {
        self.upstream.as_deref().unwrap_or(&self.id)
    }
}

pub fn model_ids(models: &[ModelInfo]) -> Vec<String> {
    models.iter().map(|model| model.id.clone()).collect()
}

/// 内置（MonkeyCode）模型 ID。
fn builtin_ids(models: &[ModelInfo]) -> Vec<String> {
    models.iter().filter(|model| model.channel.is_none()).map(|model| model.id.clone()).collect()
}

/// 渠道模型 ID。已经带渠道前缀，不再派生短名别名，避免跨渠道重名。
fn channel_ids(models: &[ModelInfo]) -> Vec<String> {
    models.iter().filter(|model| model.channel.is_some()).map(|model| model.id.clone()).collect()
}

/// Pi 模型列表：内置模型保持「全名 + 短名别名」，渠道模型只列带前缀的对外 ID。
pub fn pi_model_ids(models: &[ModelInfo]) -> Vec<String> {
    append_unique(ordered_model_ids(&builtin_ids(models)), channel_ids(models))
}

/// Codex 目录 slug：内置模型用短名，渠道模型用带前缀的对外 ID。
pub fn codex_slug_ids(models: &[ModelInfo]) -> Vec<String> {
    append_unique(unique_short_ids(&builtin_ids(models)), channel_ids(models))
}

fn append_unique(mut ids: Vec<String>, extra: Vec<String>) -> Vec<String> {
    for id in extra {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

fn meta_for<'a>(models: &'a [ModelInfo], id: &str) -> Option<&'a ModelInfo> {
    models.iter().find(|model| model.id == id).or_else(|| {
        models.iter().find(|model| short_id(&model.id) == id)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientReport {
    pub name: String,
    pub detected: bool,
    pub managed: bool,
    pub path: Option<String>,
    pub models: usize,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct IssuedKey {
    pub id: String,
    pub raw: String,
}

#[derive(Clone, Debug, Default)]
pub struct ClientStore {
    pub pi: Option<IssuedKey>,
    pub codex: Option<IssuedKey>,
}

#[derive(Clone, Debug)]
pub struct ClientPaths {
    pub user_home: PathBuf,
    pub store_path: PathBuf,
    pub pi_dir: PathBuf,
    pub pi_models: PathBuf,
    pub pi_settings: PathBuf,
    pub codex_dir: PathBuf,
    pub codex_config: PathBuf,
    pub codex_models: PathBuf,
}

impl ClientPaths {
    pub fn new(user_home: PathBuf, mk2api_home: PathBuf) -> Self {
        let pi_dir = user_home.join(".pi/agent");
        let codex_dir = user_home.join(".codex");
        Self {
            pi_models: pi_dir.join("models.json"),
            pi_settings: pi_dir.join("settings.json"),
            pi_dir,
            codex_config: codex_dir.join("config.toml"),
            codex_models: codex_dir.join("codex-models.json"),
            codex_dir,
            store_path: mk2api_home.join("clients.json"),
            user_home,
        }
    }

    pub fn pi_detected(&self) -> bool {
        self.pi_dir.is_dir() || self.pi_models.is_file()
    }

    pub fn codex_detected(&self) -> bool {
        self.codex_config.is_file() || self.codex_dir.is_dir()
    }
}

/// 模型显示名。渠道模型带上渠道前缀（`[huniu] GPT-5.6 Sol`），
/// 内置模型沿用「漂亮名 + (MonkeyCode 档位)」的历史格式。
pub fn display_name(model: &ModelInfo) -> String {
    if let Some(channel) = &model.channel {
        return format!("[{channel}] {}", pretty_short(model.upstream_model()));
    }
    let (tier, short) = split_model_id(&model.id);
    let pretty = pretty_short(short);
    match tier {
        Some(tier) => format!("{pretty} (MonkeyCode {tier})"),
        None if model.id.contains('/') => pretty,
        None => format!("{pretty} short"),
    }
}

fn model_rank(id: &str) -> u8 {
    if id.starts_with("monkeycode-basic/") {
        0
    } else if id.starts_with("monkeycode-pro/") {
        1
    } else if id.starts_with("monkeycode-ultra/") {
        2
    } else {
        3
    }
}

pub fn ordered_model_ids(ids: &[String]) -> Vec<String> {
    let mut full = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if id.trim().is_empty() || !id.contains('/') || !seen.insert(id.clone()) {
            continue;
        }
        full.push(id.clone());
    }
    full.sort_by_key(|id| model_rank(id));
    let mut short = Vec::new();
    for id in &full {
        let alias = short_id(id);
        if seen.insert(alias.clone()) {
            short.push(alias);
        }
    }
    for id in ids {
        if !id.contains('/') && seen.insert(id.clone()) {
            short.push(id.clone());
        }
    }
    full.extend(short);
    full
}

pub fn unique_short_ids(ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in ordered_model_ids(ids) {
        let short = short_id(&id);
        if seen.insert(short.clone()) {
            out.push(short);
        }
    }
    out
}

pub fn preferred_default_model(ids: &[String], current: Option<&str>) -> String {
    if let Some(current) = current {
        if ids.iter().any(|id| id == current) {
            return current.to_string();
        }
    }
    if let Some(id) = ids.iter().find(|id| id.starts_with("monkeycode-basic/")) {
        return id.clone();
    }
    if let Some(id) = ids.iter().find(|id| !id.contains('/')) {
        return id.clone();
    }
    ids.first().cloned().unwrap_or_else(|| "qwen3.8-flash".to_string())
}

/// 在已有 models.json 上做增量更新：保留其它 provider、顶层字段，以及用户在
/// mk2api provider 里自定义的字段，只覆盖我们托管的 baseUrl / api / apiKey / models。
pub fn pi_models_payload(current: &Value, base_url: &str, api_key: &str, models: &[ModelInfo]) -> Value {
    let payload_models = pi_model_ids(models)
        .into_iter()
        .map(|id| {
            let meta = meta_for(models, &id);
            json!({
                "id": id,
                "name": meta.map(display_name).unwrap_or_else(|| pretty_short(&id)),
                "reasoning": true,
                "input": ["text", "image"],
                "contextWindow": meta.map(ModelInfo::advertised_context).unwrap_or(DEFAULT_CONTEXT_WINDOW),
                "maxTokens": meta.map(|model| model.max_output).unwrap_or(DEFAULT_MAX_OUTPUT)
            })
        })
        .collect::<Vec<_>>();

    let mut root = match current {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    let mut providers = match root.remove("providers") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut provider = match providers.remove(PROVIDER) {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    provider.insert("baseUrl".into(), Value::String(base_url.into()));
    provider.insert("api".into(), Value::String("openai-responses".into()));
    provider.insert("apiKey".into(), Value::String(api_key.into()));
    provider.insert("models".into(), Value::Array(payload_models));
    providers.insert(PROVIDER.into(), Value::Object(provider));
    root.insert("providers".into(), Value::Object(providers));
    Value::Object(root)
}

/// Pi 的默认 provider 是否由我们维护：没设置过（首次接管）或已经指向 mk2api。
/// 用户主动把 `defaultProvider` 换成别的 provider 后，我们不再抢回来。
pub fn pi_client_is_ours(settings: &Value) -> bool {
    match settings.get("defaultProvider").and_then(Value::as_str) {
        None => true,
        Some(value) => value.eq_ignore_ascii_case(PROVIDER),
    }
}

pub fn patch_pi_settings(current: Value, default_model: &str) -> Value {
    let mut object = match current {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let ours = match object.get("defaultProvider").and_then(Value::as_str) {
        None => true,
        Some(value) => value.eq_ignore_ascii_case(PROVIDER),
    };
    if ours {
        object.insert("defaultProvider".into(), Value::String(PROVIDER.into()));
        object.insert("defaultModel".into(), Value::String(default_model.into()));
    }
    Value::Object(object)
}

pub fn extract_pi_key(models: &Value) -> Option<String> {
    models
        .pointer("/providers/mk2api/apiKey")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            models.get("providers").and_then(Value::as_object).and_then(|providers| {
                providers.values().find_map(|provider| {
                    provider
                        .get("apiKey")
                        .and_then(Value::as_str)
                        .filter(|value| value.starts_with("mk_live_") || value.starts_with("mk_admin_"))
                        .map(str::to_string)
                })
            })
        })
}

pub fn extract_pi_default_model(settings: &Value) -> Option<String> {
    settings.get("defaultModel").and_then(Value::as_str).map(str::to_string)
}

pub fn extract_toml_string(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return unquote(rest.trim());
            }
        }
    }
    None
}

pub fn extract_codex_key(text: &str) -> Option<String> {
    let (_, tables) = split_toml(text);
    if let Some((_, body)) = tables.iter().find(|(name, _)| name == "model_providers.mk2api") {
        if let Some(key) = extract_toml_string(body, "experimental_bearer_token") {
            return Some(key);
        }
    }
    tables.iter().find_map(|(_, body)| extract_toml_string(body, "experimental_bearer_token"))
}

/// Codex 的默认 provider 是否由我们维护：没设置过（首次接管）或已经指向 mk2api。
/// 用户主动把 `model_provider` 换成别的 provider 后，我们不再抢回来，
/// 否则每 60 秒的同步会把用户刚选好的默认改回 mk2api。
pub fn codex_client_is_ours(current: &str) -> bool {
    let (preamble, _) = split_toml(current);
    match extract_toml_string(&preamble, "model_provider") {
        None => true,
        Some(value) => value.eq_ignore_ascii_case(PROVIDER),
    }
}

/// 在已有 config.toml 上做增量更新：
///
/// - 始终创建/刷新 `[model_providers.mk2api]`（保留表内自定义键），其它表原样保留；
/// - 只有当客户端默认还没设置、或已经指向 mk2api 时，才维护
///   `model_provider` / `model` / `review_model` / `model_catalog_json` / `model_context_window`；
///   用户把默认切到别的 provider 后只补 provider 表，不再动 preamble。
pub fn patch_codex_toml(current: &str, base_url: &str, api_key: &str, default_model: &str, catalog_path: &str, context_window: u64) -> String {
    const MANAGED: [&str; 5] = ["name", "base_url", "wire_api", "requires_openai_auth", "experimental_bearer_token"];

    let (preamble, tables) = split_toml(current);
    let mut preamble = preamble;
    if codex_client_is_ours(current) {
        preamble = set_preamble_key(&preamble, "model_provider", &quote(PROVIDER));
        preamble = set_preamble_key(&preamble, "model", &quote(default_model));
        preamble = set_preamble_key(&preamble, "review_model", &quote(default_model));
        preamble = set_preamble_key(&preamble, "model_catalog_json", &quote(catalog_path));
        preamble = set_preamble_key(&preamble, "model_context_window", &context_window.to_string());
    }

    let managed_body = format!(
        "name = {}\nbase_url = {}\nwire_api = \"responses\"\nrequires_openai_auth = false\nexperimental_bearer_token = {}\n",
        quote(PROVIDER),
        quote(base_url),
        quote(api_key)
    );

    let mut kept = Vec::new();
    let mut replaced = false;
    for (name, body) in tables {
        if is_provider_table(&name, PROVIDER) {
            kept.push((name, merge_table_body(&managed_body, &body, &MANAGED)));
            replaced = true;
        } else {
            kept.push((name, body));
        }
    }
    if !replaced {
        kept.insert(0, (format!("model_providers.{PROVIDER}"), managed_body));
    }
    join_toml(&preamble, &kept)
}

fn is_provider_table(name: &str, provider: &str) -> bool {
    let normalized = name
        .split('.')
        .map(|part| part.trim().trim_matches(['"', '\'']))
        .collect::<Vec<_>>()
        .join(".");
    normalized == format!("model_providers.{provider}")
}

/// 我们托管的键优先；表里其它自定义键（headers 等）保留在原有相对顺序里。
fn merge_table_body(managed_body: &str, existing: &str, managed_keys: &[&str]) -> String {
    let mut out = managed_body.to_string();
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let ident = trimmed
            .split('=')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['"', '\'']);
        if managed_keys.contains(&ident) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub fn codex_catalog(models: &[ModelInfo]) -> Value {
    let catalog = codex_slug_ids(models)
        .into_iter()
        .enumerate()
        .map(|(index, slug)| {
            let meta = meta_for(models, &slug);
            let context_window = meta.map(ModelInfo::advertised_context).unwrap_or(DEFAULT_CONTEXT_WINDOW);
            // 渠道模型的显示名带渠道前缀，codex 的模型选择器里就能直接区分来源。
            let pretty = meta.map(display_name).unwrap_or_else(|| pretty_short(&slug));
            let effort = default_effort(meta.map(ModelInfo::upstream_model).unwrap_or(&slug));
            json!({
                "slug": slug,
                "display_name": pretty,
                "description": format!("mk2api {pretty} ({slug})."),
                "default_reasoning_level": effort,
                "supported_reasoning_levels": [
                    {"effort": "low", "description": "Fast responses with lighter reasoning"},
                    {"effort": "medium", "description": "Balanced reasoning for most coding tasks"},
                    {"effort": "high", "description": "Greater reasoning depth for coding and agent tasks"},
                    {"effort": "xhigh", "description": "Extra-high reasoning depth for difficult tasks"}
                ],
                "shell_type": "unified_exec",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 100u64.saturating_sub((index as u64) * 5),
                "context_window": context_window,
                "max_context_window": context_window,
                "input_modalities": ["text", "image"],
                "supports_parallel_tool_calls": true,
                "supports_search_tool": false,
                "use_responses_lite": false,
                "additional_speed_tiers": [],
                "service_tiers": [],
                "default_service_tier": Value::Null,
                "availability_nux": Value::Null,
                "upgrade": Value::Null,
                "model_messages": {
                    "instructions_template": format!("You are Codex, a coding agent based on {pretty}."),
                    "instructions_variables": Value::Null,
                    "approvals": Value::Null,
                    "collaboration_modes": Value::Null,
                    "auto_review": Value::Null,
                    "permissions": Value::Null,
                    "multi_agent": Value::Null,
                    "token_budget": Value::Null,
                    "guardian_v2": Value::Null
                },
                "include_skills_usage_instructions": false,
                "include_plugin_usage_instructions": false,
                "include_apps_usage_instructions": false,
                "supports_reasoning_summary_parameter": true,
                "default_reasoning_summary": "none",
                "support_verbosity": true,
                "default_verbosity": "low",
                "apply_patch_tool_type": Value::Null,
                "web_search_tool_type": "text",
                "truncation_policy": {"mode": "tokens", "limit": 10000},
                "supports_image_detail_original": false,
                "auto_compact_token_limit": Value::Null,
                "comp_hash": Value::Null,
                "effective_context_window_percent": 95,
                "experimental_supported_tools": [],
                "node_repl_auto_review_required": false,
                "node_repl_disabled": false,
                "auto_review_model_override": Value::Null,
                "model_specialty": Value::Null,
                "tool_mode": Value::Null,
                "multi_agent_version": Value::Null
            })
        })
        .collect::<Vec<_>>();
    json!({ "models": catalog })
}

pub fn load_store(path: &Path) -> ClientStore {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ClientStore::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return ClientStore::default();
    };
    ClientStore {
        pi: issued_from_value(value.get("pi")),
        codex: issued_from_value(value.get("codex")),
    }
}

pub fn store_value(store: &ClientStore) -> Value {
    json!({
        "pi": store.pi.as_ref().map(|key| json!({"key_id": key.id, "key": key.raw})),
        "codex": store.codex.as_ref().map(|key| json!({"key_id": key.id, "key": key.raw})),
        "updated_at": now(),
    })
}

pub fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn write_pretty_json(path: &Path, value: &Value) -> io::Result<()> {
    write_private(path, &format!("{}\n", serde_json::to_string_pretty(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?))
}

pub fn read_json_or_empty(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Object(Map::new()))
}

pub fn same_json(left: &Value, right: &Value) -> bool {
    left == right
}

fn issued_from_value(value: Option<&Value>) -> Option<IssuedKey> {
    let value = value?;
    Some(IssuedKey {
        id: value.get("key_id").and_then(Value::as_str)?.to_string(),
        raw: value.get("key").and_then(Value::as_str).filter(|v| !v.is_empty())?.to_string(),
    })
}

fn split_model_id(id: &str) -> (Option<&'static str>, &str) {
    if let Some(short) = id.strip_prefix("monkeycode-ultra/") {
        (Some("Ultra"), short)
    } else if let Some(short) = id.strip_prefix("monkeycode-pro/") {
        (Some("Pro"), short)
    } else if let Some(short) = id.strip_prefix("monkeycode-basic/") {
        (Some("Basic"), short)
    } else {
        (None, id)
    }
}

fn short_id(id: &str) -> String {
    split_model_id(id).1.to_string()
}

fn pretty_short(short: &str) -> String {
    match short {
        "gpt-5.6-sol" => "GPT-5.6 Sol".into(),
        "gpt-6-astra" => "GPT-6 Astra".into(),
        "gpt-5.6-terra" => "GPT-5.6 Terra".into(),
        "qwen3.8-flash" => "Qwen3.8 Flash".into(),
        "qwen3.7-max" => "Qwen3.7 Max".into(),
        "qwen3.6-plus" => "Qwen3.6 Plus".into(),
        "qwen3.5-plus" => "Qwen3.5 Plus".into(),
        "deepseek-v4-pro" => "DeepSeek V4 Pro".into(),
        "deepseek-v4-flash" => "DeepSeek V4 Flash".into(),
        "deepseek-flash" => "DeepSeek V4.1 Flash".into(),
        "glm-5.1" => "GLM-5.1".into(),
        "glm-5" => "GLM-5".into(),
        "glm-5.3-flash" => "GLM-5.3 Flash".into(),
        "kimi-k2.6" => "Kimi K2.6".into(),
        "kimi-k2.5" => "Kimi K2.5".into(),
        "minimax-m3" => "Minimax M3".into(),
        "minimax-m2.5" => "Minimax M2.5".into(),
        "hy3" => "Hy3".into(),
        other => other
            .split(['-', '_'])
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn default_effort(id: &str) -> &'static str {
    if id.contains("ultra") {
        "high"
    } else if id.contains("basic") || id.contains("flash") {
        "low"
    } else {
        "medium"
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn unquote(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(',').trim();
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn split_toml(text: &str) -> (String, Vec<(String, String)>) {
    let mut preamble = String::new();
    let mut tables = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_body = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && !trimmed.starts_with("[[") {
            if let Some(name) = current_name.take() {
                tables.push((name, std::mem::take(&mut current_body)));
            }
            current_name = Some(trimmed[1..trimmed.len() - 1].to_string());
            continue;
        }
        if current_name.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        } else {
            preamble.push_str(line);
            preamble.push('\n');
        }
    }
    if let Some(name) = current_name {
        tables.push((name, current_body));
    }
    (preamble, tables)
}

fn set_preamble_key(preamble: &str, key: &str, value: &str) -> String {
    let mut found = false;
    let mut lines = preamble.lines().map(str::to_string).collect::<Vec<_>>();
    for line in lines.iter_mut() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let ident = trimmed.split('=').next().unwrap_or("").trim();
        if ident == key {
            *line = format!("{key} = {value}");
            found = true;
            break;
        }
    }
    if !found {
        let mut insert_at = lines.len();
        while insert_at > 0 && lines[insert_at - 1].trim().is_empty() {
            insert_at -= 1;
        }
        lines.insert(insert_at, format!("{key} = {value}"));
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn join_toml(preamble: &str, tables: &[(String, String)]) -> String {
    let mut out = preamble.trim_end().to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    for (index, (name, body)) in tables.iter().enumerate() {
        if index == 0 || !out.ends_with("\n\n") {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
        }
        out.push('[');
        out.push_str(name);
        out.push_str("]\n");
        let body = body.trim_end_matches('\n');
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn local_base_url(host: &str, port: u16, tls: bool) -> String {
    let host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    let scheme = if tls { "https" } else { "http" };
    format!("{scheme}://{host}:{port}/v1")
}

fn client_enabled(name: &str, manage_pi: bool, manage_codex: bool) -> bool {
    match name {
        "pi" => manage_pi,
        "codex" => manage_codex,
        _ => true,
    }
}

/// `enabled` 是总开关；`manage_pi` / `manage_codex` 是各 CLI 自己的开关，互不影响。
/// 每条 client.enabled 只反映该 CLI 开关，方便前端在总开关关闭时仍显示原先勾选状态。
pub fn report_json(reports: &[ClientReport], enabled: bool, manage_pi: bool, manage_codex: bool, base_url: &str, catalog: usize) -> Value {
    json!({
        "enabled": enabled,
        "manage_clients": enabled,
        "manage_pi": manage_pi,
        "manage_codex": manage_codex,
        "base_url": base_url,
        "catalog": catalog,
        "clients": reports.iter().map(|report| {
            json!({
                "name": report.name,
                "enabled": client_enabled(&report.name, manage_pi, manage_codex),
                "detected": report.detected,
                "managed": report.managed,
                "path": report.path,
                "models": report.models,
                "message": report.message,
            })
        }).collect::<Vec<_>>()
    })
}

pub fn apply_pi(paths: &ClientPaths, base_url: &str, api_key: &str, models: &[ModelInfo]) -> io::Result<ClientReport> {
    let ids = pi_model_ids(models);
    if !paths.pi_detected() {
        return Ok(ClientReport {
            name: "pi".into(),
            detected: false,
            managed: false,
            path: Some(paths.pi_models.display().to_string()),
            models: 0,
            message: "not installed".into(),
        });
    }
    if ids.is_empty() {
        return Ok(ClientReport {
            name: "pi".into(),
            detected: true,
            managed: false,
            path: Some(paths.pi_models.display().to_string()),
            models: 0,
            message: "catalog empty".into(),
        });
    }
    let current_models = read_json_or_empty(&paths.pi_models);
    let payload = pi_models_payload(&current_models, base_url, api_key, models);
    if !same_json(&current_models, &payload) {
        write_pretty_json(&paths.pi_models, &payload)?;
    }
    let current_settings = read_json_or_empty(&paths.pi_settings);
    let owns_client = pi_client_is_ours(&current_settings);
    let default_model = preferred_default_model(&ids, extract_pi_default_model(&current_settings).as_deref());
    let settings = patch_pi_settings(current_settings.clone(), &default_model);
    if !same_json(&current_settings, &settings) {
        write_pretty_json(&paths.pi_settings, &settings)?;
    }
    Ok(ClientReport {
        name: "pi".into(),
        detected: true,
        managed: true,
        path: Some(paths.pi_models.display().to_string()),
        models: ids.len(),
        message: if owns_client {
            format!("default {default_model}")
        } else {
            "provider registered; client default kept".into()
        },
    })
}

pub fn apply_codex(paths: &ClientPaths, base_url: &str, api_key: &str, models: &[ModelInfo]) -> io::Result<ClientReport> {
    let ids = codex_slug_ids(models);
    if !paths.codex_detected() {
        return Ok(ClientReport {
            name: "codex".into(),
            detected: false,
            managed: false,
            path: Some(paths.codex_config.display().to_string()),
            models: 0,
            message: "not installed".into(),
        });
    }
    if ids.is_empty() {
        return Ok(ClientReport {
            name: "codex".into(),
            detected: true,
            managed: false,
            path: Some(paths.codex_config.display().to_string()),
            models: 0,
            message: "catalog empty".into(),
        });
    }
    let current = if paths.codex_config.is_file() {
        std::fs::read_to_string(&paths.codex_config)?
    } else {
        String::new()
    };
    let catalog_path = extract_toml_string(&current, "model_catalog_json")
        .map(|value| expand_home(&value, &paths.user_home))
        .unwrap_or_else(|| paths.codex_models.clone());
    let catalog_display = catalog_path
        .to_str()
        .map(|value| {
            let home = paths.user_home.to_string_lossy();
            value
                .strip_prefix(home.as_ref())
                .map(|rest| format!("~{rest}"))
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_else(|| "~/.codex/codex-models.json".to_string());
    let short_ids = ids;
    let owns_client = codex_client_is_ours(&current);
    let current_model = extract_toml_string(&current, "model");
    let default_model = preferred_default_model(&short_ids, current_model.as_deref());
    let context_window = meta_for(models, &default_model).map(ModelInfo::advertised_context).unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let next = patch_codex_toml(&current, base_url, api_key, &default_model, &catalog_display, context_window);
    if current != next {
        write_private(&paths.codex_config, &next)?;
    }
    let catalog = codex_catalog(models);
    let existing_catalog = read_json_or_empty(&catalog_path);
    if !same_json(&existing_catalog, &catalog) {
        write_pretty_json(&catalog_path, &catalog)?;
    }
    Ok(ClientReport {
        name: "codex".into(),
        detected: true,
        managed: true,
        path: Some(paths.codex_config.display().to_string()),
        models: short_ids.len(),
        message: if owns_client {
            format!("default {default_model}")
        } else {
            "provider registered; client default kept".into()
        },
    })
}

fn expand_home(value: &str, user_home: &Path) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        user_home.join(rest)
    } else if value == "~" {
        user_home.to_path_buf()
    } else {
        PathBuf::from(value)
    }
}

pub fn skipped(name: &str, path: PathBuf, detected: bool, reason: &str) -> ClientReport {
    ClientReport {
        name: name.into(),
        detected,
        managed: false,
        path: Some(path.display().to_string()),
        models: 0,
        message: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_full_ids_before_short_aliases() {
        let ids = ordered_model_ids(&[
            "qwen3.8-flash".into(),
            "monkeycode-basic/qwen3.8-flash".into(),
            "monkeycode-ultra/gpt-5.6-sol".into(),
        ]);
        assert_eq!(
            ids,
            vec![
                "monkeycode-basic/qwen3.8-flash",
                "monkeycode-ultra/gpt-5.6-sol",
                "qwen3.8-flash",
                "gpt-5.6-sol",
            ]
        );
    }

    #[test]
    fn pi_payload_keeps_other_providers() {
        let current = json!({
            "keep_top_level": true,
            "providers": {
                "monkeycode": {
                    "baseUrl": "http://127.0.0.1:8123/v1",
                    "api": "anthropic-messages",
                    "apiKey": "local-monkeycode",
                    "models": [{"id": "qwen3.8-flash"}]
                },
                "mk2api": {
                    "apiKey": "mk_live_old",
                    "headers": {"x-custom": "1"}
                }
            }
        });
        let payload = pi_models_payload(&current, "http://127.0.0.1:8124/v1", "mk_live_test", &[ModelInfo::builtin("monkeycode-basic/qwen3.8-flash", false, 200_000, 32_000)]);
        let providers = payload["providers"].as_object().unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers["monkeycode"]["baseUrl"], "http://127.0.0.1:8123/v1");
        assert_eq!(providers["monkeycode"]["models"][0]["id"], "qwen3.8-flash");
        assert_eq!(payload["keep_top_level"], true);
        assert_eq!(payload["providers"]["mk2api"]["apiKey"], "mk_live_test");
        assert_eq!(payload["providers"]["mk2api"]["baseUrl"], "http://127.0.0.1:8124/v1");
        assert_eq!(payload["providers"]["mk2api"]["headers"]["x-custom"], "1");
        assert_eq!(payload["providers"]["mk2api"]["models"][0]["id"], "monkeycode-basic/qwen3.8-flash");
        assert_eq!(payload["providers"]["mk2api"]["models"][1]["id"], "qwen3.8-flash");
        assert_eq!(payload["providers"]["mk2api"]["models"][0]["contextWindow"], 168_000);
        assert_eq!(payload["providers"]["mk2api"]["models"][0]["maxTokens"], 32_000);
    }

    #[test]
    fn pi_payload_creates_file_when_empty() {
        let payload = pi_models_payload(&Value::Object(Map::new()), "http://127.0.0.1:8124/v1", "mk_live_test", &[]);
        assert_eq!(payload["providers"].as_object().unwrap().len(), 1);
        assert!(payload["providers"]["mk2api"].is_object());
    }

    #[test]
    fn codex_patch_keeps_other_providers() {
        let current = r#"model_provider = "mk2api"
model = "qwen3.8-flash"
review_model = "qwen3.8-flash"

[model_providers.MonkeyCode]
name = "MonkeyCode Direct"
base_url = "http://127.0.0.1:8123/v1"
experimental_bearer_token = "local-monkeycode-direct"

[model_providers.mk2api]
name = "mk2api"
base_url = "http://127.0.0.1:8124/v1"
experimental_bearer_token = "mk_live_old"
http_headers = { "x-custom" = "1" }

[model_providers.OpenAI]
name = "OpenAI"
base_url = "https://example.invalid"
experimental_bearer_token = "sk-user-key"

[features]
goals = true
"#;
        let next = patch_codex_toml(
            current,
            "http://127.0.0.1:8124/v1",
            "mk_live_test",
            "qwen3.8-flash",
            "~/.codex/codex-models.json",
            168_000,
        );
        assert!(next.contains("model_context_window = 168000"));
        assert!(next.contains("model_provider = \"mk2api\""));
        assert!(next.contains("[model_providers.mk2api]"));
        assert!(next.contains("experimental_bearer_token = \"mk_live_test\""));
        assert!(next.contains("[model_providers.MonkeyCode]"));
        assert!(next.contains("local-monkeycode-direct"));
        assert!(next.contains("[model_providers.OpenAI]"));
        assert!(next.contains("https://example.invalid"));
        assert!(next.contains("sk-user-key"));
        assert!(next.contains("http_headers = { \"x-custom\" = \"1\" }"));
        assert!(next.contains("[features]"));
        assert_eq!(next.matches("[model_providers.mk2api]").count(), 1);
        assert_eq!(extract_codex_key(&next).as_deref(), Some("mk_live_test"));
    }

    #[test]
    fn codex_patch_adds_provider_once_when_absent() {
        let current = "model = \"qwen3.8-flash\"\n\n[features]\ngoals = true\n";
        let once = patch_codex_toml(current, "http://127.0.0.1:8124/v1", "mk_live_test", "qwen3.8-flash", "~/.codex/codex-models.json", 168_000);
        let twice = patch_codex_toml(&once, "http://127.0.0.1:8124/v1", "mk_live_test", "qwen3.8-flash", "~/.codex/codex-models.json", 168_000);
        assert_eq!(twice.matches("[model_providers.mk2api]").count(), 1);
        assert_eq!(once, twice);
    }

    #[test]
    fn codex_patch_matches_quoted_provider_table() {
        let current = "[model_providers.\"mk2api\"]\nname = \"old\"\nbase_url = \"http://127.0.0.1:1/v1\"\n";
        let next = patch_codex_toml(current, "http://127.0.0.1:8124/v1", "mk_live_test", "qwen3.8-flash", "~/.codex/codex-models.json", 168_000);
        assert!(next.contains("http://127.0.0.1:8124/v1"));
        assert!(!next.contains("http://127.0.0.1:1/v1"));
        assert_eq!(next.matches("mk2api\"]").count(), 1);
    }

    #[test]
    fn codex_patch_keeps_user_default_provider() {
        // 用户把 Codex 默认切到自己的 provider：只补 mk2api 表，不动 preamble。
        let current = r#"model_provider = "OpenAI"
model = "gpt-5.5"
review_model = "gpt-5.5"
network_access = "enabled"

[model_providers.OpenAI]
name = "OpenAI"
base_url = "https://example.invalid"
experimental_bearer_token = "sk-user-key"

[features]
goals = true
"#;
        let next = patch_codex_toml(
            current,
            "http://127.0.0.1:8124/v1",
            "mk_live_test",
            "deepseek-flash",
            "~/.codex/codex-models.json",
            168_000,
        );
        assert!(next.contains("model_provider = \"OpenAI\""));
        assert!(!next.contains("model_provider = \"mk2api\""));
        assert!(next.contains("model = \"gpt-5.5\""));
        assert!(next.contains("review_model = \"gpt-5.5\""));
        assert!(!next.contains("model_context_window"));
        assert!(!next.contains("model_catalog_json"));
        assert!(next.contains("[model_providers.mk2api]"));
        assert!(next.contains("experimental_bearer_token = \"mk_live_test\""));
        assert!(next.contains("[model_providers.OpenAI]"));
        assert!(next.contains("sk-user-key"));
        // 幂等：再来一轮不应该又生成新内容
        let again = patch_codex_toml(&next, "http://127.0.0.1:8124/v1", "mk_live_test", "deepseek-flash", "~/.codex/codex-models.json", 168_000);
        assert_eq!(next, again);
    }

    #[test]
    fn codex_patch_takes_over_when_provider_unset() {
        let current = "model = \"gpt-5.6-sol\"\n\n[features]\ngoals = true\n";
        let next = patch_codex_toml(
            current,
            "http://127.0.0.1:8124/v1",
            "mk_live_test",
            "qwen3.8-flash",
            "~/.codex/codex-models.json",
            168_000,
        );
        assert!(next.contains("model_provider = \"mk2api\""));
        assert!(next.contains("model = \"qwen3.8-flash\""));
        assert!(next.contains("model_context_window = 168000"));
        assert!(codex_client_is_ours(&next));
    }

    #[test]
    fn codex_client_ownership_detection() {
        assert!(codex_client_is_ours("model = \"x\"\n"));
        assert!(codex_client_is_ours("model_provider = \"mk2api\"\n"));
        assert!(!codex_client_is_ours("model_provider = \"OpenAI\"\n"));
        assert!(!codex_client_is_ours("model_provider = \"MonkeyCode\"\n"));
    }

    #[test]
    fn pi_settings_keeps_user_default_provider() {
        let current = json!({"theme": "dark", "defaultProvider": "anthropic", "defaultModel": "claude-x"});
        let patched = patch_pi_settings(current.clone(), "qwen3.8-flash");
        assert_eq!(patched, current);
        assert!(!pi_client_is_ours(&current));
    }

    #[test]
    fn pi_settings_takes_over_when_provider_unset() {
        let patched = patch_pi_settings(json!({"theme": "dark"}), "qwen3.8-flash");
        assert_eq!(patched["theme"], "dark");
        assert_eq!(patched["defaultProvider"], "mk2api");
        assert_eq!(patched["defaultModel"], "qwen3.8-flash");
        assert!(pi_client_is_ours(&patched));
    }

    #[test]
    fn preferred_model_keeps_current_if_available() {
        let ids = vec!["gpt-5.6-sol".into(), "qwen3.8-flash".into()];
        assert_eq!(preferred_default_model(&ids, Some("qwen3.8-flash")), "qwen3.8-flash");
        assert_eq!(preferred_default_model(&ids, Some("missing")), "gpt-5.6-sol");
    }

    #[test]
    fn preferred_model_uses_basic_prefix() {
        let ids = vec![
            "monkeycode-ultra/gpt-5.6-sol".into(),
            "monkeycode-basic/qwen3.8-flash".into(),
            "monkeycode-basic/deepseek-v4-flash".into(),
        ];
        assert_eq!(preferred_default_model(&ids, None), "monkeycode-basic/qwen3.8-flash");
    }

    fn scratch_paths(name: &str) -> ClientPaths {
        let home = std::env::temp_dir().join(format!("mk2api-clients-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        ClientPaths::new(home.clone(), home.join(".mk2api"))
    }

    #[test]
    fn apply_pi_keeps_existing_providers_on_disk() {
        let paths = scratch_paths("pi");
        std::fs::write(
            &paths.pi_models,
            r#"{
  "providers": {
    "monkeycode": {
      "baseUrl": "http://127.0.0.1:8123/v1",
      "api": "anthropic-messages",
      "apiKey": "local-monkeycode",
      "models": [{"id": "qwen3.8-flash"}]
    }
  }
}
"#,
        )
        .unwrap();
        std::fs::write(&paths.pi_settings, "{\"theme\":\"dark\"}\n").unwrap();
        let models = vec![ModelInfo::builtin("monkeycode-basic/qwen3.8-flash", false, 200_000, 32_000)];
        let report = apply_pi(&paths, "http://127.0.0.1:8124/v1", "mk_live_test", &models).unwrap();
        assert!(report.managed);
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&paths.pi_models).unwrap()).unwrap();
        assert_eq!(written["providers"]["monkeycode"]["baseUrl"], "http://127.0.0.1:8123/v1");
        assert_eq!(written["providers"]["monkeycode"]["models"][0]["id"], "qwen3.8-flash");
        assert_eq!(written["providers"]["mk2api"]["apiKey"], "mk_live_test");
        let settings: Value = serde_json::from_str(&std::fs::read_to_string(&paths.pi_settings).unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["defaultProvider"], "mk2api");
        let _ = std::fs::remove_dir_all(&paths.user_home);
    }

    #[test]
    fn apply_codex_keeps_existing_providers_on_disk() {
        let paths = scratch_paths("codex");
        std::fs::write(
            &paths.codex_config,
            "model = \"gpt-5.6-sol\"\n\n[model_providers.OpenAI]\nname = \"OpenAI\"\nbase_url = \"https://example.invalid\"\nexperimental_bearer_token = \"sk-user-key\"\n\n[features]\ngoals = true\n",
        )
        .unwrap();
        let models = vec![ModelInfo::builtin("monkeycode-basic/qwen3.8-flash", false, 200_000, 32_000)];
        let report = apply_codex(&paths, "http://127.0.0.1:8124/v1", "mk_live_test", &models).unwrap();
        assert!(report.managed);
        let written = std::fs::read_to_string(&paths.codex_config).unwrap();
        assert!(written.contains("[model_providers.OpenAI]"));
        assert!(written.contains("sk-user-key"));
        assert!(written.contains("[model_providers.mk2api]"));
        assert!(written.contains("[features]"));
        assert_eq!(written.matches("[model_providers.").count(), 2);
        let _ = std::fs::remove_dir_all(&paths.user_home);
    }

    #[test]
    fn report_json_exposes_per_client_switches() {
        let reports = vec![skipped("pi", PathBuf::from("/tmp/pi"), true, "disabled")];
        let value = report_json(&reports, true, false, true, "http://127.0.0.1:8123/v1", 3);
        assert_eq!(value["enabled"], true);
        assert_eq!(value["manage_clients"], true);
        assert_eq!(value["manage_pi"], false);
        assert_eq!(value["manage_codex"], true);
        assert_eq!(value["clients"][0]["enabled"], false);
        assert_eq!(value["clients"][0]["detected"], true);
        assert_eq!(value["catalog"], 3);
    }
}
