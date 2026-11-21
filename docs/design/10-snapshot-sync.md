# 快照与同步

## 1. 前提：传输由传输层负责

快照格式与落地语义不区分传输方式。两种模式：

| 模式 | 谁负责"文件到达" | 说明 |
| :--- | :--- | :--- |
| 本地文件夹 | 云盘客户端 | 保留；可离线使用 |
| WebDAV | 本应用 | 协议、凭据与冲突见 `40-webdav.md` |

以下事实只适用于**本地文件夹模式**，即云盘客户端提供的能力与限制：

| 特性 | 事实 | 对本设计的影响 |
| :--- | :--- | :--- |
| 同步范围 | 可接管任意本地文件夹，双向同步 | 快照目录放在云盘同步文件夹内即可 |
| 冲突 | 保留两份，本地那份改名（Windows `xxx-冲突.ext`，macOS/Linux `xxx-NSConflict.ext`），副本会同步到所有设备 | 必须扫描、展示、人工处理 |
| 删除 | 双向传播 | 应用快照时必须显式确认删除 |
| 历史版本 | 云端保留，期限随套餐 | 只能作为兜底，本应用不依赖 |
| 忽略规则 | 每台设备单独配置，不随账号同步 | 新机器需要重新配置；本设计不依赖客户端忽略规则 |
| 状态查询 | 无公开 API | 无法知道"是否传完"；用 manifest 完整性替代 |

## 2. 目录布局

```text
<同步文件夹>/PiAgentSync/
├── snapshot/                  # 允许跨机的"可同步视图"
│   ├── settings.json
│   ├── models.json
│   ├── mcp.json
│   ├── auth.json            # 明文凭据，权限 0600
│   ├── keybindings.json
│   ├── <其他根级 *.json>      # 扩展私有配置，如 web-search.json
│   ├── AGENTS.md
│   ├── SYSTEM.md
│   ├── APPEND_SYSTEM.md
│   ├── skills/
│   ├── extensions/
│   ├── prompts/
│   ├── themes/
│   └── agents/
└── manifest.json              # 最后写入
```

WebDAV 模式的同步根固定为 `<服务地址>/PiAgentSync/`，其内部结构与本地模式一致（见 `40-webdav.md`）。

快照目录本身不包含：`sessions/`、`npm/`、`install/`、缓存与状态文件；只包含宿主目录根级的 `auth.json`，子目录同名文件仍排除。

## 3. 受管范围（白名单 / 排除）

### 纳入

| 项 | 说明 |
| :--- | :--- |
| 根级 `*.json` | 含扩展私有配置（实测 `web-search.json`、`lmstudio.json` 均位于根级），逐个枚举无法覆盖新插件 |
| 根级 `auth.json` | Pi 的凭据文件；明文进入快照，权限 `0600` |
| `AGENTS.md` / `SYSTEM.md` / `APPEND_SYSTEM.md` | 指令与系统提示 |
| `keybindings.json` | 偏好，无密钥 |
| `skills/` `extensions/` `prompts/` `themes/` `agents/` | 资源目录 |

### 排除

| 项 | 实测体量 | 理由 |
| :--- | :--- | :--- |
| 子目录中的 `auth.json` | — | 只纳入根级；扩展自带的同名文件不碰 |
| `sessions/` | 1.6 GB / 2962 文件 | 隐私与体积 |
| `install/` | 313 MB | 宿主自身安装，平台相关 |
| `npm/` | 285 MB / 17138 文件 | 包本体，跨平台会损坏；对端改为重装 |
| `models-store.json` | 132 KB，持续变化 | 模型目录缓存 |
| `trust.json` | 绝对路径，机器绑定 | 安全边界 |
| `web-push.json` | 含设备私钥 | 设备专属 |
| `fff/` `web-search-cache/` `missions/` `pi-subagents/` `pi-web-access/` `run-history.jsonl` | — | 状态、缓存、会话数据 |
| `pi-web-session-*.json` | — | 会话索引 |

排除项以"名称/路径规则"匹配，不按大小判断。规则集中在一处定义，UI 可展示但不可编辑（避免用户把 `sessions/` 加进来）。

### 符号链接

- 导出：**物化复制**目标内容，并在 manifest 的 `symlinks` 中记录 `{path, target}`。
- 导入：目标存在时**按 manifest 重建链接**；目标缺失、断链或平台不允许建链接（Windows 默认权限下 `ERROR_PRIVILEGE_NOT_HELD`）则退化为**整棵子树物化**并告警：按快照 `<sync_root>/snapshot/<相对路径>` 递归建目录、逐个写普通文件，物化本身失败才 `apply_failed`。计划是按链接视角算的，所以「链接之下」的写入与删除一律跳过（含建不成但已物化的目录）；被跳过的已勾选删除要逐条说明原因并汇总条数。
- Windows：按目标类型分别调 `symlink_file` / `symlink_dir`；junction 与符号链接都算链接类（判定为 `read_link` 能读到目标）。
- 快照没声明的链接（导出后新增的陈旧链接）：不读取目标、不参与内容比较，只在计划里列为删除候选（`kind: link`）；确认后只删链接本身，目标内容不动。嵌在另一个待删链接里的链接、以及下面有 manifest 内容的链接都不列。
- `auth.json` 例外：即使 manifest 里是链接，也不按链接重建，而是按普通文件写入，避免凭据指向受管范围外。
- 断链：导出前检测，断链仅警告并跳过该条目。

## 4. manifest

```json
{
  "formatVersion": 2,
  "profileId": "default",
  "createdAt": "2026-11-20T05:30:00Z",
  "fingerprint": "a1b2c3d4e5f6",
  "source": {
    "app": "0.0.1",
    "os": "linux",
    "arch": "x64",
    "pi": "1.5.2",
    "node": "22.23.0"
  },
  "requirements": { "node": ">=22.19.0", "pi": ">=1.5.0" },
  "files": [
    { "path": "snapshot/settings.json", "sha256": "…", "size": 1234, "kind": "json" }
  ],
  "symlinks": [{ "path": "snapshot/skills/antd", "target": "../../../.agents/skills/antd" }],
  "requiredPackages": [{ "source": "npm:pi-web-access", "enabled": true }],
  "plaintextSecrets": true
}
```

- `source.pi` / `source.node`：导出时按环境检测同一组探测取版本（能解析就规范成 `x.y.z`），探测不到记 `null`；只作来源记录，不参与指纹与冲突判定。
- `fingerprint`：对 `formatVersion` 与所有 `{path, sha256}` 排序后取哈希，显示前 12 位。用途：两台机器肉眼核对"是不是同一份快照"。
- manifest **最后写**：文件先落盘，manifest 落盘即代表快照完整。
- 未知或过期的 `formatVersion` 一律拒绝读取，不尝试兼容；v1 快照需在源机升级后重新导出。
- `plaintextSecrets: true` 表示快照包含 `auth.json` 或扫描到高危明文，导入端据此告警。

## 5. 导出（上传快照）

```text
1. 预检
   ├─ 快照根目录存在且可写；不在受管范围内（禁止快照目录嵌进 agent 目录）
   ├─ 宿主进程提示："Pi 正在运行，配置可能继续变化"（仅提示，不阻断）
   └─ 应用配置已记录 agent 目录
2. 收集        按受管范围遍历；符号链接物化；断链跳过并记录
3. 扫描        对全部文本文件做只读密钥扫描；命中只记录，不改写、不阻断
4. 权限        快照目录 0700；根级 auth.json 强制 0600
5. 写文件      先写临时目录，全部成功后再替换 snapshot/
6. 写 manifest 最后写；计算并记录 fingerprint
7. 完成        展示 fingerprint 与文件数，提示"等待云盘上传完成"
```

- 传输层为 WebDAV 时，第 5/6 步换成"本地暂存 → 增量 PUT → manifest 最后 PUT"；步骤顺序与判定不变，失败码见 `40-webdav.md`。
- 导出是**全量替换**：先写临时目录再整体替换，避免半套快照。
- 不删除同步根目录下的其他文件（将来分享包等产物可能共存）。
- 任何一步失败：删除临时目录，保留原快照不动。

## 6. 导入（从快照应用）

```text
1. 读取 manifest
   ├─ 不存在 → 停止：云盘尚未同步完成，或路径错误
   ├─ formatVersion 不支持 → 停止
   └─ 哈希逐文件校验 → 任一不匹配 → 停止（半套快照）
2. 环境检查
   ├─ node / pi 版本是否满足 requirements → 不满足：警告 + 允许继续（记录）
   ├─ 明文密钥扫描 → 命中：警告 + 允许继续
   ├─ 快照含 auth.json → 警告：会覆盖本机凭据
   └─ 目标目录可写
3. 生成计划     新增 / 修改 / 删除（删除仅在受管范围内）
4. 预览         展示每项差异与来源（diff 文本摘要 + 大小 + 时间）
   ├─ 删除项默认不勾选，需逐项确认
   └─ 展示 requiredPackages 待办
5. 应用前复核   重算计划并按 fingerprint 比对；不一致 → `plan_stale`，整次拒绝
   备份         把将被覆盖/删除的文件与祖先链接复制到本机备份目录
6. 落地         重建链接 → 写入/删除（链接之下的项跳过）；沿用原文件权限位（仅 POSIX）；失败即停并保留备份
                 例外：链接建不成时按整棵子树物化并告警（不是失败），物化也失败才停
7. 收尾
   ├─ 包对齐：按 requiredPackages 调用宿主安装命令（需确认）
   └─ 提示重启宿主或 /reload
```

- 传输层为 WebDAV 时，第 1 步先取远端 manifest，再按清单把文件取到本机暂存目录；其后的校验、计划、预览与应用完全复用本节流程。

### 删除判定

删除候选 = **受管范围内、存在于 live、不存在于快照**的文件。

- 不需要本机历史状态；因此**本机新加、尚未导出的受管文件也会出现在删除候选里**。这是有意的：必须让人看见。
- 删除项默认不勾选；不勾选即保留。
- 受管范围之外的文件永不删除。

### 状态机

```text
idle → exporting → verify-self → ready-to-upload
idle → reading → verifying → planning → previewing → backing-up → applying → finishing
                                    ↓            ↓
                                  cancel       failed（保留备份，可回滚）
```

## 7. 备份与回滚

```text
<app-data>/backups/<UTC 时间戳>/
├── files/<相对路径>       # 将被覆盖或删除的现有文件内容
└── restore.json           # { version, createdAt, fingerprint, entries[] }
```

- `entries`：`{ path, kind: file|symlink|dir, existed, mode, linkTarget }`。符号链接只记录目标，不复制内容；`existed: false` 表示该路径是应用新增的。`mode` 为 `null` 表示平台不提供权限位（Windows）或条目无需权限（链接、目录）。
- 位置在本机应用数据目录（**不放云盘**）；目录权限 `0700`。
- 覆盖/删除前备份；被物化替换的祖先链接也会记录，便于回滚恢复链接。
- 保留最近 10 份，超出清理最旧。
- 回滚：按 `restore.json` 反做（文件写回并恢复 mode、链接重建、新增路径删除），**还原前再备份一次当前状态**；回滚不触碰快照。路径越界或备份损坏时整次拒绝。`mode` 为 `null` 时跳过权限还原，不报错。
- 回滚重建链接失败（Windows 默认权限）时，按记录的链接目标物化成本地副本并告警——目标还在，内容不会丢；与 apply 建不成链接时的回退一致。

## 8. 冲突文件

- **已实现（只读）**：扫描受管范围内匹配 `*-冲突*` 与 `*-NSConflict*` 的文件，与对应原文件配对，输出行数、差异行数、首处差异行号；孤儿副本、二进制、超 1 MB、不可读单独标记。
- **未实现**：处理动作与 UI（保留原文件 / 用冲突副本替换 / 暂不处理），见 `backlog.md`。
- 原则：不自动合并，不自动删除。

## 9. 失败码

| code | 含义 |
| :--- | :--- |
| `manifest_missing` / `manifest_corrupt` / `manifest_unsupported` | 快照缺失、无法解析或格式版本不支持 |
| `hash_mismatch` | 快照文件与 manifest 不一致 |
| `plan_stale` | 应用前重算的指纹与用户确认时不同 |
| `delete_not_in_plan` | 请求删除的路径不在本次计划或受管范围内 |
| `delete_through_symlink` | 待删路径经过非 manifest 记录的符号链接 |
| `no_write_permission` / `backup_failed` | 目标不可写 / 备份失败，已中止 |
| `apply_failed` | 落地中途失败，已停止；可用备份回滚 |
| `restore_failed` / `backup_invalid` / `backup_missing` | 回滚失败或备份不可用 |
| `host_version_unsatisfied` | 宿主版本低于要求（警告） |
| `root_not_allowed` | 计划或应用指定的目录既不是同步文件夹，也不在本机暂存目录内 |
| `remote_not_configured` | 未填地址/用户名/密码 |
| `remote_unreachable` | DNS、TLS、超时或 5xx |
| `remote_auth_failed` | 远端 401/403（凭据无效或权限不足） |
| `remote_quota` | 远端 507，或 403 且提示配额 |
| `remote_manifest_corrupt` | 远端 manifest 无法解析或格式版本不支持 |
| `remote_not_found` | 下载时远端没有 manifest |
| `remote_changed` | 提交 manifest 时远端已被改写 |
| `remote_insecure` | 快照含 `auth.json` 时使用非 HTTPS 的 WebDAV（localhost 例外） |

## 10. 已知限制

- 全量替换，不传增量（配置总量约 100 KB，增量无意义）。
- 不感知云盘同步进度，只能靠 manifest 与 fingerprint。
- 快照文件保持原字节（含格式、注释与键序）；导出不做 JSON 重写。
- `auth.json` 整文件覆盖，不按 provider 合并；本机独有登录由覆盖前备份兜底。
- 计划只读取快照声明的链接；快照没声明的链接整棵跳过，不读取目标，也不把目标内容列为删除候选。
- 跨平台路径差异（扩展内绝对路径、MCP `cwd`）不做翻译，只在预览中提示。
- 远端上传是"整份 manifest + 变更文件"，不做断点续传与限流退避。
- 远端不被当前快照引用的旧文件不自动删除，也不会被下载端应用。
