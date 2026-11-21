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
