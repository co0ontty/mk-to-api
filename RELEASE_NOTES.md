# v0.1.25 — 多端鉴权与 Anthropic 流式兼容

## 鉴权

除了 `Authorization: Bearer <API Key>`，现在也接受 Anthropic 生态的 `x-api-key` 和 Google 生态的 `x-goog-api-key`。

Claude Code、cc-switch 的 anthropic-messages、Cherry Studio 等客户端不再因为密钥头格式不同而拿到 401（也就是「获取模型列表失败」）。

## Anthropic 流式

完善 Anthropic Messages → OpenAI Responses 的流式转换：

- thinking-only 的回合不再被判定为空完成
- 上游缺少 `content_block_stop` 时，收尾会正确 flush 未闭合的块
- `message_start` 的 input tokens 在整段流里保持不被覆盖

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

管理界面：http://127.0.0.1:8123/admin
