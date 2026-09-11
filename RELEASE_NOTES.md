# v0.1.26 — Codex DeepSeek 报错修复与动态上下文窗口

## Codex / DeepSeek

Codex 里 DeepSeek 连续 502/404 的原因：

- 上游把超长上下文收成 SSE `event:error`（`InvalidParameter` / context length），网关却当成 502，Codex 当断流连着重试
- OhMyAgent 目录刷新后短名 `deepseek-v4-flash` 对不上 MonkeyCode 前缀，本地直接 404

现在会抽出真实错误改成 400 / `response.failed`，短名会回退到 OhMyAgent 同名模型并仍走 MonkeyCode 签名代理。tool_result 数组内容和空 user message 也一并修了。

## 动态上下文窗口

不再给所有模型写死 100 万 token。每个模型的窗口来自：

1. OhMyAgent `settings.json` 里的 `context_window` / `max_output`
2. 上游 token 上限报错（例如 GLM「maximum context length is 1048576 tokens」）写入 `~/.mk2api/model-limits.json`，下次同步覆盖

会写回 Pi `contextWindow`、Codex `model_context_window` / 目录，并出现在 `/v1/models` 和管理台「模型」页。广告窗口会预留 `max_output`，避免 input+completion 一起超限。

DeepSeek 那种「input length」字符限制不会当成 token 窗口。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

管理界面：http://127.0.0.1:8123/admin
