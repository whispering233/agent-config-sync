# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## U1 应用更新核心命令

- 目标：按 `50-app-update.md` 接入官方 updater，提供检查、安装、状态查询三个 command；开发态/浏览器返回 `update_unsupported`。
- 范围：`src-tauri/Cargo.toml` 依赖与插件注册；`src-tauri/src/updater.rs`；`commands.rs`/`main.rs` 接线；失败码映射；安装前取操作锁并暂停自动上行；进度事件；单元测试覆盖门禁、版本不一致与错误映射。
- 不做：UI；CI；真实下载。
- 验收：`cargo test`、`cargo check`、`cargo fmt -- --check` 通过；开发态不发网络请求；`update_busy`、`update_stale` 有测试。
- 依赖：无。
- 可并行：U2。

## U2 更新配置与签名构建

- 目标：发布构建能产出带 `.sig` 的 AppImage、deb、NSIS，普通本地构建不需要私钥。
- 范围：`tauri.conf.json` 的 `plugins.updater`（endpoint、pubkey、`requireSignedVersion`）；`src-tauri/tauri.updater.conf.json` 叠加 `createUpdaterArtifacts`；`build.md` 的密钥/构建说明；CI secret 文档。
- 不做：`latest.json` 生成（U4）；UI。
- 验收：无 `TAURI_SIGNING_PRIVATE_KEY` 的本地构建不因更新产物失败；带密钥的发布构建在三种产物旁生成 `.sig`；配置中不出现私钥。
- 依赖：U1 的 command 接口（可先按设计约定并行）。
- 可并行：U1。

## U3 设置页更新入口

- 目标：设置页“应用更新”卡片 + 发现新版本时的顶部提示；展示当前/最新版本、说明、进度、安装/稍后与失败信息。
- 范围：`src/lib/desktop.ts` 类型与 command 包装；`settings-page.tsx` 卡片；`app-shell.tsx` 或 App 级提示条；浏览器/开发态降级提示；进度事件监听与清理。
- 不做：静默安装、自动下载、定时轮询。
- 验收：浏览器预览显示“仅发布版可用”且不发请求；无更新不显示常驻横幅；失败后仅允许重新检查，不自动重试；UI 验证交给子代理，条目数设上限。
- 依赖：U1。
- 可并行：U4（若接口已定）。

## U4 发布流水线与 latest.json

- 目标：tag 发布时上传安装包、`.sig` 与 `latest.json`，客户端能检到新版本。
- 范围：`desktop.yml` 的 Linux/Windows 构建与 `.sig` 上传；新增 `manifest` job 生成并最后上传 `latest.json`；平台键 `linux-x86_64-appimage`、`linux-x86_64-deb`、`windows-x86_64-nsis`；URL 与资产名逐字一致并探测可下载；`build.md` 发布说明。
- 不做：增量补丁；代码签名。
- 验收：一次测试 Release 的 `latest.json` 覆盖三平台；每个 URL 可下载；`latest.json` 的版本与三处版本号一致；缺签名私钥时 job 失败而不是发无签名包。
- 依赖：U2。
- 可并行：U3。

## U5 三平台真机验证与收尾

- 目标：按 `50-app-update.md` §10 在 Linux AppImage/deb、Windows NSIS 上完成真实升级验证。
- 范围：检查、下载、签名失败拒绝、安装、重启、失败停在当前版本、操作锁 `update_busy`；发布侧资产核对；清理本文件中的已完成卡片；同步 `CHANGELOG.md` 与相关文档。
- 不做：修复验证中发现的新功能；只记录问题并回到对应卡片。
- 验收：三平台各至少一次成功升级；至少一次签名/版本不符被拒；至少一次网络失败不重试；验证记录可复现。
- 依赖：U1、U2、U3、U4。
- 可并行：无（串行收尾）。

## 并行/串行结论

- 第一波：U1 + U2 并行。
- 第二波：U3 + U4 并行，依赖第一波。
- 第三波：U5 串行，依赖全部。