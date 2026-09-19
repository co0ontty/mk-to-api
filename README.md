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

服务启动后会检测本机配置并增量写回 mk2api provider，同时按当前 `/v1/models` 更新自己的模型目录。
已有的其它 provider（`monkeycode`、`OpenAI` 等）和配置文件里的其它字段都会原样保留：

- Pi：`~/.pi/agent/models.json`、`~/.pi/agent/settings.json`
- Codex：`~/.codex/config.toml`、`~/.codex/codex-models.json`

客户端默认走哪个 provider 也由你决定：只有在没设置过默认、或默认已经指向 `mk2api` 时，
才会维护 `defaultProvider` / `defaultModel`（Pi）和 `model_provider` / `model` / `review_model` /
`model_catalog_json` / `model_context_window`（Codex）。一旦你把默认切到别的 provider，
mk2api 就只保留自己的 provider 表，不再改这些默认值。

之后每 60 秒对照一次目录；模型增减会写回客户端配置。也可手动执行：

```bash
mk2api clients
mk2api clients sync
```

关闭托管：在管理台「系统信息 → 系统配置同步」关掉总开关，或写入：

```json
{
  "manage_clients": false
}
```

Pi / Codex 各有独立开关，总开关关闭后不再改写任何客户端配置。也可在同一页分别切换，或使用 `manage_pi` / `manage_codex`。

## 上游渠道（接别的 API）

除了 MonkeyCode，还能把一个外部 API（自带 `名称` + `Base URL` + `API Key`）作为「渠道」接进来。
网关按模型 ID 路由：命中渠道的请求直接发往该渠道，其它请求仍走 MonkeyCode。

```bash
mk2api channels add huniu https://api.huniu.example/v1 sk-xxxx --wire chat
mk2api channels list
```

也可以在管理台 **上游渠道** 页填表新增（`GET /dashboard`，`/admin` 也指向同一页面）。

### 模型 ID 与显示名

渠道模型的对外 ID 是 `<渠道 slug>/<上游模型名>`，例如 `huniu/gpt-5.6-sol`；
写回 pi / codex 的显示名带上渠道前缀，例如 `[huniu] GPT-5.6 Sol`，
这样不同渠道的同名模型在客户端里可以直接区分。渠道模型不额外生成短名别名，避免不同渠道撞名。

slug 由渠道名生成（非 ASCII 名回退为 `channel`），`monkeycode-basic` / `monkeycode-pro` / `monkeycode-ultra` 是保留 slug。

### 上游协议（wire）

| wire | 含义 |
| --- | --- |
| `responses`（默认） | 上游就是 OpenAI Responses，网关原样转发（回包里的 `model` 也是上游自己的名字） |
| `chat` | 上游是 OpenAI Chat Completions，网关双向转换，包括 SSE 流式（含工具调用）与 usage 换算 |
| `anthropic` | 上游是 Anthropic Messages，走既有的 Anthropic 转换 |

### 模型列表

保存渠道时会自动请求 `<Base URL>/models` 拉取模型列表；拉取失败不影响保存，只把错误记在 `last_error` 上，
管理台显示「拉取失败」、`mk2api channels list` 会打印错误行。也可以手写模型列表（`refresh: false`）：

```json
{
  "channels": [
    {"slug": "huniu", "name": "huniu", "base_url": "https://api.huniu.example/v1", "api_key": "sk-xxxx", "wire_api": "chat", "models": ["gpt-5.6-sol"]}
  ]
}
```

渠道配置在 `~/.mk2api/channels.json`（权限 0600，`api_key` 只落盘、不回显给管理接口），
可用 `channels-file` 配置项或 `MK2API_CHANNELS_FILE` 环境变量改路径。
后台每 60 秒检查一次这个文件：手工编辑会被热加载；每 10 轮（约 10 分钟）重新拉一次上游模型列表，
刷新会以「上游返回的列表」覆盖手写列表。

管理接口（都需要 Admin Key）：

- `GET|POST /v1/admin/channels` — 列表 / 新增（`POST` 支持 `refresh: false` 跳过自动拉取）
- `POST /v1/admin/channels/refresh-all` — 全部重新拉取
- `GET|POST|DELETE /v1/admin/channels/<slug>` — 查看 / 修改（`redirect` 可改 slug）/ 删除
- `POST /v1/admin/channels/<slug>/refresh` — 单个重新拉取

## 网关接口

- `GET /v1` — 网关状态
- `GET /health` 或 `GET /v1/health` — 健康检查
- `GET /v1/models`
- `POST /v1/responses`
- `POST /v1/chat/completions`
- `GET /admin`、`GET /dashboard` — 管理台（含 API Key、模型、上游渠道）
- `GET /v1/admin/usage` — 用量统计（仅 Admin Key）
- `GET /v1/admin/channels` — 上游渠道管理与刷新（仅 Admin Key）

Pi / Codex 始终用 OpenAI Responses 或 Chat Completions 调用本机网关。上游默认走 MonkeyCode OhMyAgent proxy（`oma` key + HMAC 签名）：`type: openai-responses` 的模型 POST `/responses`，`type: anthropic` 的模型（如 DeepSeek V4 Flash）自动改写为 Anthropic Messages 后 POST `/messages`。短模型名优先解析到 `monkeycode-basic/`。配了上游渠道后，`<渠道 slug>/` 开头的模型先按渠道路由（见上一节）。
