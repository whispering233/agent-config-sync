import { invoke } from '@tauri-apps/api/core';

export type PathStatus =
  | 'unset'
  | 'ok'
  | 'invalid'
  | 'missing'
  | 'not_writable';

export type FindingLevel = 'error' | 'warning';

export type ConfigField =
  | 'syncRoot'
  | 'agentDir'
  | 'backupRetention'
  | 'autoSyncInterval';

export type AgentDirSource = 'override' | 'env' | 'default' | 'unresolved';

/** 快照的传输方式；旧配置没有该字段时后端按 `local` 读入。 */
export type Transport = 'local' | 'webdav';

/** 与 Rust 侧 `AppConfig` 一一对应。 */
export type AppConfig = {
  version: number;
  syncRoot: string | null;
  agentDirOverride: string | null;
  backupRetention: number;
  transport: Transport;
  /** 定时自动上行只上传，不自动下载或应用。 */
  autoSync: boolean;
  autoSyncIntervalMinutes: number;
  /** 默认关闭：避免把本机可能已轮换的 OAuth 凭据覆盖到云端。 */
  autoSyncIncludeCredentials: boolean;
  launchAtLogin: boolean;
};

export type ValidationFinding = {
  level: FindingLevel;
  field: ConfigField;
  code: string;
  message: string;
};

/** 与 Rust 侧 `ConfigValidation` 一一对应：校验结果 + 生效路径。 */
export type ConfigValidation = {
  ok: boolean;
  findings: ValidationFinding[];
  syncRootStatus: PathStatus;
  agentDirStatus: PathStatus;
  effectiveAgentDir: string | null;
  agentDirSource: AgentDirSource;
};

export type AppConfigView = {
  config: AppConfig;
  validation: ConfigValidation;
  configFilePath: string;
};

export const AGENT_DIR_ENV_VAR = 'PI_CODING_AGENT_DIR';

export function loadAppConfig(): Promise<AppConfigView> {
  return invoke<AppConfigView>('load_app_config');
}

export function validateAppConfig(config: AppConfig): Promise<AppConfigView> {
  return invoke<AppConfigView>('validate_app_config', { config });
}

export function saveAppConfig(config: AppConfig): Promise<AppConfigView> {
  return invoke<AppConfigView>('save_app_config', { config });
}
