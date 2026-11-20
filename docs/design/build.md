# 构建与分发

## 环境前提

| 项 | 要求 |
| :--- | :--- |
| Node.js | ≥ 22.19.0（宿主 CLI 与本项目前端共用） |
| Rust | stable（rustup） |
| 系统依赖（Linux） | `libwebkit2gtk-4.1-dev`、`libgtk-3-dev`、`libayatana-appindicator3-dev`、`librsvg2-dev`、`libxdo-dev`、`libssl-dev`、`build-essential` |
| 运行前提（Linux） | 目标机器需有 `libwebkit2gtk-4.1-0` 与 `libgtk-3-0`（deb 已声明为 Depends） |

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
├── deb/Agent Config Sync_0.1.0_amd64.deb          （约 3.1 MB）
└── appimage/Agent Config Sync_0.1.0_amd64.AppImage（约 83 MB）
```

release 配置：`strip = true`、`lto = "thin"`（`src-tauri/Cargo.toml`）。二进制已去符号。

## 发布前验证

```bash
# 1. deb 依赖声明（应含 libwebkit2gtk-4.1-0）
dpkg-deb -I "src-tauri/target/release/bundle/deb/Agent Config Sync_0.1.0_amd64.deb" | grep Depends

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
- Windows / macOS 尚未打包（见 `backlog.md`）。
- AppImage 依赖目标机器可执行 AppImage（必要时用 `APPIMAGE_EXTRACT_AND_RUN=1` 在无 FUSE 环境运行）。
- AppImage 打包时 `appimagetool` 会从 GitHub 下载 type2 runtime；若网络拦截 release 资产（表现为下载返回空内容），打包会失败，deb 不受影响。可自行准备并校验 runtime 后用 `--runtime-file` 手工打包。
