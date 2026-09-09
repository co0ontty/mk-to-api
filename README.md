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

默认监听 `127.0.0.1:8765`，日志写入
`~/Library/Logs/monkeycode-direct-gateway.log`。

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only."
```

流式响应：

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only." --stream
```

The script reads the signed OhMyAgent key from MonkeyCode's application
configuration and sends requests to the configured `/v1/responses` endpoint.
The local gateway accepts both OpenAI Responses and Chat Completions routes:

- `POST /v1/responses`
- `POST /v1/chat/completions`

Chat Completions requests are converted to the upstream Responses format and
converted back to a non-streaming Chat Completion response. Query parameters
and trailing slashes on these routes are accepted.


- `Authorization: Bearer oma_...`
- `X-OhMyAgent-Signature: v1=<HMAC-SHA256 hex>`
- `input` containing a `developer` message followed by a `user` message
- the full internal model name, such as `monkeycode-ultra/gpt-6-astra`

The signature covers the developer prompt and uses the local
`signing_secret`; no credential is printed by the script.

Use `--system` to replace the signed developer prompt. Override the default
configuration paths with `MONKEYCODE_CONFIG_DIR`, `MONKEYCODE_OHMYAGENT_KEY`,
and `OHMYAGENT_SETTINGS`.

## Legacy bridge

`ohmyagent-pi-bridge.mjs` and `start-ohmyagent-pi-bridge.sh` remain available
for compatibility with the previous Pi integration, but they spawn the local
Agent CLI and are not used by `direct-gateway.mjs`.

```bash
./start-ohmyagent-pi-bridge.sh status
```
