# WebDAV 传输

传输层的一个实现：把同一份快照搬到远端，或从远端取回。快照格式、校验、计划与落地语义不变（见 `10-snapshot-sync.md`）。

## 1. 边界

| 负责 | 不负责 |
| :--- | :--- |
| 远端目录创建、文件读写、manifest 比对、冲突判定、凭据读取 | 决定快照内容、修改 live、加密、多远端、增量索引、断点续传 |

- 只支持 WebDAV，不做 OAuth 类云盘直连，不做 S3 兼容。
- 明文上传。用户选择 WebDAV 即已把信任边界延伸到服务商；不提供半吊子加密（需要时用服务商侧或 rclone crypt 解决）。

## 2. 凭据

位置：系统应用数据目录下的 `webdav.json`（与 `backups/` 同级，**不在同步文件夹内**），权限 `0600`，原子写入。

| 字段 | 必选 | 说明 |
| :--- | :--- | :--- |
| `url` | 是 | WebDAV 服务地址，如 `https://dav.jianguoyun.com/dav`；只接受 http/https |
| `username` | 是 | 坚果云填注册邮箱 |
| `password` | 是 | 应用密码（可单独吊销），不是账号主密码 |

- 设置页给「坚果云」预设，点选只填地址；其他服务商由用户粘贴地址。
- 保存时空密码 = 保留原值（避免只改地址就把密码清掉）。
- 凭据不进入快照、备份、日志、错误信息与 command 返回值；也不写进同步文件夹（否则会被云盘客户端传走）。
- 明文落盘，UI 必须写明。系统钥匙串不在本期范围（见 `backlog.md`）。

## 3. 远端布局

```text
<url>/PiAgentSync/
├── snapshot/…
└── manifest.json
```

- `PiAgentSync/` 是常量，不开放配置；地址语义 = 服务地址，不是"云盘里的某个文件夹"。
- 与本地文件夹模式完全同构：同一份快照可以用两种方式搬运，校验与落地走同一套代码。

## 4. 协议

| 动词 | 用途 | 要点 |
| :--- | :--- | :--- |
| `PROPFIND` | 列探测目录 | `Depth: 1`；只取 `resourcetype` / `getcontentlength` |
| `MKCOL` | 建目录 | 幂等；405 视为已存在；**不带尾斜杠** |
| `PUT` | 上传文件、写 manifest | 覆盖同名；manifest 额外带条件头 |
| `GET` | 取 manifest 与快照文件 | 404 → 不存在，不抛错 |
| `DELETE` | 清理探测用的临时文件 | 失败不降级为认证失败 |

- 超时：控制请求 30s，单文件传输 120s；不自动重试。
- 认证：HTTP Basic。
- **不使用 `MOVE`**：每个文件独立 `PUT`，manifest 最后写。代价是 manifest 写入中断会让远端清单损坏——此时读取报 `remote_manifest_corrupt`，由用户选择以本机覆盖。

### 服务器差异（同类实现的真机结论）

| 现象 | 应对 |
| :--- | :--- |
| `PROPFIND` 对不存在的集合返回 207 + 空 body | 原始条目数为 0 即视为不存在 |
| `MKCOL` 带尾斜杠时返回 405 却没创建 | 不带尾斜杠；405/301/302 视为已存在 |
| 云盘根目录不允许 `PUT` 文件 | 写探针放在 `PiAgentSync/` 子目录内 |
| 目录 `DELETE` 需要尾斜杠 + `Depth: infinity` | 仅用于探测清理，失败只记日志 |
| 部分服务商禁止 `DELETE` | 探测仍判成功，提示残留一个测试文件 |
| 下载/上传配额（507，或 403 + 配额提示） | 映射 `remote_quota`，提示流量或空间不足 |

## 5. 上传

```text
1. 导出     按 10-snapshot-sync §5 生成新快照到本机暂存目录（含新 manifest）
2. 读远端   GET manifest.json
   ├─ 404        → 首次上传
   ├─ 无法解析    → remote_manifest_corrupt，停止，由用户选择覆盖
   └─ 正常        → 取 fingerprint 与 ETag
3. 冲突判定  见 §7
4. 建目录   按 manifest 里的路径建出缺失的目录（浅到深，MKCOL 幂等）
5. 传文件   与新 manifest 逐项比对 path + sha256，只 PUT 变化的文件
6. 提交     PUT manifest.json
   ├─ 远端已有清单 → If-Match: <etag>
   └─ 远端无清单   → If-None-Match: *
   └─ 412          → remote_changed，停止，不重试
7. 收尾     记录远端状态（§7）；提示"等待云盘侧完成"不适用于本模式
```

- 失败停在原地：已上传的文件留在远端，但旧 manifest 仍指向旧快照；不会出现"半套快照被应用"。
- 旧文件不自动删除（沿用 D2b 的删除语义）。

## 6. 下载与状态

```text
1. GET manifest.json → 404 报 remote_not_found；无法解析报 remote_manifest_corrupt
2. 状态：远端 fingerprint == 记录值 ? 已是最新 : 云端有更新
3. 下载：按 manifest 逐文件 GET 到 <应用数据目录>/staging/<时间戳>/，逐个校验 sha256
4. 之后完全复用 10-snapshot-sync §6：计划 → 预览 → 备份 → 应用
5. 成功后更新远端状态；暂存目录保留给计划与应用读取，下次成功下载时清理更早的
```

- 任一步失败删除暂存目录，本机 live 不动。
- 状态查询 = 1 次 GET manifest；不做定时轮询。

## 7. 冲突与远端状态

本机记录：应用数据目录下的 `remote-state.json`，非密钥，保存**上次成功同步时看到的整份远端 manifest** 与时间：

```json
{ "seenAt": "2026-11-20T05:30:00Z", "manifest": { …远端 manifest 原样… } }
```

| 远端 manifest | 本机记录 | 判定 | 可用动作 |
| :--- | :--- | :--- | :--- |
| 不存在 | 任意 | 首次上传 | 上传 |
| 存在 | 无 | 冲突（保守） | 下载 / 覆盖 |
| 存在 | fingerprint 相同 | 无人改过 | 上传 |
| 存在 | fingerprint 不同 | 冲突 | 下载 / 覆盖 |

- 判定只认 `fingerprint`；不用时间戳、`getlastmodified` 或 ETag 判断"内容是否相同"。
- 冲突展示到文件级：拿远端 manifest 与记录值按 `path + sha256` 比对，列出新增/修改/删除的文件名。**不下载内容做 diff**。
- 两个动作都要用户点；覆盖前提示"云端现有快照将被替换"，并在本机留一份被覆盖的远端 manifest 副本（`remote-state.json` 的上一份）。
- `remote-state.json` 缺失（换机器、换地址）按"无记录"处理，表现是一次保守的冲突确认，不丢数据。

## 8. 失败码

| code | 含义 |
| :--- | :--- |
| `remote_not_configured` | 地址/用户名/密码不完整 |
| `remote_unreachable` | DNS、TLS、超时、5xx 等网络失败 |
| `remote_auth_failed` | 401/403（凭据无效或权限不足） |
| `remote_quota` | 507，或 403 且响应体提示配额 |
| `remote_manifest_corrupt` | 远端 manifest 无法解析或格式版本不支持 |
| `remote_not_found` | 下载时远端没有 manifest |
| `remote_changed` | 提交时条件请求失败（远端在此期间被改写） |

## 9. 已知限制

- 不做断点续传、不做限流退避、不做增量索引：全量重算后只传变化文件。
- 不做多远端、不做多 profile（manifest 已预留 `profileId`）。
- 远端旧文件不会被本应用发现或清理，长期使用会在网盘侧累积；需要时在网盘里手动处理。
- 暂存目录保留最近一次下载（计划与应用需要读它），不是用完即删。
- 服务商频率限制未做专门处理；上传请求数 = 变更文件数 + 目录数 + 2。
