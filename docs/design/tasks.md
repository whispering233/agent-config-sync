# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 依赖顺序

```text
C1（Windows CI）     独立
```

## C1 Windows CI 打包核验

- 现状：`.github/workflows/desktop.yml` 已有 `windows-latest` job（`cargo test` → NSIS → Release/artifact）。
- 范围：
  - 用 `workflow_dispatch` 对一个测试 tag 跑一次，确认 `*-setup.exe` 产出并挂到 Release。
  - 如需 PR 阶段发现 Windows 编译问题，再在 `ci.yml` 增加 `windows-latest` 的 `cargo check`/`cargo test`；不在 PR 上打包。
  - 更新 `docs/design/build.md` 的 CI 现状说明（若新增 job）。
- 验证：Release 资产存在且能下载；安装、启动、导出、WebDAV 各一次。
- 回滚：仅 CI 配置，回退 workflow 即可。

## 暂不建卡

- OAuth 只同步 `api_key`：当前按“全量同步 + 警告”运行，等实际多机冲突出现再评估。
- 一次性明文导出确认：当前每次导出都显示警告，先观察是否需要降低打扰。
- 自动下载/自动应用：与删除逐项确认、`plan_stale` 冲突，不建卡。