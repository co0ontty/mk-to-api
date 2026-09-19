# v0.1.30 — 托管开关加上文字标签

上版新增的托管开关只是个纯图标小开关，容易看漏。现在管理台「系统信息 → 系统配置同步」卡片右上角会明确显示「总开关」，Pi / Codex 各自一行独立开关。

开关在左侧 **系统信息** 页（不是「设置」页）。这是单页应用，只刷新数据、不重载页面；升级后需要在浏览器里**重新加载一次**才能看到新界面。

一行配置：

```json
{
  "manage_clients": true,
  "manage_pi": true,
  "manage_codex": false
}
```

总开关关闭后不再改写任何客户端配置；各 CLI 勾选会保留，重新打开后按原设置托管。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

已安装的客户端可在管理台点版本号更新，或重新跑一遍安装脚本。

管理界面：http://127.0.0.1:8124/admin
