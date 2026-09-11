//! Web 控制台：内嵌前端页面 + 看板数据聚合。
//!
//! 中文说明：这里把「用量看板」拆成三块——
//! 1. `page()`：返回内嵌的单文件前端（`include_str!` 打包进二进制，发布产物仍然是单个可执行文件）；
//! 2. `stats()`：把用量记录聚合成 KPI、时间序列、多维度排行榜，并附带服务运行时信息；
//! 3. `logs()`：服务端日志过滤 + 分页，避免把几万条记录一次性丢给浏览器。
//!
//! 之所以把过滤放在服务端：用量文件默认上限 10 万条，前端全量拉取会明显拖慢首屏。

use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::{ApiKeyStore, Config, UsageStore};

/// 前端页面。路径相对于本文件，因此是 `src/dashboard/index.html`。
const PAGE: &str = include_str!("dashboard/index.html");

/// 单次日志查询最多返回的条数，防止前端传 `limit=100000` 拖垮响应。
const MAX_LOG_LIMIT: usize = 500;
/// 时间窗上限（约 366 天），避免离奇的 `window` 参数导致时间序列分配过大。
const MAX_WINDOW_SECONDS: u64 = 366 * 86_400;

/// 返回控制台页面。
pub fn page() -> Response {
    let mut response = Response::new(axum::body::Body::from(PAGE));
    *response.status_mut() = axum::http::StatusCode::OK;
    response
        .headers_mut()
        .insert(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/html; charset=utf-8"));
    // 页面内嵌在二进制里，但管理台数据是实时的：一律不走缓存，避免旧页面缓存住。
    response
        .headers_mut()
        .insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    response
}

/// 日志查询参数。
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub window_seconds: u64,
    pub keyword: String,
    pub key_id: String,
    pub endpoint: String,
    pub status: String,
    pub limit: usize,
    pub offset: usize,
}

impl LogQuery {
    /// 从 URL 查询串构造。所有字段都是可选的，缺省即「不过滤」。
    pub fn from_params(params: &HashMap<String, String>) -> Self {
        let window_seconds = params
            .get("window")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(86_400)
            .min(MAX_WINDOW_SECONDS);
        let limit = params
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, MAX_LOG_LIMIT);
        let offset = params.get("offset").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
        Self {
            window_seconds,
            keyword: params.get("keyword").cloned().unwrap_or_default().trim().to_lowercase(),
            key_id: params.get("key_id").cloned().unwrap_or_default(),
            endpoint: params.get("endpoint").cloned().unwrap_or_default(),
            status: params.get("status").cloned().unwrap_or_default().trim().to_lowercase(),
            limit,
            offset,
        }
    }

    /// 单条记录是否命中当前筛选条件。
    fn matches(&self, record: &crate::UsageRecord) -> bool {
        if !self.keyword.is_empty() {
            let model = record.model.to_lowercase();
            let endpoint = record.endpoint.to_lowercase();
            if !model.contains(&self.keyword) && !endpoint.contains(&self.keyword) {
                return false;
            }
        }
        if !self.key_id.is_empty() && record.key_id != self.key_id {
            return false;
        }
        if !self.endpoint.is_empty() && record.endpoint != self.endpoint {
            return false;
        }
        if !self.status.is_empty() {
            let actual = record.status.to_string();
            // 支持两种写法：精确「200」和前缀通配「4xx」/「5」。
            let hit = if let Some(prefix) = self.status.strip_suffix("xx") {
                actual.starts_with(prefix)
            } else {
                actual == self.status
            };
            if !hit {
                return false;
            }
        }
        true
    }
}

/// 记录转成前端使用的 JSON 结构。
fn record_json(record: &crate::UsageRecord) -> Value {
    json!({
        "timestamp": record.timestamp,
        "key_id": record.key_id,
        "model": record.model,
        "endpoint": record.endpoint,
        "status": record.status,
        "latency_ms": record.latency_ms,
        "input_tokens": record.input_tokens,
        "output_tokens": record.output_tokens,
    })
}

/// 日志过滤 + 分页。
///
/// 返回 `(当前页记录, 命中总数)`；记录按时间倒序（最新在前）。
pub fn logs(store: &UsageStore, query: &LogQuery) -> (Vec<Value>, usize) {
    let now_ts = crate::now();
    let since = store.window_start(query.window_seconds, now_ts);
    let matched: Vec<&crate::UsageRecord> = store
        .records
        .iter()
        .rev()
        .filter(|record| record.timestamp >= since)
        .filter(|record| query.matches(record))
        .collect();
    let total = matched.len();
    let page = matched
        .into_iter()
        .skip(query.offset)
        .take(query.limit)
        .map(record_json)
        .collect();
    (page, total)
}

/// 模型目录（含上游协议类型），用于「模型」页做交叉标注。
pub fn models(catalog: &[(String, bool)]) -> Value {
    json!({
        "object": "list",
        "data": catalog.iter().map(|(id, is_anthropic)| json!({
            "id": id,
            "object": "model",
            "anthropic": is_anthropic,
        })).collect::<Vec<_>>()
    })
}

/// 看板主统计。
///
/// 中文说明：在 `UsageStore::stats()` 的基础上补齐两类信息——
/// 1. 服务运行时信息（监听地址、鉴权开关、容量水位等），供「系统信息」页展示；
/// 2. API Key 的元数据（名称 / 备注 / 创建时间 / 状态），让前端可以把 `by_key` 里的
///    `key_id` 直接映射成人类可读的名称，而不需要额外再请求一次。
///
/// `window_seconds == 0` 表示「全部时间」：此时用最早一条记录来推断实际跨度，
/// 否则时间序列只会剩下一个点，图表就没有意义了。
pub fn stats(store: &UsageStore, keys: &ApiKeyStore, config: &Config, started_at: u64, window_seconds: u64, base_url: &str) -> Value {
    let now_ts = crate::now();
    let requested = window_seconds.min(MAX_WINDOW_SECONDS);
    let span = if requested == 0 {
        let oldest = store.records.first().map(|record| record.timestamp).unwrap_or(now_ts).min(now_ts);
        now_ts.saturating_sub(oldest).max(3_600).min(MAX_WINDOW_SECONDS)
    } else {
        requested
    };

    let mut value = store.stats(span, now_ts);
    let key_meta: Vec<Value> = keys
        .keys
        .iter()
        .map(|key| {
            json!({
                "id": key.id,
                "name": key.name,
                "note": key.note,
                "created_at": key.created_at,
                "revoked": key.revoked,
            })
        })
        .collect();

    if let Some(map) = value.as_object_mut() {
        map.insert("window_seconds".into(), json!(requested));
        map.insert("span_seconds".into(), json!(span));
        map.insert("keys".into(), Value::Array(key_meta));
        map.insert(
            "system".into(),
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "direct-signed-gateway",
                "listen": format!("{}:{}", config.host, config.port),
                "base_url": base_url,
                "auth_required": config.auth_required,
                "tls": config.tls_cert.is_some(),
                "trust_proxy": config.trust_proxy,
                "allowed_ips": config.allowed_ips,
                "manage_clients": config.manage_clients,
                "started_at": started_at,
                "usage_records": store.records.len(),
                "max_usage_records": store.max_records,
                "config_dir": config.api_keys_path.parent().map(|path| path.display().to_string()).unwrap_or_default(),
                "admin_key_file": config.admin_key_path.display().to_string(),
            }),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UsageRecord;

    fn record(timestamp: u64, key: &str, model: &str, status: u16) -> UsageRecord {
        UsageRecord {
            timestamp,
            key_id: key.to_string(),
            model: model.to_string(),
            endpoint: "chat.completions".to_string(),
            status,
            latency_ms: 100,
            input_tokens: 10,
            output_tokens: 5,
        }
    }

    fn store_with(records: Vec<UsageRecord>) -> UsageStore {
        UsageStore {
            path: std::env::temp_dir().join("mk2api-dashboard-test-usage.json"),
            max_records: 100,
            records,
        }
    }

    #[test]
    fn log_query_parses_defaults_and_clamps_limit() {
        let query = LogQuery::from_params(&HashMap::new());
        assert_eq!(query.window_seconds, 86_400);
        assert_eq!(query.limit, 50);

        let mut params = HashMap::new();
        params.insert("limit".to_string(), "99999".to_string());
        params.insert("window".to_string(), "0".to_string());
        params.insert("keyword".to_string(), "  DeepSeek ".to_string());
        let query = LogQuery::from_params(&params);
        assert_eq!(query.limit, MAX_LOG_LIMIT);
        assert_eq!(query.window_seconds, 0);
        assert_eq!(query.keyword, "deepseek");
    }

    #[test]
    fn logs_filter_by_keyword_status_and_paginate() {
        let now_ts = crate::now();
        let store = store_with(vec![
            record(now_ts - 30, "key_a", "monkeycode-basic/deepseek-v4-flash", 200),
            record(now_ts - 20, "key_a", "monkeycode-basic/gpt-5.2-codex", 500),
            record(now_ts - 10, "key_b", "monkeycode-basic/gpt-5.2-codex", 401),
        ]);

        let mut params = HashMap::new();
        params.insert("window".to_string(), "3600".to_string());
        params.insert("status".to_string(), "4xx".to_string());
        let query = LogQuery::from_params(&params);
        let (items, total) = logs(&store, &query);
        assert_eq!(total, 1);
        assert_eq!(items[0]["key_id"], "key_b");

        let mut params = HashMap::new();
        params.insert("window".to_string(), "3600".to_string());
        params.insert("keyword".to_string(), "codex".to_string());
        let query = LogQuery::from_params(&params);
        let (items, total) = logs(&store, &query);
        assert_eq!(total, 2);
        // 倒序：最新的在最前面
        assert_eq!(items[0]["status"], 401);

        let mut params = HashMap::new();
        params.insert("window".to_string(), "3600".to_string());
        params.insert("limit".to_string(), "1".to_string());
        params.insert("offset".to_string(), "1".to_string());
        let query = LogQuery::from_params(&params);
        let (items, total) = logs(&store, &query);
        assert_eq!(total, 3);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["status"], 500);
    }

    #[test]
    fn stats_reports_windows_and_key_metadata() {
        let now_ts = crate::now();
        let store = store_with(vec![
            record(now_ts - 10, "key_a", "m1", 200),
            record(now_ts - 5, "key_a", "m1", 500),
        ]);
        let mut keys = ApiKeyStore { path: std::env::temp_dir().join("mk2api-dashboard-test-keys.json"), keys: Vec::new() };
        keys.keys.push(crate::ApiKeyRecord {
            id: "key_a".into(),
            name: "pi".into(),
            key_hash: "hash".into(),
            secret: Some("mk_live_test".into()),
            created_at: now_ts - 100,
            revoked: false,
            note: "本地测试".into(),
        });
        let config = Config {
            host: "0.0.0.0".into(),
            port: 8123,
            local_key: String::new(),
            auth_required: true,
            trust_proxy: false,
            allowed_origins: Vec::new(),
            allowed_ips: Vec::new(),
            tls_cert: None,
            tls_key: None,
            key_path: std::env::temp_dir().join("mk2api-test-key.json"),
            settings_path: std::env::temp_dir().join("mk2api-test-settings.json"),
            api_keys_path: std::env::temp_dir().join("mk2api-test-api-keys.json"),
            usage_path: std::env::temp_dir().join("mk2api-test-usage.json"),
            admin_key_path: std::env::temp_dir().join("mk2api-test-admin.key"),
            admin_key: None,
            max_body_bytes: 1024,
            max_usage_records: 100,
            request_timeout: std::time::Duration::from_secs(1),
            upstream_host: None,
            upstream_key: None,
            signing_secret: None,
            manage_clients: true,
            manage_pi: true,
            manage_codex: true,
        };

        let value = stats(&store, &keys, &config, now_ts - 50, 86_400, "http://127.0.0.1:8123/v1");
        assert_eq!(value["requests"], 2);
        assert_eq!(value["errors"], 1);
        assert_eq!(value["window_seconds"], 86_400);
        assert_eq!(value["keys"][0]["name"], "pi");
        assert_eq!(value["system"]["listen"], "0.0.0.0:8123");
        assert_eq!(value["series"].as_array().unwrap().len(), 24);
    }

    #[test]
    fn stats_all_time_window_uses_record_span() {
        let now_ts = crate::now();
        // 全部时间窗口下不能只返回一个数据点：跨度应由最早记录推断。
        let store = store_with(vec![record(now_ts - 7_200, "key_a", "m1", 200), record(now_ts, "key_a", "m1", 200)]);
        let keys = ApiKeyStore { path: std::env::temp_dir().join("mk2api-dashboard-test-keys.json"), keys: Vec::new() };
        let mut config = Config {
            host: "127.0.0.1".into(),
            port: 8123,
            local_key: String::new(),
            auth_required: true,
            trust_proxy: false,
            allowed_origins: Vec::new(),
            allowed_ips: Vec::new(),
            tls_cert: None,
            tls_key: None,
            key_path: std::env::temp_dir().join("mk2api-test-key.json"),
            settings_path: std::env::temp_dir().join("mk2api-test-settings.json"),
            api_keys_path: std::env::temp_dir().join("mk2api-test-api-keys.json"),
            usage_path: std::env::temp_dir().join("mk2api-test-usage.json"),
            admin_key_path: std::env::temp_dir().join("mk2api-test-admin.key"),
            admin_key: None,
            max_body_bytes: 1024,
            max_usage_records: 100,
            request_timeout: std::time::Duration::from_secs(1),
            upstream_host: None,
            upstream_key: None,
            signing_secret: None,
            manage_clients: false,
            manage_pi: false,
            manage_codex: false,
        };
        config.auth_required = true;
        let value = stats(&store, &keys, &config, now_ts, 0, "http://127.0.0.1:8123/v1");
        assert_eq!(value["window_seconds"], 0);
        let span = value["span_seconds"].as_u64().unwrap();
        assert!(span >= 7_200, "span should cover the oldest record, got {span}");
        assert!(value["series"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn models_expose_protocol_type() {
        let value = models(&[("monkeycode-basic/x".to_string(), false), ("deepseek-v4-flash".to_string(), true)]);
        assert_eq!(value["data"][0]["anthropic"], false);
        assert_eq!(value["data"][1]["anthropic"], true);
    }
}
