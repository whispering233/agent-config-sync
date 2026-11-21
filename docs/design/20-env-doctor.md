# 环境检测与宿主安装

## 1. 原则

- **只读**：环境检测自身不写任何文件、不改环境变量、不装任何东西。
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
| `pi.latest` | `npm view @earendil-works/pi-coding-agent version`（需网络，可关闭） | 与本地比较 | 提示升级 |
| `auth` | 对已配置的 provider 执行 `pi auth check --provider <id> --json` | `ready` / `not_ready` / `invalid` | 在终端执行 `pi auth login <provider>` |
| `packages` | 读 `settings.json` 的 `packages` + 解析 `pi list` 文本；实装条目带版本号（读**宿主目录内**安装目录 `package.json` 的 `version` 字段，读不到或落在宿主目录之外则留空） | 声明与实装一致/缺包 | 一键重装 |
| `skills.links` | 统计 `skills/` 下的链接类条目与断链（POSIX 符号链接；Windows 符号链接与 junction） | 有断链 | 提示重建 |
| `conflicts` | 受管范围内匹配 `*-冲突*` / `*-NSConflict*` | 有冲突 | 打开冲突处理 |
| `snapshot` | 快照目录与 `manifest.json` 存在性、格式版本、哈希、创建时间 | 可用/不完整/不存在 | 重新上传或检查云盘 |
| `json.parse` | 解析受管 JSON 文件（容忍 JSON5 注释） | 可解析 | 展示错误位置 |

不做的检查（v1）：models.json 的官方 schema 全量校验、MCP 服务器连通性、模型可用性、网络代理连通性。

## 3. 探测实现约束

- **哨兵分隔**：POSIX 用一条登录 shell 命令执行多个探测，每个结果前后打唯一标记，避免被 shell 启动脚本的噪音污染。
- **命令形态分平台**：Windows 每个探测点单独启动进程（`cmd /d /s /c`），不拼批处理脚本——批处理没有可读的错误处理，且 cmd 自身输出走 OEM 代码页。整行本身再套一层双引号：`/c` 之后引号数 ≥ 3 时 cmd 会剥掉首个引号再删最后一个，`/s` 让它只去掉最外层那一对、内部原样交给程序。
- **入口解析**：Windows 上由 Rust 侧按 `PATH` 与 `PATHEXT` 找可执行文件；`pi` 再兜底 `<agent_dir>/bin/pi.cmd` 与 npm 全局前缀下的 `pi.cmd`。检测、安装、包对齐与计划页共用同一处入口解析，解析不到就是「未找到」，不猜路径。
- **编码与引号**：子进程输出按 UTF-8 解码（宿主 CLI 由 Node 写出），不经 cmd 代码页转换；参数按平台转义，POSIX 用单引号，Windows 用双引号并拒绝 `"`、`%`、`!`、`&`、`|`、`<`、`>`、`^`、`\r`、`\n`（cmd 会展开或换义、换行会把一条命令拆成两条），拒绝时报错而不是改写成别的命令。程序路径只加引号，不过这份名单——它由文件系统解析得出，`C:\Users\a&b\...` 这类目录名在双引号内不构成命令分隔。
- **超时**：普通命令 5 秒；`pi auth check` 15 秒；`npm view` 10 秒。超时即判 `unknown`，不重试。超时终止整棵进程树：Windows 用 `taskkill /T /F`（`child.kill()` 只杀 `cmd.exe`，npm 的子进程会残留），POSIX 安装命令按进程组终止。
- **区分状态**：`found` / `not_found` / `broken`（存在但执行失败）/ `unknown`（超时或输出不可解析）。
- **不打印密钥**：命令输出在落日志前统一过滤（`sk-`、`Bearer`、`token=` 等模式）。
- **不并发狂扫**：同时最多 2 个子进程，避免在低配机器上拖慢宿主。
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
  ]
}
```

`status`：`pass` / `warn` / `fail` / `unknown`。`action` 是建议动作的 id，不内嵌命令文本。

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
