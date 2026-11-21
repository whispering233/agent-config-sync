# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 依赖顺序

```text
A2（图标） ──可选──▶ A1（托盘 + 自动上行）
B1（版本显示）       独立
C1（Windows CI）     独立
```

A1 不依赖 A2，但托盘资产优先用 A2 的正式图标；A1 与 B1 可并行，但不要在同一 worktree 里同时改 `doctor`/`commands`。

## A1 托盘常驻与 WebDAV 定时自动上行

- 前置：`docs/design/00-master-design.md` Q3 已改为“可选托盘常驻 + 定时自动上行”；本卡实现该决策。
- 范围：
  - `tauri` 开启 `tray-icon`；新增 `tauri-plugin-single-instance`、`tauri-plugin-autostart`，只在桌面端注册。
  - 关闭主窗口隐藏到托盘；托盘菜单至少“打开 / 立即同步 / 暂停自动同步 / 退出”；退出必须真正终止进程。
  - 单实例：第二次启动只唤回已有窗口，不创建第二条同步线程。
  - 自动同步：启动后延迟一次 + 固定间隔（默认 30 分钟）；只上行，不自动下载/应用；默认不包含 `auth.json`，设置里可显式开启。
  - 与手动上传/下载/应用共用一个互斥锁；自动任务拿不到锁就跳过本次，不排队。
  - 远端 fingerprint 变化时停止并显示“云端有更新”，不自动覆盖；失败只记录状态与错误，不自动重试。
  - 状态展示：托盘 tooltip/菜单 + 主窗口状态；失败不弹阻塞对话框。
  - 配置：自动同步开关、间隔、是否包含 `auth.json`、开机自启；默认全部保守。
  - 平台：Linux AppIndicator 只出菜单；macOS 用 template 图标与 Accessory 策略；Windows 隐藏窗口后不占任务栏。
  - 文档：补 `docs/design/00`、`architecture.md`、`40-webdav.md` 的自动上行边界与线程/锁说明。
- 验证：
  - 单实例、关闭到托盘、退出、开机自启各一次。
  - 自动上行只传变化文件；冲突/不可达/未配置凭据时停下并显示原因。
  - 自动上行不传 `auth.json`（默认）；开启后才传，且沿用 HTTPS 闸门。
  - 自动任务与手动上传同时触发时，只有一个执行。
  - `cargo test`、`cargo fmt -- --check`、`npm run build`。
- 回滚：移除插件与调度线程，配置字段按缺省忽略；不改变手动上传/下载语义。

## A2 产品图标与托盘图标

- 范围：
  - 定稿一个 1024×1024 SVG：双档 + 双向箭头；绿色圆角底 `#2f6f4f`，浅色前景 `#f5f6f3`。
  - 生成 `src-tauri/icons/` 全套 PNG/ICO/ICNS；删除占位图与 `icon-1024.png` 的非 CLI 残留。
  - 增加托盘用透明底彩色图与 macOS 单色 template 图。
  - 更新 `docs/design/build.md` 的“占位图”说明与图标生成命令。
- 验证：16/32/64/128/512/1024 逐尺寸目视；Windows 托盘、Linux 托盘、macOS 菜单栏各一次；`npm run build` 不受影响。
- 回滚：还原旧 `icons/` 与配置文件即可，无逻辑依赖。

## B1 Pi 与插件最新版本显示

- 前置决策：当前架构写“出站网络只有 WebDAV”；本卡需要允许“手动触发、经 npm CLI、10 秒超时、不重试”的版本查询，先更新 `docs/design/00` 与 `architecture.md`。
- 范围：
  - Doctor 增加 `pi.latest`：`npm view @earendil-works/pi-coding-agent version`，与本地 `pi --version` 比较。
  - 插件最新版本：解析 `settings.json` 的 `packages` 与 `pi list` 实装；只查 npm 源，git/URL/本地路径标“不适用”。
  - 只比较精确 `x.y.z`；范围版本标“未固定”，不引入 semver crate。
  - 404/401/未发布包标“未知/未发布”，不算失败；超时/离线保留旧缓存。
  - 缓存 `versions-cache.json` 于应用配置目录，TTL 6 小时；默认读缓存，手动“检查更新”才刷新；并发 2。
  - UI：Doctor 宿主版本行显示“本地 / 最新”；扩展包列表显示“实装 / 最新”。
- 验证：假 Runner 覆盖成功、超时、404、非 npm 源；离线读缓存；不因单个包失败中断整批；`cargo test`、`npm run build`。
- 回滚：删除查询入口与缓存读取，Doctor 恢复只显示本地版本。

## C1 Windows CI 打包核验

- 现状：`.github/workflows/desktop.yml` 已有 `windows-latest` job（`cargo test` → NSIS → Release/artifact）。
- 范围：
  - 用 `workflow_dispatch` 对一个测试 tag 跑一次，确认 `*-setup.exe` 产出并挂到 Release。
  - 如需 PR 阶段发现 Windows 编译问题，再在 `ci.yml` 增加 `windows-latest` 的 `cargo check`/`cargo test`；不在 PR 上打包。
  - 更新 `docs/design/build.md` 的 CI 现状说明（若新增 job）。
- 验证：Release 资产存在且能下载；安装、启动、导出、WebDAV 各一次。
- 回滚：仅 CI 配置，回退 workflow 即可。

## 暂不建卡

- OAuth 只同步 `api_key`：当前按“全量同步 + 警告”运行，等实际多机冲突出现再评估。
- 一次性明文导出确认：当前每次导出都显示警告，先观察是否需要降低打扰。
- 自动下载/自动应用：与删除逐项确认、`plan_stale` 冲突，不建卡。