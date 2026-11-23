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

### WSL2

WSLg 里 `npm run tauri -- dev` 能开窗口，但 GTK 会挑错后端、WebKit 走 DMA-BUF 合成，刷一串 `libEGL` / `MESA` / `GTK` 告警。压掉噪音：

```bash
GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1 \
  npm run tauri -- dev
```

- 与「发布前验证」的烟测同一组变量，只影响开发窗口的渲染路径，不进发布产物。
- 代价：走 X11 + 软件渲染，动画会掉帧；仍黑屏或撕裂时再加 `WEBKIT_DISABLE_COMPOSITING_MODE=1`（关掉 WebKit 加速合成，滚动更卡）。
- 别在 WSL2 里验证 Windows 行为：本应用不跨 WSL，Windows 版只管理 Windows 原生环境（路径大小写、链接、`cmd /d /s /c` 形态都在 Windows 上验）。

## 打包（Linux）

```bash
npm run tauri -- build --bundles deb,appimage
```

产物：

```text
src-tauri/target/release/bundle/
├── deb/Agent Config Sync_0.0.4_amd64.deb          （约 3.1 MB）
└── appimage/Agent Config Sync_0.0.4_amd64.AppImage（约 83 MB）
```

release 配置：`strip = true`、`lto = "thin"`（`src-tauri/Cargo.toml`）。二进制已去符号。

## 发布构建（带更新产物）

普通本地构建不传 `--config`：不产出更新产物，也不需要签名私钥。发布构建叠加 `src-tauri/tauri.updater.conf.json`（只含 `bundle.createUpdaterArtifacts: true`）：

```bash
TAURI_SIGNING_PRIVATE_KEY=… TAURI_SIGNING_PRIVATE_KEY_PASSWORD=… \
  npm run tauri -- build --bundles deb,appimage --config src-tauri/tauri.updater.conf.json
```

- `--config` 路径按执行目录解析（仓库根），内容按 JSON merge patch 深合并进 `tauri.conf.json`。
- `createUpdaterArtifacts` 开启时 CLI 要求 `plugins.updater` 存在且带 `pubkey`；更新源与公钥常驻在 `tauri.conf.json`，与是否发布构建无关。
- 两个 secret 只放 CI 与离线备份，不进仓库：`TAURI_SIGNING_PRIVATE_KEY`（私钥原文或指向私钥文件的路径）、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。缺私钥时发布构建直接失败，不会产出无签名包。
- 每个安装包旁生成同名 `.sig`：`*.AppImage.sig`、`*.deb.sig`、`*-setup.exe.sig`。签名把版本写进 trusted comment，供 `requireSignedVersion` 拒绝版本不符与降级。
- 上传顺序：安装包与 `.sig` 先，`latest.json` 最后；`latest.json` 出现即代表本次发布可更新。

## Release 资产命名

打包产物名带空格（`Agent Config Sync_0.0.4_amd64.AppImage`），GitHub 会把它归一化成 `%20`，而 `latest.json` 的 `url` 必须与资产名逐字一致。所以 CI 构建后先复制成无空格的名字，`latest.json` 只引用这些名字：

| 平台键 | Release 资产名 |
| :--- | :--- |
| `linux-x86_64-appimage` | `agent-config-sync-<version>-linux-x86_64.AppImage` |
| `linux-x86_64-deb` | `agent-config-sync-<version>-linux-amd64.deb` |
| `windows-x86_64-nsis` | `agent-config-sync-<version>-windows-x86_64-setup.exe` |

- 每个资产旁有同名 `.sig`；本地 `bundle/` 目录里的原名不动，只在 `release-assets/` 里改名。
- 命名只在两端各写一次：workflow 负责改名，`scripts/updater-manifest.mjs` 按后缀从 `release-assets/` 的实际文件名认领资产并取其 `.sig`。缺资产、缺 `.sig`、`.sig` 为空或文件名含空格都会让脚本直接失败。
- `latest.json` 的 `url` 形如 `https://github.com/whispering233/agent-config-sync/releases/download/<tag>/<asset>`，`version` 取 tag 去掉 `v`，`pub_date` 用生成时的 UTC 时间。

## 打包（Windows）

只在 Windows 上构建（本地 Windows 或 CI 的 `windows-latest`）；不在 Linux 上交叉编译（官方定位为 last resort）。

```bash
npm run tauri -- build --bundles nsis
```

产物：`src-tauri/target/release/bundle/nsis/` 下的 `*-setup.exe`。

- 安装范围取 NSIS 默认的 `currentUser`：不需要管理员提权，程序装到 `%LOCALAPPDATA%`。
- 未签名：首次运行会有 SmartScreen 提示；自测点「更多信息 → 仍要运行」，或先 `Unblock-File` 去掉 MOTW。
- WebView2 用默认 `downloadBootstrapper`：Win11 与已更新的 Win10 自带运行时；只有断网且缺运行时的机器才需要 `offlineInstaller`（安装包会涨到上百 MB）。
- 应用数据在 `%APPDATA%\<identifier>`（配置、WebDAV 凭据、备份、暂存、webview 存储同目录）。卸载**不删除**该目录：安装器默认只删程序文件，本项目不装清理钩子。
- 超时终止子进程时杀整棵进程树（Windows 只杀直接子进程会留下 npm 的子进程）。

## Windows 真机验证清单

1. 安装后启动：无控制台窗口，三个页面可切换。
2. 环境检测：node/npm/pi 的路径与版本、`pi list` 的声明与实装比对都能出结论；安装方式显示 npm 全局安装。
3. 导出快照包 → 包内 `manifest.json` 完整、`source.pi` / `source.node` 有值；对同一份包再跑一次计划应为「无变化」（包与 live 都保留原字节与明文密钥）。
4. 应用一份含链接来源的快照包：能建链接则重建，建不了则物化并告警。
5. 快照没声明的陈旧链接（junction 也算）：计划里列为删除候选，确认后只删链接，目标内容不动；无建链接权限时回滚按目标物化并告警。
6. 覆盖/回滚：备份清单 `mode` 为 `null`，回滚后内容一致且不尝试改权限。
7. WebDAV：上传 → 同一账号下载（默认最新包）→ 应用；上传时先 `PUT .tmp-*` 再 `MOVE`，正式名下只出现完整包；远端有本机未见的新包时显示「云端有更新」并可选历史包；故意传一个坏包应被解包闸门或哈希校验拒绝。

## CI 流水线

| 工作流 | 触发 | 做什么 |
| :--- | :--- | :--- |
| `ci.yml` | push main / PR | Linux：`cargo test`、`cargo check`、`npm run build`；Windows：`cargo test`（含平台分支）；不打包 |
| `release.yml` | push tag `v*` | 校验三处版本号一致 → 从 `CHANGELOG.md` 对应版本段建 GitHub Release |
| `desktop.yml` | push tag `v*` / 手动 | 在 `ubuntu-22.04` 构建 deb 与 AppImage；在 `windows-latest` 跑 `cargo test` 并构建 NSIS；两者用 `tauri.updater.conf.json` 生成 `.sig`，挂到该 tag 的 Release。最后 `manifest` job 生成并上传 `latest.json`。手动入口可补包到已有 Release |

约束：

- runner 锁 `ubuntu-22.04`：AppImage 的最低 glibc 跟随 runner，用 `latest` 会让老发行版报 `GLIBC_2.xx not found`。
- 版本号有三处（`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`），CI 只校验不写回；不一致就失败，不建 Release。
- 首个 tag 之前必须先在 `CHANGELOG.md` 落 `## [vX.Y.Z] - <日期>` 段，否则 Release 没有正文。
- 打包依赖 GitHub release 资产（`appimagetool` 的 type2 runtime）；runner 无法出网时 AppImage 会失败，deb 不受影响。
- Release 正文只由 `release.yml` 负责；资产上传不改正文。
- Windows 的 PR 检查只编译并跑测试，不构建 NSIS；NSIS 仍由 `desktop.yml` 在 tag 或手动触发时构建。
- 发布 job 必须配置 `TAURI_SIGNING_PRIVATE_KEY` 与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`；缺失时构建失败，不发无签名包。
- `tauri-plugin-updater` 不低于 2.14：`requireSignedVersion` 需要 2.12+，2.14 起安装前校验下载字节；`@tauri-apps/cli` 不低于 2.11.5 才能把版本写进签名 trusted comment。
- `latest.json` 的 `url` 必须与 Release 上实际资产名逐字一致；生成脚本从构建产物文件名读取，不手写。上传顺序：安装包与 `.sig` 先，`latest.json` 最后。
- `manifest` job 在两边安装包都上传后生成 `latest.json`，上传前逐个探测 `url` 可下载（HEAD 请求，刚上传的资产有 CDN 延迟，最多重试 10 次）；探测失败就不传 `latest.json`，本次发布不算可更新。
- `latest.json` 的 `version` 必须与三处版本号一致；`platforms` 至少覆盖 `linux-x86_64-appimage`、`linux-x86_64-deb`、`windows-x86_64-nsis`。

## 更新签名密钥

私钥与密码各存一份在仓库外（POSIX `0600`）：`~/.tauri/agent-config-sync.key`、`~/.tauri/agent-config-sync.key.password`。仓库里只有公钥（`src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`）。

配置到 GitHub Actions（`gh auth status` 确认是仓库属主后）：

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/agent-config-sync.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD < ~/.tauri/agent-config-sync.key.password
gh secret list
```

- 也可在仓库 Settings → Secrets and variables → Actions 手动添加；值同上，不写进任何被提交的文件。
- 换私钥必须同时换公钥并重发版本：已发布版本的 `requireSignedVersion` 只认内置的那把公钥，旧公钥签出的包会被拒绝。
- 私钥丢失后无法再用自动更新推包：已安装的旧版本只信任现有公钥，只能发新版本让用户手动下载，并随新包换公钥。
- 备份放离线介质；不进快照、日志与错误信息。

## 发布前验证

```bash
# 1. deb 依赖声明（应含 libwebkit2gtk-4.1-0）
dpkg-deb -I "src-tauri/target/release/bundle/deb/Agent Config Sync_0.0.4_amd64.deb" | grep Depends

# 2. 已去符号（应显示 stripped / no symbols）
file src-tauri/target/release/agent-config-sync
nm src-tauri/target/release/agent-config-sync

# 3. 无头启动烟测（进程存活到超时即视为通过）
cd src-tauri/target/release
timeout --preserve-status 10 xvfb-run -a env \
  GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1 \
  ./agent-config-sync
```

## 图标

源文件在仓库根目录的 `src-tauri/icon.svg`（应用图标）与 `src-tauri/icon-tray.svg` / `src-tauri/icon-tray-template.svg`（托盘）。

```bash
# 应用图标：先生成桌面端全套 PNG/ICO/ICNS，再补 16 与 1024 两档
npx tauri icon src-tauri/icon.svg
npx tauri icon src-tauri/icon.svg --png 16,1024

# 托盘彩色图（透明底）
npx tauri icon src-tauri/icon-tray.svg --png 32 -o /tmp/tray-color
cp /tmp/tray-color/32x32.png src-tauri/icons/tray.png

# macOS 菜单栏单色模板图
npx tauri icon src-tauri/icon-tray-template.svg --png 32 -o /tmp/tray-template
cp /tmp/tray-template/32x32.png src-tauri/icons/tray-template.png
```

- 16/32/64/128/256/512/1024 都应来自 CLI，不再保留手工放大的占位图。
- 托盘图固定 32×32；彩色图用于 Windows/Linux，template 图只用于 macOS。
- 改图后重新跑上述命令并提交 `src-tauri/icons/`；图标不参与 Rust 逻辑，回滚只需还原目录。

## 发布流程

```bash
# 1. 三处版本号与 CHANGELOG 同步
#    package.json / src-tauri/tauri.conf.json / src-tauri/Cargo.toml
git commit -am "release: vX.Y.Z"
git tag vX.Y.Z

# 2. 先推 main，再推 tag
git push origin main
git push origin vX.Y.Z
```

- tag 触发 `release.yml`（校验三处版本一致、按 CHANGELOG 建 Release）与 `desktop.yml`（Linux deb/AppImage、Windows NSIS，上传安装包、`.sig` 与 `latest.json` 到同一 Release）。
- 手动补包：`gh workflow run desktop.yml -f release_tag=vX.Y.Z`；目标 Release 必须已存在。
- 更新公钥固定在 `src-tauri/tauri.conf.json`；私钥只在 CI secret 与离线备份中。私钥丢失后，已安装用户无法继续自动更新。
- 首次带更新能力的版本必须手动安装一次：旧版本里没有更新器，无法自动升到它。
- 本地 Windows 验证：`npm run tauri -- build --bundles nsis`，产物在 `src-tauri/target/release/bundle/nsis/`。

## 已知限制

- 未签名：deb/AppImage 均无平台代码签名与公证；分发给他人前需补信任链。应用更新自身使用 minisign，不依赖平台签名。
- 应用更新为全量安装包；增量更新未做（见 `backlog.md`）。
- macOS 尚未打包（见 `backlog.md`）；Windows 未签名、无卸载清理钩子。
- AppImage 依赖目标机器可执行 AppImage（必要时用 `APPIMAGE_EXTRACT_AND_RUN=1` 在无 FUSE 环境运行）。
- AppImage 打包时 `appimagetool` 会从 GitHub 下载 type2 runtime（`AppImage/type2-runtime` 的 `continuous` tag，x86_64 约 923 KB）；若网络拦截 release 资产，打包会失败（deb 不受影响）。可用官方 `--runtime-file` 指定本地 runtime 手工打包。
