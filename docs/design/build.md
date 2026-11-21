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
├── deb/Agent Config Sync_0.0.1_amd64.deb          （约 3.1 MB）
└── appimage/Agent Config Sync_0.0.1_amd64.AppImage（约 83 MB）
```

release 配置：`strip = true`、`lto = "thin"`（`src-tauri/Cargo.toml`）。二进制已去符号。

## CI 流水线

| 工作流 | 触发 | 做什么 |
| :--- | :--- | :--- |
| `ci.yml` | push main / PR | `cargo test`、`cargo check`、`npm run build`；不打包 |
| `release.yml` | push tag `v*` | 校验三处版本号一致 → 从 `CHANGELOG.md` 对应版本段建 GitHub Release |
| `desktop.yml` | push tag `v*` / 手动 | 在 `ubuntu-22.04` 构建 deb 与 AppImage 并挂到该 tag 的 Release；手动入口可补包到已有 Release |

约束：

- runner 锁 `ubuntu-22.04`：AppImage 的最低 glibc 跟随 runner，用 `latest` 会让老发行版报 `GLIBC_2.xx not found`。
- 版本号有三处（`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`），CI 只校验不写回；不一致就失败，不建 Release。
- 首个 tag 之前必须先在 `CHANGELOG.md` 落 `## [vX.Y.Z] - <日期>` 段，否则 Release 没有正文。
- 打包依赖 GitHub release 资产（`appimagetool` 的 type2 runtime）；runner 无法出网时 AppImage 会失败，deb 不受影响。
- Release 正文只由 `release.yml` 负责；资产上传不改正文。

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
- AppImage 打包时 `appimagetool` 会从 GitHub 下载 type2 runtime（`AppImage/type2-runtime` 的 `continuous` tag，x86_64 约 923 KB）；若网络拦截 release 资产，打包会失败（deb 不受影响）。可用官方 `--runtime-file` 指定本地 runtime 手工打包。
