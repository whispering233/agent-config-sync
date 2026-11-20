# Changelog

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added

- 桌面应用骨架：Tauri v2 + React + TypeScript + Tailwind，三页导航与设计 token。
- 应用配置：同步文件夹与宿主目录覆盖、路径校验、生效目录解析、原子写入。
- 受管范围与遍历：白名单/排除规则、符号链接物化、断链与越界防护。
- 只读环境医生：登录 shell 探测 node/npm/pi、凭据状态、包与断链，输出分级报告。
- 宿主安装与升级：前置检查、按平台与安装类型选择命令、确认后执行、进程组终止。
- 快照导出：脱敏与原生环境变量引用、manifest 与指纹、原子替换、明文密钥阻断。
- 快照校验与计划：manifest 校验、哈希比对、新增/修改/删除预览。
- 应用快照：计划过期保护、删除逐项确认、覆盖前备份、权限沿用、链接重建优先。
- 回滚：备份列表、还原前自动备份、按 restore.json 反做。
- 密钥向导：requiredSecrets 状态、本机字面值写入、环境变量命令提示。
- 包对齐：声明与实装比对，确认后安装缺失项。
- 冲突文件扫描（只读）：命名识别、配对与差异摘要。
- Linux 分发：deb 与 AppImage 打包、关于页、release 去符号。

### Note

- 仅面向 Linux；Windows/macOS 未打包，分享包与自研 WebDAV 传输在 backlog。
- 安装包未签名；图标为占位图。
- AppImage 打包需要从 GitHub 下载 type2 runtime，网络受限时会失败（deb 不受影响）。
