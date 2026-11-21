# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 依赖顺序

```text
B1（版本显示）       独立
C1（Windows CI）     独立
```

B1 与 C1 可并行；不要在同一 worktree 里同时改 `doctor`/`commands`。

## B1 Pi 与插件最新版本显示

- 前置决策：当前架构写“出站网络只有 WebDAV”；本卡需要允许“手动触发、经 npm CLI、10 秒超时、不重试”的版本查询，先更新 `docs/design/00` 与 `architecture.md`。
- 范围：
  - Doctor 增加 `pi.latest`：`npm view @earendil-works/pi-coding-agent version`，与本地 `pi --version` 比较。
  - 插件最新版本：解析 `settings.json` 的 `packages` 与 `pi list` 实装；只查 npm 源，git/URL/本地路径标“不适用”。
  - 只比较精确 `x.y.z`；范围版本标“未固定”，不引入 semver crate。
  - 404/401/未发布包标“未知/未发布”，不算失败；超时/离线保留旧缓存。
  - 缓存 `versions-cache.json` 于应用配置目录，TTL 6 小时；默认读缓存，手动“检查更新”才刷新；并发 2。
  - UI：Doctor 宿主版本行显示“本地 / 最新”；扩展包列表显示“实装 / 最新”。
- 验证：假 Runner 覆盖成功、超时、404、非 npm 源；离线读缓存；不因单个包失败中断整批；`cargo test`、`npm run build`。
- 回滚：删除查询入口与缓存读取，Doctor 恢复只显示本地版本。

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