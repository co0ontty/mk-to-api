# v0.1.0 — Rust API Gateway and Key Management

首次正式 Release，将本地转发器重构为 Rust 服务，并加入面向 macOS 的一键安装和 API Key 管理能力。

## 主要功能

- macOS Apple Silicon（aarch64）和 Intel（x86_64）预编译二进制
- 默认监听 `0.0.0.0:8123`
- OpenAI-compatible Responses 和 Chat Completions 接口
- Responses/Chat SSE 流式转发
- 独立 Admin Key 和客户端 API Key
- 客户端 Key 创建、列表、撤销和轮换
- API Key 只保存 SHA-256 哈希，明文只在创建/轮换时返回一次
- `/admin` 管理页面
- 按客户端 Key、模型和接口记录请求次数、耗时及输入/输出 token
- 用量记录持久化到 `usage.json`
- macOS launchd 后台服务管理

## 安装

```bash
bash -c "$(curl -fsSL 'https://monkeycode-ai.com/online/install')"
```

如果域名安装入口尚未配置，可以使用 GitHub Raw 地址：

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

安装完成后访问：

```text
http://服务器地址:8123/admin
```

首次启动生成的 Admin Key 位于：

```text
~/Library/Application Support/com.chaitin.baizhi.monkeycode/admin.key
```

> 直接暴露到公网前，请配置 HTTPS（网关 TLS 或 Nginx/Caddy），否则 HTTP 请求中的 Bearer Key 可能被窃听。
