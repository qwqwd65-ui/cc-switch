# Fork 应用内自动更新设计

日期：2026-09-25

## 目标

让 `qwqwd65-ui/cc-switch` 发布的桌面应用在应用内检查 Fork Release，下载经过 Tauri minisign 验证的更新包，安装后自动重启，不再把用户跳转到 GitHub 手动下载。

## 发布格式

- Windows：新增 NSIS 安装版和签名更新包；继续保留 Portable ZIP。安装版支持应用内更新，Portable 保持手动更新。
- macOS：继续发布用户下载用 ZIP，同时发布 Tauri updater 使用的 `.app.tar.gz` 和 `.sig`。
- Linux：继续发布 `.deb`，同时新增 updater 使用的 AppImage 和 `.sig`。
- Release 必须包含由各平台签名汇总生成的 `latest.json`。

## 客户端流程

1. 应用启动后检查 `qwqwd65-ui/cc-switch` 最新稳定 Release。
2. 有更新时显示版本、说明和操作入口。
3. “立即更新”调用后端 `install_update_and_restart`，不再打开 Release 页面。
4. 前端监听 `update-download-progress` 并显示下载百分比。
5. 后端下载、验签、安装，完成退出清理后重启应用。
6. Portable 模式不尝试安装 NSIS 更新，明确提示用户下载 Portable 包。

## 信任与密钥

- 为 Fork 生成独立 Tauri updater minisign 密钥对。
- 公钥提交到 `src-tauri/tauri.conf.json`。
- 私钥和密码仅保存为 GitHub Actions Secrets：`TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。
- Release 构建缺少密钥、签名或平台更新包时直接失败，不发布不可更新的稳定版本。

## 验证

- 本机只做文本、JSON/YAML、TypeScript 静态检查，不执行完整编译或测试。
- 推送后由 GitHub Actions 执行前端检查和 Windows/macOS/Linux/WSL2 后端验证。
- Fork Release 在 GitHub Actions 上完成实际构建、签名和发布。
