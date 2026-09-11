# v0.1.21 — 安装收尾报错与 Qwen reasoning none

修复 macOS 一键安装在打印分隔线时 `printf: --: invalid option`，以及 Pi 关闭思考时 Qwen 返回 502。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

安装完成后服务在后台运行。用浏览器打开：

```text
http://127.0.0.1:8123/admin
```

## 这个版本

- 安装脚本不再把 `---` 当成 printf 选项，安装结束不会报错
- Pi 传入 `reasoning.effort = none` 时不再转发给上游，避免 Qwen 502
