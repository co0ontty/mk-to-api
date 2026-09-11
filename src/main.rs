#![recursion_limit = "256"]

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{ConnectInfo},
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

mod anthropic;
mod cli;
mod clients;
mod dashboard;
mod update;

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
    manage_clients: bool,
    manage_pi: bool,
    manage_codex: bool,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    client: reqwest::Client,
    admin_key: Arc<String>,
    api_keys: Arc<Mutex<ApiKeyStore>>,
    usage: Arc<Mutex<UsageStore>>,
    started_at: u64,
}

#[derive(Clone, Debug)]
struct ApiKeyRecord {
    id: String,
    name: String,
    key_hash: String,
    /// 明文 Key，供管理台查看。旧记录可能为空。
    secret: Option<String>,
    created_at: u64,
    revoked: bool,
    note: String,
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

/// 单组统计累加器。
///
/// 中文说明：看板需要同时展示「请求数 / 输入 token / 输出 token / 错误数 / 平均延迟 / 最高延迟 /
/// 最后一次调用时间」，为了避免在多个维度（API Key、模型、端点、状态码）上重复写四份几乎相同的
/// 代码，这里统一用一个结构体承载。
#[derive(Default, Clone, Copy)]
struct GroupTotals {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    errors: u64,
    latency_total: u64,
    latency_max: u64,
    last_seen: u64,
}

impl GroupTotals {
    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn avg_latency(&self) -> u64 {
        if self.requests == 0 {
            0
        } else {
            self.latency_total / self.requests
        }
    }

    fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            100.0
        } else {
            let ok = self.requests - self.errors;
            (ok as f64) * 100.0 / (self.requests as f64)
        }
    }

    /// 输出成前端直接可用的 JSON。
    fn to_json(&self, name: &str) -> Value {
        json!({
            "name": name,
            "requests": self.requests,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens(),
            "errors": self.errors,
            "success_rate": (self.success_rate() * 100.0).round() / 100.0,
            "avg_latency_ms": self.avg_latency(),
            "max_latency_ms": self.latency_max,
            "last_seen": self.last_seen,
        })
    }
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
        let legacy_dir = PathBuf::from(&home).join("Library/Application Support/com.chaitin.baizhi.monkeycode");
        let default_dir = std::env::var("MK2API_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(&home).join(".mk2api"));
        let config_path = flag(args, "--config")
            .or_else(|| std::env::var("MK2API_CONFIG").ok())
            .or_else(|| std::env::var("MONKEYCODE_GATEWAY_CONFIG").ok())
            .map(PathBuf::from)
            .unwrap_or_else(|| default_dir.join("config.json"));
        let file = match tokio::fs::read_to_string(&config_path).await {
            Ok(text) => serde_json::from_str::<Value>(&text).map_err(|e| boxed(format!("cannot read gateway config: {} ({e})", config_path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
            Err(e) => return Err(boxed(format!("cannot read gateway config: {} ({e})", config_path.display()))),
        };

        let config_dir = configured_path(args, &file, "config-dir", "MK2API_HOME", default_dir.clone());
        let key_path = configured_path(args, &file, "key-file", "MONKEYCODE_OHMYAGENT_KEY", legacy_dir.join("monkeycode-ohmyagent-key.json"));
        let settings_path = configured_path(args, &file, "settings", "OHMYAGENT_SETTINGS", legacy_dir.join("ohmyagent/settings.json"));
        let api_keys_path = configured_path(args, &file, "api-keys-file", "DIRECT_GATEWAY_API_KEYS_FILE", config_dir.join("api-keys.json"));
        let usage_path = configured_path(args, &file, "usage-file", "DIRECT_GATEWAY_USAGE_FILE", config_dir.join("usage.json"));
        let admin_key_path = configured_path(args, &file, "admin-key-file", "DIRECT_GATEWAY_ADMIN_KEY_FILE", config_dir.join("admin.key"));
        let host = configured(args, &file, "host", "DIRECT_GATEWAY_HOST", std::env::var("OHMYAGENT_BRIDGE_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()));
        let port = parse_u16(&configured(args, &file, "port", "DIRECT_GATEWAY_PORT", std::env::var("OHMYAGENT_BRIDGE_PORT").unwrap_or_else(|_| "8123".to_string())), "port")?;
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
            manage_clients: parse_bool(&configured(args, &file, "manage-clients", "MK2API_MANAGE_CLIENTS", "true".into()), true),
            manage_pi: parse_bool(&configured(args, &file, "manage-pi", "MK2API_MANAGE_PI", "true".into()), true),
            manage_codex: parse_bool(&configured(args, &file, "manage-codex", "MK2API_MANAGE_CODEX", "true".into()), true),
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
        let mut keys = match tokio::fs::read_to_string(&path).await {
            Ok(text) => {
                let value: Value = serde_json::from_str(&text)?;
                value.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(|item| Some(ApiKeyRecord {
                    id: item.get("id")?.as_str()?.to_string(),
                    name: item.get("name").and_then(Value::as_str).unwrap_or("Unnamed key").to_string(),
                    key_hash: item.get("key_hash")?.as_str()?.to_string(),
                    secret: item.get("key").or_else(|| item.get("secret")).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string),
                    created_at: item.get("created_at").and_then(Value::as_u64).unwrap_or(0),
                    revoked: item.get("revoked").and_then(Value::as_bool).unwrap_or(false),
                    note: item.get("note").and_then(Value::as_str).unwrap_or("").to_string(),
                })).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(boxed(format!("cannot read API keys: {} ({e})", path.display()))),
        };
        if let Some(parent) = path.parent() {
            if let Ok(text) = std::fs::read_to_string(parent.join("clients.json")) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    for name in ["pi", "codex"] {
                        let id = value.pointer(&format!("/{name}/key_id")).and_then(Value::as_str);
                        let secret = value.pointer(&format!("/{name}/key")).and_then(Value::as_str);
                        if let (Some(id), Some(secret)) = (id, secret) {
                            if let Some(key) = keys.iter_mut().find(|item| item.id == id && item.secret.is_none()) {
                                key.secret = Some(secret.to_string());
                            }
                        }
                    }
                }
            }
        }
        Ok(Self { path, keys })
    }

    fn serialized(&self) -> Result<String, BoxError> {
        let value = json!({"keys": self.keys.iter().map(|key| json!({
            "id": key.id, "name": key.name, "key_hash": key.key_hash,
            "key": key.secret, "created_at": key.created_at, "revoked": key.revoked, "note": key.note,
        })).collect::<Vec<_>>()});
        Ok(serde_json::to_string_pretty(&value)?)
    }

    fn public_key(key: &ApiKeyRecord) -> Value {
        json!({
            "id": key.id,
            "name": key.name,
            "created_at": key.created_at,
            "revoked": key.revoked,
            "note": key.note,
            "key": key.secret,
        })
    }
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

    fn record(&mut self, record: UsageRecord) -> Result<(PathBuf, String), BoxError> {
        self.records.push(record);
        if self.records.len() > self.max_records { let remove = self.records.len() - self.max_records; self.records.drain(..remove); }
        let value = json!({"records": self.records.iter().map(|item| json!({
            "timestamp": item.timestamp, "key_id": item.key_id, "model": item.model,
            "endpoint": item.endpoint, "status": item.status, "latency_ms": item.latency_ms,
            "input_tokens": item.input_tokens, "output_tokens": item.output_tokens,
        })).collect::<Vec<_>>()});
        Ok((self.path.clone(), serde_json::to_string(&value)?))
    }

    /// 时间窗内的起始时间戳。
    ///
    /// 中文说明：`window_seconds == 0` 表示「全部时间」，此时返回 0（而不是 now），
    /// 否则会得到空集合；其余情况返回 `now - window`，并做饱和减法避免回绕。
    fn window_start(&self, window_seconds: u64, now_ts: u64) -> u64 {
        if window_seconds == 0 {
            0
        } else {
            now_ts.saturating_sub(window_seconds)
        }
    }

    /// 通用分组聚合：按调用方给出的字段（Key / 模型 / 端点 / 状态码）累计统计。
    fn totals_by<F>(&self, window_seconds: u64, now_ts: u64, pick: F) -> std::collections::BTreeMap<String, GroupTotals>
    where
        F: Fn(&UsageRecord) -> String,
    {
        let since = self.window_start(window_seconds, now_ts);
        let mut map: std::collections::BTreeMap<String, GroupTotals> = std::collections::BTreeMap::new();
        for item in self.records.iter().filter(|item| item.timestamp >= since) {
            let totals = map.entry(pick(item)).or_default();
            totals.requests += 1;
            totals.input_tokens += item.input_tokens;
            totals.output_tokens += item.output_tokens;
            // 约定：HTTP 状态码 >= 400 一律计入错误（含 401/403/429/5xx）。
            totals.errors += u64::from(item.status >= 400);
            totals.latency_total += item.latency_ms;
            totals.latency_max = totals.latency_max.max(item.latency_ms);
            totals.last_seen = totals.last_seen.max(item.timestamp);
        }
        map
    }

    /// 把分组结果按请求量倒序（同量按名称升序）转成数组，便于前端直接渲染排行榜。
    fn totals_json(map: &std::collections::BTreeMap<String, GroupTotals>) -> Vec<Value> {
        let mut items: Vec<Value> = map.iter().map(|(name, totals)| totals.to_json(name)).collect();
        items.sort_by(|left, right| {
            let left_requests = left.get("requests").and_then(Value::as_u64).unwrap_or(0);
            let right_requests = right.get("requests").and_then(Value::as_u64).unwrap_or(0);
            right_requests.cmp(&left_requests).then_with(|| {
                left.get("name").and_then(Value::as_str).unwrap_or("").cmp(right.get("name").and_then(Value::as_str).unwrap_or(""))
            })
        });
        items
    }

    /// P95 延迟（毫秒）。数据量不大时直接排序取分位，避免引入额外依赖。
    fn percentile(values: &mut [u64], ratio: f64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        values.sort_unstable();
        let index = (((values.len() - 1) as f64) * ratio).round() as usize;
        values[index.min(values.len() - 1)]
    }

    /// 数据看板主统计。
    ///
    /// 中文说明：这里替换了早期 `summary()` 的实现。旧 `summary()` 只返回全局总量和按 Key / 模型
    /// 的分组，缺少时间序列、延迟分位、状态码分布、端点分布，无法支撑图表化看板；
    /// 新实现保留 `recent` 字段以兼容旧的 TUI 与脚本调用方，同时新增看板所需的全部维度。
    fn stats(&self, window_seconds: u64, now_ts: u64) -> Value {
        let since = self.window_start(window_seconds, now_ts);
        let mut latencies: Vec<u64> = Vec::new();
        let mut requests = 0u64;
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut errors = 0u64;
        let mut latency_total = 0u64;
        let mut last_activity = 0u64;
        for item in self.records.iter().filter(|item| item.timestamp >= since) {
            requests += 1;
            input_tokens += item.input_tokens;
            output_tokens += item.output_tokens;
            errors += u64::from(item.status >= 400);
            latency_total += item.latency_ms;
            latencies.push(item.latency_ms);
            last_activity = last_activity.max(item.timestamp);
        }

        // 时间序列自适应粒度：<= 3 天按小时，超过则按天，保证图表点数可控（<= ~90 个点）。
        let bucket_seconds: u64 = if window_seconds > 3 * 86_400 { 86_400 } else { 3_600 };
        let bucket_count = ((window_seconds / bucket_seconds).max(1)) as usize;
        let series_start = since / bucket_seconds * bucket_seconds;
        let mut series = vec![(0u64, 0u64, 0u64); bucket_count];
        for item in self.records.iter().filter(|item| item.timestamp >= since) {
            let index = ((item.timestamp.saturating_sub(series_start)) / bucket_seconds) as usize;
            let slot = &mut series[index.min(bucket_count - 1)];
            slot.0 += 1;
            slot.1 += item.input_tokens + item.output_tokens;
            slot.2 += u64::from(item.status >= 400);
        }
        let series: Vec<Value> = series
            .iter()
            .enumerate()
            .map(|(index, (bucket_requests, bucket_tokens, bucket_errors))| {
                json!({
                    "timestamp": series_start + (index as u64) * bucket_seconds,
                    "requests": bucket_requests,
                    "tokens": bucket_tokens,
                    "errors": bucket_errors,
                })
            })
            .collect();

        let by_key = self.totals_by(window_seconds, now_ts, |item| item.key_id.clone());
        let by_model = self.totals_by(window_seconds, now_ts, |item| item.model.clone());
        let by_endpoint = self.totals_by(window_seconds, now_ts, |item| item.endpoint.clone());
        let by_status = self.totals_by(window_seconds, now_ts, |item| item.status.to_string());
        let p95 = Self::percentile(&mut latencies, 0.95);
        let avg_latency = if requests == 0 { 0 } else { latency_total / requests };
        let error_rate = if requests == 0 { 0.0 } else { (errors as f64) * 100.0 / (requests as f64) };
        let success_rate = 100.0 - error_rate;

        json!({
            "window_seconds": window_seconds,
            "generated_at": now_ts,
            "requests": requests,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
            "errors": errors,
            // error_rate / success_rate 保留两位小数，避免前端再格式化。
            "error_rate": (error_rate * 100.0).round() / 100.0,
            "success_rate": (success_rate * 100.0).round() / 100.0,
            "avg_latency_ms": avg_latency,
            "p95_latency_ms": p95,
            "active_keys": by_key.values().filter(|totals| totals.requests > 0).count(),
            "active_models": by_model.values().filter(|totals| totals.requests > 0).count(),
            "last_activity": last_activity,
            "series": series,
            "by_key": Self::totals_json(&by_key),
            "by_model": Self::totals_json(&by_model),
            "by_endpoint": Self::totals_json(&by_endpoint),
            "by_status": Self::totals_json(&by_status),
            // recent 保持旧字段形态，兼容 `mk2api` TUI 与既有脚本。
            "recent": self.records.iter().rev().take(50).map(|item| json!({"timestamp": item.timestamp, "key_id": item.key_id, "model": item.model, "endpoint": item.endpoint, "status": item.status, "latency_ms": item.latency_ms, "input_tokens": item.input_tokens, "output_tokens": item.output_tokens})).collect::<Vec<_>>(),
        })
    }
}

pub(crate) fn usage_tokens(usage: Option<&Value>) -> (u64, u64) {
    let Some(usage) = usage else { return (0, 0); };
    (usage.get("input_tokens").or_else(|| usage.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0), usage.get("output_tokens").or_else(|| usage.get("completion_tokens")).and_then(Value::as_u64).unwrap_or(0))
}

async fn record_usage(state: &AppState, key_id: &str, model: &str, endpoint: &str, status: StatusCode, started: std::time::Instant, usage: Option<&Value>) {
    let (input_tokens, output_tokens) = usage_tokens(usage);
    let record = UsageRecord { timestamp: now(), key_id: key_id.to_string(), model: model.to_string(), endpoint: endpoint.to_string(), status: status.as_u16(), latency_ms: started.elapsed().as_millis() as u64, input_tokens, output_tokens };
    let snapshot = {
        let mut store = state.usage.lock().await;
        store.record(record)
    };
    match snapshot {
        Ok((path, contents)) => if let Err(error) = write_private(&path, &contents).await { eprintln!("usage record failed: {error}"); },
        Err(error) => eprintln!("usage record failed: {error}"),
    }
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
    if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin_allowed(state, origin) {
            if let Ok(value) = HeaderValue::from_str(origin) { headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value); }
            headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
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

async fn authenticated_key_id(token: Option<String>, state: &AppState) -> Option<String> {
    if !state.config.auth_required { return Some("anonymous".into()); }
    let token = token?;
    if !state.config.local_key.is_empty() && safe_equal(Some(token.as_str()), &state.config.local_key) { return Some("legacy".into()); }
    let hash = hash_key(&token);
    let keys = {
        let store = state.api_keys.lock().await;
        store.keys.iter().find(|key| !key.revoked && safe_equal(Some(hash.as_str()), key.key_hash.as_str())).map(|key| key.id.clone())
    };
    keys
}

fn admin_authorized(request: &Request<Body>, state: &AppState) -> bool {
    safe_equal(bearer_token(request), state.admin_key.as_ref().as_str())
}

/// 站点图标。
///
/// 中文说明：浏览器会自动请求 `/favicon.ico`。网关对所有未匹配路径返回 401/404，
/// 会让控制台每次刷新都报一条“加载资源失败”的噪声错误，所以这里直接内联一个小 SVG 图标——
/// 既不引入二进制资源，也不影响单文件构建。
fn favicon() -> Response {
    const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#0b0e14"/><circle cx="32" cy="32" r="9" fill="#4c8dff"/><circle cx="32" cy="32" r="17" fill="none" stroke="#4c8dff" stroke-opacity=".4" stroke-width="3"/><circle cx="32" cy="32" r="25" fill="none" stroke="#a78bfa" stroke-opacity=".25" stroke-width="3"/></svg>"##;
    let mut response = Response::new(Body::from(ICON));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    response
}

/// 解析 URL 查询串。
///
/// 中文说明：看板只用几个简单参数，引入 `serde_urlencoded` 之类依赖并不划算，
/// 这里做最小实现：按 `&`/`=` 切分，并处理 `%XX` 百分号编码与 `+` 空格。
fn query_params(uri: &axum::http::Uri) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    let Some(query) = uri.query() else { return params; };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => { output.push(byte); index += 3; }
                    None => { output.push(bytes[index]); index += 1; }
                }
            }
            b'+' => { output.push(b' '); index += 1; }
            byte => { output.push(byte); index += 1; }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

async fn handle_admin_keys(request: Request<Body>, state: AppState, key_id: Option<&str>, action: &str) -> Response {
    if !admin_authorized(&request, &state) { return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key")); }
    if action == "list" {
        let keys = {
            let store = state.api_keys.lock().await;
            store.keys.iter().map(ApiKeyStore::public_key).collect::<Vec<_>>()
        };
        return json_response(&request, &state.config, StatusCode::OK, json!({"object": "list", "data": keys}));
    }
    if action == "get" {
        let Some(key_id) = key_id else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
        let public = {
            let store = state.api_keys.lock().await;
            store.keys.iter().find(|key| key.id == key_id).map(ApiKeyStore::public_key)
        };
        let Some(public) = public else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
        return json_response(&request, &state.config, StatusCode::OK, json!({"data": public}));
    }
    if action == "create" {
        let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => return e.into_response() };
        let name = body.get("name").and_then(Value::as_str).unwrap_or("Unnamed key").trim();
        if name.is_empty() { return GatewayError::new(StatusCode::BAD_REQUEST, "name must not be empty").into_response(); }
        // note 为可选备注字段，看板用它标记 Key 用途（如「给同事 A」）。
        let note = body.get("note").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let raw_key = new_api_key();
        let record = ApiKeyRecord { id: format!("key_{}", Uuid::new_v4().simple()), name: name.to_string(), key_hash: hash_key(&raw_key), secret: Some(raw_key.clone()), created_at: now(), revoked: false, note };
        let public = ApiKeyStore::public_key(&record);
        let save_snapshot = {
            let mut store = state.api_keys.lock().await;
            store.keys.push(record);
            store.serialized().map(|contents| (store.path.clone(), contents))
        };
        let (path, contents) = match save_snapshot { Ok(value) => value, Err(e) => return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot serialize API key: {e}")).with_type("api_error").into_response() };
        if let Err(e) = write_private(&path, &contents).await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).with_type("api_error").into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::CREATED, json!({"key": raw_key, "data": public}));
    }
    let Some(key_id) = key_id else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
    if action == "rotate" {
        let save_snapshot = {
            let mut store = state.api_keys.lock().await;
            let Some(index) = store.keys.iter().position(|key| key.id == key_id) else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
            let name = store.keys[index].name.clone();
            let note = store.keys[index].note.clone();
            store.keys[index].revoked = true;
            let raw_key = new_api_key();
            let record = ApiKeyRecord { id: format!("key_{}", Uuid::new_v4().simple()), name, key_hash: hash_key(&raw_key), secret: Some(raw_key.clone()), created_at: now(), revoked: false, note };
            let public = ApiKeyStore::public_key(&record);
            store.keys.push(record);
            let snapshot = store.serialized().map(|contents| (store.path.clone(), contents));
            (raw_key, public, snapshot)
        };
        let (raw_key, public, snapshot) = save_snapshot;
        let (path, contents) = match snapshot { Ok(value) => value, Err(e) => return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot serialize API key: {e}")).into_response() };
        if let Err(e) = write_private(&path, &contents).await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, json!({"key": raw_key, "data": public}));
    }
    if action == "revoke" {
        let save_snapshot = {
            let mut store = state.api_keys.lock().await;
            let Some(index) = store.keys.iter().position(|key| key.id == key_id) else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
            store.keys[index].revoked = true;
            let public = ApiKeyStore::public_key(&store.keys[index]);
            store.serialized().map(|contents| (store.path.clone(), contents)).map(|snapshot| (public, snapshot))
        };
        let (public, snapshot) = match save_snapshot { Ok(value) => value, Err(e) => return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot serialize API key: {e}")).into_response() };
        let (path, contents) = snapshot;
        if let Err(e) = write_private(&path, &contents).await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, json!({"data": public}));
    }
    if action == "edit" {
        // 重命名 / 修改备注 / 恢复启用。看板用 PATCH 调这个分支。
        let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => return e.into_response() };
        let public = {
            let mut store = state.api_keys.lock().await;
            let Some(index) = store.keys.iter().position(|key| key.id == key_id) else { return GatewayError::new(StatusCode::NOT_FOUND, "API key not found").into_response(); };
            if let Some(name) = body.get("name").and_then(Value::as_str) {
                let name = name.trim();
                if name.is_empty() { return GatewayError::new(StatusCode::BAD_REQUEST, "name must not be empty").into_response(); }
                store.keys[index].name = name.to_string();
            }
            if let Some(note) = body.get("note").and_then(Value::as_str) { store.keys[index].note = note.trim().to_string(); }
            if let Some(revoked) = body.get("revoked").and_then(Value::as_bool) { store.keys[index].revoked = revoked; }
            ApiKeyStore::public_key(&store.keys[index])
        };
        let snapshot = {
            let store = state.api_keys.lock().await;
            store.serialized().map(|contents| (store.path.clone(), contents))
        };
        let (path, contents) = match snapshot { Ok(value) => value, Err(e) => return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot serialize API key: {e}")).into_response() };
        if let Err(e) = write_private(&path, &contents).await { return GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save API key: {e}")).into_response(); }
        return json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, json!({"data": public}));
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
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() { return instructions.to_string(); }
    }
    let system = anthropic::system_text(body.get("system"));
    if !system.is_empty() { return system; }
    for collection_name in ["messages", "input"] {
        if let Some(Value::Array(collection)) = body.get(collection_name) {
            let texts: Vec<_> = collection.iter().filter(|item| matches!(item.get("role").and_then(Value::as_str), Some("system") | Some("developer"))).map(|item| text_from_content(item.get("content").or_else(|| item.get("text")))).filter(|v| !v.is_empty()).collect();
            if !texts.is_empty() { return texts.join("\n\n"); }
        }
    }
    "You are a helpful assistant.".to_string()
}

fn input_content(content: Option<&Value>, role: &str) -> Value {
    let text_type = if role == "assistant" { "output_text" } else { "input_text" };
    let parts = match content {
        Some(Value::String(value)) => vec![json!({"type": text_type, "text": value})],
        Some(Value::Array(values)) => values.iter().filter_map(|part| match part {
            Value::String(value) => Some(json!({"type": text_type, "text": value})),
            Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("refusal") => {
                if role == "assistant" {
                    Some(json!({"type": "refusal", "refusal": map.get("refusal").or_else(|| map.get("text")).and_then(Value::as_str).unwrap_or("")}))
                } else {
                    Some(json!({"type": text_type, "text": map.get("refusal").or_else(|| map.get("text")).and_then(Value::as_str).unwrap_or("")}))
                }
            }
            Value::Object(map) if matches!(map.get("type").and_then(Value::as_str), Some("text") | Some("input_text") | Some("output_text")) => Some(json!({"type": text_type, "text": map.get("text").and_then(Value::as_str).unwrap_or("")})),
            Value::Object(map) if role != "assistant" && map.get("type").and_then(Value::as_str) == Some("image_url") => map.get("image_url").and_then(|v| v.get("url")).and_then(Value::as_str).map(|url| json!({"type": "input_image", "image_url": url, "detail": map.get("image_url").and_then(|v| v.get("detail")).and_then(Value::as_str).unwrap_or("auto")})),
            Value::Object(map) if role != "assistant" && map.get("type").and_then(Value::as_str) == Some("input_image") => Some(Value::Object(map.clone())),
            _ => None,
        }).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    Value::Array(if parts.is_empty() { vec![json!({"type": text_type, "text": ""})] } else { parts })
}

fn messages_to_input(messages: Option<&Value>) -> Value {
    let Some(Value::Array(messages)) = messages else { return Value::Array(Vec::new()); };
    Value::Array(messages.iter().filter_map(|message| {
        let map = message.as_object()?;
        let role = map.get("role")?.as_str()?;
        if !["system", "developer", "user", "assistant"].contains(&role) { return None; }
        let normalized_role = if role == "system" { "developer" } else { role };
        Some(json!({"role": normalized_role, "content": input_content(map.get("content"), normalized_role)}))
    }).collect())
}

fn normalize_response_input(input: Option<&Value>) -> Value {
    match input {
        Some(Value::String(value)) => Value::Array(vec![json!({"role": "user", "content": input_content(Some(&Value::String(value.clone())), "user")} )]),
        Some(Value::Array(items)) => Value::Array(items.iter().map(|item| {
            if item.is_string() { return json!({"role": "user", "content": input_content(Some(item), "user")}); }
            let mut map = item.as_object().cloned().unwrap_or_default();
            if map.get("type").and_then(Value::as_str).is_some_and(|v| v != "message") { return Value::Object(map); }
            let role = map.get("role").and_then(Value::as_str).unwrap_or("user");
            let normalized_role = if role == "system" { "developer" } else { role }.to_string();
            map.insert("role".into(), Value::String(normalized_role.clone()));
            let content = map.remove("content").or_else(|| map.remove("text")).unwrap_or(Value::String(String::new()));
            map.insert("content".into(), input_content(Some(&content), &normalized_role));
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
    let mut input = messages_to_input(body.get("messages"));
    if !input.as_array().is_some_and(|items| items.iter().any(|item| item.get("role").and_then(Value::as_str) == Some("developer"))) {
        let prompt = developer_prompt(body);
        input.as_array_mut().unwrap().insert(0, json!({"role": "developer", "content": input_content(Some(&Value::String(prompt)), "developer")}));
    }
    outgoing.insert("input".into(), input);
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
        input.as_array_mut().unwrap().insert(0, json!({"role": "developer", "content": input_content(Some(&Value::String(prompt.into())), "developer")}));
    }
    outgoing.insert("input".into(), input);
    outgoing.insert("store".into(), body.get("store").cloned().unwrap_or(json!(false)));
    outgoing.remove("messages"); outgoing.remove("system");
    strip_disabled_reasoning(&mut outgoing);
    Value::Object(outgoing)
}

/// Pi 在关闭思考时会带 `reasoning.effort = none`。不少上游模型（如 Qwen）拒绝这个取值，
/// 直接 502。effort 为 none/off 时改为不传 reasoning，让上游走默认非思考路径。
fn strip_disabled_reasoning(outgoing: &mut Map<String, Value>) {
    let effort = outgoing
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .or_else(|| outgoing.get("reasoning_effort"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(effort.as_str(), "none" | "off" | "false" | "0") {
        outgoing.remove("reasoning");
        outgoing.remove("reasoning_effort");
    }
}

async fn json_file(path: &Path, label: &str) -> Result<Value, GatewayError> {
    let text = tokio::fs::read_to_string(path).await.map_err(|e| GatewayError::config(format!("cannot read {label}: {} ({e})", path.display())))?;
    serde_json::from_str(&text).map_err(|e| GatewayError::config(format!("cannot parse {label}: {} ({e})", path.display())))
}

struct Runtime {
    base_url: String,
    api_key: String,
    model: String,
    signing_secret: String,
    anthropic: bool,
    max_output: u64,
}

fn same_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn resolve_model<'a>(models: &'a [Value], requested_model: &str) -> Option<&'a Value> {
    if requested_model.contains('/') {
        return models.iter().find(|entry| entry.get("model").and_then(Value::as_str) == Some(requested_model));
    }
    for prefix in ["monkeycode-basic/", "monkeycode-pro/", "monkeycode-ultra/"] {
        let candidate = format!("{prefix}{requested_model}");
        if let Some(found) = models.iter().find(|entry| entry.get("model").and_then(Value::as_str) == Some(candidate.as_str())) {
            return Some(found);
        }
    }
    models.iter().find(|entry| entry.get("model").and_then(Value::as_str) == Some(requested_model))
}

fn models_for_upstream(settings: &Value, base_url: &str) -> Vec<Value> {
    settings.get("models").and_then(Value::as_object).into_iter().flat_map(|models| models.values().cloned()).filter(|entry| {
        match entry.get("base_url").and_then(Value::as_str) {
            Some(url) => same_url(url, base_url),
            None => true,
        }
    }).collect()
}

async fn load_runtime(config: &Config, requested_model: &str) -> Result<Runtime, GatewayError> {
    let (key_config, settings) = tokio::join!(json_file(&config.key_path, "OhMyAgent key"), json_file(&config.settings_path, "OhMyAgent settings"));
    let key_config = key_config?; let settings = settings?;
    let base_url = config.upstream_host.clone().or_else(|| key_config.get("base_url").and_then(Value::as_str).map(str::to_string)).unwrap_or_default().trim_end_matches('/').to_string();
    let upstream_key = config.upstream_key.clone().or_else(|| key_config.get("api_key").and_then(Value::as_str).map(str::to_string));
    let signing_secret = config.signing_secret.clone().or_else(|| key_config.get("signing_secret").and_then(Value::as_str).map(str::to_string));
    if base_url.is_empty() || upstream_key.as_deref().unwrap_or("").is_empty() || signing_secret.as_deref().unwrap_or("").is_empty() { return Err(GatewayError::config("gateway configuration is missing upstream_host, upstream_key, or signing_secret")); }
    let models = models_for_upstream(&settings, &base_url);
    let Some(model_config) = resolve_model(&models, requested_model) else { return Err(GatewayError::new(StatusCode::NOT_FOUND, format!("model is not configured: {requested_model}")).with_code("model_not_found")); };
    let model = model_config.get("model").and_then(Value::as_str).unwrap_or(requested_model).to_string();
    let api_key = model_config.get("api_key").and_then(Value::as_str).unwrap_or(upstream_key.as_deref().unwrap()).to_string();
    Ok(Runtime {
        base_url,
        api_key,
        model,
        signing_secret: signing_secret.unwrap(),
        anthropic: anthropic::is_anthropic_type(model_config.get("type").and_then(Value::as_str)),
        max_output: model_config.get("max_output").and_then(Value::as_u64).unwrap_or(32_000),
    })
}

async fn configured_model_ids(config: &Config) -> Result<Vec<String>, GatewayError> {
    let (key_config, settings) = tokio::join!(json_file(&config.key_path, "OhMyAgent key"), json_file(&config.settings_path, "OhMyAgent settings"));
    let key_config = key_config?; let settings = settings?;
    let base_url = config.upstream_host.clone().or_else(|| key_config.get("base_url").and_then(Value::as_str).map(str::to_string)).unwrap_or_default().trim_end_matches('/').to_string();
    let mut ids = Vec::new();
    for id in models_for_upstream(&settings, &base_url).iter().filter_map(|entry| entry.get("model").and_then(Value::as_str)) {
        let id = id.to_string();
        if !ids.contains(&id) { ids.push(id.clone()); }
        let short = id.replacen("monkeycode-basic/", "", 1).replacen("monkeycode-pro/", "", 1).replacen("monkeycode-ultra/", "", 1);
        if !ids.contains(&short) { ids.push(short); }
    }
    Ok(ids)
}

/// 模型目录：`(模型 id, 是否为 anthropic 协议)`。
///
/// 与 `configured_model_ids()` 的差别：后者只给调用方一串 id（用于写客户端配置），
/// 这里额外带上协议类型，供看板「模型」页展示并区分 `/responses` 与 `/messages` 路由。
async fn configured_model_catalog(config: &Config) -> Vec<(String, bool)> {
    let (key_config, settings) = tokio::join!(json_file(&config.key_path, "OhMyAgent key"), json_file(&config.settings_path, "OhMyAgent settings"));
    let (Ok(key_config), Ok(settings)) = (key_config, settings) else { return Vec::new(); };
    let base_url = config.upstream_host.clone().or_else(|| key_config.get("base_url").and_then(Value::as_str).map(str::to_string)).unwrap_or_default().trim_end_matches('/').to_string();
    let mut catalog: Vec<(String, bool)> = Vec::new();
    for entry in models_for_upstream(&settings, &base_url) {
        let Some(id) = entry.get("model").and_then(Value::as_str) else { continue; };
        let is_anthropic = anthropic::is_anthropic_type(entry.get("type").and_then(Value::as_str));
        let short = id.replacen("monkeycode-basic/", "", 1).replacen("monkeycode-pro/", "", 1).replacen("monkeycode-ultra/", "", 1);
        // 完整 id 与短别名都登记；重名时以先出现的为准，保证顺序稳定。
        if !catalog.iter().any(|(existing, _)| existing == id) { catalog.push((id.to_string(), is_anthropic)); }
        if short != id && !catalog.iter().any(|(existing, _)| existing == &short) { catalog.push((short, is_anthropic)); }
    }
    catalog
}

async fn request_upstream(state: &AppState, outgoing: &Value, runtime: &Runtime) -> Result<reqwest::Response, GatewayError> {
    let prompt = developer_prompt(outgoing);
    let mut signer = HmacSha256::new_from_slice(runtime.signing_secret.as_bytes()).map_err(|_| GatewayError::config("invalid signing_secret"))?;
    signer.update(prompt.as_bytes());
    let signature = hex::encode(signer.finalize().into_bytes());
    let path = if runtime.anthropic { "messages" } else { "responses" };
    let mut request = state.client.post(format!("{}/{path}", runtime.base_url))
        .header(header::AUTHORIZATION, format!("Bearer {}", runtime.api_key))
        .header("X-OhMyAgent-Signature", format!("v1={signature}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, if outgoing.get("stream").and_then(Value::as_bool).unwrap_or(false) { "text/event-stream" } else { "application/json" });
    if runtime.anthropic {
        request = request.header("anthropic-version", anthropic::anthropic_version());
    }
    request.json(outgoing).send().await.map_err(|e| GatewayError::new(StatusCode::BAD_GATEWAY, format!("upstream request failed: {e}")).with_type("api_error").with_code("upstream_unavailable"))
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

pub(crate) fn normalized_usage(usage: Option<&Value>) -> Option<Value> {
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

pub(crate) async fn send_sse(tx: &mpsc::Sender<Result<Bytes, Infallible>>, event: Option<&str>, data: &str) -> bool {
    let mut output = String::new();
    if let Some(event) = event { output.push_str("event: "); output.push_str(event); output.push('\n'); }
    output.push_str("data: "); output.push_str(data); output.push_str("\n\n");
    tx.send(Ok(Bytes::from(output))).await.is_ok()
}

pub(crate) fn sse_block(block: &str) -> (String, String) {
    let mut event = String::new(); let mut data = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") { event = value.trim().to_string(); }
        else if let Some(value) = line.strip_prefix("data:") { data.push(value.strip_prefix(' ').unwrap_or(value).trim_end_matches('\r').to_string()); }
    }
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
        let Ok(bytes) = result else { break; }; buffer.push_str(&String::from_utf8_lossy(&bytes)); buffer = buffer.replace("\r\n", "\n");
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
            buffer.push_str(&String::from_utf8_lossy(&bytes)); buffer = buffer.replace("\r\n", "\n");
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

pub(crate) fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }

async fn handle_chat(request: Request<Body>, state: AppState, key_id: String) -> Response {
    let started = std::time::Instant::now(); let cors = cors_headers(&request, &state.config);
    let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, "unknown", "chat.completions", e.status, started, None).await; return e.into_response(); } };
    let requested_model = body.get("model").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("gpt-6-astra").to_string();
    let runtime = match load_runtime(&state.config, &requested_model).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } };
    let outgoing = if runtime.anthropic {
        anthropic::to_anthropic_request(&body, &runtime.model, runtime.max_output)
    } else {
        match normalize_chat_request(&body, &runtime.model) { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } }
    };
    let upstream = match request_upstream(&state, &outgoing, &runtime).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", e.status, started, None).await; return e.into_response(); } };
    if !upstream.status().is_success() { let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY); let error = upstream_error(upstream).await; record_usage(&state, &key_id, &requested_model, "chat.completions", status, started, None).await; return error.into_response(); }
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let (tx, rx) = mpsc::channel(16); let response = sse_response(cors, rx);
        if runtime.anthropic {
            let model = requested_model.clone();
            tokio::spawn(async move {
                if let Some(completed) = anthropic::stream_as_chat(upstream, tx, model.clone()).await {
                    record_usage(&state, &key_id, &model, "chat.completions", StatusCode::OK, started, completed.get("usage")).await;
                }
            });
        } else {
            tokio::spawn(stream_chat(upstream, tx, requested_model, state, key_id, started));
        }
        return response;
    }
    let data = match upstream.json::<Value>().await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::BAD_GATEWAY, started, None).await; return GatewayError::new(StatusCode::BAD_GATEWAY, format!("invalid upstream response: {e}")).with_type("api_error").with_code("invalid_upstream_response").into_response(); } };
    if runtime.anthropic {
        let chat = anthropic::to_chat_json(&data, &requested_model);
        record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::OK, started, chat.get("usage")).await;
        return axum::Json(chat).into_response();
    }
    record_usage(&state, &key_id, &requested_model, "chat.completions", StatusCode::OK, started, data.get("usage")).await;
    let response_id = data.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4()));
    axum::Json(json!({"id": response_id, "object": "chat.completion", "created": now(), "model": requested_model, "choices": [{"index": 0, "message": {"role": "assistant", "content": response_output_text(&data)}, "finish_reason": response_finish_reason(Some(&data))}], "usage": normalized_usage(data.get("usage"))})).into_response()
}

async fn handle_responses(request: Request<Body>, state: AppState, key_id: String) -> Response {
    let started = std::time::Instant::now(); let cors = cors_headers(&request, &state.config);
    let body = match read_json(request, state.config.max_body_bytes).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, "unknown", "responses", e.status, started, None).await; return e.into_response(); } };
    let requested_model = body.get("model").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("gpt-6-astra").to_string();
    let runtime = match load_runtime(&state.config, &requested_model).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", e.status, started, None).await; return e.into_response(); } };
    let outgoing = if runtime.anthropic {
        anthropic::to_anthropic_request(&body, &runtime.model, runtime.max_output)
    } else {
        normalize_responses_request(&body, &runtime.model, &developer_prompt(&body))
    };
    let upstream = match request_upstream(&state, &outgoing, &runtime).await { Ok(v) => v, Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", e.status, started, None).await; return e.into_response(); } };
    if !upstream.status().is_success() { let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY); let error = upstream_error(upstream).await; record_usage(&state, &key_id, &requested_model, "responses", status, started, None).await; return error.into_response(); }
    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        let (tx, rx) = mpsc::channel(16); let response = sse_response(cors, rx);
        if runtime.anthropic {
            let model = requested_model.clone();
            tokio::spawn(async move {
                if let Some(completed) = anthropic::stream_as_responses(upstream, tx, model.clone()).await {
                    record_usage(&state, &key_id, &model, "responses", StatusCode::OK, started, completed.get("usage")).await;
                }
            });
        } else {
            tokio::spawn(stream_responses(upstream, tx, state, key_id, requested_model, started));
        }
        return response;
    }
    match upstream.json::<Value>().await {
        Ok(data) => {
            let payload = if runtime.anthropic { anthropic::to_responses_json(&data, &requested_model) } else { data };
            record_usage(&state, &key_id, &requested_model, "responses", StatusCode::OK, started, payload.get("usage")).await;
            axum::Json(payload).into_response()
        },
        Err(e) => { record_usage(&state, &key_id, &requested_model, "responses", StatusCode::BAD_GATEWAY, started, None).await; GatewayError::new(StatusCode::BAD_GATEWAY, format!("invalid upstream response: {e}")).with_type("api_error").with_code("invalid_upstream_response").into_response() }
    }
}

/// 版本检查 / 更新：`GET /v1/admin/update` 查看，`POST /v1/admin/update {"tag":"latest"|"v0.1.20"}` 安装。
async fn handle_admin_update(request: Request<Body>, state: AppState, apply: bool) -> Response {
    if !admin_authorized(&request, &state) {
        return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key"));
    }
    if !apply {
        let payload = update::status(&state.client).await;
        return json_response(&request, &state.config, StatusCode::OK, payload);
    }
    let body = match read_json(request, state.config.max_body_bytes).await {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let tag = body.get("tag").and_then(Value::as_str).unwrap_or("latest");
    match update::apply(&state.client, tag).await {
        Ok(value) => json_response(&Request::new(Body::empty()), &state.config, StatusCode::OK, value),
        Err(error) => GatewayError::new(StatusCode::BAD_GATEWAY, format!("{error}")).with_type("api_error").into_response(),
    }
}

/// 看板主统计：`GET /v1/admin/stats?window=<秒>`。
///
/// `window=0` 表示全部时间；旧的无参数形式等价于 24 小时。
async fn handle_admin_stats(request: Request<Body>, state: AppState) -> Response {
    if !admin_authorized(&request, &state) {
        return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key"));
    }
    let params = query_params(request.uri());
    let window = params.get("window").and_then(|value| value.parse::<u64>().ok()).unwrap_or(86_400);
    let base_url = clients::local_base_url(&state.config.host, state.config.port, state.config.tls_cert.is_some());
    let summary = {
        // 统计与 Key 列表在同一个锁粒度下读取，避免两次加锁之间 Key 被撤销导致名称对不上。
        let usage = state.usage.lock().await;
        let keys = state.api_keys.lock().await;
        dashboard::stats(&usage, &keys, &state.config, state.started_at, window, &base_url)
    };
    json_response(&request, &state.config, StatusCode::OK, summary)
}

/// 用量统计（旧接口，保持兼容）：等价于 `GET /v1/admin/stats`。
async fn handle_admin_usage(request: Request<Body>, state: AppState) -> Response {
    handle_admin_stats(request, state).await
}

/// 调用日志：`GET /v1/admin/logs?window=&keyword=&key_id=&endpoint=&status=&limit=&offset=`。
async fn handle_admin_logs(request: Request<Body>, state: AppState) -> Response {
    if !admin_authorized(&request, &state) {
        return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key"));
    }
    let query = dashboard::LogQuery::from_params(&query_params(request.uri()));
    let (items, total) = {
        let usage = state.usage.lock().await;
        dashboard::logs(&usage, &query)
    };
    json_response(
        &request,
        &state.config,
        StatusCode::OK,
        json!({
            "object": "list",
            "data": items,
            "total": total,
            "limit": query.limit,
            "offset": query.offset,
            "window_seconds": query.window_seconds,
        }),
    )
}

/// 模型目录：`GET /v1/admin/models`。除 id 外带上上游协议类型，便于看板与客户端配置对照。
async fn handle_admin_models(request: Request<Body>, state: AppState) -> Response {
    if !admin_authorized(&request, &state) {
        return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key"));
    }
    let catalog = configured_model_catalog(&state.config).await;
    json_response(&request, &state.config, StatusCode::OK, dashboard::models(&catalog))
}

async fn listener(state: AppState, request: Request<Body>) -> Response {
    let remote = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0).unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let path = request.uri().path().trim_end_matches('/').to_string(); let path = if path.is_empty() { "/".to_string() } else { path };
    if !ip_allowed(&request, &state.config, remote) { return error_response(&request, &state.config, GatewayError::new(StatusCode::FORBIDDEN, "client IP is not allowed").with_type("permission_error").with_code("ip_not_allowed")); }
    if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if !origin_allowed(&state.config, origin) { return error_response(&request, &state.config, GatewayError::new(StatusCode::FORBIDDEN, "request origin is not allowed").with_type("permission_error").with_code("origin_not_allowed")); }
    }
    if request.method() == axum::http::Method::OPTIONS { let mut response = StatusCode::NO_CONTENT.into_response(); response.headers_mut().extend(cors_headers(&request, &state.config)); response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS")); response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Authorization, Content-Type")); response.headers_mut().insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("86400")); return response; }
    if request.method() == axum::http::Method::GET && (path == "/" || path == "/v1") { return json_response(&request, &state.config, StatusCode::OK, json!({"object": "gateway", "name": "monkeycode-direct-gateway", "status": "ok", "endpoints": ["/health", "/v1/models", "/v1/responses", "/v1/chat/completions", "/v1/admin/clients"]})); }
    if request.method() == axum::http::Method::GET && (path == "/health" || path == "/v1/health") { return json_response(&request, &state.config, StatusCode::OK, json!({"ok": true, "mode": "direct-signed-gateway", "auth_required": state.config.auth_required, "tls": state.config.tls_cert.is_some()})); }
    if request.method() == axum::http::Method::GET && path == "/admin" { return dashboard::page(); }
    if request.method() == axum::http::Method::GET && (path == "/dashboard" || path == "/ui") { return dashboard::page(); }
    if request.method() == axum::http::Method::GET && (path == "/favicon.ico" || path == "/favicon.svg") { return favicon(); }
    if path == "/v1/admin/update" {
        if request.method() == axum::http::Method::GET { return handle_admin_update(request, state, false).await; }
        if request.method() == axum::http::Method::POST { return handle_admin_update(request, state, true).await; }
    }
    if path == "/v1/admin/stats" && request.method() == axum::http::Method::GET { return handle_admin_stats(request, state).await; }
    if path == "/v1/admin/models" && request.method() == axum::http::Method::GET { return handle_admin_models(request, state).await; }
    if path == "/v1/admin/logs" && request.method() == axum::http::Method::GET { return handle_admin_logs(request, state).await; }
    if path == "/v1/admin/usage" && request.method() == axum::http::Method::GET { return handle_admin_usage(request, state).await; }
    if (path == "/v1/admin/clients" || path == "/v1/admin/clients/sync") && matches!(request.method(), &axum::http::Method::GET | &axum::http::Method::POST) { return handle_admin_clients(request, state).await; }
    if path == "/v1/admin/keys" {
        if request.method() == axum::http::Method::GET { return handle_admin_keys(request, state, None, "list").await; }
        if request.method() == axum::http::Method::POST { return handle_admin_keys(request, state, None, "create").await; }
    }
    if let Some(key_id) = path.strip_prefix("/v1/admin/keys/") {
        if let Some(id) = key_id.strip_suffix("/rotate") {
            if request.method() == axum::http::Method::POST { return handle_admin_keys(request, state, Some(id), "rotate").await; }
        }
        if request.method() == axum::http::Method::GET { return handle_admin_keys(request, state, Some(key_id), "get").await; }
        if request.method() == axum::http::Method::DELETE { return handle_admin_keys(request, state, Some(key_id), "revoke").await; }
        if request.method() == axum::http::Method::PATCH { return handle_admin_keys(request, state, Some(key_id), "edit").await; }
    }
    let token = bearer_token(&request).map(str::to_owned);
    let Some(key_id) = authenticated_key_id(token, &state).await else { return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid API key").with_type("authentication_error").with_code("invalid_api_key")); };
    if request.method() == axum::http::Method::GET && (path == "/models" || path == "/v1/models") {
        return match configured_model_ids(&state.config).await {
            Ok(ids) => json_response(&request, &state.config, StatusCode::OK, json!({"object": "list", "data": ids.into_iter().map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "monkeycode"})).collect::<Vec<_>>() })),
            Err(e) => error_response(&request, &state.config, e),
        };
    }
    if request.method() == axum::http::Method::POST && (path == "/responses" || path == "/v1/responses") { return handle_responses(request, state, key_id).await; }
    if request.method() == axum::http::Method::POST && (path == "/chat/completions" || path == "/v1/chat/completions") { return handle_chat(request, state, key_id).await; }
    error_response(&request, &state.config, GatewayError::new(StatusCode::NOT_FOUND, format!("unknown endpoint: {path}")).with_code("not_found"))
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "serve" => return run_server(&args[1..]).await,
        "start" | "stop" | "restart" | "status" | "tui" | "install" | "setup" | "clients" | "dashboard" | "web" | "help" | "-h" | "--help" => return cli::run(&args).await,
        "" => return cli::run(&[]).await,
        _ => {}
    }
    run_server(&args).await
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

fn mk2api_home(config: &Config) -> PathBuf {
    config.api_keys_path.parent().map(Path::to_path_buf).unwrap_or_else(|| user_home().join(".mk2api"))
}

async fn ensure_named_key(state: &AppState, name: &str, preferred: Option<String>) -> Result<clients::IssuedKey, BoxError> {
    let preferred_raw = preferred.filter(|value| !value.is_empty());
    let (issued, snapshot) = {
        let mut store = state.api_keys.lock().await;
        if let Some(raw) = preferred_raw.clone() {
            if let Some(existing) = store.keys.iter_mut().find(|key| !key.revoked && key.key_hash == hash_key(&raw)) {
                let issued = clients::IssuedKey { id: existing.id.clone(), raw: raw.clone() };
                let snapshot = if existing.secret.as_ref().map(|value| value.is_empty()).unwrap_or(true) {
                    existing.secret = Some(raw);
                    Some(store.serialized().map(|contents| (store.path.clone(), contents))?)
                } else {
                    None
                };
                (issued, snapshot)
            } else {
                let record = ApiKeyRecord {
                    id: format!("key_{}", Uuid::new_v4().simple()),
                    name: name.to_string(),
                    key_hash: hash_key(&raw),
                    secret: Some(raw.clone()),
                    created_at: now(),
                    revoked: false,
                    note: "client-managed".into(),
                };
                let issued = clients::IssuedKey { id: record.id.clone(), raw };
                store.keys.push(record);
                (issued, Some(store.serialized().map(|contents| (store.path.clone(), contents))?))
            }
        } else {
            let raw = new_api_key();
            let record = ApiKeyRecord {
                id: format!("key_{}", Uuid::new_v4().simple()),
                name: name.to_string(),
                key_hash: hash_key(&raw),
                secret: Some(raw.clone()),
                created_at: now(),
                revoked: false,
                note: "client-managed".into(),
            };
            let issued = clients::IssuedKey { id: record.id.clone(), raw };
            store.keys.push(record);
            (issued, Some(store.serialized().map(|contents| (store.path.clone(), contents))?))
        }
    };
    if let Some((path, contents)) = snapshot {
        write_private(&path, &contents).await?;
    }
    Ok(issued)
}

async fn sync_managed_clients(state: &AppState) -> Result<Vec<clients::ClientReport>, BoxError> {
    let paths = clients::ClientPaths::new(user_home(), mk2api_home(&state.config));
    if !state.config.manage_clients {
        return Ok(vec![
            clients::skipped("pi", paths.pi_models, "disabled"),
            clients::skipped("codex", paths.codex_config, "disabled"),
        ]);
    }
    let ids = match configured_model_ids(&state.config).await {
        Ok(ids) => ids,
        Err(error) => {
            eprintln!("client sync catalog unavailable: {}", error.message);
            Vec::new()
        }
    };
    let mut store = clients::load_store(&paths.store_path);
    let base_url = clients::local_base_url(&state.config.host, state.config.port, state.config.tls_cert.is_some());
    let mut reports = Vec::new();
    if state.config.manage_pi {
        let preferred = store.pi.as_ref().map(|key| key.raw.clone()).or_else(|| clients::extract_pi_key(&clients::read_json_or_empty(&paths.pi_models)));
        let key = ensure_named_key(state, "pi", preferred).await?;
        store.pi = Some(key.clone());
        reports.push(clients::apply_pi(&paths, &base_url, &key.raw, &ids)?);
    } else {
        reports.push(clients::skipped("pi", paths.pi_models.clone(), "disabled"));
    }
    if state.config.manage_codex {
        let preferred = store.codex.as_ref().map(|key| key.raw.clone()).or_else(|| std::fs::read_to_string(&paths.codex_config).ok().and_then(|text| clients::extract_codex_key(&text)));
        let key = ensure_named_key(state, "codex", preferred).await?;
        store.codex = Some(key.clone());
        reports.push(clients::apply_codex(&paths, &base_url, &key.raw, &ids)?);
    } else {
        reports.push(clients::skipped("codex", paths.codex_config.clone(), "disabled"));
    }
    clients::write_pretty_json(&paths.store_path, &clients::store_value(&store))?;
    Ok(reports)
}

async fn client_sync_loop(state: AppState) {
    let mut last = String::new();
    loop {
        match sync_managed_clients(&state).await {
            Ok(reports) => {
                let summary = reports.iter().map(|report| format!("{}:{}:{}", report.name, report.managed, report.models)).collect::<Vec<_>>().join(",");
                if summary != last {
                    for report in &reports {
                        if report.managed {
                            println!("managed {} ({} models) -> {}", report.name, report.models, report.path.as_deref().unwrap_or("-"));
                        } else {
                            println!("{}: {}", report.name, report.message);
                        }
                    }
                    last = summary;
                }
            }
            Err(error) => eprintln!("client sync failed: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn handle_admin_clients(request: Request<Body>, state: AppState) -> Response {
    if !admin_authorized(&request, &state) {
        return error_response(&request, &state.config, GatewayError::new(StatusCode::UNAUTHORIZED, "invalid admin API key").with_type("authentication_error").with_code("invalid_admin_key"));
    }
    let reports = match sync_managed_clients(&state).await {
        Ok(reports) => reports,
        Err(error) => return error_response(&request, &state.config, GatewayError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error}")).with_type("api_error")),
    };
    let ids = configured_model_ids(&state.config).await.unwrap_or_else(|_| Vec::new());
    let base_url = clients::local_base_url(&state.config.host, state.config.port, state.config.tls_cert.is_some());
    json_response(&request, &state.config, StatusCode::OK, clients::report_json(&reports, state.config.manage_clients, &base_url, ids.len()))
}

async fn run_server(args: &[String]) -> Result<(), BoxError> {
    let config = Arc::new(Config::load(args).await?);
    let client = reqwest::Client::builder().timeout(config.request_timeout).build()?;
    let (admin_key, generated_admin_key) = load_admin_key(&config).await?;
    if generated_admin_key {
        println!("generated Admin Key (save it securely): {admin_key}");
    }
    let api_keys = ApiKeyStore::load(config.api_keys_path.clone()).await?;
    let usage = UsageStore::load(config.usage_path.clone(), config.max_usage_records).await?;
    let started_at = now();
    let state = AppState { config: config.clone(), client, admin_key: Arc::new(admin_key), api_keys: Arc::new(Mutex::new(api_keys)), usage: Arc::new(Mutex::new(usage)), started_at };
    if config.manage_clients {
        tokio::spawn(client_sync_loop(state.clone()));
    }
    let state_for_fallback = state.clone();
    let fallback = tower::service_fn(move |request: Request<Body>| {
        let state = state_for_fallback.clone();
        async move { Ok::<Response, Infallible>(listener(state, request).await) }
    });
    let app = Router::new().fallback_service(fallback);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_history_uses_output_text_for_assistant() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"}
            ]
        });
        let outgoing = normalize_chat_request(&body, "monkeycode-ultra/gpt-test").unwrap();
        assert_eq!(outgoing["input"][0]["role"], "developer");
        assert_eq!(outgoing["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(outgoing["input"][2]["role"], "assistant");
        assert_eq!(outgoing["input"][2]["content"], json!([{"type": "output_text", "text": "hello"}]));
    }

    #[test]
    fn responses_history_rewrites_assistant_input_text() {
        let body = json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "input_text", "text": "hello"}]},
                {"role": "user", "content": [{"type": "input_text", "text": "again"}]}
            ]
        });
        let outgoing = normalize_responses_request(&body, "monkeycode-ultra/gpt-test", "You are a helpful assistant.");
        assert_eq!(outgoing["input"][0]["role"], "developer");
        assert_eq!(outgoing["input"][2]["role"], "assistant");
        assert_eq!(outgoing["input"][2]["content"], json!([{"type": "output_text", "text": "hello"}]));
    }

    #[test]
    fn responses_drop_reasoning_effort_none() {
        let body = json!({
            "model": "monkeycode-basic/qwen3.8-flash",
            "input": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "none"},
            "reasoning_effort": "none"
        });
        let outgoing = normalize_responses_request(&body, "monkeycode-basic/qwen3.8-flash", "You are a helpful assistant.");
        assert!(outgoing.get("reasoning").is_none());
        assert!(outgoing.get("reasoning_effort").is_none());
    }

    #[test]
    fn sse_block_strips_crlf() {
        let (event, data) = sse_block("event: response.completed\r\ndata: {\"ok\":true}\r");
        assert_eq!(event, "response.completed");
        assert_eq!(data, "{\"ok\":true}");
    }

    #[test]
    fn resolve_model_prefers_monkeycode_prefix() {
        let models = vec![
            json!({"model": "qwen3.8-flash", "base_url": "https://other.example/v1"}),
            json!({"model": "monkeycode-basic/qwen3.8-flash", "base_url": "https://proxy.monkeycode-ai.com/v1"}),
        ];
        let found = resolve_model(&models, "qwen3.8-flash").unwrap();
        assert_eq!(found["model"], "monkeycode-basic/qwen3.8-flash");
    }

    #[test]
    fn resolve_model_prefers_basic_over_ultra() {
        let models = vec![
            json!({"model": "monkeycode-ultra/deepseek-v4-flash"}),
            json!({"model": "monkeycode-basic/deepseek-v4-flash"}),
        ];
        let found = resolve_model(&models, "deepseek-v4-flash").unwrap();
        assert_eq!(found["model"], "monkeycode-basic/deepseek-v4-flash");
    }
}
