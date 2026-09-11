# v0.1.23 — 源码启动会重新编译

修复 `./start.sh` 漏编译：过去只对比少数几个源文件，改动 `src/update.rs` 或内嵌的 `src/dashboard/index.html` 不会触发重新构建，导致「更新了却看不到新功能」。

## 安装

```bash
bash -c "$(curl -fsSL 'https://raw.githubusercontent.com/co0ontty/mk-to-api/main/online/install')"
```

管理界面：http://127.0.0.1:8123/admin
