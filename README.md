# MonkeyCode Direct Gateway

This project calls MonkeyCode's OhMyAgent gateway directly. It does not start
or use the local `ohmyagent` CLI as a forwarding process.

## Direct call

启动本地直连网关：

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

默认监听 `127.0.0.1:8123`，日志写入
`~/Library/Logs/monkeycode-direct-gateway.log`。

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

自定义本地监听地址和鉴权 key：

```bash
node direct-gateway-server.mjs --host 127.0.0.1 --port 8124 --key my-local-key
```

也可以创建 `~/Library/Application Support/com.chaitin.baizhi.monkeycode/direct-gateway.json`：

```json
{
  "host": "127.0.0.1",
  "port": 8124,
  "key": "my-local-key",
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

安全默认值是仅监听 `127.0.0.1` 且要求 Bearer key。对外提供服务时建议由 Nginx、Caddy
等反向代理终止 HTTPS；也可以配置 `tls_cert` 与 `tls_key` 让网关直接启用 HTTPS。
`allowed_origins` 用于浏览器来源/CORS 限制，`allowed_ips` 用于客户端 IP 白名单。
仅在可信反向代理覆盖并清理 `X-Forwarded-For` 时启用 `trust_proxy`。可用对应启动参数
`--allowed-origins`、`--allowed-ips`、`--trust-proxy`、`--tls-cert`、`--tls-key`，或
`DIRECT_GATEWAY_*` 同名环境变量配置。除隔离测试环境外，不建议设置
`auth_required: false`。

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
