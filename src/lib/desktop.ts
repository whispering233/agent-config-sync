import { isTauri } from '@tauri-apps/api/core';
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
