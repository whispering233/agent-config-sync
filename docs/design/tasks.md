# 开发任务卡片

> 临时文件。任务完成并验证后必须从本文件删除，不留历史记录。

## 批次：Windows 自测链路

依赖顺序：W1 → W2、W1 → W3。W2 与 W3 改动的文件不重叠，可并行。

### W1 编译面：Windows 可编译

- 范围：`apply` / `rollback` / `wizard` / `webdav_transfer` 的 POSIX 权限位调用改为编译期分段；Windows 侧不设权限、备份清单 `mode` 记 `null`；符号链接重建按文件/目录分别调用，创建失败回退物化并告警；`main.rs` 加 `windows_subsystem`；测试里的 unix 符号链接加门控。
- 不做：探测命令、打包配置。
- 验证：`cargo test` 全绿；`rustup target add x86_64-pc-windows-msvc` 后 `cargo check --target x86_64-pc-windows-msvc` 通过；`cargo fmt -- --check`。
- 回滚：单点提交，回退即回到仅 Linux 可编译的状态，不影响已有快照格式（`mode` 由必填字段变可选，仅新增取值）。

### W2 环境检测与包对齐在 Windows 可用

- 依赖：W1。
- 范围：`ShellRunner` 与安装命令按平台分支（Windows `cmd /C`）；`doctor` 的三段探测、`installer` 的前置探测、`packages` 的 `pi list` 在 Windows 改为逐命令直跑；参数转义按平台；`npm` 全局前缀下的 `pi` 入口按平台拼接；`skills.links` 的「链接类」判定改为读得到目标即算链接（覆盖 junction）；超时终止整棵进程树。
- 不做：卸载钩子、签名。
- 验证：Windows 命令形态与参数转义有单测（假 runner 断言 argv）；Linux 侧 `cargo test` 全绿且探测行为不变；Windows 真机跑一次环境检测，node/npm/pi 三项有结论、`pi list` 能比对。
- 回滚：单点提交；回退后 Linux 行为不变，Windows 退回「探测不可用」但编译仍通过。

### W3 Windows 安装包

- 依赖：W1。
- 范围：`bundle.targets` 加 `nsis`、`bundle.licenseFile`、`desktop.yml` 加 `windows-latest` job（`cargo test` + `--bundles nsis`，只传 artifact 与 Release 资产）、`build.md` 已补的 Windows 段随代码一起落地。
- 不做：签名、卸载清理钩子（见 `backlog.md`）。
- 验证：CI 出 `*-setup.exe`；真机按 `build.md` 的「Windows 真机验证清单」跑 1–6 项，只报结论不修。
- 回滚：单点提交；回退后只剩 Linux 产物。
