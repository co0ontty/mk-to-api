# v0.2.2 — 上游渠道：把别的 API 也接进来

现在可以填一个外部 API（名称 + Base URL + API Key），它会被当成一个「渠道」接进网关：
模型按 ID 路由过去，显示名带渠道前缀，用量单独统计。

```bash
mk2api channels add huniu https://api.huniu.example/v1 sk-xxxx --wire chat
mk2api channels list
```

管理台多了 **上游渠道** 页，填表就能加：渠道名会变成模型前缀，
模型对外 ID 形如 `huniu/gpt-5.6-sol`，pi / codex 里显示为 `[huniu] GPT-5.6 Sol`，
不同渠道的同名模型一眼就能分开。

## 上游协议

新增渠道时选上游吃什么协议，网关负责转换：

| wire | 上游是什么 | 网关怎么做 |
| --- | --- | --- |
| `responses`（默认） | OpenAI Responses | 原样转发 |
| `chat` | OpenAI Chat Completions | 双向转换，含 SSE 流式（文本 + 工具调用）与 usage 换算 |
| `anthropic` | Anthropic Messages | 走既有 Anthropic 转换 |

也就是说，只支持 Chat Completions 的第三方 API 也能直接给 Codex 用。

## 模型列表

保存渠道时会请求 `<Base URL>/models` 自动拉模型；拉失败不影响保存，
错误会记在渠道上（管理台显示「拉取失败」，`mk2api channels list` 打印错误行）。
也可以手写模型列表，或点「刷新模型」重新拉一次；每 10 分钟后台自动重新拉一次，
手工编辑 `~/.mk2api/channels.json`（权限 0600）会被热加载。

渠道上的 `api_key` 只落盘、不通过管理接口回显（只返回 `has_key`）。

## 其它

- 修复：`mk2api` 遇到不认识的子命令会静默起一个服务进程，现在会打印用法并报错。
- 修复：只在启动时已有渠道才起后台同步循环，导致启动后新加的渠道拿不到定时刷新，现在一直运行。
- 修复：没有 MonkeyCode 凭据时整个模型目录不可用，现在只要配了渠道就能正常工作。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

已安装的客户端可在管理台点版本号更新，或重新跑一遍安装脚本。

管理界面：http://127.0.0.1:8124/admin
