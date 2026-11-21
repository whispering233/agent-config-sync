# 项目规则

## 定位

Pi CLI 配置的换机交付工具（手动快照交付；可选托盘常驻与定时自动上行，不做实时监听/自动应用）。设计与决策以 `docs/` 为准，索引见 `docs/docs-map.md`。

## 不可擅自变更的决策

改动以下任何一条，必须先改 `docs/design/00-master-design.md` 的决策表并征得确认：

- 传输层：本地文件夹与 WebDAV 双传输并存；两种模式共用同一套快照与 manifest，落地语义不变。
- 落地语义：快照为准，删除需逐项确认，覆盖前备份。
- 触发方式：手动双向；可选托盘常驻与定时自动上行（只上传，不自动下载/应用），不做实时监听。
- 密钥：明文自包含，`auth.json` 纳入快照；含 `auth.json` 的 WebDAV 传输强制 HTTPS（localhost 例外）。
- 符号链接：apply 时按 manifest 重建链接优先；目标缺失回退物化并告警；链接之下的写入/删除跳过；快照没声明的链接只按链接本身列为删除候选，不读目标。
- 技术栈：Tauri v2 + Rust + React + Tailwind，不引入 Node sidecar。
- 配置套数：v1 单套，manifest 预留 `profileId`。
- 平台顺序：Linux → Windows → macOS；Windows 安装宿主只走 npm。
- 平台边界：不跨 WSL；Windows 版只管理 Windows 原生环境。
- 包对齐：只安装“已启用且缺失”的声明包；多余包只展示，不自动移除。

## 实现边界

- 文件系统读写、子进程、哈希、遍历全部在 Rust 侧；前端不直接访问文件系统。
- 无数据库、无文件监听；托盘常驻是可选能力。
- 出站网络只有 WebDAV：控制请求 30s、单文件传输 120s，不自动重试。
- 子进程命令由核心层拼接，前端只能传枚举与受限参数。
- 受管范围之外的文件永不读取内容、永不写入、永不删除。
- 应用前必须重算计划并按 fingerprint 比对（`plan_stale`）；删除项必须来自计划。
- manifest 最后写；manifest 落盘即代表快照完整。
- 失败停在原地：不自动重试、不自动降级、不静默修复。
- 安装/包对齐两段式：`preview` 只探测，`execute` 确认后重新探测并校验动作；安装命令超时整组终止。

## 密钥边界

- 快照与备份允许明文密钥，但目录权限 `0700`、`auth.json` `0600`；日志、错误信息、manifest 中不得出现密钥值或片段。
- `auth.json` 只纳入宿主目录根级，整文件覆盖；不按 provider 合并，不重建符号链接。
- WebDAV 凭据单独存于应用数据目录（POSIX 0600；Windows 依赖用户配置目录的继承 ACL），不进快照、日志、错误信息与同步文件夹。
- 含 `auth.json` 的 WebDAV 上传/下载只允许 HTTPS（`localhost`/`127.0.0.1`/`::1` 例外），否则报告 `remote_insecure`。
- 不发明私有占位语法，不对快照整体加密，不自动写 shell 启动文件。

## 代码与注释纪律

- 注释只解释“为什么”，不写任务卡语义（不出现 `T1`/“本卡”/“后续卡”）。
- 不在代码里保留死分支与未使用的占位组件；删掉比注释掉好。
- 复用现有模块（`scope`/`apply` 的备份格式/`doctor` 的 Runner），不另造一套。
- 新增依赖要说明理由；能用标准库就不加 crate。

## 文档纪律

- `docs/` 只描述设计与格式，不粘贴实现代码、不记录历史实现。
- 已完成的临时任务卡必须从 `docs/design/tasks.md` 删除。
- `CHANGELOG.md` 不写任务卡语义与卡号。
- 行为变化必须同步 `docs/design/` 对应章节（快照格式、失败码、备份格式等）。

## 验证

```bash
cd src-tauri && cargo test      # 期望全绿（另含需真机环境的 ignored 测试）
cd src-tauri && cargo check
cd src-tauri && cargo fmt -- --check
npm run build
```

- WebDAV 互通检查需要真实服务器：`DAV_URL=… DAV_USER=… DAV_PASS=… cargo test interop -- --ignored --nocapture`（会覆盖该地址下 `PiAgentSync/` 的内容，只对测试账号跑）。

- 文件遍历、manifest、哈希、密钥扫描、应用/回滚、包/安装必须有单元测试与临时目录夹具。
- UI 验证交给子代理，条目数给定上限，只报不修。
- 每个任务卡必须能独立验证、独立回滚。
- 打包：`npm run tauri -- build --bundles deb,appimage`（AppImage 需要能访问 GitHub release 下载 runtime）；Windows 上 `npm run tauri -- build --bundles nsis`，CI 的 `windows-latest` job 会先跑 `cargo test` 再出包。
- 平台差异收在 `src-tauri/src/platform.rs`：权限位、链接判定与创建、`cmd /d /s /c` 命令行、`CREATE_NO_WINDOW`、进程树终止。调用点不直接写 `cfg(windows)`，命令形态由注入的宿主平台分支。

## 提交

- 一个任务一次提交，提交信息说明“做了什么、为什么”，不写卡号。
