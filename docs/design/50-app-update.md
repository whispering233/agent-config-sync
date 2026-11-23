# 应用自身更新

## 1. 定位

只更新本应用自己的程序文件，不更新 Pi 宿主、Pi 包、快照包或用户配置。v1 采用**全量安装包**：检查到新版本后下载对应平台的完整安装包，由用户确认后安装并重启。

增量更新（delta / 差分包）不做，登记在 `backlog.md`。桌面壳保持 Tauri，不为此切换 Electron。

## 2. 决策

| # | 决策 | 结论 | 理由 |
| :--- | :--- | :--- | :--- |
| U1 | 更新源 | GitHub Releases 的 `latest.json`，仓库 public，不塞 token | 无服务端；GitHub 的 `releases/latest` 已排除草稿与预发布 |
| U2 | 更新组件 | 官方 `tauri-plugin-updater`（>= 2.14），Rust 侧包装成自己的 command | 复用官方检查、下载、安装、签名校验；2.14 起安装前校验下载字节；不引入 Node sidecar |
| U3 | 检查时机 | 启动后异步检查一次 + 设置页手动检查；不做定时轮询 | 启动检查只发一个请求；失败不打扰；手动入口给明确回应 |
| U4 | 安装时机 | 用户确认后下载并安装；不静默安装、不随退出自动装 | 避免更新打断快照操作；用户能选择重启时机 |
| U5 | 平台范围 | Linux AppImage、Linux deb、Windows NSIS；macOS 未分发，不做 | 与现有打包范围一致 |
| U6 | 安全 | minisign 签名 + `requireSignedVersion` + 禁止降级 | `latest.json` 本身不签名，签名必须绑定版本，防止降级攻击 |
| U7 | 网络边界 | 检查 10 秒、下载 120 秒；不自动重试；失败停在当前版本 | 与现有网络与失败语义一致 |
| U8 | 并发 | 安装前取操作锁并暂停自动上行；下载阶段不写 live | 安装会替换程序文件，不能和快照/上行并发 |
| U9 | 开发态 | 浏览器预览与 `tauri dev` 不检查更新 | 开发版本不是发布版本，避免误报与无意义请求 |
| U10 | 发布 | CI 生成 `.sig` 与 `latest.json`，资产先传、`latest.json` 最后传 | 正式名出现即代表完整；避免客户端看到半套资产 |

## 3. 流程

```text
启动/手动检查
  → 请求 latest.json（10s 超时，不重试）
  → 无更新：记录当前状态，不打扰
  → 有更新：展示版本、说明、平台与体积
  → 用户点“下载并安装”
  → 重新检查并确认版本未变（update_stale）
  → 下载完整安装包（120s 超时）
  → 取操作锁、暂停自动上行
  → 安装并重启
  → 失败：停在当前版本，报告失败码；不自动重试、不降级
```

- 检查与安装是两次独立动作；安装前重新检查，避免用户看到的信息与实际资产不一致。
- 下载阶段不写 live，也不改配置；安装只替换程序文件。
- 用户数据（配置、WebDAV 凭据、备份、暂存、远端状态）不因更新而移动或删除。
- 更新失败不影响当前版本继续使用。

## 4. 平台策略

| 平台/格式 | 检查 | 安装 | 备注 |
| :--- | :--- | :--- | :--- |
| Linux AppImage | ✅ | ✅ 替换 AppImage 后重启 | 需要 AppImage 文件可写；只读目录/系统级安装会失败 |
| Linux deb | ✅ | ✅ 通过系统提权安装 `.deb` | 等价于 `dpkg -i`，不经过 apt 依赖解析；UI 明示，失败提示手动下载 |
| Windows NSIS | ✅ | ✅ 静默/被动安装后重启 | 安装范围沿用 currentUser；首次可能出现 UAC/SmartScreen 提示 |
| macOS | 不做 | 不做 | 未分发；后续需要签名与公证 |

- deb 安装会弹系统授权框；用户取消或权限不足时按 `update_install_failed` 处理，不自动改走手动命令。
- AppImage 缺少写权限时按 `update_install_failed` 处理，并提示把 AppImage 放到可写目录。
- Windows 更新前应用会退出；托盘驻留不阻止更新，但安装前必须先释放操作锁。

## 5. UI

- 设置页新增「应用更新」卡片：当前版本、检查按钮、检查结果、最新版本、发布说明、下载进度、安装按钮、失败信息。
- 启动检查发现新版本时，在应用壳顶部显示一条非模态提示；点击进入设置页更新卡片。没有更新时不显示任何常驻横幅。
- 浏览器预览与开发态显示「仅发布版可用」，不发起请求。
- 安装按钮在下载完成前不可用；下载期间显示已下载/总量（拿不到总量时只显示已下载）；失败后允许重新检查，不自动重试。
- 不引入新的 UI 库；复用现有 `Card`、`Button`、`lucide-react` 图标与 Tailwind token。

## 6. 命令与事件

Rust 侧新增 `updater` 模块，前端只经由类型化 command 调用，不直接使用 updater 的 JS 插件。

| command | 入参 | 返回 | 说明 |
| :--- | :--- | :--- | :--- |
| `check_app_update` | 无 | `AppUpdateInfo \| null` | 10s 超时；开发态/浏览器返回 `update_unsupported` |
| `install_app_update` | `{ version }` | `AppUpdateResult` | 重新检查并校验版本；下载、取锁、安装、重启 |

- `AppUpdateInfo`：当前版本、最新版本、发布说明、发布时间、目标平台、下载体积（能拿到时）。
- `AppUpdateResult`：安装是否已发起、版本、是否需要重启；失败时返回失败码与可读信息。
- 进度通过事件推送：`app-update://progress`，载荷为阶段（`downloading` / `installing`）与已下载/总量；事件里不含路径、密钥或安装包内容。
- `install_app_update` 的 `version` 必须与本次重新检查到的版本一致，否则返回 `update_stale`。

## 7. 安全

- 公钥固定在 `tauri.conf.json`；私钥只放 CI secret（`TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`），不进仓库、不进日志、不进快照。
- 开启 `requireSignedVersion`：签名必须绑定版本；拒绝降级与版本不一致的资产。
- `latest.json` 只通过 HTTPS 获取；GitHub 仓库写权限与 2FA 是发布侧信任锚。
- 公钥/私钥丢失会导致已安装用户无法继续更新；私钥必须离线备份。
- 更新日志只记录版本、目标、失败码；不记录下载 URL 的查询参数、凭据或用户路径。
- deb 安装会提权；UI 必须说明这一步等价于系统包安装，不能在用户不知情时执行。

## 8. 失败码

| code | 含义 |
| :--- | :--- |
| `update_unsupported` | 浏览器预览、开发态、未分发平台，或当前安装格式不支持自更新 |
| `update_check_failed` | 请求 `latest.json` 失败、超时或格式不可解析 |
| `update_download_failed` | 下载安装包失败、超时或体积异常 |
| `update_verify_failed` | 签名、版本绑定或资产校验失败 |
| `update_busy` | 有快照/上行/技能盘点等操作持锁，当前不能安装 |
| `update_stale` | 用户确认的版本与重新检查到的版本不一致 |
| `update_install_failed` | 安装器启动失败、用户取消提权、AppImage 不可写等 |

失败即停：不自动重试、不自动降级、不静默改用手动命令。

## 9. 发布

- 更新 endpoint：`https://github.com/whispering233/agent-config-sync/releases/latest/download/latest.json`。
- `bundle.createUpdaterArtifacts` 只在发布构建的配置叠加层开启（`src-tauri/tauri.updater.conf.json`），本地普通构建不需要签名私钥。
- 构建产物旁生成 `.sig`；AppImage、deb、NSIS 各自上传安装包与 `.sig`。
- CI 新增 `manifest` job：等 Linux/Windows 产物齐全后生成 `latest.json`，最后上传。`latest.json` 的 `platforms` 至少包含：
  - `linux-x86_64-appimage`
  - `linux-x86_64-deb`
  - `windows-x86_64-nsis`
- `latest.json` 的 `url` 必须与 Release 上实际资产名逐字一致；生成脚本从实际文件名读取，不手写。上传后逐个探测 URL 可访问。
- 上传顺序：先安装包与 `.sig`，最后 `latest.json`。`latest.json` 出现即代表本次发布可更新。
- 版本号仍按现有三处同步；`latest.json` 的 `version` 必须与它们一致。

## 10. 验收

1. Linux AppImage：从上一版本检查到新版本，下载、签名校验、替换、重启后版本号变化；只读目录下安装失败并给出可读原因。
2. Linux deb：检查、下载、系统授权、安装、重启；取消授权时停在当前版本。
3. Windows NSIS：检查、下载、安装、重启；托盘驻留不阻止更新。
4. 故意破坏签名或 `latest.json` 版本：拒绝安装，当前版本仍可启动。
5. 网络超时/断网：检查或下载失败，不自动重试，当前版本不受影响。
6. 自动上行或快照操作进行中：安装返回 `update_busy`，不替换程序。
7. 浏览器预览与 `tauri dev`：不发起更新请求。
8. 发布侧：每个 Release 有安装包、`.sig` 与 `latest.json`；`latest.json` 的 URL 全部可下载。

## 11. 不做

- 增量更新/差分包：见 `backlog.md`。
- 静默自动安装、退出时自动安装、灰度发布、自动降级。
- 后台定时轮询更新。
- 为更新功能切换桌面壳。
- macOS 更新（未分发）。