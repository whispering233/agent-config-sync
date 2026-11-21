# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 批次与依赖

| 卡 | 内容 | 依赖 | 可并行 |
| :--- | :--- | :--- | :--- |
| T4 | 传输方式与 WebDAV 凭据 | — | 与 T5 |
| T5 | WebDAV 客户端 | — | 与 T4 |
| T6 | 连通性探测 | — | 与 T7、T8 |
| T7 | 上传与冲突判定 | — | 与 T6、T8 |
| T8 | 下载与暂存 | — | 与 T6、T7 |

## T6 连通性探测

- 目标：设置页能测出地址/凭据/写权限是否可用。
- 范围：command + 设置页按钮；流程 `PROPFIND PiAgentSync/`（不存在则 `MKCOL`）→ 复核 → `PUT` 写探针 → `DELETE` 清理；删除失败不算认证失败，提示残留。
- 验收：错误地址 → `remote_unreachable`；错误密码 → `remote_auth_failed`；只读账号 → 写探针失败但读通过时给出可读文案；成功后再点一次仍成功（幂等）。
- 依赖：无。回滚：移除 command 与按钮。

## T7 上传与冲突判定

- 目标：快照页能把当前快照推到 WebDAV，并在远端被别人改过时停下来问。
- 范围：导出到本机暂存 → 读远端 manifest → 四态判定 → MKCOL → 增量 PUT → manifest 条件 PUT（`If-Match` / `If-None-Match: *`）→ 写 `remote-state.json`；快照页上传入口与最小冲突交互（下载 / 覆盖）。
- 验收：四种状态各一个用例；只传 `sha256` 变化的文件；模拟 412 → `remote_changed` 且不重试；上传中断后远端旧 manifest 仍可读、下载端报哈希不匹配；本机 live 全程不被修改。
- 依赖：无。回滚：移除入口与 command；远端残留文件不影响本机。

## T8 下载与暂存

- 目标：能按远端 manifest 取回快照并走现有校验与落地流程。
- 范围：`GET manifest` → 逐文件取到 `<应用数据目录>/staging/<时间戳>/` → 校验哈希 → 复用 `verify` / `plan` / `apply`；成功后更新 `remote-state.json` 并删除暂存目录；失败删除暂存并保持 live 不动；同步更新 `README.md` 与 `CHANGELOG.md` 的传输说明。
- 验收：哈希不匹配时拒绝且未写 live；暂存目录在成功与失败两条路径上都被清理；`plan_stale` 仍按现有语义生效；状态区显示远端 fingerprint 与上次同步时间。
- 依赖：无。回滚：移除 command 与入口。
