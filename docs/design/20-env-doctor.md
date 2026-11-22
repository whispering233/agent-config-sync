# 环境检测与宿主安装

## 1. 原则

- **只读**：环境检测自身不写任何文件、不改环境变量、不装任何东西。技能盘点是独立显式动作，不并入环境检测：它会启动宿主 RPC，宿主可能在缺失时创建空 `auth.json`/`models-store.json`，UI 必须提示并报告。
- **修复是独立动作**：每个建议都由用户单独点击触发，且先展示将执行的命令。
- **区分"没装"与"装了但坏了"**：命令存在但 `--version` 失败，与命令不存在是两种状态。
- **不修改 shell 启动文件**。环境变量只给命令或写入用户级环境（Windows），不改 `.zshrc`。
- **命令按平台执行**：POSIX 走登录 shell（GUI 进程的 PATH 通常不含 nvm/fnm/mise 的路径）；Windows 逐命令经 `cmd /d /s /c "<整行>"` 执行（`npm` / `pi` 是 `.cmd` 中间层，且 PATH 由用户环境直接继承，不需要登录 shell）。

## 2. 探测矩阵

| id | 探测方式 | 判定 | 建议动作 |
| :--- | :--- | :--- | :--- |
| `os` | 系统信息 | — | 展示 |
| `agent_dir` | `PI_CODING_AGENT_DIR` 或默认 `~/.pi/agent` | 存在/可写 | 创建或修正路径 |
| `node.path` | POSIX：登录 shell `command -v node` + 常见目录兜底；Windows：按 `PATH` 与 `PATHEXT` 解析（等价于 `where`） | 找到/未找到 | 安装 Node 说明 |
| `node.version` | `node -p process.versions.node` | `>=22.19.0` | 升级 Node |
| `npm` | `where`/`command -v npm` + `npm -v` + `npm prefix -g` | 找到/未找到 | 随 Node 一起装 |
| `pi.path` | `where`/`command -v pi` + `<agent_dir>/bin/pi` + npm 全局前缀下的入口（POSIX `bin/pi`，Windows `pi.cmd`） | 找到/未找到 | 安装宿主 |
| `pi.version` | `pi --version`（输出裸版本号，如 `1.5.2`） | 可解析 | 升级宿主 |
| `pi.install_kind` | 是否存在 `<agent_dir>/install/managed-install.json`；路径是否在 `node_modules` 下 | 托管安装 / npm 安装 / 未知 | 决定升级命令 |
| `pi.latest` | 仅点「检查更新」时执行 `npm view @earendil-works/pi-coding-agent version`；10 秒超时，最多 2 并发 | 可解析/未发布/未知 | 提示升级 |
| `auth` | 对已配置的 provider 执行 `pi auth check --provider <id> --json` | `ready` / `not_ready` / `invalid` | 在终端执行 `pi auth login <provider>` |
| `packages` | 读 `settings.json` 的 `packages` + 解析 `pi list` 文本；实装条目带版本号（读**宿主目录内**安装目录 `package.json` 的 `version` 字段，读不到或落在宿主目录之外则留空） | 声明与实装一致/缺包 | 一键重装 |
| `skills.links` | 递归统计 `<agent_dir>/skills` 与 `~/.agents/skills` 下的链接类条目与断链（POSIX 符号链接；Windows 符号链接与 junction） | 有断链 | 提示重建 |
| `skills_inventory` | 独立命令（不在报告 checks 里），仅用户点“刷新技能”时执行：`pi --mode rpc --offline --no-session --no-extensions --no-approve` + `get_commands`，过滤 `source == "skill"`；按 `sourceInfo.path/scope/origin` 标注来源与是否随快照 | 可读/不可读；报告宿主创建的空文件 | 展示技能卡片，不自动修复 |
| `conflicts` | 两个受管根内匹配 `*-冲突*` / `*-NSConflict*` | 有冲突 | 打开冲突处理 |
| `snapshot` | 快照目录与 `manifest.json` 存在性、格式版本、哈希、创建时间 | 可用/不完整/不存在 | 重新上传或检查云盘 |
| `json.parse` | 解析受管 JSON 文件（容忍 JSON5 注释） | 可解析 | 展示错误位置 |

不做的检查（v1）：models.json 的官方 schema 全量校验、MCP 服务器连通性、模型可用性、网络代理连通性。

`skills.links` 已实现：递归扫两个受管根；技能盘点是独立命令 `skills_inventory`（结构见 §4），不出现在报告里。`conflicts` 的扫描层已实现（覆盖两个受管根、条目带 `root`）；处理动作与 UI 未实现（见 `10-snapshot-sync.md` §8 与 `backlog.md`）。

## 3. 探测实现约束

- **哨兵分隔**：POSIX 用一条登录 shell 命令执行多个探测，每个结果前后打唯一标记，避免被 shell 启动脚本的噪音污染。
- **命令形态分平台**：Windows 每个探测点单独启动进程（`cmd /d /s /c`），不拼批处理脚本——批处理没有可读的错误处理，且 cmd 自身输出走 OEM 代码页。整行本身再套一层双引号：`/c` 之后引号数 ≥ 3 时 cmd 会剥掉首个引号再删最后一个，`/s` 让它只去掉最外层那一对、内部原样交给程序。
- **入口解析**：Windows 上由 Rust 侧按 `PATH` 与 `PATHEXT` 找可执行文件；`pi` 再兜底 `<agent_dir>/bin/pi.cmd` 与 npm 全局前缀下的 `pi.cmd`。检测、安装、包对齐与计划页共用同一处入口解析，解析不到就是「未找到」，不猜路径。
- **编码与引号**：子进程输出按 UTF-8 解码（宿主 CLI 由 Node 写出），不经 cmd 代码页转换；参数按平台转义，POSIX 用单引号，Windows 用双引号并拒绝 `"`、`%`、`!`、`&`、`|`、`<`、`>`、`^`、`\r`、`\n`（cmd 会展开或换义、换行会把一条命令拆成两条），拒绝时报错而不是改写成别的命令。程序路径只加引号，不过这份名单——它由文件系统解析得出，`C:\Users\a&b\...` 这类目录名在双引号内不构成命令分隔。
- **超时**：普通命令 5 秒；`pi auth check` 15 秒；`npm view` 10 秒。超时即判 `unknown`，不重试。超时终止整棵进程树：Windows 用 `taskkill /T /F`（`child.kill()` 只杀 `cmd.exe`，npm 的子进程会残留），POSIX 安装命令按进程组终止。
- **区分状态**：`found` / `not_found` / `broken`（存在但执行失败）/ `unknown`（超时或输出不可解析）。
- **不打印密钥**：命令输出在落日志前统一过滤（`sk-`、`Bearer`、`token=` 等模式）。
- **不并发狂扫**：同时最多 2 个子进程，避免在低配机器上拖慢宿主。版本查询也只在这个上限内并发。
- **技能盘点复用宿主自身**：`skills_inventory` 走 `pi` RPC，不在 Rust 侧重写技能发现与 frontmatter 校验。命令 15 秒超时，不重试；stdin 关闭后宿主自行退出，超时按进程树终止。`--no-extensions` 会漏掉扩展运行时动态注册的技能，这类技能本来也不在快照内，UI 需注明。入口解析与 `pi.path` 同一顺序（`PATH` → `<agent_dir>/bin/pi` → npm 全局前缀下的入口），找到后直接执行该路径。宿主按 `PI_CODING_AGENT_DIR` 读宿主目录，起进程时传入应用解析到的生效目录（可能来自设置里的覆盖值）。
- **技能盘点的副作用**：只在用户显式点击后执行，不随页面加载自动跑。命令执行前后比较 `<agent_dir>/auth.json` 与 `<agent_dir>/models-store.json`，把本次新出现的文件放进 `createdFiles` 并在 UI 告警；已存在时内容不变。不删除宿主创建的空文件。
- 如果 `auth.json` 原本不存在，RPC 会创建空 `{}`；后续导出会把它当作源机凭据状态纳入并标记 `plaintextSecrets`，应用端会覆盖目标 `auth.json`（覆盖前有备份）。首次点击的提示必须写明这一点，而不是只说“可能创建缓存文件”。
- **两个受管根的链接与冲突**：`skills.links` 递归统计两个根的链接类条目，断链明细带根名（`宿主目录 里的 skills/x`、`~/.agents/skills 里的 x`）；根不存在就跳过，读不出来记 `unknown`，不让整份报告失败。根自身的问题（根是断链、根是链接、根读不出来）同样点名被检查的目录：`宿主目录 里的 skills` 或 `~/.agents/skills`，不只写「宿主目录」。`conflicts` 同样扫两个根，条目带 `root`、路径相对各自根；外部根不存在跳过，根是链接时按目标内容扫描。
- **最新版本缓存**：结果写入应用配置目录的 `versions-cache.json`，TTL 6 小时；「开始检查」只读缓存，只有「检查更新」才联网。刷新全部失败时保留旧缓存并提示；404/401 记为未发布/未知，不算失败。
- **只查 npm 源**：`npm:包名` 才查询；git/URL/本地路径标「不适用」；带范围或非精确版本的来源标「未固定」，不拿远端 latest 冒充。
- **不弹控制台**：GUI 进程起子进程时带 `CREATE_NO_WINDOW`；否则从 Explorer 启动后每次探测都会闪出终端窗口。

## 4. 报告结构

```json
{
  "generatedAt": "2026-11-20T05:30:00Z",
  "platform": { "os": "linux", "arch": "x64" },
  "checks": [
    {
      "id": "node.version",
      "status": "pass",
      "value": "22.23.0",
      "detail": "from /home/u/.nvm/versions/node/v22.23.0/bin/node",
      "action": null
    }
  ],
  "latest": {
    "checkedAt": "2026-11-20T05:30:00Z",
    "stale": false,
    "pi": { "version": "1.6.0", "note": null },
    "packages": {
      "npm:pi-lens": { "version": "2.1.0", "note": null }
    },
    "error": null
  }
}
```

`status`：`pass` / `warn` / `fail` / `unknown`。`action` 是建议动作的 id，不内嵌命令文本。

### 技能盘点

技能清单是独立命令（`skills_inventory`），不并入 `run_check`，避免每次环境检测都启动宿主进程。输出是结构化列表，不是 `Check`：

```json
{
  "error": null,
  "skills": [
    {
      "name": "council-mode",
      "description": "…",
      "path": "/home/u/.agents/skills/council-mode/SKILL.md",
      "root": "agents-skills",
      "snapshot": true,
      "source": "user",
      "reason": null
    },
    {
      "name": "lens",
      "description": "…",
      "path": "/home/u/.pi/agent/npm/node_modules/pi-lens/skills/lens/SKILL.md",
      "root": null,
      "snapshot": false,
      "source": "package",
      "reason": "包提供：随包重装恢复，不进快照"
    }
  ],
  "createdFiles": [],
  "warnings": []
}
```

- `error` 非空表示 RPC 失败（超时、宿主缺失或输出里没有 `get_commands` 响应），此时 `skills` 为空，但 `createdFiles` 与 `warnings` 仍然返回，不丢掉本次观察到的副作用。
- `source`：`user`（受管根内，随快照）、`package`（包提供）、`project`（项目级）、`other`（CLI 内置、受管根之外的路径等）；不随快照时 `reason` 说明原因。
- 排序：随快照（`agent` 在前）→ 包提供 → 其它；同名同路径的条目只列一次。
- `createdFiles` 是本次 RPC 在 agent 目录新创建的文件（通常是 `auth.json`、`models-store.json`）；不是错误，但必须在 UI 明示。
- `warnings` 至少包含 `--no-extensions` 的漏项说明与「被宿主跳过的 `SKILL.md` 仍会随快照导出」，另有未分类路径条数与 home 缺失的提示。

分类规则：

- `<agent_dir>/skills` → `root: agent`，随快照。
- `~/.agents/skills` → `root: agents-skills`，随快照。
- `origin: package` → 包提供，不随快照，随包重装。
- 其它宿主目录内路径或 `settings.skills` 外部路径 → 不随快照，展示原因。
- 项目级技能不出现（命令以应用自己的中性目录为 cwd，不读项目配置）。
- 盘点只列宿主实际加载的技能；frontmatter 无效或名称冲突被宿主跳过的 `SKILL.md` 不会出现，但导出仍会按文件收录。UI 需说明这个边界。

真实宿主的响应形状（`pi 1.6.0`，`pi --mode rpc --offline --no-session --no-extensions --no-approve`，stdin 写一行 `{"type":"get_commands"}`）：

```json
{"type":"response","command":"get_commands","success":true,"data":{"commands":[{"name":"skill:demo","description":"…","source":"skill","sourceInfo":{"path":"/home/u/.pi/agent/skills/demo/SKILL.md","source":"auto","scope":"user","origin":"top-level","baseDir":"/home/u/.pi/agent"}}]}}
```

- 输出是逐行 JSON，宿主启动日志与 shell 噪音混在其它行里；只认 `type == "response"` 且 `command == "get_commands"` 的行，一个响应都没有才算失败。
- 命令列表在 `data.commands`；技能条目的 `name` 带 `skill:` 前缀，展示时去掉。
- 分类只用 `sourceInfo`：`path` 判定是否落在受管根内，`scope`/`origin` 解释「不随快照」的原因。`sourceInfo.source`（实测为 `auto`）与 `baseDir` 目前不参与分类。
- `success: false` 时带 `error` 文本，按失败报出，不猜列表；宿主答完再非零退出时清单仍然可用，但要在 `warnings` 里说明可能不完整。

## 5. 宿主安装与升级

### 平台策略

| 平台 | 安装 | 升级 | 原因 |
| :--- | :--- | :--- | :--- |
| Linux / macOS | 官方 installer：以登录 shell 执行 `curl -fsSL https://pi.dev/install.sh \| sh`，**不分配终端** | 托管安装：`pi update`；npm 安装：`npm i -g @earendil-works/pi-coding-agent@latest` | 官方 installer 会 pin 依赖；无终端时行为确定 |
| Windows | 仅 npm：`npm i -g @earendil-works/pi-coding-agent` | 同上 | 官方 `install.ps1` 在输入被重定向时，Portable Git 的默认选项可能被静默执行；v1 规避 |

### 前置条件

- Node `>=22.19.0` 必须已存在。**v1 不自动安装 Node**：官方 installer 装 Node 需要终端确认，本应用不引入 PTY。
- 缺失时展示：官方下载地址、`nvm`/`fnm` 的建议命令，以及"安装完成后重新检测"按钮。

### 执行约束

- 必须先展示完整命令与影响，用户确认后执行。
- 安装/升级过程输出实时展示（脱敏后），结束后自动重跑环境检测。
- 失败不自动重试、不切换备用方案；把退出码与最后 20 行输出展示给用户。
- 安装完成后，若 `pi` 仍不在登录 shell 的 PATH 中，提示重启终端或手动指定路径；本应用记录"用户手动指定"的路径。
- 执行是两段式：`preview` 只探测并给出命令；`execute` 在确认后**重新探测、重算命令并校验动作**，前端只能提交动作枚举与确认位。
- 安装命令超时 10 分钟；POSIX 下以独立进程组启动，超时整组终止（避免 `curl | sh` 的孙进程残留）。输出上限 200 KB / 最后 100 行，落日志前脱敏。

### 版本比较

- 语义化版本比较；`pi --version` 可能带前缀以外的杂质，取第一个形如 `x.y.z` 的片段。
- `requirements` 不满足时：快照仍可浏览，但"应用"按钮降级为警告确认，不硬阻断。

## 6. 失败码

| code | 含义 |
| :--- | :--- |
| `tool_not_found` | 命令不存在 |
| `tool_broken` | 命令存在但执行失败（如 Node 版本过低导致无法启动） |
| `probe_timeout` | 探测超时 |
| `version_unparsable` | 版本号无法解析 |
| `install_declined` | 用户取消安装 |
| `install_failed` | 安装命令非零退出 |
| `path_not_updated` | 安装成功但 PATH 未生效 |
