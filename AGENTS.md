# 项目规则

## 定位

Pi CLI 配置的换机交付工具（不是同步客户端）。设计与决策以 `docs/` 为准，索引见 `docs/docs-map.md`。

## 不可擅自变更的决策

改动以下任何一条，必须先改 `docs/design/00-master-design.md` 的决策表并征得确认：

- 传输层：云文件夹托管，不写同步引擎。
- 落地语义：快照为准，删除需逐项确认，覆盖前备份。
- 触发方式：手动双向，不做实时监听。
- 密钥：只允许宿主原生引用语法，`auth.json` 永不进入快照。
- 符号链接：apply 时按 manifest 重建链接优先；目标缺失回退物化并告警；链接之下的写入/删除跳过。
- 技术栈：Tauri v2 + Rust + React + Tailwind，不引入 Node sidecar。
- 配置套数：v1 单套，manifest 预留 `profileId`。
- 平台顺序：Linux → Windows → macOS；Windows 安装宿主只走 npm。
- 包对齐：只安装“已启用且缺失”的声明包；多余包只展示，不自动移除。

## 实现边界

- 文件系统读写、子进程、哈希、遍历全部在 Rust 侧；前端不直接访问文件系统。
- 无数据库、无常驻进程、无文件监听。
- 子进程命令由核心层拼接，前端只能传枚举与受限参数。
- 受管范围之外的文件永不读取内容、永不写入、永不删除。
- 应用前必须重算计划并按 fingerprint 比对（`plan_stale`）；删除项必须来自计划。
- manifest 最后写；manifest 落盘即代表快照完整。
- 失败停在原地：不自动重试、不自动降级、不静默修复。
- 安装/包对齐两段式：`preview` 只探测，`execute` 确认后重新探测并校验动作；安装命令超时整组终止。

## 密钥铁律

- 快照、日志、备份、错误信息、manifest 中都不得出现明文密钥。
- 只使用宿主原生引用：`models.json`/`web-search.json` 用 `$NAME`，`mcp.json` 用 `${NAME}`。
- 补齐只写 `usedBy` 列出的本机文件；值不进入日志、错误或返回结构。
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
cd src-tauri && cargo test      # 期望全绿（当前 163）
cd src-tauri && cargo check
npm run build
```

- 文件遍历、manifest、哈希、密钥扫描、应用/回滚、包/安装必须有单元测试与临时目录夹具。
- UI 验证交给子代理，条目数给定上限，只报不修。
- 每个任务卡必须能独立验证、独立回滚。
- 打包：`npm run tauri build -- --bundles deb,appimage`（AppImage 需要能访问 GitHub release 下载 runtime）。

## 提交

- 一个任务一次提交，提交信息说明“做了什么、为什么”，不写卡号。
