# v0.1.20 — 后台安装与 Web 管理界面

一键安装改为 launchd 后台常驻，安装完成后打印管理界面地址。本版本同时带上 Web 控制台看板，以及 Anthropic 模型自动改写。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

安装完成后服务在后台运行。用浏览器打开：

```text
http://127.0.0.1:8123/admin
```

也可以执行：

```bash
open http://127.0.0.1:8123/admin
mk2api dashboard
```

首次启动生成的 Admin Key 位于：

```text
~/.mk2api/admin.key
```

## 这个版本

- 安装脚本通过 launchd 后台启动，不再前台常驻
- 安装完成后打印管理界面地址、打开方式和 Admin Key 路径
- Web 控制台：`/admin`、`mk2api dashboard`
- Anthropic 协议模型自动改写，供 Pi / Codex 使用
- `mk2api start` / `status` 同样打印管理界面访问方式

> 直接暴露到公网前，请配置 HTTPS（网关 TLS 或 Nginx/Caddy），否则 HTTP 请求中的 Bearer Key 可能被窃听。
