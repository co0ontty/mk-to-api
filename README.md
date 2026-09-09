# MonkeyCode Direct Gateway

This project calls MonkeyCode's OhMyAgent gateway directly. It does not start
or use the local `ohmyagent` CLI as a forwarding process.

## Direct call

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only."
```

流式响应：

```bash
node direct-gateway.mjs --model gpt-6-astra --prompt "Reply with OK only." --stream
```

The script reads the signed OhMyAgent key from MonkeyCode's application
configuration and sends requests to the configured `/v1/responses` endpoint.
The request uses the same mechanism as the Agent:

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
