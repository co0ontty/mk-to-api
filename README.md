# mk2api

本地转发服务由 Rust 实现。`mk2api` 是全局命令，配置和用量数据保存在 `~/.mk2api`。

## 源码启动

```bash
./start.sh
```

`start.sh` 会编译 release 二进制、安装到 `~/.local/bin/mk2api`，然后进入用量 TUI。
如果还没有配置文件，会提示输入；每一项都有默认值，直接回车即可。

也可以显式管理服务：

```bash
./start.sh start
./start.sh stop
./start.sh status
./start.sh restart
mk2api                 # 进入终端用量统计页面（按 q 退出）
mk2api start
mk2api stop
mk2api restart
mk2api status
```

默认监听 `0.0.0.0:8123`。端口被占用时会自动选择下一个可用端口，并写入 `~/.mk2api/port`。
服务通过 macOS `launchd` 后台常驻，日志在 `~/Library/Logs/mk2api.log`。

## 一键安装（macOS）

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

安装脚本会下载 Release 产物、安装 `mk2api`、写入 `~/.mk2api`，然后用 launchd 在后台启动（不是前台常驻）。完成后会打印管理界面地址：

```text
http://127.0.0.1:8123/admin
```

浏览器打开该地址，用 `~/.mk2api/admin.key` 登录。也可以执行：

```bash
open http://127.0.0.1:8123/admin
mk2api dashboard
```

## 配置

首次启动会创建 `~/.mk2api/config.json`：

```json
{
  "host": "0.0.0.0",
  "port": 8123,
  "auth_required": true
}
```

Admin Key 位于 `~/.mk2api/admin.key`，用量记录位于 `~/.mk2api/usage.json`。
上游签名密钥仍读取 MonkeyCode 本地 OhMyAgent 配置。

## 本机 Pi / Codex 托管

服务启动后会检测本机配置并自动改写为只走 mk2api，同时按当前 `/v1/models` 全量更新模型目录：

- Pi：`~/.pi/agent/models.json`、`~/.pi/agent/settings.json`
- Codex：`~/.codex/config.toml`、`~/.codex/codex-models.json`

之后每 60 秒对照一次目录；模型增减会写回客户端配置。也可手动执行：

```bash
mk2api clients
mk2api clients sync
```

关闭托管：

```json
{
  "manage_clients": false
}
```

只托管其中一个时，可用 `manage_pi` / `manage_codex`。

## 网关接口

- `GET /v1` — 网关状态
- `GET /health` 或 `GET /v1/health` — 健康检查
- `GET /v1/models`
- `POST /v1/responses`
- `POST /v1/chat/completions`
- `GET /admin` — API Key 管理页面
- `GET /v1/admin/usage` — 用量统计（仅 Admin Key）

Pi / Codex 始终用 OpenAI Responses 或 Chat Completions 调用本机网关。上游只走 MonkeyCode OhMyAgent proxy（`oma` key + HMAC 签名）：`type: openai-responses` 的模型 POST `/responses`，`type: anthropic` 的模型（如 DeepSeek V4 Flash）自动改写为 Anthropic Messages 后 POST `/messages`。短模型名优先解析到 `monkeycode-basic/`。
