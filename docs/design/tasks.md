# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 批次与依赖

| 卡 | 内容 | 依赖 | 可并行 |
| :--- | :--- | :--- | :--- |
| T1 | CI：提交与 PR 验证 | — | 与 T4、T5 |
| T2 | Release：tag 建发布 | T1 | 否 |
| T3 | Linux 打包与资产上传 | T2 | 否 |
| T4 | 传输方式与 WebDAV 凭据 | — | 与 T1、T5 |
| T5 | WebDAV 客户端 | — | 与 T1、T4 |
| T6 | 连通性探测 | T4、T5 | 否 |
| T7 | 上传与冲突判定 | T4、T5 | 与 T8 |
| T8 | 下载与暂存 | T4、T5 | 与 T7 |

## T1 CI：提交与 PR 验证

- 目标：每次提交与 PR 在 3 分钟内给出"能否编译、测试是否通过"。
- 范围：`.github/workflows/ci.yml`；触发 `push`（main）与 `pull_request`；`ubuntu-22.04`；步骤 `npm ci` → `npm run build` → `cargo check` → `cargo test`。
- 验收：故意改坏一个测试 → 流水线红；还原 → 绿；不加缓存、不打包。
- 回滚：删除该文件。

## T2 Release：tag 建发布

- 目标：push `v*` tag 后生成带正文的 GitHub Release。
- 范围：`.github/workflows/release.yml`；第一步校验三处版本号（`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`）与 tag 一致；正文取 `CHANGELOG.md` 对应 `## [vX.Y.Z]` 段；`permissions: contents: write`。同时把 `CHANGELOG.md` 的 `[Unreleased]` 内容整理成 `## [v0.0.1] - <日期>` 段。
- 验收：版本不一致时失败且不建 Release；在仓库推一个临时 tag 后 Release 正文与 CHANGELOG 段一致。
- 依赖：T1。回滚：删除工作流；Release 手工删除。

## T3 Linux 打包与资产上传

- 目标：同一个 tag 产出 deb 与 AppImage 并挂到该 Release。
- 范围：`.github/workflows/desktop.yml`；`ubuntu-22.04`；安装 webkit 依赖；`tauri-action` 出 `deb,appimage` 并上传；`workflow_dispatch` 输入 `release_tag` 用于补包。
- 验收：tag 触发的运行里 Release 有 deb + AppImage 两个资产；手动触发能把资产补到已有 Release；资产上传不改 Release 正文。
- 依赖：T2。回滚：删除工作流。

## T4 传输方式与 WebDAV 凭据

- 目标：设置页能选传输方式并保存 WebDAV 凭据，旧配置不受影响。
- 范围：`AppConfig` 加 `transport`（`local` | `webdav`，`serde(default)` = `local`，`CONFIG_VERSION` 仍为 1）；`webdav.json` 读写（0600、原子写、空密码保留原值、只存三字段）；设置页三字段 + 坚果云预设 + 传输方式选择；未选过传输方式的新配置默认选中 WebDAV。
- 验收：现有 `config.json` 能正常读入并落为 `local`；保存后 `webdav.json` 权限 600；密码留空保存后原值不变；密码不出现在 command 返回值、日志与错误信息中。
- 依赖：无。回滚：删字段与文件读写；`webdav.json` 为新增文件，无迁移负担。

## T5 WebDAV 客户端

- 目标：五个动词可用，协议细节可单测。
- 范围：新增传输模块；`reqwest`（阻塞客户端 + rustls 系统根证书，说明新增依赖的理由）；`PROPFIND` / `MKCOL` / `PUT` / `GET` / `DELETE`；窄 XML 解析（只取 `href`、`resourcetype`、`getcontentlength`，容忍命名空间前缀与 URL 编码）；HTTP 状态到失败码的映射。
- 验收：XML 夹具覆盖空 multistatus、多前缀、含中文与空格的 href；请求形状（方法、路径编码、`Depth`、条件头）用 `HttpTransport` 假实现断言；不引 HTTP mock 依赖、不引 WebDAV SDK。
- 依赖：无。回滚：删除模块与 `Cargo.toml` 依赖。

## T6 连通性探测

- 目标：设置页能测出地址/凭据/写权限是否可用。
- 范围：command + 设置页按钮；流程 `PROPFIND PiAgentSync/`（不存在则 `MKCOL`）→ 复核 → `PUT` 写探针 → `DELETE` 清理；删除失败不算认证失败，提示残留。
- 验收：错误地址 → `remote_unreachable`；错误密码 → `remote_auth_failed`；只读账号 → 写探针失败但读通过时给出可读文案；成功后再点一次仍成功（幂等）。
- 依赖：T4、T5。回滚：移除 command 与按钮。

## T7 上传与冲突判定

- 目标：快照页能把当前快照推到 WebDAV，并在远端被别人改过时停下来问。
- 范围：导出到本机暂存 → 读远端 manifest → 四态判定 → MKCOL → 增量 PUT → manifest 条件 PUT（`If-Match` / `If-None-Match: *`）→ 写 `remote-state.json`；快照页上传入口与最小冲突交互（下载 / 覆盖）。
- 验收：四种状态各一个用例；只传 `sha256` 变化的文件；模拟 412 → `remote_changed` 且不重试；上传中断后远端旧 manifest 仍可读、下载端报哈希不匹配；本机 live 全程不被修改。
- 依赖：T4、T5。回滚：移除入口与 command；远端残留文件不影响本机。

## T8 下载与暂存

- 目标：能按远端 manifest 取回快照并走现有校验与落地流程。
- 范围：`GET manifest` → 逐文件取到 `<应用数据目录>/staging/<时间戳>/` → 校验哈希 → 复用 `verify` / `plan` / `apply`；成功后更新 `remote-state.json` 并删除暂存目录；失败删除暂存并保持 live 不动；同步更新 `README.md` 与 `CHANGELOG.md` 的传输说明。
- 验收：哈希不匹配时拒绝且未写 live；暂存目录在成功与失败两条路径上都被清理；`plan_stale` 仍按现有语义生效；状态区显示远端 fingerprint 与上次同步时间。
- 依赖：T4、T5。回滚：移除 command 与入口。
