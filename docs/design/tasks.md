# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

本轮只做“技能范围”：把用户级 `~/.agents/skills` 纳入快照。版本显示与 `pi update --all` 另建任务卡，不混入本轮。

## 卡 1：多根模型与导出/计划（format v3）

### 目标

把单根快照改成多根快照，先打通“导出 → 校验 → 计划”这条只读链路。

### 范围

- `scope.rs`：引入 `RootId` / `ManagedRoot` / `ManagedRoots`；扫描、物化、虚拟路径解析都按根处理。
- `snapshot.rs`：`FORMAT_VERSION = 3`；新增 `managedRoots`；布局 `snapshot/external/agents-skills/`；指纹纳入 `managedRoots`；导出 precheck 检查两个根与同步根不互相嵌套。
- `plan.rs`：按 `managedRoots` 扫 live；计划项带 `root`；v2 manifest 报 `manifest_unsupported`；未知 root id 同样拒绝。
- `main.rs` / command 层：导出与计划命令把 home 目录传给核心层。
- `src/pages/snapshot-page.tsx`：计划项类型补 `root`（此卡只补类型，不改交互）。

### 关键规则

- 源机 `~/.agents/skills` 不存在 → 不写入 `managedRoots`；存在（含空目录）→ 写入；不可读或不是目录 → 导出失败。
- 根是符号链接 → 跟随内容，导出告警，不记录根链接；根链接不按“链接之下跳过”处理，写入/删除落到链接目标。
- `settings.skills` 指向的其它外部路径、项目级目录、包内技能都不纳管。

### 验收

- v3 manifest 含 `managedRoots`；v2 快照被拒绝；未知 root id 被拒绝；`managedRoots` 不同则指纹不同。
- 两个受管根的新增/修改/删除计划正确；两个根同相对路径不串。
- 外部根不存在时不产生任何外部路径；存在但为空时计划能列出目标机的外部删除候选。
- 同步根与任一受管根互相嵌套时导出失败。
- 原有 agent-only 测试全部通过。

### 验证

```bash
cd src-tauri && cargo test
cd src-tauri && cargo check
cd src-tauri && cargo fmt -- --check
```

## 卡 2：应用/备份/回滚与外部删除交互

### 目标

让 v3 快照真正可落地、可回滚，并把外部共享目录的删除交互收紧。

### 依赖

卡 1。

### 范围

- `apply.rs`：所有写入/删除/链接/备份按 root 解析；`auth.json` 特殊处理只对 agent 根生效。
- `rollback.rs`：按 `restore.json` 的 root 还原；版本 1 旧备份默认 agent 根。
- `apply.rs`：`restore.json` 升到 v2，entry 增加 `root`；备份文件按虚拟路径存放。
- `src/pages/snapshot-page.tsx`：计划项显示 root；外部根条目标注“共享目录，其它 agent 工具也可能使用”；“全选”只选 agent 根；应用报告展示外部删除汇总告警。
- `docs/design/10-snapshot-sync.md` 与 `00-master-design.md` 的对应章节随行为更新。

### 验收

- 两个根都能导出、计划、应用、回滚；回滚后两棵目录恢复原状。
- 外部根删除默认不勾选；“全选”不会勾选外部根；逐项勾选后可删除并进入备份。
- 旧 `restore.json` v1 仍可回滚。
- 外部根的符号链接条目按现有“链接重建优先/物化回退”语义处理；外部根自身是链接时，写入/删除落到链接目标并告警。
- agent 根的 `auth.json` 仍只按普通文件落地，不重建链接。

### 验证

```bash
cd src-tauri && cargo test
cd src-tauri && cargo check
cd src-tauri && cargo fmt -- --check
npm run build
```

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
