import { invoke, isTauri } from '@tauri-apps/api/core';
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

/** 与 Rust 侧 `DownloadView` 一一对应：一次下载落到了本机暂存目录。 */
export type RemoteDownload = {
  /** 暂存目录；可以当作 `root` 传给 `verify_and_plan` 与 `apply_snapshot`。 */
  stagingRoot: string;
  fingerprint: string;
  fileCount: number;
  createdAt: string;
  /** 本次下载之前记录的同步时间；本机没有记录时为 null。 */
  previousSeenAt: string | null;
};

/** 按远端 manifest 把快照取到本机暂存目录。不写本机配置，也不碰同步文件夹。 */
export function downloadRemoteSnapshot(): Promise<RemoteDownload> {
  return invoke<RemoteDownload>('download_remote_snapshot');
}

/** 一次上传的结局。冲突不是错误：远端一个字节都没写，等用户选一边。 */
export type RemoteUpload =
  | {
      kind: 'uploaded';
      fingerprint: string;
      /** 真正 PUT 上去的文件数。 */
      uploaded: number;
      /** 与远端已一致、跳过的文件数。 */
      skipped: number;
      /** 本次上传后记录的同步时间。 */
      seenAt: string;
    }
  | {
      kind: 'conflict';
      remoteFingerprint: string;
      /** 下面三个清单描述远端相对上次记录的变化；本机无记录时都是空数组。 */
      added: string[];
      modified: string[];
      removed: string[];
      /** 远端清单损坏：只能覆盖，无法先下载。 */
      remoteCorrupt: boolean;
    };

/**
 * 把当前配置导出并推到 WebDAV。
 *
 * `overwrite` 只在用户看过冲突清单后点「覆盖云端」时为 true。
 */
export function uploadSnapshot(overwrite: boolean): Promise<RemoteUpload> {
  return invoke<RemoteUpload>('upload_snapshot', { overwrite });
}
