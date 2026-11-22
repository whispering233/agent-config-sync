# 快照与同步

## 1. 前提：传输由传输层负责

快照格式与落地语义不区分传输方式。两种模式：

| 模式 | 谁负责"包到达" | 说明 |
| :--- | :--- | :--- |
| 本地文件夹 | 云盘客户端 | 可离线使用；包就是同步文件夹里的文件 |
| WebDAV | 本应用 | 协议、凭据与状态判定见 `40-webdav.md` |

两种模式共用**同一套快照包**：一个按时间戳命名的 zip，内含 `manifest.json` 与 `snapshot/`。差别只在"包放在哪里、怎么搬过去"。

本地文件夹模式仍然受云盘客户端能力限制（下表）。这些事实现在只作用于**包文件**，不再作用于受管配置本身：

| 特性 | 事实 | 对本设计的影响 |
| :--- | :--- | :--- |
| 同步范围 | 可接管任意本地文件夹，双向同步 | 快照包目录放在云盘同步文件夹内即可 |
| 冲突 | 保留两份，本地那份改名（Windows `xxx-冲突.ext`，macOS/Linux `xxx-NSConflict.ext`），副本会同步到所有设备 | 冲突副本的形态变成 `<包名> (冲突).zip`，落进"无法识别的文件"桶；`snapshot/` 内部不再产生成片的冲突副本 |
| 删除 | 双向传播 | 保留清理会删掉同步文件夹里的包，删除会传播到其他机器；四类绝不删（见 §2） |
| 历史版本 | 云端保留，期限随套餐 | 只能作为兜底，本应用不依赖 |
| 忽略规则 | 每台设备单独配置，不随账号同步 | 新机器需要重新配置；本设计不依赖客户端忽略规则 |
| 状态查询 | 无公开 API | 无法知道"是否传完"；用包内的 manifest 与哈希替代 |

## 2. 快照包

### 包内布局

```text
20261122-105230-a1b2c3d4.zip
├── manifest.json              # 见 §4
└── snapshot/
    ├── settings.json
    ├── models.json
    ├── mcp.json
    ├── auth.json              # 明文凭据，权限 0600
    ├── keybindings.json
    ├── <其他根级 *.json>       # 扩展私有配置，如 web-search.json
    ├── AGENTS.md
    ├── SYSTEM.md
    ├── APPEND_SYSTEM.md
    ├── skills/
    ├── extensions/
    ├── prompts/
    ├── themes/
    └── agents/
```

- 包内布局与旧版同步根**逐字节同构**（`manifest.json` + `snapshot/…`），所以解包到暂存目录后可以直接交给计划与应用，落地链路不认识"包"这个概念。
- `manifest.json` 必须位于包根，且在同一个 zip 内：**包自包含**，导出/导入/分享都只有一个文件。
- 包内**只有普通文件与目录条目**。符号链接不写成 zip 的 symlink 条目：链接目标内容已经物化进 `snapshot/`，链接身份只记在 manifest 的 `symlinks` 里。由此不需要 Info-ZIP 的 `S_IFLNK` 约定，也不可能通过归档内容在本机创建符号链接。

### 命名

```text
<UTC yyyymmdd-HHmmss>-<fingerprint 前 8 位>.zip
20261122-105230-a1b2c3d4.zip
```

- **UTC、秒级**：字典序 = 时间序，这是"最新 = 名字最大"可用的前提；本地时区会让不同时区的机器产出无法比较的名字。秒级够用：手动动作人点不快，自动上行最小间隔 10 分钟。UI 展示时换算成本地时间，**名字只给机器用**。
- **fingerprint 前 8 位**：名字即内容标识，同秒不撞名，人工可核对。它**只用于显示与去重提示**，绝不用于安全判定；"内容未变 → 跳过上传"用完整 fingerprint 比对。
- **不写设备名、不写用户标签、不支持重命名**。远端名字只由本应用产生；导入靠包内 manifest 判定，不靠文件名。
- **只认这一个形态**。不符合格式的文件（`.tmp-*`、旧版目录布局、云盘冲突副本、用户手放的文件）一律进"无法识别的文件"：不参与"最新"判定、不参与保留清理、不参与任何自动动作。

### 存储目录布局

```text
<同步根 或 WebDAV 工作根>/
├── 20261122-105230-a1b2c3d4.zip
├── 20261121-091210-7f3e9c01.zip
└── .tmp-<纳秒戳>-<fp8>          # 上传中的临时名，永不当作包
```

- 本地文件夹模式的存储根 = 用户指定的文件夹；WebDAV 模式固定为 `<服务地址>/PiAgentSync/`。
- 包**平铺**，不再有 `snapshot/` 子目录与外部 `manifest.json`（旧版残留会落进"无法识别的文件"）。
- WebDAV 模式下存储根固定为 `PiAgentSync/` 子目录，不落在云盘根（坚果云对根目录下的集合 `DELETE` 返回 403）；本地文件夹模式由用户选目录，应用无法可靠识别“这是不是云盘根”。
- 写入顺序：**先写临时名，再原子改名到正式名**。本地模式是同盘 `fs::rename`；WebDAV 模式是 `PUT .tmp-*` + `MOVE`。正式名下出现的包一定是完整的。

### 历史与保留

- **保留最近 20 份**（硬编码，不做配置项）。旧包按名字倒序丢弃。
- 只在上传（导出）成功后清理；下载不改动远端。
- **三类绝不删**：名字不可解析的（含 `.tmp-*`、旧布局、外来文件）、本次刚上传的包、名字 ≥ 本次包名的份。清理失败不做记忆，下次上传重新列举重算（所以失败项可能在下次被清掉）——「失败就再也不删」会让一次瞬时错误永久占住保留名额。
- 清理会重新列一次远端/目录，不拿"上传前的列表 + 新名字"推算：多一次列举，换掉"两台机器同时上传时按陈旧列表误删对方新包"的风险。
- 清理失败**不阻塞**上传成功：结果里单独上报，下次上传自然重算。不重试、不降级。
- 内容未变时不上传新包（完整 fingerprint 与本机记录相同 → 跳过），空闲期不产生重复历史。

### 解包安全闸门

包来自远端或用户手选的文件，一律按不可信输入处理。**不使用 `zip` 库的批量解压接口**，逐条目校验后写文件：

| 检查 | 规则 | 失败码 |
| :--- | :--- | :--- |
| 顶层白名单 | 每个条目名必须是 `manifest.json`，或以 `snapshot/` 开头 | `package_corrupt` |
| 路径合法 | 拒绝绝对路径、`..` 段、反斜杠、盘符、空段 | `package_unsafe_entry` |
| 落点 | 逐条用受管路径校验写到 `<暂存>/<包名>/` 之内，越界即拒 | `package_unsafe_entry` |
| 条目数 | ≤ 20000 | `package_unsafe_entry` |
| 解压总体积 | ≤ 256 MB（防 zip 炸弹） | `package_unsafe_entry` |
| 包文件体积 | ≤ 64 MB（上传前本地判；下载前看 `content-length`） | `package_too_large` |
| 条目类型 | 只接受普通文件与目录；出现符号链接或特殊类型条目即拒 | `package_corrupt` |
| manifest | 解包后 `manifest.json` 必须存在 | `package_corrupt` |
| 目标目录 | 必须是尚不存在的路径；已存在即拒（不覆盖、不删除） | `package_corrupt` |
| 权限 | 解包后目录 `0700`、文件 `0600`；不套用包内记录 mode | — |

数量级参照：真实受管范围约几十到几百个文件、约 100 KB，上限留了两个数量级余量。

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
- 导入：目标存在时**按 manifest 重建链接**；目标缺失、断链或平台不允许建链接（Windows 默认权限下 `ERROR_PRIVILEGE_NOT_HELD`）则退化为**整棵子树物化**并告警：按快照解压目录的 `<相对路径>` 递归建目录、逐个写普通文件，物化本身失败才 `apply_failed`。计划是按链接视角算的，所以「链接之下」的写入与删除一律跳过（含建不成但已物化的目录）；被跳过的已勾选删除要逐条说明原因并汇总条数。
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
    "app": "0.0.2",
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

- `formatVersion` 保持 **2**：快照包化没有改动 manifest 的结构，变的只是容器与存放方式，那是传输层的概念。`files[].path` 仍带 `snapshot/` 前缀，与包内条目名一致。
- `source.pi` / `source.node`：导出时按环境检测同一组探测取版本（能解析就规范成 `x.y.z`），探测不到记 `null`；只作来源记录，不参与指纹与“是否有更新”的判定。
- `fingerprint`：对 `formatVersion` 与所有 `{path, sha256}` 排序后取哈希，显示前 12 位；包名里用的是前 8 位。用途：两台机器肉眼核对"是不是同一份快照"。
- manifest **随包一起写**，包在正式名下出现即代表快照完整；包内不再有"先文件后清单"的中间态。
- 未知或过期的 `formatVersion` 一律拒绝读取，不尝试兼容。
- `plaintextSecrets: true` 表示快照包含 `auth.json` 或扫描到高危明文，导入端据此告警。

## 5. 导出（生成快照包）

```text
1. 预检
   ├─ 存储根存在且可写；不在受管范围内（禁止存储目录嵌进 agent 目录）
   ├─ 宿主进程提示："Pi 正在运行，配置可能继续变化"（仅提示，不阻断）
   └─ 应用配置已记录 agent 目录
2. 收集        按受管范围遍历；符号链接物化；断链跳过并记录
3. 扫描        对全部文本文件做只读密钥扫描；命中只记录，不改写、不阻断
4. 权限        打包目录 0700；根级 auth.json 强制 0600
5. 打包        写临时目录 → 打成 zip（snapshot/… + manifest.json）→ 落到临时名
6. 定案        临时名原子改到正式名（本地：同盘 rename；WebDAV：PUT + MOVE）
7. 完成        展示包名、fingerprint 与文件数；提示"等待云盘上传完成"（本地模式）
```

- 任何一步失败：删除临时目录与临时名，保留旧包不动。
- 导出是**全量**：每个包都是完整快照，不依赖前一份包。
- 内容未变（完整 fingerprint 与本机记录相同）时跳过上传，不产生新包。
- 不删除存储根下的其他文件（无法识别的文件只展示，由用户显式清理）。

## 6. 导入（从快照包应用）

```text
1. 取包
   ├─ 本地文件夹模式：直接读存储根里的包
   └─ WebDAV 模式：GET 选中的包（默认名字最大的那份）
2. 解包        按 §2 安全闸门解到 <应用数据目录>/staging/<包名>/，失败即删该目录
3. 读取 manifest
   ├─ 不存在 → package_corrupt
   ├─ formatVersion 不支持 → manifest_unsupported
   └─ 哈希逐文件校验 → 任一不匹配 → hash_mismatch（半套快照）
4. 环境检查
   ├─ node / pi 版本是否满足 requirements → 不满足：警告 + 允许继续（记录）
   ├─ 明文密钥扫描 → 命中：警告 + 允许继续
   ├─ 快照含 auth.json → 警告：会覆盖本机凭据
   └─ 目标目录可写
5. 生成计划     新增 / 修改 / 删除（删除仅在受管范围内）
6. 预览         展示每项差异与来源（diff 文本摘要 + 大小 + 时间）
   ├─ 删除项默认不勾选，需逐项确认
   └─ 展示 requiredPackages 待办
7. 应用前复核   重算计划并按 fingerprint 比对；不一致 → plan_stale，整次拒绝
   备份         把将被覆盖/删除的文件与祖先链接复制到本机备份目录
8. 落地         重建链接 → 写入/删除（链接之下的项跳过）；沿用原文件权限位（仅 POSIX）；失败即停并保留备份
                 例外：链接建不成时按整棵子树物化并告警（不是失败），物化也失败才停
9. 收尾
   ├─ 包对齐：按 requiredPackages 调用宿主安装命令（需确认）
   └─ 提示重启宿主或 /reload
```

- 解包目录就是"同步根"：计划与应用对包没有任何感知，`plan_stale` 与删除确认照旧生效。
- 暂存目录常驻**只保留当前这一份**；包间对比使用的临时解包放在 `staging/compare/` 并在对比结束后整体清理。

### 删除判定

删除候选 = **受管范围内、存在于 live、不存在于快照**的文件。

- 不需要本机历史状态；因此**本机新加、尚未导出的受管文件也会出现在删除候选里**。这是有意的：必须让人看见。
- 删除项默认不勾选；不勾选即保留。
- 受管范围之外的文件永不删除。

### 状态机

```text
idle → exporting → packing → deciding → ready
idle → fetching → unpacking → verifying → planning → previewing → backing-up → applying → finishing
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
- 备份份数与快照包保留份数是**两套不同的东西**：前者是 live 的回滚点，后者是跨机快照历史，不要合并。

## 8. 冲突文件

- **已实现（只读）**：扫描受管范围内匹配 `*-冲突*` 与 `*-NSConflict*` 的文件，与对应原文件配对，输出行数、差异行数、首处差异行号；孤儿副本、二进制、超 1 MB、不可读单独标记。
- 快照包化后，云盘冲突副本主要出现在**包文件**上（`<包名> (冲突).zip`），它们落进"无法识别的文件"；若确认是完整包，用户可改名成合法包名后手动导入。受管配置本身不再位于同步文件夹内，因此 live 侧冲突副本只会来自"宿主目录自己就在云盘里"的用法，扫描逻辑保留。
- **未实现**：处理动作与 UI（保留原文件 / 用冲突副本替换 / 暂不处理），见 `backlog.md`。
- 原则：不自动合并，不自动删除。

## 9. 失败码

| code | 含义 |
| :--- | :--- |
| `manifest_missing` | 计划侧目录里没有 `manifest.json`（例如手选了旧布局目录）；解包阶段缺 manifest 是 `package_corrupt` |
| `manifest_corrupt` / `manifest_unsupported` | manifest 无法解析或格式版本不支持 |
| `package_corrupt` | zip 无法解析、顶层条目不符白名单、缺 manifest、或出现符号链接/特殊类型条目 |
| `package_unsafe_entry` | 条目路径越界、条目数或解压体积超限 |
| `package_too_large` | 包超过 64 MB |
| `hash_mismatch` | 包内文件与 manifest 不一致 |
| `plan_stale` | 应用前重算的指纹与用户确认时不同 |
| `delete_not_in_plan` | 请求删除的路径不在本次计划或受管范围内 |
| `delete_through_symlink` | 待删路径经过非 manifest 记录的符号链接 |
| `no_write_permission` / `backup_failed` | 目标不可写 / 备份失败，已中止 |
| `apply_failed` | 落地中途失败，已停止；可用备份回滚 |
| `restore_failed` / `backup_invalid` / `backup_missing` | 回滚失败或备份不可用 |
| `host_version_unsatisfied` | 宿主版本低于要求（警告） |
| `root_not_allowed` | 计划或应用指定的目录既不是存储根，也不在本机暂存目录内 |
| `remote_not_configured` | 未填地址/用户名/密码 |
| `remote_unreachable` | DNS、TLS、超时或 5xx |
| `remote_auth_failed` | 远端 401/403（凭据无效或权限不足） |
| `remote_quota` | 远端 507，或 403 且提示配额 |
| `remote_insecure` | WebDAV 地址不是 HTTPS（本地回环例外） |
| `remote_no_packages` | 下载时远端没有任何可解析的包 |
| `package_not_found` | 选中的包已不在远端 |
| `remote_move_failed` | 上传定案时 `MOVE` 失败 |
| `remote_prune_failed` | 保留清理失败（不阻塞上传成功，单独上报） |

表外还有一组**本机与通用**失败码，属于执行环境问题而非设计语义，UI 照原样展示即可：
`app_config_unavailable` / `app_data_unavailable`（定位不到应用目录）、`export_failed`（打包或读回刚打好的包失败）、
`upload_failed` / `download_failed` / `list_failed` / `cleanup_failed` / `compare_failed`（各自动作的整体失败，细节在 `message` 里）、
`compare_empty`（对比没给候选包）、`no_write_permission`（目标不可写或写本机状态失败）。

## 10. 已知限制

- 每个包都是全量快照，不做增量（配置总量约 100 KB，增量无意义）；代价是同一份内容在多个包里重复。
- **空目录不进包**：manifest 只有文件条目，导出后空目录丢失（与包化前一致）。
- 不感知云盘同步进度，只能靠包内 manifest 与哈希。
- 快照文件保持原字节（含格式、注释与键序）；导出不做 JSON 重写。
- `auth.json` 整文件覆盖，不按 provider 合并；本机独有登录由覆盖前备份兜底。
- 计划只读取快照声明的链接；快照没声明的链接整棵跳过，不读取目标，也不把目标内容列为删除候选。
- 跨平台路径差异（扩展内绝对路径、MCP `cwd`）不做翻译，只在预览中提示。
- 包名里的时间戳来自**各机本地时钟换算的 UTC**，时钟偏差会让"名字最大 = 最近写入"不成立；因此"云端是否有更新"用包名集合判定，名字大小只用来挑默认项。
- 无法识别的文件不会被自动处理；`.tmp-*` 残留只在失败路径内立即删除，进程被杀留下的孤儿需要用户手动清理。
