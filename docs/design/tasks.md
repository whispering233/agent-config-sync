# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

本轮只做“技能范围”：把用户级 `~/.agents/skills` 纳入快照。版本显示与 `pi update --all` 另建任务卡，不混入本轮。

## 卡 3：技能盘点与只读检查（技能清单显式刷新）

### 目标

让用户看见“`pi` 实际加载的技能”和“快照是否覆盖”，并补齐两个根的链接/冲突检查。

### 依赖

卡 1；与卡 2 可并行（文件重叠少，文档合并时指定一人统一收口）。

### 范围

- 新增 `src-tauri/src/skills.rs`：起 `pi --mode rpc --offline --no-session --no-extensions --no-approve`，发送 `get_commands`，过滤 `source == "skill"`，按 `sourceInfo` 分类。
- `doctor.rs`：`skills.links` 递归扫描 `<agent_dir>/skills` 与 `~/.agents/skills`（不再只数顶层）。
- `conflicts.rs`：扫描两个受管根，冲突项带 root。
- 新增 command（`skills_inventory`）与前端类型；医生页新增“技能”卡片，按来源分组展示名称、描述、路径、是否随快照。
- 技能卡片不随页面加载自动执行；用户点“刷新技能”才启动 RPC。首次点击前显示副作用提示；命令返回 `createdFiles`（本次新出现的 `auth.json`/`models-store.json`）并在 UI 告警。
- 不删除宿主创建的空文件；已存在时内容不变。
- `20-env-doctor.md`、`10-snapshot-sync.md`、`architecture.md` 随实现更新。

### 分类规则

- `<agent_dir>/skills` → `root: agent`，随快照。
- `~/.agents/skills` → `root: agents-skills`，随快照。
- `origin: package` → 包提供，不随快照。
- 其它路径 → 不随快照，展示原因。
- 项目级技能不出现（RPC 以中性 cwd 启动）。

### 验收

- 清单与 `pi` 启动列表的用户级技能一致（允许 `--no-extensions` 漏掉扩展动态注册技能，UI 注明）。
- 技能卡片不会自动启动 RPC；首次点击前有副作用提示；返回后显示实际创建的 `auth.json`/`models-store.json`。
- 提示必须说明：新建的空 `auth.json` 会被后续导出当作源机凭据状态纳入，并在应用端覆盖目标凭据（有备份）。
- RPC 坏行、超时、宿主缺失都有可读错误；不重试、不猜列表。
- 不删除宿主创建的空文件；已存在时内容不变。
- 两个根的断链都能报出（含嵌套链接）；外部根链接告警可见。
- 两个根的冲突文件都能被扫描并带 root 标记。
- 不新增 YAML/frontmatter 依赖。

### 验证

```bash
cd src-tauri && cargo test
cd src-tauri && cargo check
npm run build
```

UI 验证交给子代理，条目上限 8 条，只报不修。

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
