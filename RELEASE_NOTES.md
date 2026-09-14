# v0.1.28 — 多张截图历史再次卡住

上次只限制了单张图 750KB。Pi 会把每次 `read` 到的截图都留在对话里，1080×2400 PNG 的 base64 大约 0.5–0.9MB；五六张叠在一起就会：

- DeepSeek / Anthropic：整包超过 MonkeyCode `983616` 字符上限，上游 400，空等约 22 秒
- GPT / OpenAI Responses：没有这条上限，但好几 MB 的图会把流拖死

现在会从最旧的图开始换成短占位，直到请求体积落在上限内（Anthropic 980KB，OpenAI 2MB），最新的截图尽量留给模型。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

管理界面：http://127.0.0.1:8124/admin
