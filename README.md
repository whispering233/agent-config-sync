# agent-config-sync

Pi CLI 配置的换机交付工具：把配置打包成一个**可校验的快照包**（zip，按时间戳命名，远端保留最近 20 份），经云盘文件夹或 WebDAV 带到另一台机器，再**安全落地**——校验哈希、预览差异、确认删除、备份回滚、按声明重装插件；密钥随包明文自包含。

当前版本不是实时同步客户端：不监听文件系统、不自动下载或应用。关闭窗口后应用驻留托盘，可按间隔只执行自动上行；上传、下载与应用仍由用户手动发起。

## 能力

| 模块 | 做什么 |
| :--- | :--- |
| 设置 | 选择传输方式（本地文件夹 / WebDAV）与宿主目录；WebDAV 凭据单独保存；自动上行间隔、是否包含 `auth.json`、本机备份保留数、开机自启 |
| 环境检测 | 探测 node/npm/pi/凭据/包/断链；安装或升级 Pi（确认后执行）；「刷新技能」显式启动宿主 RPC，可能新建空 `auth.json`/`models-store.json` |
| 版本显示 | 点「检查更新」查询 Pi 与插件 npm 最新版；结果缓存 6 小时，不后台联网 |
| 快照包 | 按 manifest 声明的受管根（宿主目录，源机存在时加 `~/.agents/skills`）收集，打包成单个 zip（内含 `manifest.json` 与 `snapshot/`，外部根落在 `snapshot/external/agents-skills/`）、原样保存密钥与 `auth.json`、包名带时间戳与指纹前 8 位；存储只保留最近 20 份 |
| 校验 | 解包后校验 manifest 与逐文件哈希，按受管根生成新增/修改/删除计划 |
| 历史包 | 列出远端/本地快照包、指定任意一份恢复、多包对比（哪份会删掉什么）、清理上传残留 |
| 应用 | 计划过期保护、删除逐项确认（外部根逐项）、备份、原子写、按根链接重建；快照没声明的陈旧链接只删链接本身 |
| 回滚 | 列出备份、还原前再备份、按 restore.json 反做 |
| 密钥 | 只读扫描明文密钥与 `auth.json`，导出/应用时告警 |
| 包对齐 | 比对 `settings.json` 声明与 `pi list` 实装，安装缺失项 |
| 托盘常驻 | 关闭窗口隐藏；菜单打开/立即同步/暂停自动同步/退出；单实例；开机自启 |
| 应用更新 | 启动时与设置页检查 GitHub Releases，展示版本与说明；确认后下载完整安装包、安装并重启；签名与版本绑定，失败不重试、不降级 |

## 安装

Linux：

```bash
# deb（推荐）
sudo dpkg -i "src-tauri/target/release/bundle/deb/Agent Config Sync_0.0.4_amd64.deb"

# AppImage（不安装）
chmod +x "src-tauri/target/release/bundle/appimage/Agent Config Sync_0.0.4_amd64.AppImage"
"./Agent Config Sync_0.0.4_amd64.AppImage"
```

文件名含空格，命令需要引号。deb 依赖 `libwebkit2gtk-4.1-0` 与 `libgtk-3-0`。

也可以直接下载 Releases 里的资产：`agent-config-sync-<版本>-linux-x86_64.AppImage`、`agent-config-sync-<版本>-linux-amd64.deb`、`agent-config-sync-<版本>-windows-x86_64-setup.exe`（旁边都带同名 `.sig`）。从 0.0.4 起应用可以自己检查并安装新版本；首次带更新能力的版本仍需手动装一次。

Windows：运行 `src-tauri/target/release/bundle/nsis/Agent Config Sync_<版本>_x64-setup.exe`，按当前用户安装、不需要管理员。安装包未签名，首次运行会弹 SmartScreen：点「更多信息 → 仍要运行」。

Windows 上命令执行走 `cmd /d /s /c`，环境检测与包对齐按逐命令探测；宿主安装仍只走 npm。子进程不弹控制台窗口。

## 第一次使用

1. 二选一：装一个文件夹同步工具（坚果云、OneDrive、Syncthing 均可）拿到本地目录；或直接准备 WebDAV 地址与应用密码（坚果云在「账户设置 → 安全设置 → 第三方应用管理」生成）。
2. 应用 → 设置：传输方式选本地文件夹（同步文件夹选 `<同步目录>/PiAgentSync`）或 WebDAV（填地址、邮箱、应用密码，可先「测试连接」）；宿主目录通常留空（生效 `~/.pi/agent`，或 `PI_CODING_AGENT_DIR`）。
3. 环境检测 → 开始检查；需要时安装/升级 Pi、对齐包，或点「刷新技能」查看宿主实际加载的技能。
4. 快照 → 导出（本地模式等云盘传完；WebDAV 模式点「上传到 WebDAV」，只传一个包）。
5. 另一台机器指向同一目录或同一 WebDAV 账号 → 选一份快照包（默认最新，也可挑历史的一份）→ 检查 → 勾选删除项 → 应用 → 按提示装包。

## 不做什么

- 只支持 WebDAV 一种协议，不做 OAuth 云盘账号直连；WebDAV 凭据明文单独存本机（Linux/macOS 为 0600 文件，Windows 依赖用户配置目录的继承 ACL）。
- 不做文件监听与自动应用；托盘常驻的后台任务只执行定时上行，默认不包含 `auth.json`。
- 不做冲突自动合并。远端包只增不改，因此没有「谁覆盖谁」的问题：两台机器各推自己的包，由你在历史列表里选。云盘冲突副本只做只读扫描（处理动作在 backlog）。
- 只纳管用户级宿主目录与 `~/.agents/skills`；项目级目录与 `settings.json` 的 `skills` 指向的其它外部路径不纳管。
- 不做模型路由、代理、用量统计、会话管理。
- 不自动安装 Node，不修改 shell 启动文件，不代写 OAuth。
- 快照明文包含 `auth.json` 与配置文件里的字面密钥；不做引用化、不整体加密。WebDAV 一律只接受 HTTPS（本机回环地址可以用 http）。

## 价格与风险

- 安装包未签名，只适合自己本机安装；转发前需补签名。
- 应用更新的信任链是 minisign：公钥固定在应用里，私钥只在发布环境；换私钥必须同时换公钥并发新版本，否则已安装版本会拒绝新包。
- Windows 的 NSIS 可在本机或 CI 构建；正式发布由 CI 的 `desktop.yml` 构建并挂到 Release。
- 快照含明文密钥：云盘服务商、WebDAV 服务端及其版本历史/备份可见。OAuth 多机共享 refresh token 可能互相失效；自动上行默认不包含 `auth.json`。
- WebDAV 凭据明文存于应用数据目录的 `webdav.json`，不进快照。Linux 为 `~/.local/share/dev.agentconfigsync.app/`（0600），Windows 为 `%APPDATA%\dev.agentconfigsync.app\`（继承 ACL）。
- 应用每次落地前都会备份；备份目录在同一个应用数据目录下（Linux 0700）。
- 链接优先按快照重建；没有建链接权限时（Windows 默认权限）退化为把内容写成真实文件并提示。
- 快照格式已升到 v3（单个 zip 包）。0.0.2 及更早版本产出的快照不再被读取，升级后在源机重新导出一份即可；旧目录残留只列在「无法识别的文件」里，不会被当成包。
- 首次在真实配置上应用前，建议先用临时目录做一次演练。

## 开发

```bash
npm install
npm run tauri -- dev           # 桌面窗口
npm run dev                    # 仅前端（浏览器预览，命令降级）
cd src-tauri && cargo test     # Rust 测试
npm run build                  # 前端构建
npm run tauri -- build --bundles deb,appimage   # Linux
npm run tauri -- build --bundles nsis          # Windows（只能在 Windows 上构建）
```

构建与分发的完整说明见 [docs/design/build.md](docs/design/build.md)。

## 文档

设计与决策以 `docs/` 为准，入口见 [docs/docs-map.md](docs/docs-map.md)。

- 总体设计与决策：`docs/design/00-master-design.md`
- 快照与落地：`docs/design/10-snapshot-sync.md`
- 环境检测与安装：`docs/design/20-env-doctor.md`
- 密钥策略：`docs/design/30-secrets.md`
- WebDAV 传输：`docs/design/40-webdav.md`
- 应用自身更新：`docs/design/50-app-update.md`
- 架构：`docs/design/architecture.md`
- 构建与分发：`docs/design/build.md`
- 遗留项：`docs/design/backlog.md`

## 许可证

MIT，见 [LICENSE](LICENSE)。
