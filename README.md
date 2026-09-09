# MonkeyCode Direct Gateway

本项目的本地转发服务已使用 Rust 实现，源码入口为 `src/main.rs`，首次启动会通过 Cargo 构建 release 二进制。Node 脚本 `direct-gateway.mjs` 仍作为一次性请求客户端保留。

## 一键安装（macOS）

```bash
bash -c "$(curl -fsSL 'https://monkeycode-ai.com/online/install')"
```

也可以使用 GitHub Raw 安装入口：

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install?v=22af0d4')"
```

安装程序会自动识别 Apple Silicon/Intel，下载对应 GitHub Release，安装 launchd 管理命令并启动网关。

## Direct call

启动本地直连网关（需要 Rust/Cargo）：

```bash
./start-direct-gateway.sh start
```

查看状态和健康信息：

```bash
./start-direct-gateway.sh status
```

停止或重启：

```bash
./start-direct-gateway.sh stop
./start-direct-gateway.sh restart
```

默认监听 `0.0.0.0:8123`，日志写入
`~/Library/Logs/monkeycode-direct-gateway.log`。首次启动会自动生成 Admin Key，写入
`~/Library/Application Support/com.chaitin.baizhi.monkeycode/admin.key`，同时在首次启动日志中打印一次。

每次 `start` 都会根据当前配置重新生成 plist，并卸载、重新加载已有的
launchd 服务（会短暂中断正在进行的请求）。旧 plist 中的端口会被更新，
无需手动删除。脚本与直接运行 Node 服务的默认端口均为 8123。

自定义端口时，对 start、status、stop 使用相同的环境变量，例如：

```bash
DIRECT_GATEWAY_PORT=8124 ./start-direct-gateway.sh start
```

目标端口被其他进程占用时，启动会失败并打印监听进程信息，不会终止该进程。
启动健康检查失败时会打印诊断并卸载本次服务，避免 launchd 持续崩溃重启。
本地健康检查绕过 HTTP 代理，并检查返回的网关标识。

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only."
```

流式响应：

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only." --stream
```

The script reads the signed OhMyAgent key from MonkeyCode's application
configuration and sends requests to the configured `/v1/responses` endpoint.
The local gateway accepts OpenAI-compatible Responses and Chat Completions routes:

- `GET /v1` — gateway status and endpoint discovery
- `GET /health` or `GET /v1/health` — health check
- `GET /v1/models`
- `POST /v1/responses` — non-streaming JSON and native Responses SSE
- `POST /v1/chat/completions` — non-streaming JSON and OpenAI Chat SSE
- `GET /admin` — API Key 管理页面
- `GET /v1/admin/keys` — 列出客户端 Key（仅 Admin Key）
- `POST /v1/admin/keys` — 创建客户端 Key，明文只返回一次
- `POST /v1/admin/keys/{id}/rotate` — 轮换并立即撤销旧 Key
- `GET /v1/admin/usage` — 查看总用量、按 Key/模型聚合和最近请求

Chat requests are normalized to the upstream Responses format, including full
model names, developer/user messages, multimodal text/image content,
`max_output_tokens`, and common sampling/tool parameters. Streaming Chat
responses use `data: {...}` chunks, a final chunk with `finish_reason`, and
`data: [DONE]`. Responses streams preserve upstream event names and payloads.


- `Authorization: Bearer oma_...`
- `X-OhMyAgent-Signature: v1=<HMAC-SHA256 hex>`
- `input` containing a `developer` message followed by a `user` message
- the full internal model name, such as `monkeycode-ultra/gpt-6-astra`

The signature covers the developer prompt and uses the local
`signing_secret`; no credential is printed by the script.

也可以直接运行 Rust 服务：

```bash
cargo run --release -- --host 127.0.0.1 --port 8124 --key my-local-key
```

也可以创建 `~/Library/Application Support/com.chaitin.baizhi.monkeycode/direct-gateway.json`：

```json
{
  "host": "127.0.0.1",
  "port": 8124,
  "key": "my-legacy-client-key",
  "admin_key": "optional-admin-key",
  "api_keys_file": "/path/to/api-keys.json",
  "usage_file": "/path/to/usage.json",
  "admin_key_file": "/path/to/admin.key",
  "auth_required": true,
  "allowed_origins": ["https://client.example.com"],
  "allowed_ips": ["127.0.0.1", "10.0.0.8"],
  "trust_proxy": false,
  "upstream_host": "https://your-gateway.example.com/v1",
  "upstream_key": "your-api-key",
  "tls_cert": "/path/to/fullchain.pem",
  "tls_key": "/path/to/private-key.pem"
}
```

通过 `--config /path/to/direct-gateway.json` 或环境变量
`MONKEYCODE_GATEWAY_CONFIG` 指定配置文件。启动参数优先级高于环境变量，环境变量高于配置文件。

安全默认值是监听 `0.0.0.0` 且要求 Bearer key。普通客户端 Key 由 `/admin` 管理页面创建，
Admin Key 与客户端 Key 分离。对外提供服务时建议由 Nginx、Caddy 等反向代理终止 HTTPS；
也可以配置 `tls_cert` 与 `tls_key` 让网关直接启用 HTTPS。
`allowed_origins` 用于浏览器来源/CORS 限制，`allowed_ips` 用于客户端 IP 白名单。
仅在可信反向代理覆盖并清理 `X-Forwarded-For` 时启用 `trust_proxy`。可用对应启动参数
`--allowed-origins`、`--allowed-ips`、`--trust-proxy`、`--tls-cert`、`--tls-key`，或
`DIRECT_GATEWAY_*` 同名环境变量配置。除隔离测试环境外，不建议设置 `auth_required: false`。

管理页面：

```text
http://服务器地址:8123/admin
```

首次启动生成的 Admin Key 位于 `admin.key`。
用量记录保存在 `usage.json`，默认最多保留 100,000 条请求记录。记录包含时间、Key ID、模型、
接口、HTTP 状态、耗时和输入/输出 token，不保存明文 API Key 或请求正文。创建的客户端 API Key 通过
`Authorization: Bearer <key>` 访问 `/v1/models`、`/v1/responses` 和
`/v1/chat/completions`。客户端 Key 只在创建或轮换时明文返回，网关磁盘仅保存 SHA-256
哈希；撤销后立即失效。默认不启用固定的弱本地 Key，如需兼容旧客户端请显式配置 `key`
或 `DIRECT_GATEWAY_KEY`。

一次性直连请求也支持自定义上游 host/key：

```bash
node direct-gateway.mjs --host https://your-gateway.example.com/v1 --key your-api-key --model gpt-6-astra --prompt "Reply with OK only."
```

`direct-gateway.mjs` 同样支持 `--config` 配置文件；其中 `host` 和 `key` 分别对应上游地址与 API key。

## Legacy bridge

`ohmyagent-pi-bridge.mjs` and `start-ohmyagent-pi-bridge.sh` remain available
for compatibility with the previous Pi integration, but they spawn the local
Agent CLI and are not used by `direct-gateway.mjs`.

```bash
./start-ohmyagent-pi-bridge.sh status
```
