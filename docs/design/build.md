# 构建与分发

## 环境前提

| 项 | 要求 |
| :--- | :--- |
| Node.js | ≥ 22.19.0（宿主 CLI 与本项目前端共用） |
| Rust | stable（rustup） |
| 系统依赖（Linux） | `libwebkit2gtk-4.1-dev`、`libgtk-3-dev`、`libayatana-appindicator3-dev`、`librsvg2-dev`、`libxdo-dev`、`libssl-dev`、`build-essential` |
| 运行前提（Linux） | 目标机器需有 `libwebkit2gtk-4.1-0` 与 `libgtk-3-0`（deb 已声明为 Depends） |
| 系统依赖（Windows） | Visual Studio Build Tools（C++ 桌面开发）+ WebView2 运行时（系统自带或由安装器拉取）；不需要 GTK/WebKitGTK |

## 日常开发

```bash
npm install
npm run dev            # 只起前端（浏览器可预览，桌面命令降级）
npm run tauri -- dev   # 起桌面窗口（会先跑 npm run dev）
```

Rust 测试必须进入 `src-tauri/` 执行：

```bash
cd src-tauri && cargo test
cd src-tauri && cargo check
```

前端构建：

```bash
npm run build          # tsc -b && vite build
```

## 打包（Linux）

```bash
npm run tauri -- build -- --bundles deb,appimage
```

产物：

```text
src-tauri/target/release/bundle/
├── deb/Agent Config Sync_0.0.1_amd64.deb          （约 3.1 MB）
└── appimage/Agent Config Sync_0.0.1_amd64.AppImage（约 83 MB）
```

release 配置：`strip = true`、`lto = "thin"`（`src-tauri/Cargo.toml`）。二进制已去符号。

## 打包（Windows）

只在 Windows 上构建（本地 Windows 或 CI 的 `windows-latest`）；不在 Linux 上交叉编译（官方定位为 last resort）。

```bash
npm run tauri -- build -- --bundles nsis
```

产物：`src-tauri/target/release/bundle/nsis/`。

- 安装范围 `perUser`：不需要管理员提权，程序装到 `%LOCALAPPDATA%`。
- 未签名：首次运行会有 SmartScreen 提示；自测点「更多信息 → 仍要运行」，或先 `Unblock-File` 去掉 MOTW。
- WebView2 用默认 `downloadBootstrapper`：Win11 与已更新的 Win10 自带运行时；只有断网且缺运行时的机器才需要 `offlineInstaller`（安装包会涨到上百 MB）。
- 应用数据在 `%APPDATA%\<identifier>`（配置、WebDAV 凭据、备份、暂存、webview 存储同目录）。卸载**不删除**该目录：安装器默认只删程序文件，本项目不装清理钩子。
- 超时终止子进程时杀整棵进程树（Windows 只杀直接子进程会留下 npm 的子进程）。

## Windows 真机验证清单

1. 安装后启动：无控制台窗口，三个页面可切换。
2. 环境检测：node/npm/pi 的路径与版本、`pi list` 的声明与实装比对都能出结论。
3. 导出快照 → manifest 完整；对同一份快照再跑一次计划应为「无变化」。
4. 应用一份含链接来源的快照：能建链接则重建，建不了则物化并告警。
5. 覆盖/回滚：备份清单 `mode` 为 `null`，回滚后内容一致且不尝试改权限。
6. WebDAV：上传 → 另一台下载 → 应用。

## CI 流水线

| 工作流 | 触发 | 做什么 |
| :--- | :--- | :--- |
| `ci.yml` | push main / PR | `cargo test`、`cargo check`、`npm run build`；不打包 |
| `release.yml` | push tag `v*` | 校验三处版本号一致 → 从 `CHANGELOG.md` 对应版本段建 GitHub Release |
| `desktop.yml` | push tag `v*` / 手动 | 在 `ubuntu-22.04` 构建 deb 与 AppImage；在 `windows-latest` 跑 `cargo test` 并构建 NSIS；两者都挂到该 tag 的 Release。手动入口可补包到已有 Release |

约束：

- runner 锁 `ubuntu-22.04`：AppImage 的最低 glibc 跟随 runner，用 `latest` 会让老发行版报 `GLIBC_2.xx not found`。
- 版本号有三处（`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`），CI 只校验不写回；不一致就失败，不建 Release。
- 首个 tag 之前必须先在 `CHANGELOG.md` 落 `## [vX.Y.Z] - <日期>` 段，否则 Release 没有正文。
- 打包依赖 GitHub release 资产（`appimagetool` 的 type2 runtime）；runner 无法出网时 AppImage 会失败，deb 不受影响。
- Release 正文只由 `release.yml` 负责；资产上传不改正文。

## 发布前验证

```bash
# 1. deb 依赖声明（应含 libwebkit2gtk-4.1-0）
dpkg-deb -I "src-tauri/target/release/bundle/deb/Agent Config Sync_0.0.1_amd64.deb" | grep Depends

# 2. 已去符号（应显示 stripped / no symbols）
file src-tauri/target/release/agent-config-sync
nm src-tauri/target/release/agent-config-sync

# 3. 无头启动烟测（进程存活到超时即视为通过）
cd src-tauri/target/release
timeout --preserve-status 10 xvfb-run -a env \
  GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1 \
  ./agent-config-sync
```

## 已知限制

- 未签名：deb/AppImage 均无签名与公证；分发给他人前需补信任链。
- 图标为 128→1024 放大的占位图，发布前应替换为正式设计稿。
- 无自动更新：升级需重新下载安装包。
- macOS 尚未打包（见 `backlog.md`）；Windows 未签名、无卸载清理钩子。
- AppImage 依赖目标机器可执行 AppImage（必要时用 `APPIMAGE_EXTRACT_AND_RUN=1` 在无 FUSE 环境运行）。
- AppImage 打包时 `appimagetool` 会从 GitHub 下载 type2 runtime（`AppImage/type2-runtime` 的 `continuous` tag，x86_64 约 923 KB）；若网络拦截 release 资产，打包会失败（deb 不受影响）。可用官方 `--runtime-file` 指定本地 runtime 手工打包。
