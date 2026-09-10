use serde_json::{json, Map, Value};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const PROVIDER: &str = "mk2api";
const PI_CONTEXT_WINDOW: u64 = 1_000_000;
const PI_MAX_TOKENS: u64 = 32_000;

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

pub fn display_name(id: &str) -> String {
    let (tier, short) = split_model_id(id);
    let pretty = pretty_short(short);
    match tier {
        Some(tier) => format!("{pretty} (MonkeyCode {tier})"),
        None if id.contains('/') => pretty,
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

pub fn pi_models_payload(base_url: &str, api_key: &str, ids: &[String]) -> Value {
    let models = ordered_model_ids(ids)
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "name": display_name(&id),
                "reasoning": true,
                "input": ["text", "image"],
                "contextWindow": PI_CONTEXT_WINDOW,
                "maxTokens": PI_MAX_TOKENS
            })
        })
        .collect::<Vec<_>>();
    json!({
        "providers": {
            PROVIDER: {
                "baseUrl": base_url,
                "api": "openai-responses",
                "apiKey": api_key,
                "models": models
            }
        }
    })
}

pub fn patch_pi_settings(current: Value, default_model: &str) -> Value {
    let mut object = match current {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    object.insert("defaultProvider".into(), Value::String(PROVIDER.into()));
    object.insert("defaultModel".into(), Value::String(default_model.into()));
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

pub fn patch_codex_toml(current: &str, base_url: &str, api_key: &str, default_model: &str, catalog_path: &str) -> String {
    let (preamble, tables) = split_toml(current);
    let mut preamble = set_preamble_key(&preamble, "model_provider", &quote(PROVIDER));
    preamble = set_preamble_key(&preamble, "model", &quote(default_model));
    preamble = set_preamble_key(&preamble, "review_model", &quote(default_model));
    preamble = set_preamble_key(&preamble, "model_catalog_json", &quote(catalog_path));

    let mut kept = Vec::new();
    for (name, body) in tables {
        if name.starts_with("model_providers.") {
            continue;
        }
        kept.push((name, body));
    }
    let provider = format!(
        "name = {}\nbase_url = {}\nwire_api = \"responses\"\nrequires_openai_auth = false\nexperimental_bearer_token = {}\n",
        quote(PROVIDER),
        quote(base_url),
        quote(api_key)
    );
    kept.insert(0, ("model_providers.mk2api".into(), provider));
    join_toml(&preamble, &kept)
}

pub fn codex_catalog(ids: &[String]) -> Value {
    let models = unique_short_ids(ids)
        .into_iter()
        .enumerate()
        .map(|(index, slug)| {
            let full = ids
                .iter()
                .find(|id| *id == &slug || short_id(id) == slug && id.contains('/'))
                .cloned()
                .unwrap_or_else(|| slug.clone());
            let pretty = pretty_short(&slug);
            let effort = default_effort(&full);
            json!({
                "slug": slug,
                "display_name": pretty,
                "description": format!("mk2api {pretty} ({full})."),
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
                "context_window": PI_CONTEXT_WINDOW,
                "max_context_window": PI_CONTEXT_WINDOW,
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
    json!({ "models": models })
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

pub fn report_json(reports: &[ClientReport], enabled: bool, base_url: &str, catalog: usize) -> Value {
    json!({
        "enabled": enabled,
        "base_url": base_url,
        "catalog": catalog,
        "clients": reports.iter().map(|report| {
            json!({
                "name": report.name,
                "detected": report.detected,
                "managed": report.managed,
                "path": report.path,
                "models": report.models,
                "message": report.message,
            })
        }).collect::<Vec<_>>()
    })
}

pub fn apply_pi(paths: &ClientPaths, base_url: &str, api_key: &str, ids: &[String]) -> io::Result<ClientReport> {
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
    let models = pi_models_payload(base_url, api_key, ids);
    let current_models = read_json_or_empty(&paths.pi_models);
    if !same_json(&current_models, &models) {
        write_pretty_json(&paths.pi_models, &models)?;
    }
    let current_settings = read_json_or_empty(&paths.pi_settings);
    let default_model = preferred_default_model(ids, extract_pi_default_model(&current_settings).as_deref());
    let settings = patch_pi_settings(current_settings.clone(), &default_model);
    if !same_json(&current_settings, &settings) {
        write_pretty_json(&paths.pi_settings, &settings)?;
    }
    Ok(ClientReport {
        name: "pi".into(),
        detected: true,
        managed: true,
        path: Some(paths.pi_models.display().to_string()),
        models: ordered_model_ids(ids).len(),
        message: format!("default {default_model}"),
    })
}

pub fn apply_codex(paths: &ClientPaths, base_url: &str, api_key: &str, ids: &[String]) -> io::Result<ClientReport> {
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
    let short_ids = unique_short_ids(ids);
    let current_model = extract_toml_string(&current, "model");
    let default_model = preferred_default_model(&short_ids, current_model.as_deref());
    let next = patch_codex_toml(&current, base_url, api_key, &default_model, &catalog_display);
    if current != next {
        write_private(&paths.codex_config, &next)?;
    }
    let catalog = codex_catalog(ids);
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
        message: format!("default {default_model}"),
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

pub fn skipped(name: &str, path: PathBuf, reason: &str) -> ClientReport {
    ClientReport {
        name: name.into(),
        detected: false,
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
    fn pi_payload_keeps_only_mk2api() {
        let payload = pi_models_payload("http://127.0.0.1:8124/v1", "mk_live_test", &["monkeycode-basic/qwen3.8-flash".into()]);
        assert_eq!(payload["providers"].as_object().unwrap().len(), 1);
        assert_eq!(payload["providers"]["mk2api"]["apiKey"], "mk_live_test");
        assert_eq!(payload["providers"]["mk2api"]["models"][0]["id"], "monkeycode-basic/qwen3.8-flash");
        assert_eq!(payload["providers"]["mk2api"]["models"][1]["id"], "qwen3.8-flash");
    }

    #[test]
    fn codex_patch_drops_other_providers() {
        let current = r#"model_provider = "MonkeyCode"
model = "qwen3.8-flash"
review_model = "qwen3.8-flash"

[model_providers.MonkeyCode]
name = "MonkeyCode Direct"
base_url = "http://127.0.0.1:8123/v1"
experimental_bearer_token = "local-monkeycode-direct"

[model_providers.OpenAI]
name = "OpenAI"
base_url = "https://example.invalid"

[features]
goals = true
"#;
        let next = patch_codex_toml(
            current,
            "http://127.0.0.1:8124/v1",
            "mk_live_test",
            "qwen3.8-flash",
            "~/.codex/codex-models.json",
        );
        assert!(next.contains("model_provider = \"mk2api\""));
        assert!(next.contains("[model_providers.mk2api]"));
        assert!(next.contains("experimental_bearer_token = \"mk_live_test\""));
        assert!(!next.contains("[model_providers.MonkeyCode]"));
        assert!(!next.contains("[model_providers.OpenAI]"));
        assert!(next.contains("[features]"));
        assert_eq!(extract_codex_key(&next).as_deref(), Some("mk_live_test"));
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
}
