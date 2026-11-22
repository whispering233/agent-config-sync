# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

本轮只做“技能范围”：把用户级 `~/.agents/skills` 纳入快照。版本显示与 `pi update --all` 另建任务卡，不混入本轮。

## 卡 4：收尾与发布准备

### 目标

全量验证、文档一致性、CHANGELOG 与版本号。

### 依赖

卡 2、卡 3。

### 范围

- 跑完整验证：`cargo test`、`cargo check`、`cargo fmt -- --check`、`npm run build`。
- 检查 `docs/` 没有历史实现记录与任务卡语义。
- `CHANGELOG.md` 增加对应版本条目（不写卡号）。
- 版本号三处同步：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`；更新 lock 文件。
- 清理 `docs/design/tasks.md` 中已完成卡片。

### 验收

- 全量验证全绿。
- v2 快照拒绝、v3 快照可导出/应用/回滚的说明在文档中一致。
- 发布说明明确：升级后需在源机重新导出快照。
