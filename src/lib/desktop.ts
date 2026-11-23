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
  /** 包名：`<UTC yyyymmdd-HHmmss>-<指纹前 8 位>.zip`。 */
  package: string;
  /** 包文件字节数。 */
  size: number;
  /** 包内快照文件数（不含 manifest）。 */
  files: number;
  /** manifest 里的完整指纹。 */
  fingerprint: string;
  /** manifest 里的创建时间，RFC 3339 UTC。 */
  createdAt: string;
  /** 暂存目录；可以当作 `root` 传给 `verify_and_plan` 与 `apply_snapshot`。 */
  stagingRoot: string;
  /** 上一次成功上传/下载时看到的远端时间；本地文件夹模式没有远端，为 null。 */
  previousSeenAt: string | null;
};

/** 存储根 / 云端里的一个包。 */
export type PackageEntry = {
  /** `<UTC yyyymmdd-HHmmss>-<指纹前 8 位>.zip`。 */
  name: string;
  size: number;
  /** 包名里的时间戳换算成的 RFC 3339 UTC；展示时换算本地时区。 */
  createdAt: string;
  /** 包名里的指纹前 8 位：只用于显示与人工核对。 */
  fingerprint8: string;
  /** 名字最大的一份：列表的默认选中项。 */
  isNewest: boolean;
};

/** 与 Rust 侧 `PackageListView` 一一对应。 */
export type PackageList = {
  /** 按名字倒序：最新在前。 */
  packages: PackageEntry[];
  /** 无法识别的文件（`.tmp-*`、云盘冲突副本等）：只展示，不可操作。 */
  unrecognized: string[];
  /** `unrecognized` 里名字是本应用上传残留的那些：只有它们可以被清理动作删掉。 */
  tempFiles: string[];
  /** 上次成功上传/下载时看到的云端时间；本地文件夹模式没有记录，为 null。 */
  seenAt: string | null;
  /** 云端有、本机记录里没有的包数；本地文件夹模式恒为 0。 */
  unseenCount: number;
};

/**
 * 列举当前传输方式的快照包，按名字倒序。只读：WebDAV 模式下只发 `PROPFIND`，
 * 不改远端，也不改本机记录。
 */
export function listSnapshotPackages(): Promise<PackageList> {
  return invoke<PackageList>('list_snapshot_packages');
}

/** 对比表的一行：一份候选包对当前配置的只读计划结果。 */
export type CompareRow = {
  /** 包名：`<UTC yyyymmdd-HHmmss>-<指纹前 8 位>.zip`。 */
  package: string;
  /** 包名时间戳换算成的 RFC 3339 UTC。 */
  createdAt: string;
  /** 包名里的指纹前 8 位。 */
  fingerprint8: string;
  /** 失败行为 null：下面的计数与路径都不成立。 */
  added: number | null;
  modified: number | null;
  /** 「将删除」：当前配置里有、包里没有的受管文件数。 */
  deleted: number | null;
  unchanged: number | null;
  /** 将被删掉的具体条目：所属根 + 根内相对路径。 */
  deletedItems: { root: RootId; path: string }[];
  error: { code: string; message: string } | null;
};

/** 两个候选包之间的文件级差异：`added` 是右边有左边没有。 */
export type ComparePair = {
  left: string;
  right: string;
  added: string[];
  modified: string[];
  removed: string[];
};

/** 与 Rust 侧 `CompareReport` 一一对应：`rows` 按包名倒序。 */
export type CompareReport = {
  rows: CompareRow[];
  /** 候选少于两份时为 null。 */
  pair: ComparePair | null;
};

/**
 * 对比候选包与当前配置：只读，不改本机配置、不改云端、不写备份。
 *
 * 缺省不传名字就没得对比；每份包解到临时目录跑计划，结束后整体清掉。
 */
export function compareSnapshotPackages(
  packages: string[],
): Promise<CompareReport> {
  return invoke<CompareReport>('compare_snapshot_packages', { packages });
}

/** 与 Rust 侧 `CleanupOutcome` 一一对应：清理上传残留的三桶结果。 */
export type LeftoverCleanup = {
  /** 已删掉的项。 */
  deleted: string[];
  /** 删不掉的项：单项失败不影响其它项，整次清理仍算跑完。 */
  failed: { name: string; reason: string }[];
  /** 不在清理范围、没有动的无法识别文件。 */
  skipped: string[];
};

/**
 * 清理当前传输方式下的上传残留，只删本应用自己产生过的临时名。
 *
 * 候选由核心侧重新列举后算出，因此重复调用是幂等的；合法包、`manifest.json`、
 * `snapshot/` 与外来文件不会进入候选。
 */
export function cleanupUploadLeftovers(): Promise<LeftoverCleanup> {
  return invoke<LeftoverCleanup>('cleanup_upload_leftovers');
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

/** 受管根，与 Rust 侧 `RootId` 一致：宿主目录与用户级 `~/.agents/skills`。 */
export type RootId = 'agent' | 'agents-skills';

/** 一次导出里的一条密钥提示：只有位置，不含密钥值。 */
export type SecretFinding = {
  path: string;
  field: string | null;
  line: number;
  level: 'high' | 'low';
  rule: string;
};

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
  /** 本次快照声明的受管根。 */
  managedRoots: RootId[];
  requiredPackages: { source: string; enabled: boolean }[];
  /** 断链等被跳过的条目数。 */
  skipped: number;
  /** 只读扫描结果：不阻断，也没有自动改写。 */
  warnings: SecretFinding[];
  /** 根级别的告警：根是符号链接、外部根是断链等。 */
  rootWarnings: string[];
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
  /** 导出时的根告警（根是链接、外部根断链等）。 */
  rootWarnings: string[];
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

/** 与 Rust 侧 `AppUpdateInfo` 一一对应：一次检查发现的可用版本。 */
export type AppUpdateInfo = {
  currentVersion: string;
  version: string;
  notes: string | null;
  /** `latest.json` 里的发布时间原文；没有填写时为 null。 */
  date: string | null;
  target: string;
  /** 检查阶段拿不到体积，恒为 null；总量由进度事件给出。 */
  downloadSize: number | null;
};

/** 与 Rust 侧 `AppUpdateResult` 一一对应：只描述已经发起的安装。 */
export type AppUpdateResult = {
  installStarted: boolean;
  version: string;
  restartRequired: boolean;
};

/** 与 Rust 侧 `AppUpdateProgress` 一一对应：事件里只有阶段与字节数。 */
export type AppUpdateProgress = {
  phase: 'downloading' | 'installing';
  downloaded: number;
  /** 拿不到 `Content-Length` 时为 null，此时界面只显示已下载。 */
  total: number | null;
};

/** 检查一次更新。开发态与未分发平台返回 `update_unsupported`，不发请求。 */
export function checkAppUpdate(): Promise<AppUpdateInfo | null> {
  return invoke<AppUpdateInfo | null>('check_app_update');
}

/**
 * 下载并安装用户确认的版本。
 *
 * Linux 安装成功后本进程重启，Promise 不会正常返回；Windows 由安装器退出进程。
 * 因此正常返回也只代表安装已发起，不代表界面还能继续用。
 */
export function installAppUpdate(version: string): Promise<AppUpdateResult> {
  return invoke<AppUpdateResult>('install_app_update', { version });
}

export function listenAppUpdateProgress(
  handler: (progress: AppUpdateProgress) => void,
): Promise<UnlistenFn> {
  return listen<AppUpdateProgress>('app-update://progress', (event) =>
    handler(event.payload),
  );
}
