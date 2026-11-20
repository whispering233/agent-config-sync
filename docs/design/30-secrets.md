# 密钥处理

## 1. 原则

1. **快照里不出现明文密钥**。唯一允许的形式是宿主原生支持的环境变量/命令引用。
2. **不发明私有语法**。快照文件必须能被宿主直接读取，不做 `${secret:...}` 这类需要二次解释的占位。
3. **不把快照当密钥库**。应用不加密快照、不管理密钥文件；密钥属于本机。
4. **可解释**：每一条被替换的密钥都能在 manifest 里找到对应条目与补齐说明。

## 2. 引用语法（宿主原生）

| 文件 | 语法 | 示例 | 依据 |
| :--- | :--- | :--- | :--- |
| `models.json` | `"$NAME"` 或 `"!command"` | `"apiKey": "$OPENROUTER_API_KEY"` | 官方 models 配置说明 |
| `mcp.json` | `"${NAME}"` 或 `"!command"` | `"Authorization": "Bearer ${DOCS_TOKEN}"` | 官方 MCP 说明 |
| `web-search.json` | `"$NAME"` | `"exaApiKey": "$EXA_API_KEY"` | 该扩展自身约定（本机实测已使用） |

注意三者语法不同：`models.json` 与 `web-search.json` 用 `$NAME`，`mcp.json` 用 `${NAME}`。导出时按文件选择正确形式。

## 3. `auth.json`

- **永不纳入快照**。它只有账号与密钥，没有可移植的配置语义。
- 本机由密钥向导直接写入宿主原生结构（`{provider: {type, key}}`）。
- OAuth 类凭据**不代写**：只能引导用户执行 `pi auth login <provider>`（该命令需要终端，应用只提供"在终端中打开"）。

## 4. 扫描规则

扫描对象：受管范围内的所有文本文件（不只 JSON，包含 `extensions/*.ts`、`skills/**/SKILL.md`）。

| 规则 | 说明 |
| :--- | :--- |
| 字段名启发式 | 键名匹配 `api[-_]?key`、`token`、`secret`、`password`、`authorization`、`bearer` 等 |
| 已知前缀 | `sk-`、`sk-ant-`、`ghp_`、`github_pat_`、`xox[baprs]-`、`AKIA`、`AIza`、`hf_`、`bce-v3/`、`oc_sk_` 等 |
| 熵阈值 | 长度 ≥ 20 且 Shannon 熵 ≥ 4.5 的连续字符串（排除纯十六进制哈希与版本号） |
| 排除 | `sha256`/`integrity`/`etag` 字段、语义化版本、模型 id、URL 中的公开路径 |

判定分级：

- `high`：命中前缀或字段名+高熵 → 导出时必须逐条确认。
- `low`：仅高熵 → 提示，默认可忽略。

导出时：`high` 未处理则**阻断**，必须选择"替换为引用"或"显式保留明文"（后者在 manifest 记录 `plaintextSecrets: true`，导入端会警告）。

## 5. `requiredSecrets`

导出时生成，写入 manifest：

```json
{ "name": "EXA_API_KEY", "usedBy": ["snapshot/web-search.json"], "hint": "https://exa.ai" }
```

- 名称来源：用户已用环境变量引用 → 沿用原名；字面值被替换 → 按 provider/字段生成稳定名称（如 `PI_<PROVIDER>_API_KEY`），并在 UI 中允许改名。
- `hint` 只放官方申请地址，不猜测第三方中转站。

## 6. 补齐流程（导入侧）

对每个缺失项：

1. 检测本机环境是否已有该变量（登录 shell 读取一次快照）。
2. 给出两条路：
   - **A 环境变量**：展示 `export NAME=...`（POSIX）或 `setx NAME ...`（Windows）命令，并提供"复制命令"。v1 **不自动写 shell 启动文件**。
   - **B 写入本机字面值**：用户输入后写进本机对应文件（仅本机，不回写快照）。此路径会提示"下次导出仍会被引用化"。
3. 无输入时允许跳过，标记为待办，不阻断应用。

补齐顺序：先写配置，再提示重启宿主或 `/reload`。

### 写入本机字面值的实现约束

- 只处理 manifest 里 `requiredSecrets[].usedBy` 列出的文件：去掉 `snapshot/` 前缀后映射回宿主目录，并经受管范围校验。
- 替换顺序：先 `${NAME}` 再 `$NAME`；`$NAME` 后必须不是 `[A-Za-z0-9_]`，避免 `$FOO` 吃掉 `$FOOBAR`。
- 原子写（同目录临时文件 + rename），保留原文件 mode；至少命中一处，否则报 `reference_not_found`。
- 值上限 8192 字节；值本身不参与二次匹配；**不写入日志、错误信息或返回结构**。
- v1 不自动写环境变量、不修改 shell 启动文件，只提供可复制的 `export` / `setx` 命令。

## 7. 禁止项

- 不把密钥写入日志、备份目录、错误信息。
- 不在 manifest 中记录密钥值或片段。
- 不对快照整体加密（加密会让文件无法被宿主直接读取，且制造"必须用本应用"的锁定）。
- 不删除用户原有环境变量，不覆盖不相关的 shell 配置。
- 不代写 OAuth 令牌。

## 8. 已知限制

- 不支持字段级加密分享（P2 的分享包才考虑，且优先"剥离 + 提示"而非加密）。
- 用户若在 `models.json` 里使用 `!command` 形式，导出原样保留；但导入端无法验证命令是否可用。
- 第三方扩展的配置文件若不在 agent 目录根级，本设计覆盖不到，需在 backlog 中评估。
