# agent-config-sync

Pi CLI 配置同步桌面工具（设计阶段，尚无代码）。

把用户的 Pi 配置变成**可校验的快照**，经云盘文件夹传到另一台机器，再**安全落地**：校验哈希、预览差异、确认删除、备份回滚、提示补齐密钥、按声明重装插件。

## 它解决什么

- 多台机器上的 Pi 配置（`settings.json`、`models.json`、`mcp.json`、`AGENTS.md`、skills、extensions、插件私有配置）保持一致。
- 密钥不随快照走：快照里只保留宿主原生的环境变量引用，导入时逐项提示补齐。
- 换机后能跑：检测 Node / Pi / 凭据 / 插件状态，给出安装与修复动作。
- 不依赖同步引擎：传输交给任意云盘客户端（当前面向坚果云），本工具只负责"传过去的东西对不对"。

## 它不做什么

- 不做 WebDAV 协议实现、不做冲突自动合并、不做后台常驻同步。
- 不做模型路由、代理、用量统计、会话管理。
- 不自动安装 Node，不修改 shell 启动文件，不代写 OAuth 凭据。
- v1 只面向 Pi；其他宿主在架构上预留，不实现。

## 文档

设计与决策以 `docs/` 为准，入口见 [docs-map.md](docs-map.md)。

- 总体设计与已定决策：`docs/design/00-master-design.md`
- 快照格式与同步流程：`docs/design/10-snapshot-sync.md`
- 环境医生与安装：`docs/design/20-env-doctor.md`
- 密钥策略：`docs/design/30-secrets.md`
- 技术架构：`docs/design/architecture.md`
- 遗留项：`docs/design/backlog.md`
- 任务卡片（临时）：`docs/design/tasks.md`

## 当前状态

设计完成，未开始编码。技术栈：Tauri v2 + Rust + React + TypeScript + Tailwind + shadcn/ui；首个平台 Linux。

## 目录结构

```text
docs/
├── design/            设计与任务卡片
docs-map.md            文档索引
README.md              本文件
CHANGELOG.md           变更记录
```
