# v0.1.27 — DeepSeek 大图卡住与思考回传

## DeepSeek 读截图卡住

Pi 读完 1080×2400 截图后 DeepSeek 会空等约 22 秒：网关把 `function_call_output` 里的 `input_image` data URL 整段 JSON 塞进 Anthropic `tool_result` 文本，超过 MonkeyCode `983616` 字符上限，上游 400。

现在会转成 Anthropic `image` + `base64`；超过 750KB 的图改成短占位，不再把任务卡死。小图会真正送给模型。

## 思考签名、档位与提示缓存

随上一笔未发版提交一并发布：

- Anthropic `thinking` + `signature` ↔ Responses `reasoning.encrypted_content`，下一轮插回同条 assistant 的 `tool_use` 之前
- `cache_control: ephemeral` 打在 system、最后一个 tool、最后一个非 thinking 内容块
- 思考预算按 OhMyAgent 模型档位封顶（DeepSeek `thinking.effort: low` → 1024），不再盲从客户端 `high`

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

管理界面：http://127.0.0.1:8124/admin
