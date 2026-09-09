use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::{mpsc, Mutex}, time::Duration};
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

type BoxError = Box<dyn Error + Send + Sync>;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    local_key: String,
    auth_required: bool,
    trust_proxy: bool,
    allowed_origins: Vec<String>,
    allowed_ips: Vec<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    key_path: PathBuf,
    settings_path: PathBuf,
    api_keys_path: PathBuf,
    usage_path: PathBuf,
    admin_key_path: PathBuf,
    admin_key: Option<String>,
    max_body_bytes: usize,
    max_usage_records: usize,
    request_timeout: Duration,
    upstream_host: Option<String>,
    upstream_key: Option<String>,
    signing_secret: Option<String>,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    client: reqwest::Client,
    admin_key: Arc<String>,
    api_keys: Arc<Mutex<ApiKeyStore>>,
    usage: Arc<Mutex<UsageStore>>,
}

#[derive(Clone, Debug)]
struct ApiKeyRecord {
    id: String,
    name: String,
    key_hash: String,
    created_at: u64,
    revoked: bool,
}

struct ApiKeyStore {
    path: PathBuf,
    keys: Vec<ApiKeyRecord>,
}

#[derive(Clone, Debug)]
struct UsageRecord {
    timestamp: u64,
    key_id: String,
    model: String,
    endpoint: String,
    status: u16,
    latency_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
}

struct UsageStore {
    path: PathBuf,
    max_records: usize,
    records: Vec<UsageRecord>,
}

#[derive(Debug)]
struct GatewayError {
    status: StatusCode,
    message: String,
    error_type: &'static str,
    code: Option<&'static str>,
    param: Option<String>,
}

impl GatewayError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), error_type: "invalid_request_error", code: None, param: None }
    }

    fn config(message: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: message.into(), error_type: "gateway_configuration_error", code: None, param: None }
    }

    fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn with_type(mut self, error_type: &'static str) -> Self {
        self.error_type = error_type;
        self
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": self.param,
                "code": self.code,
            }
        });
        (self.status, axum::Json(body)).into_response()
    }
}

impl Config {
    async fn load(args: &[String]) -> Result<Self, BoxError> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let default_dir = PathBuf::from(home).join("Library/Application Support/com.chaitin.baizhi.monkeycode");
        let config_path = flag(args, "--config")
            .or_else(|| std::env::var("MONKEYCODE_GATEWAY_CONFIG").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_dir.join("direct-gateway.json"));
        let file = match tokio::fs::read_to_string(&config_path).await {
            Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| boxed(format!("cannot read gateway config: {} ({e})", config_path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(e) => return Err(boxed(format!("cannot read gateway config: {} ({e})", config_path.display()))),
        };

        let config_dir = configured_path(args, &file, "config-dir", "MONKEYCODE_CONFIG_DIR", default_dir.clone());
        let key_path = configured_path(args, &file, "key-file", "MONKEYCODE_OHMYAGENT_KEY", config_dir.join("monkeycode-ohmyagent-key.json"));
        let settings_path = configured_path(args, &file, "settings", "OHMYAGENT_SETTINGS", config_dir.join("ohmyagent/settings.json"));
        let api_keys_path = configured_path(args, &file, "api-keys-file", "DIRECT_GATEWAY_API_KEYS_FILE", config_dir.join("api-keys.json"));
        let usage_path = configured_path(args, &file, "usage-file", "DIRECT_GATEWAY_USAGE_FILE", config_dir.join("usage.json"));
        let admin_key_path = configured_path(args, &file, "admin-key-file", "DIRECT_GATEWAY_ADMIN_KEY_FILE", config_dir.join("admin.key"));
        let host = configured(args, &file, "host", "DIRECT_GATEWAY_HOST", std::env::var("OHMYAGENT_BRIDGE_HOST").unwrap_or_else(|_| "0.0.0.0"));
        let port = parse_u16(&configured(args, &file, "port", "DIRECT_GATEWAY_PORT", std::env::var("OHMYAGENT_BRIDGE_PORT").unwrap_or_else(|_| "8123")), "port")?;
        let local_key = configured(args, &file, "key", "DIRECT_GATEWAY_KEY", std::env::var("OHMYAGENT_BRIDGE_KEY").unwrap_or_default());
        let auth_required = parse_bool(&configured(args, &file, "auth-required", "DIRECT_GATEWAY_AUTH_REQUIRED", "true".into()), true);
        let trust_proxy = parse_bool(&configured(args, &file, "trust-proxy", "DIRECT_GATEWAY_TRUST_PROXY", "false".into()), false);
        let allowed_origins = list_value(configured_optional(args, &file, "allowed-origins", "DIRECT_GATEWAY_ALLOWED_ORIGINS")).into_iter().filter(|v| !v.is_empty()).collect();
        let allowed_ips = list_value(configured_optional(args, &file, "allowed-ips", "DIRECT_GATEWAY_ALLOWED_IPS")).into_iter().map(|v| normalize_ip(&v)).filter(|v| !v.is_empty()).collect();
        let tls_cert = configured_optional(args, &file, "tls-cert", "DIRECT_GATEWAY_TLS_CERT").map(PathBuf::from);
        let tls_key = configured_optional(args, &file, "tls-key", "DIRECT_GATEWAY_TLS_KEY").map(PathBuf::from);
        if tls_cert.is_some() != tls_key.is_some() { return Err(boxed("both tls_cert and tls_key are required for HTTPS")); }
        let max_body_bytes = configured_optional(args, &file, "max-body-bytes", "DIRECT_GATEWAY_MAX_BODY_BYTES")
            .map(|v| v.parse::<usize>().map_err(|_| boxed("max-body-bytes must be a positive integer")))
            .transpose()?.unwrap_or(32 * 1024 * 1024);
        let max_usage_records = configured_optional(args, &file, "max-usage-records", "DIRECT_GATEWAY_MAX_USAGE_RECORDS")
            .map(|v| v.parse::<usize>().map_err(|_| boxed("max-usage-records must be a positive integer")))
            .transpose()?.unwrap_or(100_000);
        let request_timeout = configured_optional(args, &file, "request-timeout-ms", "DIRECT_GATEWAY_REQUEST_TIMEOUT_MS")
            .map(|v| v.parse::<u64>().map_err(|_| boxed("request-timeout-ms must be an integer")))
            .transpose()?.unwrap_or(600_000);

        Ok(Self {
            host,
            port,
            local_key,
            auth_required,
            trust_proxy,
            allowed_origins,
            allowed_ips,
            tls_cert,
            tls_key,
            key_path,
            settings_path,
            api_keys_path,
            usage_path,
            admin_key_path,
            admin_key: configured_optional(args, &file, "admin-key", "DIRECT_GATEWAY_ADMIN_KEY"),
            max_body_bytes,
            max_usage_records,
            request_timeout: Duration::from_millis(request_timeout),
            upstream_host: configured_optional(args, &file, "upstream-host", "DIRECT_GATEWAY_UPSTREAM_HOST"),
            upstream_key: configured_optional(args, &file, "upstream-key", "DIRECT_GATEWAY_UPSTREAM_KEY"),
            signing_secret: configured_optional(args, &file, "signing-secret", "DIRECT_GATEWAY_SIGNING_SECRET"),
        })
    }
}

fn hash_key(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn new_api_key() -> String {
    format!("mk_live_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

async fn write_private(path: &Path, contents: &str) -> Result<(), BoxError> {
    if let Some(parent) = path.parent() { tokio::fs::create_dir_all(parent).await?; }
    tokio::fs::write(path, contents).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

async fn load_admin_key(config: &Config) -> Result<(String, bool), BoxError> {
    if let Some(key) = &config.admin_key { return Ok((key.clone(), false)); }
    match tokio::fs::read_to_string(&config.admin_key_path).await {
        Ok(value) if !value.trim().is_empty() => Ok((value.trim().to_string(), false)),
        Ok(_) => {
            let key = format!("mk_admin_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            write_private(&config.admin_key_path, &key).await?;
            Ok((key, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = format!("mk_admin_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            write_private(&config.admin_key_path, &key).await?;
            Ok((key, true))
        }
        Err(e) => Err(boxed(format!("cannot read admin key: {} ({e})", config.admin_key_path.display()))),
    }
}

impl ApiKeyStore {
    async fn load(path: PathBuf) -> Result<Self, BoxError> {
        let keys = match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let value: Value = serde_json::from_str(&text)?;
                value.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(|item| Some(ApiKeyRecord {
                    id: item.get("id")?.as_str()?.to_string(),
                    name: item.get("name").and_then(Value::as_str).unwrap_or("Unnamed key").to_string(),
                    key_hash: item.get("key_hash")?.as_str()?.to_string(),
                    created_at: item.get("created_at").and_then(Value::as_u64).unwrap_or(0),
                    revoked: item.get("revoked").and_then(Value::as_bool).unwrap_or(false),
                })).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(boxed(format!("cannot read API keys: {} ({e})", path.display()))),
        };
        Ok(Self { path, keys })
    }

    async fn save(&self) -> Result<(), BoxError> {
        let value = json!({"keys": self.keys.iter().map(|key| json!({
            "id": key.id, "name": key.name, "key_hash": key.key_hash,
            "created_at": key.created_at, "revoked": key.revoked,
        })).collect::<Vec<_>>()});
        write_private(&self.path, &serde_json::to_string_pretty(&value)?).await
    }

    fn public_key(key: &ApiKeyRecord) -> Value {
        json!({"id": key.id, "name": key.name, "created_at": key.created_at, "revoked": key.revoked})
    }

    fn find(&self, id: &str) -> Option<&ApiKeyRecord> { self.keys.iter().find(|key| key.id == id) }
}

impl UsageStore {
    async fn load(path: PathBuf, max_records: usize) -> Result<Self, BoxError> {
        let records = match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let value: Value = serde_json::from_str(&text)?;
                value.get("records").and_then(Value::as_array).into_iter().flatten().filter_map(|item| Some(UsageRecord {
                    timestamp: item.get("timestamp")?.as_u64()?,
                    key_id: item.get("key_id").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                    model: item.get("model").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                    endpoint: item.get("endpoint").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                    status: item.get("status").and_then(Value::as_u64).unwrap_or(500) as u16,
                    latency_ms: item.get("latency_ms").and_then(Value::as_u64).unwrap_or(0),
                    input_tokens: item.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens: item.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                })).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(boxed(format!("cannot read usage records: {} ({e})", path.display()))),
        };
        let start = records.len().saturating_sub(max_records);
        Ok(Self { path, max_records, records: records[start..].to_vec() })
    }

    async fn record(&mut self, record: UsageRecord) -> Result<(), BoxError> {
        self.records.push(record);
        if self.records.len() > self.max_records { let remove = self.records.len() - self.max_records; self.records.drain(..remove); }
        let value = json!({"records": self.records.iter().map(|item| json!({
            "timestamp": item.timestamp, "key_id": item.key_id, "model": item.model,
            "endpoint": item.endpoint, "status": item.status, "latency_ms": item.latency_ms,
            "input_tokens": item.input_tokens, "output_tokens": item.output_tokens,
        })).collect::<Vec<_>>()});
        write_private(&self.path, &serde_json::to_string(&value)?).await
    }

    fn summary(&self) -> Value {
        let mut by_key: std::collections::BTreeMap<String, [u64; 4]> = std::collections::BTreeMap::new();
        let mut by_model: std::collections::BTreeMap<String, [u64; 4]> = std::collections::BTreeMap::new();
        let mut requests = 0u64; let mut input = 0u64; let mut output = 0u64;
        for item in &self.records {
            requests += 1; input += item.input_tokens; output += item.output_tokens;
            for (group, key) in [(&mut by_key, &item.key_id), (&mut by_model, &item.model)] {
                let totals = group.entry(key.clone()).or_insert([0; 4]);
                totals[0] += 1; totals[1] += item.input_tokens; totals[2] += item.output_tokens; totals[3] += u64::from(item.status >= 400);
            }
        }
        let groups = |values: std::collections::BTreeMap<String, [u64; 4]>| values.into_iter().map(|(name, totals)| json!({"name": name, "requests": totals[0], "input_tokens": totals[1], "output_tokens": totals[2], "errors": totals[3]})).collect::<Vec<_>>();
        json!({"requests": requests, "input_tokens": input, "output_tokens": output, "total_tokens": input + output, "by_key": groups(by_key), "by_model": groups(by_model), "recent": self.records.iter().rev().take(50).map(|item| json!({"timestamp": item.timestamp, "key_id": item.key_id, "model": item.model, "endpoint": item.endpoint, "status": item.status, "latency_ms": item.latency_ms, "input_tokens": item.input_tokens, "output_tokens": item.output_tokens})).collect::<Vec<_>>()})
    }
}

fn usage_tokens(usage: Option<&Value>) -> (u64, u64) {
    let Some(usage) = usage else { return (0, 0); };
    (usage.get("input_tokens").or_else(|| usage.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0), usage.get("output_tokens").or_else(|| usage.get("completion_tokens")).and_then(Value::as_u64).unwrap_or(0))
}

async fn record_usage(state: &AppState, key_id: &str, model: &str, endpoint: &str, status: StatusCode, started: std::time::Instant, usage: Option<&Value>) {
    let (input_tokens, output_tokens) = usage_tokens(usage);
    let record = UsageRecord { timestamp: now(), key_id: key_id.to_string(), model: model.to_string(), endpoint: endpoint.to_string(), status: status.as_u16(), latency_ms: started.elapsed().as_millis() as u64, input_tokens, output_tokens };
    if let Err(error) = state.usage.lock().await.record(record).await { eprintln!("usage record failed: {error}"); }
}



fn boxed(message: impl Into<String>) -> BoxError {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into()).into()
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone())
}

fn file_value(file: &Value, name: &str) -> Option<String> {
    let key = name.replace('-', "_");
    file.get(&key).or_else(|| file.get(name)).and_then(value_string)
}

fn configured(args: &[String], file: &Value, name: &str, env_name: &str, fallback: String) -> String {
    flag(args, &format!("--{name}"))
        .or_else(|| std::env::var(env_name).ok())
        .or_else(|| file_value(file, name))
        .unwrap_or(fallback)
}

fn configured_optional(args: &[String], file: &Value, name: &str, env_name: &str) -> Option<String> {
    flag(args, &format!("--{name}")).or_else(|| std::env::var(env_name).ok()).or_else(|| file_value(file, name))
}

fn configured_path(args: &[String], file: &Value, name: &str, env_name: &str, fallback: PathBuf) -> PathBuf {
    PathBuf::from(configured(args, file, name, env_name, fallback.to_string_lossy().into_owned()))
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(v) => Some(v.clone()),
        Value::Number(v) => Some(v.to_string()),
        Value::Bool(v) => Some(v.to_string()),
        Value::Array(values) => Some(values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")),
        _ => None,
    }
}

fn list_value(value: Option<String>) -> Vec<String> {
    match value { Some(value) => value.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect(), None => Vec::new() }
}

fn parse_u16(value: &str, name: &str) -> Result<u16, BoxError> { value.parse().map_err(|_| boxed(format!("{name} must be a valid port"))) }
fn parse_bool(value: &str, fallback: bool) -> bool { match value.to_ascii_lowercase().as_str() { "true" => true, "false" => false, _ => fallback } }
fn normalize_ip(value: &str) -> String { value.trim().trim_start_matches("::ffff:").split('%').next().unwrap_or("").to_string() }

fn origin_allowed(state: &Config, origin: &str) -> bool { state.allowed_origins.is_empty() || state.allowed_origins.iter().any(|v| v == "*" || v == origin) }

fn cors_headers(request: &Request<Body>, state: &Config) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) && origin_allowed(state, origin) {
        if let Ok(value) = HeaderValue::from_str(origin) { headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value); }
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    headers
}

fn json_response(request: &Request<Body>, state: &Config, status: StatusCode, value: Value) -> Response {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":{\"message\":\"serialization failed\"}}".to_vec());
    let mut response = Response::new(Body::from(body.clone()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    if let Ok(value) = HeaderValue::from_str(&body.len().to_string()) { headers.insert(header::CONTENT_LENGTH, value); }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.extend(cors_headers(request, state));
    response
}

fn error_response(request: &Request<Body>, state: &Config, error: GatewayError) -> Response {
    json_response(request, state, error.status, json!({"error": {"message": error.message, "type": error.error_type, "param": error.param, "code": error.code}}))
}

fn bearer_token(request: &Request<Body>) -> Option<&str> {
    let value = request.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}

fn safe_equal(left: Option<&str>, right: &str) -> bool {
    let Some(left) = left else { return false; };
    let a = left.as_bytes(); let b = right.as_bytes();
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn legacy_authorized(request: &Request<Body>, config: &Config) -> bool {
    !config.auth_required || (!config.local_key.is_empty() && safe_equal(bearer_token(request), &config.local_key))
}

async fn authenticated_key_id(request: &Request<Body>, state: &AppState) -> Option<String> {
    if !state.config.auth_required { return Some("anonymous".into()); }
    if !state.config.local_key.is_empty() && safe_equal(bearer_token(request), &state.config.local_key) { return Some("legacy".into()); }
    let token = bearer_token(request)?;
    let hash = hash_key(token);
    let store = state.api_keys.lock().await;
    store.keys.iter().find(|key| !key.revoked && safe_equal(Some(hash.as_str()), key.key_hash.as_str())).map(|key| key.id.clone())
}

fn admin_authorized(request: &Request<Body>, state: &AppState) -> bool {
    safe_equal(bearer_token(request), state.admin_key.as_ref().as_str())
}

fn html_response(body: &str) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn admin_page() -> Response {
    html_response(r##"<!doctype html>
<html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>MonkeyCode API Key 管理</title>
<style>
body{font:15px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;max-width:960px;margin:40px auto;padding:0 20px;color:#202124;background:#f7f7f8}main{background:white;padding:28px;border-radius:14px;box-shadow:0 4px 24px #0001}h1{margin-top:0}input,button{font:inherit;padding:9px 12px;border:1px solid #ccd0d5;border-radius:7px}input{width:min(520px,90%)}button{cursor:pointer;background:#202124;color:white;margin-left:6px}.danger{background:#b42318}.key{font-family:monospace;word-break:break-all;background:#fff8d6;padding:12px;border-radius:7px}.muted{color:#6b7280}table{width:100%;border-collapse:collapse;margin-top:22px}td,th{text-align:left;padding:11px 7px;border-bottom:1px solid #eee}code{font-family:monospace}.hidden{display:none}
</style><main><h1>API Key 管理</h1><p class="muted">Admin Key 只保存在本浏览器，不会发送到第三方服务。</p>
<p><input id="admin" type="password" placeholder="输入 Admin Key"><button onclick="loadKeys()">登录 / 刷新</button></p>
<section id="app" class="hidden"><p><input id="name" placeholder="Key 名称，例如 production-app"><button onclick="createKey()">创建 API Key</button><button onclick="loadUsage()">刷新用量</button></p><div id="new" class="hidden"></div><div id="usage" class="muted"></div><pre id="usage_detail" class="muted"></pre><table><thead><tr><th>名称</th><th>ID</th><th>创建时间</th><th>状态</th><th>操作</th></tr></thead><tbody id="keys"></tbody></table></section><p id="msg" class="muted"></p></main>
<script>
const admin=document.querySelector('#admin'),msg=document.querySelector('#msg'); admin.value=localStorage.getItem('monkeycode_admin_key')||'';
function auth(){return {Authorization:'Bearer '+admin.value,'Content-Type':'application/json'}}
async function api(url,opt={}){let r=await fetch(url,{...opt,headers:{...auth(),...(opt.headers||{})}});let j=await r.json().catch(()=>({}));if(!r.ok)throw Error(j.error?.message||('HTTP '+r.status));return j}
async function loadKeys(){try{localStorage.setItem('monkeycode_admin_key',admin.value);let j=await api('/v1/admin/keys');document.querySelector('#app').classList.remove('hidden');document.querySelector('#keys').innerHTML=j.data.map(k=>`<tr><td>${esc(k.name)}</td><td><code>${esc(k.id)}</code></td><td>${new Date(k.created_at*1000).toLocaleString()}</td><td>${k.revoked?'已撤销':'有效'}</td><td>${k.revoked?'':`<button onclick="rotate('${k.id}')">轮换</button><button class="danger" onclick="revoke('${k.id}')">撤销</button>`}</td></tr>`).join('');msg.textContent='已加载 '+j.data.length+' 个 Key';await loadUsage()}catch(e){msg.textContent=e.message}}
async function loadUsage(){try{let j=await api('/v1/admin/usage');document.querySelector('#usage_detail').textContent='按 Key:\n'+j.by_key.map(x=>`${x.name}: ${x.requests} 次 / ${x.input_tokens+x.output_tokens} tokens / 错误 ${x.errors}`).join('\n')+'\n\n按模型:\n'+j.by_model.map(x=>`${x.name}: ${x.requests} 次 / ${x.input_tokens+x.output_tokens} tokens / 错误 ${x.errors}`).join('\n'); }catch(e){msg.textContent=e.message}}
async function createKey(){try{let j=await api('/v1/admin/keys',{method:'POST',body:JSON.stringify({name:document.querySelector('#name').value})});let n=document.querySelector('#new');n.classList.remove('hidden');n.innerHTML='<p>请立即复制，关闭页面后不会再次显示：</p><div class="key">'+esc(j.key)+'</div>';document.querySelector('#name').value='';await loadKeys()}catch(e){msg.textContent=e.message}}
async function rotate(id){if(!confirm('轮换后旧 Key 会立即失效，确认继续？'))return;try{let j=await api('/v1/admin/keys/'+encodeURIComponent(id)+'/rotate',{method:'POST'});let n=document.querySelector('#new');n.classList.remove('hidden');n.innerHTML='<p>新 Key（旧 Key 已失效，请立即复制）：</p><div class="key">'+esc(j.key)+'</div>';await loadKeys()}catch(e){msg.textContent=e.message}}
function esc(v){return String(v).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
</script></html>"##)
}

async fn handle_admin_keys(request: Request<Body>, state: AppState, key_id: Option<&str>, action: &str) -> Response {
    if !admin_authorized(&request, &state) { return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key")); }
    if action == "list" {
        let store = state.api_keys.lock().await;
        return json_response(&request, &state.config, StatusCode::OK, json!({"object": "list", "data": store.keys.iter().map(ApiKeyStore::public_key).collect::<Vec<_>>() }));
    }
    if action == "create" {
        let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => return e.into_response() };
        let name = body.get("name").and_then(Value::as_str).unwrap_or("Unnamed key").trim();
        if name.is_empty() { return GatewayError::new(StatusCode::BAD_REQUEST, "name must not be empty").into_response(); }
        let raw_key = new_api_key();
        let record = ApiKeyRecord { id: format!("key_{}", Uuid::new_v4().simple()), name: name.to_string(), key_hash: hash_key(&raw_key), created_at: now(), revoked: false };
        let public = ApiKeyStore::public_key(&record);
        let mut store = state.api_keys.lock().await; store.keys.push(record);
        if let Err(e) = store.save().await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).with_type("api_error").into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::CREATED, json!({"key": raw_key, "data": public}));
    }
    let Some(key_id) = key_id else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
    let mut store = state.api_keys.lock().await;
    let Some(index) = store.keys.iter().position(|key| key.id == key_id) else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
    if action == "rotate" {
        let name = store.keys[index].name.clone();
        store.keys[index].revoked = true;
        let raw_key = new_api_key();
        let record = ApiKeyRecord { id: format!("key_{}", Uuid::new_v4().simple()), name, key_hash: hash_key(&raw_key), created_at: now(), revoked: false };
        let public = ApiKeyStore::public_key(&record);
        store.keys.push(record);
        if let Err(e) = store.save().await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, json!({"key": raw_key, "data": public}));
    }
    if action == "revoke" {
        store.keys[index].revoked = true;
        if let Err(e) = store.save().await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, json!({"data": ApiKeyStore::public_key(&store.keys[index])}));
    }
    GatewayError::new(StatusCode::NOT_FOUND, "unknown admin operation").into_response()
}

fn client_ip(request: &Request<Body>, remote: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(value) = request.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            return normalize_ip(value.split(',').next().unwrap_or(""));
        }
    }
    normalize_ip(&remote.ip().to_string())
}


fn ip_allowed(request: &Request<Body>, config: &Config, remote: SocketAddr) -> bool {
    config.allowed_ips.is_empty() || config.allowed_ips.iter().any(|ip| ip == &client_ip(request, remote, config.trust_proxy))
}

async fn read_json(request: Request<Body>, max_bytes: usize) -> Result<Value, GatewayError> {
    let bytes = to_bytes(request.into_body(), max_bytes).await.map_err(|_| GatewayError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"))?;
    if bytes.is_empty() { return Ok(Value::Object(Map::new())); }
    serde_json::from_slice(&bytes).map_err(|_| GatewayError::new(StatusCode::BAD_REQUEST, "invalid JSON request body"))
}

fn text_from_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|part| match part { Value::String(value) => Some(value.clone()), Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string), _ => None }).filter(|v| !v.is_empty()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn developer_prompt(body: &Value) -> String {
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) && !instructions.is_empty() { return instructions.to_string(); }
    for collection_name in ["messages", "input"] {
        if let Some(Value::Array(collection)) = body.get(collection_name) {
            let texts: Vec<_> = collection.iter().filter(|item| matches!(item.get("role").and_then(Value::as_str), Some("system") | Some("developer"))).map(|item| text_from_content(item.get("content").or_else(|| item.get("text")))).filter(|v| !v.is_empty()).collect();
            if !texts.is_empty() { return texts.join("\n\n"); }
        }
    }
    "You are a helpful assistant.".to_string()
}

fn input_content(content: Option<&Value>) -> Value {
    let parts = match content {
        Some(Value::String(value)) => vec![json!({"type": "input_text", "text": value})],
        Some(Value::Array(values)) => values.iter().filter_map(|part| match part {
            Value::String(value) => Some(json!({"type": "input_text", "text": value})),
            Value::Object(map) if matches!(map.get("type").and_then(Value::as_str), Some("text") | Some("input_text")) => Some(json!({"type": "input_text", "text": map.get("text").and_then(Value::as_str).unwrap_or("")})),
            Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("image_url") => map.get("image_url").and_then(|v| v.get("url")).and_then(Value::as_str).map(|url| json!({"type": "input_image", "image_url": url, "detail": map.get("image_url").and_then(|v| v.get("detail")).and_then(Value::as_str).unwrap_or("auto")})),
            Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("input_image") => Some(Value::Object(map.clone())),
            _ => None,
        }).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    Value::Array(if parts.is_empty() { vec![json!({"type": "input_text", "text": ""})] } else { parts })
}

fn messages_to_input(messages: Option<&Value>) -> Value {
    let Some(Value::Array(messages)) = messages else { return Value::Array(Vec::new()); };
    Value::Array(messages.iter().filter_map(|message| {
        let map = message.as_object()?;
        let role = map.get("role")?.as_str()?;
        if !["system", "developer", "user", "assistant"].contains(&role) { return None; }
        Some(json!({"role": if role == "system" { "developer" } else { role }, "content": input_content(map.get("content"))}))
    }).collect())
}

fn normalize_response_input(input: Option<&Value>) -> Value {
    match input {
        Some(Value::String(value)) => Value::Array(vec![json!({"role": "user", "content": input_content(Some(&Value::String(value.clone())))} )]),
        Some(Value::Array(items)) => Value::Array(items.iter().map(|item| {
            if item.is_string() { return json!({"role": "user", "content": input_content(Some(item))}); }
            let mut map = item.as_object().cloned().unwrap_or_default();
            if map.get("type").and_then(Value::as_str).is_some_and(|v| v != "message") { return Value::Object(map); }
            let role = map.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
            map.insert("role".into(), Value::String(if role == "system" { "developer".into() } else { role }));
            let content = map.remove("content").or_else(|| map.remove("text")).unwrap_or(Value::String(String::new()));
            map.insert("content".into(), input_content(Some(&content)));
            Value::Object(map)
        }).collect()),
        _ => Value::Array(Vec::new()),
    }
}

fn normalize_chat_request(body: &Value, model: &str) -> Result<Value, GatewayError> {
    let Some(Value::Array(messages)) = body.get("messages") else { return Err(GatewayError::new(StatusCode::BAD_REQUEST, "messages must be a non-empty array")); };
    if messages.is_empty() { return Err(GatewayError::new(StatusCode::BAD_REQUEST, "messages must be a non-empty array")); }
    let mut outgoing = Map::new();
    outgoing.insert("model".into(), Value::String(model.into()));
    outgoing.insert("input".into(), messages_to_input(body.get("messages")));
    outgoing.insert("stream".into(), json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)));
    outgoing.insert("store".into(), body.get("store").cloned().unwrap_or(json!(false)));
    if let Some(value) = body.get("max_completion_tokens").filter(|v| !v.is_null()).or_else(|| body.get("max_tokens").filter(|v| !v.is_null())) { outgoing.insert("max_output_tokens".into(), value.clone()); }
    for field in ["temperature", "top_p", "metadata", "tools", "tool_choice", "parallel_tool_calls", "user"] { if let Some(value) = body.get(field) { outgoing.insert(field.into(), value.clone()); } }
    Ok(Value::Object(outgoing))
}

fn normalize_responses_request(body: &Value, model: &str, prompt: &str) -> Value {
    let mut outgoing = body.as_object().cloned().unwrap_or_default();
    outgoing.insert("model".into(), Value::String(model.into()));
    let mut input = normalize_response_input(body.get("input"));
    if matches!(input, Value::Array(ref values) if values.is_empty()) { input = messages_to_input(body.get("messages")); }
    if !input.as_array().is_some_and(|items| items.iter().any(|item| item.get("role").and_then(Value::as_str) == Some("developer"))) {
        input.as_array_mut().unwrap().insert(0, json!({"role": "developer", "content": input_content(Some(&Value::String(prompt.into())))}));
    }
    outgoing.insert("input".into(), input);
    outgoing.insert("store".into(), body.get("store").cloned().unwrap_or(json!(false)));
    outgoing.remove("messages"); outgoing.remove("system");
    Value::Object(outgoing)
}

async fn json_file(path: &Path, label: &str) -> Result<Value, GatewayError> {
    let text = tokio::fs::read_to_string(path).await.map_err(|e| GatewayError::config(format!("cannot read {label}: {} ({e})", path.display())))?;
    serde_json::from_str(&text).map_err(|e| GatewayError::config(format!("cannot parse {label}: {} ({e})", path.display())))
}

struct Runtime { base_url: String, api_key: String, model: String, signing_secret: String, model_ids: Vec<String> }

async fn load_runtime(config: &Config, requested_model: &str) -> Result<Runtime, GatewayError> {
    let (key_config, settings) = tokio::join!(json_file(&config.key_path, "OhMyAgent key"), json_file(&config.settings_path, "OhMyAgent settings"));
    let key_config = key_config?; let settings = settings?;
    let base_url = config.upstream_host.clone().or_else(|| key_config.get("base_url").and_then(Value::as_str).map(str::to_string)).unwrap_or_default().trim_end_matches('/').to_string();
    let upstream_key = config.upstream_key.clone().or_else(|| key_config.get("api_key").and_then(Value::as_str).map(str::to_string));
    let signing_secret = config.signing_secret.clone().or_else(|| key_config.get("signing_secret").and_then(Value::as_str).map(str::to_string));
    let models = settings.get("models").and_then(Value::as_object).cloned().unwrap_or_default();
    let normalized = if requested_model.contains('/') { requested_model.to_string() } else { format!("monkeycode-ultra/{requested_model}") };
    let model_config = models.values().find(|entry| entry.get("model").and_then(Value::as_str) == Some(requested_model) || entry.get("model").and_then(Value::as_str) == Some(normalized.as_str()));
    if base_url.is_empty() || upstream_key.as_deref().unwrap_or("").is_empty() || signing_secret.as_deref().unwrap_or("").is_empty() { return Err(GatewayError::config("gateway configuration is missing upstream_host, upstream_key, or signing_secret")); }
    let Some(model_config) = model_config else { return Err(GatewayError::new(StatusCode::NOT_FOUND, format!("model is not configured: {requested_model}")).with_code("model_not_found")); };
    let model = model_config.get("model").and_then(Value::as_str).unwrap_or(normalized.as_str()).to_string();
    let api_key = model_config.get("api_key").and_then(Value::as_str).unwrap_or(upstream_key.as_deref().unwrap()).to_string();
    Ok(Runtime { base_url, api_key, model, signing_secret: signing_secret.unwrap(), model_ids: models.values().filter_map(|v| v.get("model").and_then(Value::as_str).map(str::to_string)).collect() })
}

async fn request_upstream(state: &AppState, outgoing: &Value, runtime: &Runtime) -> Result<reqwest::Response, GatewayError> {
    let prompt = developer_prompt(outgoing);
    let mut signer = HmacSha256::new_from_slice(runtime.signing_secret.as_bytes()).map_err(|_| GatewayError::config("invalid signing_secret"))?;
    signer.update(prompt.as_bytes());
    let signature = hex::encode(signer.finalize().into_bytes());
    state.client.post(format!("{}/responses", runtime.base_url))
        .header(header::AUTHORIZATION, format!("Bearer {}", runtime.api_key))
        .header("X-OhMyAgent-Signature", format!("v1={signature}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, if outgoing.get("stream").and_then(Value::as_bool).unwrap_or(false) { "text/event-stream" } else { "application/json" })
        .json(outgoing).send().await.map_err(|e| GatewayError::new(StatusCode::BAD_GATEWAY, format!("upstream request failed: {e}")).with_type("api_error").with_code("upstream_unavailable"))
}

async fn upstream_error(response: reqwest::Response) -> GatewayError {
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = response.text().await.unwrap_or_default().chars().take(4000).collect::<String>();
    let parsed = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("error").cloned()).unwrap_or_else(|| json!({"message": if text.is_empty() { format!("upstream HTTP {status}") } else { text }}));
    let error_type = match parsed.get("type").and_then(Value::as_str) {
        Some("invalid_request_error") => "invalid_request_error",
        Some("authentication_error") => "authentication_error",
        Some("permission_error") => "permission_error",
        Some("api_error") => "api_error",
        _ => "upstream_error",
    };
    GatewayError { status, message: parsed.get("message").and_then(Value::as_str).unwrap_or("upstream request failed").to_string(), error_type, code: None, param: parsed.get("param").and_then(Value::as_str).map(str::to_string) }
}

fn response_output_text(data: &Value) -> String {
    if let Some(text) = data.get("output_text").and_then(Value::as_str) { return text.to_string(); }
    data.get("output").and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.get("content").and_then(Value::as_array)).flatten().filter(|item| item.get("type").and_then(Value::as_str) == Some("output_text")).filter_map(|item| item.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("")
}

fn response_finish_reason(data: Option<&Value>) -> &'static str {
    if data.and_then(|v| v.get("status")).and_then(Value::as_str) == Some("incomplete") && data.and_then(|v| v.get("incomplete_details")).and_then(|v| v.get("reason")).and_then(Value::as_str) == Some("max_output_tokens") { "length" } else { "stop" }
}

fn normalized_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?;
    let prompt = usage.get("input_tokens").or_else(|| usage.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let completion = usage.get("output_tokens").or_else(|| usage.get("completion_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Some(json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(prompt + completion)}))
}

fn sse_response(cors: HeaderMap, rx: mpsc::Receiver<Result<Bytes, Infallible>>) -> Response {
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.extend(cors);
    response
}

async fn send_sse(tx: &mpsc::Sender<Result<Bytes, Infallible>>, event: Option<&str>, data: &str) -> bool {
    let mut output = String::new();
    if let Some(event) = event { output.push_str("event: "); output.push_str(event); output.push('\n'); }
    output.push_str("data: "); output.push_str(data); output.push_str("\n\n");
    tx.send(Ok(Bytes::from(output))).await.is_ok()
}

fn sse_block(block: &str) -> (String, String) {
    let mut event = String::new(); let mut data = Vec::new();
    for line in block.lines() { if let Some(value) = line.strip_prefix("event:") { event = value.trim().to_string(); } else if let Some(value) = line.strip_prefix("data:") { data.push(value.strip_prefix(' ').unwrap_or(value).to_string()); } }
    (event, data.join("\n"))
}

async fn stream_chat(upstream: reqwest::Response, tx: mpsc::Sender<Result<Bytes, std::convert::Infallible>>, requested_model: String, state: AppState, key_id: String, started: std::time::Instant) {
    let id = format!("chatcmpl-{}", Uuid::new_v4()); let created = now();
    let chunk = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
        let mut value = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": requested_model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
        if let Some(usage) = usage { value["usage"] = usage; }
        value
    };
    if !send_sse(&tx, None, &chunk(json!({"role": "assistant", "content": ""}), None, None).to_string()).await { return; }
    let mut stream = upstream.bytes_stream(); let mut buffer = String::new(); let mut completed = None;
    while let Some(result) = stream.next().await {
        let Ok(bytes) = result else { break; }; buffer.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(index) = buffer.find("\n\n") {
            let block = buffer[..index].to_string(); buffer = buffer[index + 2..].to_string();
            let (_, data) = sse_block(&block); if data.is_empty() || data == "[DONE]" { continue; }
            let Ok(event) = serde_json::from_str::<Value>(&data) else { continue; };
            match event.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta") => if let Some(delta) = event.get("delta").and_then(Value::as_str) { if !send_sse(&tx, None, &chunk(json!({"content": delta}), None, None).to_string()).await { return; } },
                Some("response.completed") => completed = event.get("response").cloned(),
                Some("response.failed") | Some("error") => { let error = event.get("error").cloned().unwrap_or_else(|| json!({"message": "upstream stream failed", "type": "upstream_error"})); if !send_sse(&tx, None, &json!({"error": error}).to_string()).await { return; } },
                _ => {}
            }
        }
    }
    if !buffer.trim().is_empty() { let (_, data) = sse_block(&buffer); if let Ok(event) = serde_json::from_str::<Value>(&data) { if event.get("type").and_then(Value::as_str) == Some("response.completed") { completed = event.get("response").cloned(); } } }
    let finish = response_finish_reason(completed.as_ref());
    let _ = send_sse(&tx, None, &chunk(json!({}), Some(finish), normalized_usage(completed.as_ref().and_then(|v| v.get("usage")))).to_string()).await;
    let _ = send_sse(&tx, None, "[DONE]").await;
    record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::OK, started, completed.as_ref().and_then(|v| v.get("usage"))).await;
}

async fn stream_responses(upstream: reqwest::Response, tx: mpsc::Sender<Result<Bytes, std::convert::Infallible>>, state: AppState, key_id: String, requested_model: String, started: std::time::Instant) {
    if upstream.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").contains("text/event-stream") {
        let mut stream = upstream.bytes_stream(); let mut buffer = String::new(); let mut usage = None;
        while let Some(result) = stream.next().await {
            let Ok(bytes) = result else { break; };
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            if tx.send(Ok(bytes)).await.is_err() { return; }
            while let Some(index) = buffer.find("\n\n") {
                let block = buffer[..index].to_string(); buffer = buffer[index + 2..].to_string();
                let (_, data) = sse_block(&block);
                if let Ok(event) = serde_json::from_str::<Value>(&data) {
                    if event.get("type").and_then(Value::as_str) == Some("response.completed") { usage = event.get("response").and_then(|v| v.get("usage")).cloned(); }
                }
            }
        }
        record_usage(&state, &key_id, &requested_model, "responses", StatusCode::OK, started, usage.as_ref()).await;
        return;
    }
    let Ok(data) = upstream.json::<Value>().await else { return; };
    let mut sequence = 0u64;
    let mut event = |event_type: &str, extra: Value| { let mut map = extra.as_object().cloned().unwrap_or_default(); map.insert("type".into(), Value::String(event_type.into())); map.insert("sequence_number".into(), json!(sequence)); sequence += 1; Value::Object(map) };
    let mut response_body = data.as_object().cloned().unwrap_or_default();
    response_body.insert("status".into(), Value::String("in_progress".into()));
    response_body.insert("output".into(), Value::Array(Vec::new()));
    let created = event("response.created", json!({"response": response_body}));
    let _ = send_sse(&tx, None, &created.to_string()).await;
    let text = response_output_text(&data); if !text.is_empty() { let _ = send_sse(&tx, None, &event("response.output_text.delta", json!({"output_index": 0, "content_index": 0, "delta": text})).to_string()).await; }
    let usage = data.get("usage").cloned();
    let _ = send_sse(&tx, None, &event("response.completed", json!({"response": data})).to_string()).await;
    record_usage(&state, &key_id, &requested_model, "responses", StatusCode::OK, started, usage.as_ref()).await;
}

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

async fn handle_chat(request: Request<Body>, state: AppState, key_id: String) -> Response {
    let started = std::time::Instant::now(); let cors = cors_headers(&request, &state.config);
    let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, "unknown", "chat.completions", e.status, started, None).await; return e.into_response(); } };
    let requested_model = body.get("model").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("gpt-6-astra").to_string();
    let runtime = match load_runtime(&state.config, &requested_model).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } };
    let outgoing = match normalize_chat_request(&body, &runtime.model) { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } };
    let upstream = match request_upstream(&state, &outgoing, &runtime).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } };
    if !upstream.status().is_success() { let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY); let error = upstream_error(upstream).await; record_usage(&state, &key_id, &requested_model, "chat.completions", status, started, None).await; return error.into_response(); }
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) { let (tx, rx) = mpsc::channel(16); let response = sse_response(cors, rx); tokio::spawn(stream_chat(upstream, tx, requested_model, state, key_id, started)); return response; }
    let data = match upstream.json::<Value>().await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::BAD_GATEWAY, started, None).await; return GatewayError::new(StatusCode::BAD_GATEWAY, format!("invalid upstream response: {e}")).with_type("api_error").with_code("invalid_upstream_response").into_response(); } };
    record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::OK, started, data.get("usage")).await;
    let response_id = data.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4()));
    json!({"id": response_id, "object": "chat.completion", "created": now(), "model": requested_model, "choices": [{"index": 0, "message": {"role": "assistant", "content": response_output_text(&data)}, "finish_reason": response_finish_reason(Some(&data))}], "usage": normalized_usage(data.get("usage"))}).into_response()
}

async fn handle_responses(request: Request<Body>, state: AppState, key_id: String) -> Response {
    let started = std::time::Instant::now(); let cors = cors_headers(&request, &state.config);
    let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, "unknown", "responses", e.status, started, None).await; return e.into_response(); } };
    let requested_model = body.get("model").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("gpt-6-astra").to_string();
    let runtime = match load_runtime(&state.config, &requested_model).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", e.status, started, None).await; return e.into_response(); } };
    let outgoing = normalize_responses_request(&body, &runtime.model, &developer_prompt(&body));
    let upstream = match request_upstream(&state, &outgoing, &runtime).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", e.status, started, None).await; return e.into_response(); } };
    if !upstream.status().is_success() { let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY); let error = upstream_error(upstream).await; record_usage(&state, &key_id, &requested_model, "responses", status, started, None).await; return error.into_response(); }
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) { let (tx, rx) = mpsc::channel(16); let response = sse_response(cors, rx); tokio::spawn(stream_responses(upstream, tx, state, key_id, requested_model, started)); return response; }
    match upstream.json::<Value>().await {
        Ok(data) => { record_usage(&state, &key_id, &requested_model, "responses", StatusCode::OK, started, data.get("usage")).await; data.into_response() },
        Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", StatusCode::BAD_GATEWAY, started, None).await; GatewayError::new(StatusCode::BAD_GATEWAY, format!("invalid upstream response: {e}")).with_type("api_error").with_code("invalid_upstream_response").into_response() }
    }
}

async fn handle_admin_usage(request: Request<Body>, state: AppState) -> Response {
    if !admin_authorized(&request, &state) { return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key")); }
    let usage = state.usage.lock().await;
    json_response(&request, &state.config, StatusCode::OK, usage.summary())
}


    let path = request.uri().path().trim_end_matches('/'); let path = if path.is_empty() { "/" } else { path };
    if !ip_allowed(&request, &state.config, remote) { return error_response(&request, &state.config, GatewayError::new(StatusCode::FORBIDDEN, "client IP is not allowed").with_type("permission_error").with_code("ip_not_allowed")); }
    if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) && !origin_allowed(&state.config, origin) { return error_response(&request, &state.config, GatewayError::new(StatusCode::FORBIDDEN, "request origin is not allowed").with_type("permission_error").with_code("origin_not_allowed")); }
    if request.method() == axum::http::Method::OPTIONS { let mut response = StatusCode::NO_CONTENT.into_response(); response.headers_mut().extend(cors_headers(&request, &state.config)); response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS")); response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Authorization, Content-Type")); response.headers_mut().insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400")); return response; }
    if request.method() == axum::http::Method::GET && (path == "/" || path == "/v1") { return json_response(&request, &state.config, StatusCode::OK, json!({"object": "gateway", "name": "monkeycode-direct-gateway", "status": "ok", "endpoints": ["/health", "/v1/models", "/v1/responses", "/v1/chat/completions"]})); }
    if request.method() == axum::http::Method::GET && (path == "/health" || path == "/v1/health") { return json_response(&request, &state.config, StatusCode::OK, json!({"ok": true, "mode": "direct-signed-gateway", "auth_required": state.config.auth_required, "tls": state.config.tls_cert.is_some()})); }
    if request.method() == axum::http::Method::GET && path == "/admin" { return admin_page(); }
    if path == "/v1/admin/usage" && request.method() == axum::http::Method::GET { return handle_admin_usage(request, state).await; }
    if path == "/v1/admin/keys" {
        if request.method() == axum::http::Method::GET { return handle_admin_keys(request, state, None, "list").await; }
        if request.method() == axum::http::Method::POST { return handle_admin_keys(request, state, None, "create").await; }
    }
    if let Some(key_id) = path.strip_prefix("/v1/admin/keys/") {
        if let Some(id) = key_id.strip_suffix("/rotate") {
            if request.method() == axum::http::Method::POST { return handle_admin_keys(request, state, Some(id), "rotate").await; }
        }
        if request.method() == axum::http::Method::DELETE { return handle_admin_keys(request, state, Some(key_id), "revoke").await; }
    }
    let Some(key_id) = authenticated_key_id(&request, &state).await else { return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid API key").with_type("authentication_error").with_code("invalid_api_key")); };
    if request.method() == axum::http::Method::GET && (path == "/models" || path == "/v1/models") {
        return match load_runtime(&state.config, "gpt-6-astra").await { Ok(runtime) => { let mut ids = Vec::new(); for id in runtime.model_ids { if !ids.contains(&id) { ids.push(id.clone()); } let short = id.replacen("monkeycode-basic/", "", 1).replacen("monkeycode-pro/", "", 1).replacen("monkeycode-ultra/", "", 1); if !ids.contains(&short) { ids.push(short); } } json_response(&request, &state.config, StatusCode::OK, json!({"object": "list", "data": ids.into_iter().map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "monkeycode"})).collect::<Vec<_>>() })), Err(e) => error_response(&request, &state.config, e) };
    }
    if request.method() == axum::http::Method::POST && (path == "/responses" || path == "/v1/responses") { return handle_responses(request, state, key_id).await; }
    if request.method() == axum::http::Method::POST && (path == "/chat/completions" || path == "/v1/chat/completions") { return handle_chat(request, state, key_id).await; }
    error_response(&request, &state.config, GatewayError::new(StatusCode::NOT_FOUND, format!("unknown endpoint: {path}")).with_code("not_found"))
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Arc::new(Config::load(&args).await?);
    let client = reqwest::Client::builder().timeout(config.request_timeout).build()?;
    let (admin_key, generated_admin_key) = load_admin_key(&config).await?;
    if generated_admin_key {
        println!("generated Admin Key (save it securely): {admin_key}");
    }
    let api_keys = ApiKeyStore::load(config.api_keys_path.clone()).await?;
    let usage = UsageStore::load(config.usage_path.clone(), config.max_usage_records).await?;
    let state = AppState { config: config.clone(), client, admin_key: Arc::new(admin_key), api_keys: Arc::new(Mutex::new(api_keys)), usage: Arc::new(Mutex::new(usage)) };
    let app = Router::new().fallback(listener).with_state(state);
    let mut addresses = tokio::net::lookup_host(format!("{}:{}", config.host, config.port)).await.map_err(|e| boxed(format!("invalid listen address: {}:{} ({e})", config.host, config.port)))?;
    let address = addresses.next().ok_or_else(|| boxed(format!("cannot resolve listen address: {}:{}", config.host, config.port)))?;
    if let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) {
        let tls = RustlsConfig::from_pem_file(cert, key).await?;
        println!("direct-gateway-server listening on https://{}:{}", config.host, config.port);
        println!("mode=direct-signed-gateway");
        axum_server::bind_rustls(address, tls).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    } else {
        println!("direct-gateway-server listening on http://{}:{}", config.host, config.port);
        println!("mode=direct-signed-gateway");
        let tcp = TcpListener::bind(address).await?;
        axum::serve(tcp, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    }
    Ok(())
}
