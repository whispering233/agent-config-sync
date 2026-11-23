import { useSyncExternalStore } from 'react';
import { getVersion } from '@tauri-apps/api/app';
import {
  checkAppUpdate,
  installAppUpdate,
  isDesktop,
  listenAppUpdateProgress,
  type AppUpdateInfo,
  type AppUpdateProgress,
} from './desktop';

/** 浏览器预览与 `tauri dev` 都不检查：开发版本不是发布版本，也不该发请求。 */
export const appUpdateSupported = isDesktop && !import.meta.env.DEV;

/** 启动检查、顶部提示与设置页共用这一份状态，任意一方都不重复请求。 */
export type AppUpdateState = {
  /** 当前版本，来自打包元数据；还没读到时为 null。 */
  currentVersion: string | null;
  /** 最近一次检查发现的可用版本；没有更新或还没检查为 null。 */
  available: AppUpdateInfo | null;
  /** 本进程内至少完成过一次检查。 */
  checked: boolean;
  checking: boolean;
  /** 安装已发起：进程随后重启或退出，界面不再回到可操作状态。 */
  installing: boolean;
  progress: AppUpdateProgress | null;
  failure: string | null;
  /** 用户点了「稍后」：只收起顶部提示，检查结果仍留在设置页。 */
  dismissed: boolean;
};

const INITIAL: AppUpdateState = {
  currentVersion: null,
  available: null,
  checked: false,
  checking: false,
  installing: false,
  progress: null,
  failure: null,
  dismissed: false,
};

let state = INITIAL;
const listeners = new Set<() => void>();
let started = false;
let stopProgress: (() => void) | null = null;
/** 每次重新接上进度监听就换一个号：先取消后重订时，旧的监听器能被认出并摘掉。 */
let progressToken = 0;

function setState(patch: Partial<AppUpdateState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) {
    listener();
  }
}

/** 命令失败时 Rust 侧返回 `{code, message}`，不是 Error 实例。 */
function describeFailure(error: unknown): string {
  if (typeof error === 'object' && error !== null && 'message' in error) {
    return String((error as { message: unknown }).message);
  }
  return error instanceof Error ? error.message : String(error);
}

async function check(): Promise<void> {
  if (!appUpdateSupported || state.checking || state.installing) {
    return;
  }

  setState({ checking: true, failure: null });
  try {
    const available = await checkAppUpdate();
    setState({ checking: false, checked: true, available, dismissed: false });
  } catch (error) {
    // 失败即停：报一次失败，等用户手动重新检查，不自动重试。
    setState({
      checking: false,
      checked: true,
      available: null,
      failure: describeFailure(error),
    });
  }
}

async function install(version: string): Promise<void> {
  if (!appUpdateSupported || state.checking || state.installing) {
    return;
  }

  setState({ installing: true, failure: null, progress: null });
  try {
    // 正常返回也只代表安装已发起（Windows 随后退出进程），不能当成更新完成，
    // 所以这里不回到可操作状态，等进程自己重启或退出。
    await installAppUpdate(version);
  } catch (error) {
    setState({
      installing: false,
      progress: null,
      failure: describeFailure(error),
    });
  }
}

/** 启动检查：本进程只跑一次；开发态与浏览器不发任何请求。 */
export function startAppUpdate(): void {
  if (started) {
    return;
  }
  started = true;

  if (!appUpdateSupported) {
    return;
  }

  getVersion()
    .then((version) => setState({ currentVersion: version }))
    .catch(() => {});
  void check();
}

export function checkForAppUpdate(): void {
  void check();
}

export function installAvailableUpdate(version: string): void {
  void install(version);
}

export function dismissAppUpdate(): void {
  setState({ dismissed: true });
}

function subscribe(listener: () => void): () => void {
  const isFirst = listeners.size === 0;
  listeners.add(listener);

  // 进度事件只服务于正在进行的安装：第一个订阅者出现时接上，最后一个走了就摘掉。
  if (appUpdateSupported && isFirst) {
    const token = (progressToken += 1);
    listenAppUpdateProgress((progress) => setState({ progress }))
      .then((stop) => {
        if (token === progressToken && listeners.size > 0) {
          stopProgress = stop;
        } else {
          stop();
        }
      })
      .catch(() => {});
  }

  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      progressToken += 1;
      stopProgress?.();
      stopProgress = null;
    }
  };
}

export function useAppUpdate(): AppUpdateState {
  return useSyncExternalStore(subscribe, () => state);
}