# agent-config-sync

Pi CLI 配置的换机交付工具：把配置导出成**可校验的快照**，经云盘文件夹或 WebDAV 带到另一台机器，再**安全落地**——校验哈希、预览差异、确认删除、备份回滚、补齐密钥、按声明重装插件。

它不是实时同步客户端：只在手动触发时上传或应用，不常驻监听、不自动双向同步。

## 能力

| 模块 | 做什么 |
| :--- | :--- |
| 设置 | 选择传输方式（本地文件夹 / WebDAV）与宿主目录；WebDAV 凭据单独保存；路径校验 |
| 环境检测 | 探测 node/npm/pi/凭据/包/断链；安装或升级 Pi（确认后执行） |
| 快照 | 按受管范围导出、密钥引用化、写 manifest 与指纹 |
| 校验 | 校验 manifest 与逐文件哈希，生成新增/修改/删除计划 |
| 应用 | 计划过期保护、删除逐项确认、备份、原子写、链接重建 |
| 回滚 | 列出备份、还原前再备份、按 restore.json 反做 |
| 密钥 | 列出 requiredSecrets、检查环境变量、写入本机字面值 |
| 包对齐 | 比对 `settings.json` 声明与 `pi list` 实装，安装缺失项 |

## 安装（Linux）

```bash
# deb（推荐）
sudo dpkg -i "src-tauri/target/release/bundle/deb/Agent Config Sync_0.0.1_amd64.deb"

# AppImage（不安装）
chmod +x "src-tauri/target/release/bundle/appimage/Agent Config Sync_0.0.1_amd64.AppImage"
"./Agent Config Sync_0.0.1_amd64.AppImage"
```

文件名含空格，命令需要引号。deb 依赖 `libwebkit2gtk-4.1-0` 与 `libgtk-3-0`。

## 第一次使用

1. 二选一：装一个文件夹同步工具（坚果云、OneDrive、Syncthing 均可）拿到本地目录；或直接准备 WebDAV 地址与应用密码（坚果云在「账户设置 → 安全设置 → 第三方应用管理」生成）。
2. 应用 → 设置：传输方式选本地文件夹（同步文件夹选 `<同步目录>/PiAgentSync`）或 WebDAV（填地址、邮箱、应用密码，可先「测试连接」）；宿主目录通常留空（生效 `~/.pi/agent`，或 `PI_CODING_AGENT_DIR`）。
3. 环境检测 → 开始检查；需要时安装/升级 Pi 或对齐包。
4. 快照 → 导出（本地模式等云盘传完；WebDAV 模式点「上传到 WebDAV」）。
5. 另一台机器指向同一目录或同一 WebDAV 账号 → 检查/下载快照 → 勾选删除项 → 应用 → 按提示补密钥/装包。

## 不做什么

- 只支持 WebDAV 一种协议，不做 OAuth 云盘账号直连；WebDAV 凭据以 0600 明文存本机。
- 不做后台常驻、文件监听、自动应用。
- 不做冲突自动合并；云盘冲突副本只做只读扫描（处理动作在 backlog），WebDAV 冲突在应用内选「覆盖云端」或「从云端更新」。
- 不做模型路由、代理、用量统计、会话管理。
- 不自动安装 Node，不修改 shell 启动文件，不代写 OAuth。
- 不把密钥写入快照：只保留 `$NAME` / `${NAME}` 引用，`auth.json` 永不进入快照。

## 价格与风险

- 安装包未签名，只适合自己本机安装；转发前需补签名与正式图标。
- WebDAV 凭据明文存于 `~/.local/share/dev.agentconfigsync.app/webdav.json`（权限 0600），不进快照。
- 应用每次落地前都会备份；备份目录：`~/.local/share/dev.agentconfigsync.app/backups/`。
- 首次在真实配置上应用前，建议先用临时目录做一次演练。

## 开发

```bash
npm install
npm run tauri -- dev           # 桌面窗口
npm run dev                    # 仅前端（浏览器预览，命令降级）
cd src-tauri && cargo test     # Rust 测试
npm run build                  # 前端构建
npm run tauri build -- --bundles deb,appimage
```

构建与分发的完整说明见 [docs/design/build.md](docs/design/build.md)。

## 文档

设计与决策以 `docs/` 为准，入口见 [docs/docs-map.md](docs/docs-map.md)。

- 总体设计与决策：`docs/design/00-master-design.md`
- 快照与落地：`docs/design/10-snapshot-sync.md`
- 环境检测与安装：`docs/design/20-env-doctor.md`
- 密钥策略：`docs/design/30-secrets.md`
- WebDAV 传输：`docs/design/40-webdav.md`
- 架构：`docs/design/architecture.md`
- 构建与分发：`docs/design/build.md`
- 遗留项：`docs/design/backlog.md`
