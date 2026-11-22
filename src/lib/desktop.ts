import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { open } from '@tauri-apps/plugin-dialog';

/** 是否运行在 Tauri 桌面壳内。普通浏览器里为 false。 */
export const isDesktop = isTauri();

/** 弹出文件夹选择框。浏览器下不可用，调用方需要先判断 `isDesktop`。 */
export async function pickDirectory(title: string): Promise<string | null> {
  if (!isDesktop) {
    return null;
  }

  const selected = await open({ directory: true, multiple: false, title });
  return typeof selected === 'string' ? selected : null;
}

/** 与 Rust 侧 `WebdavSummary` 一一对应：只有地址、用户名与「是否已存密码」。 */
export type WebdavSummary = {
  url: string;
  username: string;
  hasPassword: boolean;
};

export type WebdavInput = {
  url: string;
  username: string;
  /** 缺省或空串表示保留已保存的密码。 */
  password?: string;
};

/** 与 Rust 侧 `WebdavProbeView` 一一对应。 */
export type WebdavProbe = {
  /** 本次探测新建了 PiAgentSync 目录。 */
  created: boolean;
  /** 写权限测试文件没能删掉（只读账号常见），整体仍算连接成功。 */
  leftoverWriteTestFile: boolean;
};

/** 凭据只在桌面壳内可读写，所以和目录选择放在同一层。 */
export function loadWebdavSummary(): Promise<WebdavSummary> {
  return invoke<WebdavSummary>('load_webdav_summary');
}

export function saveWebdavCredentials(
  input: WebdavInput,
): Promise<WebdavSummary> {
  return invoke<WebdavSummary>('save_webdav_credentials', {
    url: input.url,
    username: input.username,
    password: input.password ?? null,
  });
}

/** 试一次连接：不保存凭据，可以测还没保存的表单值。 */
export function testWebdavConnection(input: WebdavInput): Promise<WebdavProbe> {
  return invoke<WebdavProbe>('test_webdav_connection', {
    url: input.url,
    username: input.username,
    password: input.password ?? null,
  });
}

/** 与 Rust 侧 `DownloadView` 一一对应：一份包已经解到本机暂存目录。 */
export type RemoteDownload = {
  packageName: string;
  /** manifest 里的创建时间，RFC 3339 UTC。 */
  createdAt: string;
  fileCount: number;
  /** 暂存目录；可以当作 `root` 传给 `verify_and_plan` 与 `apply_snapshot`。 */
  stagingRoot: string;
};

/** 存储根里的一个包。 */
export type PackageEntry = {
  /** `<UTC yyyymmdd-HHmmss>-<指纹前 8 位>.zip`。 */
  name: string;
  size: number;
  createdAt: string;
};

/** 与 Rust 侧 `PackageListView` 一一对应。 */
export type PackageList = {
  /** 按名字倒序：最新在前。 */
  packages: PackageEntry[];
  /** 默认选中项（名字最大的一份）；存储根为空时为 null。 */
  newest: string | null;
  /** 无法识别的文件（`.tmp-*`、云盘冲突副本等）：只展示，不可操作。 */
  unrecognized: string[];
};

/** 列举存储根里的快照包。 */
export function listSnapshotPackages(): Promise<PackageList> {
  return invoke<PackageList>('list_snapshot_packages');
}

/**
 * 取一份包到本机暂存目录，不写本机配置。
 *
 * 省略 `packageName` 时取存储里名字最大的一份。
 */
export function downloadRemoteSnapshot(
  packageName?: string | null,
): Promise<RemoteDownload> {
  return invoke<RemoteDownload>('download_remote_snapshot', {
    package: packageName ?? null,
  });
}

/** 与 Rust 侧 `ExportReport` 一一对应：导出得到的包。 */
export type SnapshotExport = {
  /** 内容与存储里最新包相同：这次没有写新包。 */
  unchanged: boolean;
  packageName: string;
  /** 包文件字节数。 */
  size: number;
  /** 包内快照文件数（不含 manifest）。 */
  files: number;
  fingerprint: string;
  createdAt: string;
  plaintextSecrets: boolean;
};

/** 与 Rust 侧 `UploadView` 一一对应：一次上传的包语义结果。 */
export type UploadResult = {
  /** 上传的包名；内容未变时是上次上传的那份。 */
  package: string;
  /** 内容与上次上传相同：没有写新包、没动远端。 */
  unchanged: boolean;
  /** 本次清理掉的旧包名。 */
  pruned: string[];
  /** 清理失败的份：不阻塞上传成功。`name` 为空表示整个清理动作失败。 */
  pruneFailed: { name: string; reason: string }[];
};

/** 把当前配置导出成一个快照包，WebDAV 模式下同时上传并清理旧包。 */
export function uploadSnapshot(): Promise<UploadResult> {
  return invoke<UploadResult>('upload_snapshot');
}

/** 与 Rust 侧 `AutoSyncStatus` 一一对应。`state` 是有限状态机，不是自由文本。 */
export type AutoSyncState =
  | 'idle'
  | 'running'
  | 'success'
  | 'diverged'
  | 'failed'
  | 'skipped'
  | 'paused';

export type AutoSyncStatus = {
  state: AutoSyncState;
  message: string | null;
  /** 最近一次真正尝试上传的时间；从未尝试过为 null。 */
  lastAt: string | null;
};

export function loadAutoSyncStatus(): Promise<AutoSyncStatus> {
  return invoke<AutoSyncStatus>('auto_sync_status');
}

/** 唤醒后台线程立即上行；只返回当前状态，完成结果走 `auto-sync-status` 事件。 */
export function triggerAutoSync(): Promise<AutoSyncStatus> {
  return invoke<AutoSyncStatus>('trigger_auto_sync');
}

export function listenAutoSyncStatus(
  handler: (status: AutoSyncStatus) => void,
): Promise<UnlistenFn> {
  return listen<AutoSyncStatus>('auto-sync-status', (event) => handler(event.payload));
}
