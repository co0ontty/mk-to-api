# v0.1.29 — 系统配置同步可开关

管理台「系统信息」里可以为系统配置同步加总开关，Pi / Codex 也各有独立开关。改完立即生效，并写回 `~/.mk2api/config.json`。

```json
{
  "manage_clients": true,
  "manage_pi": true,
  "manage_codex": false
}
```

总开关关闭后不再改写任何客户端配置；各 CLI 勾选会保留，重新打开后按原设置托管。

一并发布此前未发版的改动：Pi / Codex 配置改为增量写回，保留其它 provider；只有在没设置过默认、或默认已经指向 mk2api 时，才会维护客户端默认模型。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

已安装的客户端可在管理台点版本号更新，或重新跑一遍安装脚本。

管理界面：http://127.0.0.1:8123/admin
