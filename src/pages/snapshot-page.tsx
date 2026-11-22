import { Fragment, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { TriangleAlert } from 'lucide-react';
import { Button } from '../components/ui/button';
import { Card } from '../components/ui/card';
import { loadAppConfig, type PathStatus, type Transport } from '../lib/app-config';
import {
  cleanupUploadLeftovers,
  compareSnapshotPackages,
  downloadRemoteSnapshot,
  isDesktop,
  listSnapshotPackages,
  uploadSnapshot,
  type ComparePair,
  type CompareReport,
  type LeftoverCleanup,
  type PackageList,
  type RemoteDownload,
  type SnapshotExport,
  type UploadResult,
} from '../lib/desktop';
import { cn } from '../lib/utils';

/** 与 Rust 侧 `ExportFailure` 一一对应。 */
type ExportFailure = {
  code: string;
  message: string;
};

/** 计划项的差异类别，与 Rust 侧 `PlanCategory` 一致。 */
type PlanCategory = 'added' | 'modified' | 'deleted';

/** 受管根，与 Rust 侧 `RootId` 一致：agent 根与用户级 `~/.agents/skills`。 */
type RootId = 'agent' | 'agents-skills';

/** 受管根的可读名称；与 `RootId` 逐一对应。 */
const rootLabels: Record<RootId, string> = {
  agent: '宿主目录',
  'agents-skills': '~/.agents/skills',
};

/** 与 Rust 侧 `PlanItem` 一一对应；`deleted` 时 `snapshotSha256` 为 null。 */
type PlanItem = {
  /** 所属受管根；`path` 相对它，两个根可能有相同相对路径。 */
  root: RootId;
  path: string;
  category: PlanCategory;
  /** `link` 只会出现在删除候选里：只删链接本身，不碰目标。 */
  kind: 'file' | 'link';
  size: number;
  snapshotSha256: string | null;
  liveSha256: string | null;
};

/** 与 Rust 侧 `PlanReport` 一一对应。 */
type PlanReport = {
  manifestInfo: {
    profileId: string;
    createdAt: string;
    fingerprint: string;
    files: number;
    source: {
      app: string;
      os: string;
      arch: string;
      pi: string | null;
      node: string | null;
    };
    requirements: { node: string; pi: string };
  };
  verification: { checked: number };
  plan: { added: PlanItem[]; modified: PlanItem[]; deleted: PlanItem[] };
  totals: {
    snapshotFiles: number;
    liveFiles: number;
    added: number;
    modified: number;
    deleted: number;
    unchanged: number;
  };
  requirements: RequirementCheck[];
};

/** 一条宿主版本要求；`satisfied` 为 null 表示没探测到。 */
type RequirementCheck = {
  tool: string;
  required: string;
  found: string | null;
  satisfied: boolean | null;
};

/** 与 Rust 侧 `HashMismatch` 一一对应。 */
type HashMismatch = {
  path: string;
  reason: 'missing' | 'unreadable' | 'size' | 'hash' | 'out_of_bounds';
  expectedSha256: string;
  actualSha256: string | null;
};

/** 与 Rust 侧 `ApplyReport` 一一对应。 */
type ApplyReport = {
  fingerprint: string;
  backupDir: string;
  backupId: string;
  written: number;
  deleted: number;
  skipped: number;
  linked: number;
  skippedDeletes: number;
  warnings: string[];
  failedAt: string | null;
};

/** 一条删除请求：所属根 + 根内相对路径，与 Rust 侧 `DeleteRequest` 一一对应。 */
type DeleteRequest = {
  root: RootId;
  path: string;
};

/** 与 Rust 侧 `ApplyFailure` 一一对应。 */
type ApplyFailure = {
  code: string;
  message: string;
  completed: number;
  backupDir: string | null;
};

/** 与 Rust 侧 `BackupSummary` 一一对应。 */
type BackupSummary = {
  id: string;
  createdAt: string;
  fingerprint: string;
  entries: number;
};

/** 与 Rust 侧 `BackupList` 一一对应。 */
type BackupList = {
  backups: BackupSummary[];
  warnings: string[];
};

/** 与 Rust 侧 `RestoreReport` 一一对应。 */
type RestoreReport = {
  backupId: string;
  safetyBackupId: string;
  restored: number;
  removed: number;
  warnings: string[];
  failedAt: string | null;
};

/** 与 Rust 侧 `RestoreFailure` 一一对应。 */
type RestoreFailure = {
  code: string;
  message: string;
  completed: number;
  safetyBackupId: string | null;
};

/** 与 Rust 侧 `PlanFailure` 一一对应。 */
type PlanFailure = {
  code: string;
  message: string;
  mismatches: HashMismatch[];
};

type ConfigState =
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; syncRoot: string | null; status: PathStatus; transport: Transport }
  | { kind: 'failed'; message: string };

type DownloadState =
  | { kind: 'idle' }
  | { kind: 'downloading' }
  | { kind: 'done'; view: RemoteDownload }
  | { kind: 'failed'; code: string | null; message: string };

type UploadState =
  | { kind: 'idle' }
  | { kind: 'uploading' }
  | { kind: 'done'; result: UploadResult }
  | { kind: 'failed'; message: string };

type ExportState =
  | { kind: 'idle' }
  | { kind: 'exporting' }
  | { kind: 'done'; report: SnapshotExport }
  | { kind: 'failed'; message: string };

type PackagesState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'done'; list: PackageList }
  | { kind: 'failed'; code: string | null; message: string };

type CleanupState =
  | { kind: 'idle' }
  | { kind: 'confirming' }
  | { kind: 'cleaning' }
  | { kind: 'done'; result: LeftoverCleanup }
  | { kind: 'failed'; code: string | null; message: string };

type CompareState =
  | { kind: 'idle' }
  | { kind: 'comparing' }
  | { kind: 'done'; report: CompareReport }
  | { kind: 'failed'; code: string | null; message: string };

type PlanState =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'done'; report: PlanReport }
  | { kind: 'failed'; message: string; mismatches: HashMismatch[] };

type ApplyState =
  | { kind: 'idle' }
  | { kind: 'confirming' }
  | { kind: 'applying' }
  | { kind: 'done'; report: ApplyReport }
  | { kind: 'failed'; failure: ApplyFailure | null; message: string };

type BackupsState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'done'; list: BackupList }
  | { kind: 'failed'; message: string };

type RestoreState =
  | { kind: 'idle' }
  | { kind: 'confirming'; id: string }
  | { kind: 'restoring'; id: string }
  | { kind: 'done'; report: RestoreReport }
  | { kind: 'failed'; failure: RestoreFailure | null; message: string };

/** 每个分组最多列出的条数；其余只计数。 */
const PLAN_LIST_LIMIT = 20;

/** 应用提示最多列出的条数；其余只计数。 */
const APPLY_WARNING_LIMIT = 5;

/** 备份卡片最多列出的份数；更旧的仍在磁盘上。 */
const BACKUP_LIST_SHOWN = 5;

/** 删除选择按 root + path 做 key：两个根可能有同一相对路径，裸路径会串。 */
function deleteKey(request: DeleteRequest): string {
  return `${request.root}:${request.path}`;
}

/** 「全选」只选 agent 根：外部共享目录里的删除必须逐项确认。 */
function selectableDeletes(items: PlanItem[]): DeleteRequest[] {
  return items
    .filter((item) => item.root === 'agent')
    .map((item) => ({ root: item.root, path: item.path }));
}

/** 外部共享目录的删除条目提示：别的 agent 工具也可能在用这些技能。 */
const SHARED_ROOT_HINT = '共享目录，其它 agent 工具也可能使用';

const mismatchReasonText: Record<HashMismatch['reason'], string> = {
  missing: '快照里没有这个文件',
  unreadable: '文件读不出来',
  size: '字节数不同',
  hash: '哈希不同',
  out_of_bounds: '路径越出同步文件夹',
};

const statusText: Partial<Record<PathStatus, string>> = {
  ok: '可用',
  invalid: '路径无效',
  missing: '目录不存在',
  not_writable: '不可写',
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Tauri 侧的错误载荷是结构化对象；拿不到结构时退回文本。 */
function asFailure(error: unknown): ExportFailure | null {
  if (typeof error !== 'object' || error === null) {
    return null;
  }
  const candidate = error as Partial<ExportFailure>;
  if (typeof candidate.code !== 'string' || typeof candidate.message !== 'string') {
    return null;
  }
  return {
    code: candidate.code,
    message: candidate.message,
  };
}

function asPlanFailure(error: unknown): PlanFailure | null {
  const failure = asFailure(error);
  if (failure === null) {
    return null;
  }
  const candidate = error as Partial<PlanFailure>;
  return {
    code: failure.code,
    message: failure.message,
    mismatches: Array.isArray(candidate.mismatches) ? candidate.mismatches : [],
  };
}

function asApplyFailure(error: unknown): ApplyFailure | null {
  const failure = asFailure(error);
  if (failure === null) {
    return null;
  }
  const candidate = error as Partial<ApplyFailure>;
  return {
    code: failure.code,
    message: failure.message,
    completed: typeof candidate.completed === 'number' ? candidate.completed : 0,
    backupDir: candidate.backupDir ?? null,
  };
}

function asRestoreFailure(error: unknown): RestoreFailure | null {
  const failure = asFailure(error);
  if (failure === null) {
    return null;
  }
  const candidate = error as Partial<RestoreFailure>;
  return {
    code: failure.code,
    message: failure.message,
    completed: typeof candidate.completed === 'number' ? candidate.completed : 0,
    safetyBackupId: candidate.safetyBackupId ?? null,
  };
}

export function SnapshotPage() {
  const [config, setConfig] = useState<ConfigState>({ kind: 'loading' });
  const [downloaded, setDownloaded] = useState<DownloadState>({ kind: 'idle' });
  const [upload, setUpload] = useState<UploadState>({ kind: 'idle' });
  /** 计划与应用的根：未下载时为 null（用同步根），下载后是暂存目录。 */
  const [planRoot, setPlanRoot] = useState<string | null>(null);
  const [exported, setExported] = useState<ExportState>({ kind: 'idle' });
  const [planning, setPlanning] = useState<PlanState>({ kind: 'idle' });
  const [selected, setSelected] = useState<DeleteRequest[]>([]);
  const [applying, setApplying] = useState<ApplyState>({ kind: 'idle' });
  const [backups, setBackups] = useState<BackupsState>({ kind: 'idle' });
  const [restoring, setRestoring] = useState<RestoreState>({ kind: 'idle' });
  /** 存储根里的包列表，与选中的那一份（缺省跟随最新）。 */
  const [packages, setPackages] = useState<PackagesState>({ kind: 'idle' });
  const [picked, setPicked] = useState<string | null>(null);
  /** 要对比的包：多选，与「取哪一份」无关。 */
  const [comparePicked, setComparePicked] = useState<string[]>([]);
  const [compare, setCompare] = useState<CompareState>({ kind: 'idle' });
  /** 清理上传残留：确认框、执行与结果。 */
  const [cleanup, setCleanup] = useState<CleanupState>({ kind: 'idle' });

  useEffect(() => {
    if (!isDesktop) {
      setConfig({ kind: 'browser' });
      return;
    }

    let cancelled = false;
    loadAppConfig()
      .then((view) => {
        if (cancelled) {
          return;
        }
        setConfig({
          kind: 'ready',
          syncRoot: view.config.syncRoot,
          status: view.validation.syncRootStatus,
          transport: view.config.transport,
        });
      })
      .catch((error: unknown) => {
        if (cancelled) {
          return;
        }
        setConfig({ kind: 'failed', message: errorMessage(error) });
      });

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!isDesktop) {
      return;
    }
    void loadBackups();
  }, []);

  const syncRoot = config.kind === 'ready' ? config.syncRoot : null;
  const isWebdav = config.kind === 'ready' && config.transport === 'webdav';

  useEffect(() => {
    if (!isDesktop || config.kind !== 'ready') {
      return;
    }
    // 本地模式没有同步文件夹就没有包可列；WebDAV 模式靠已存的凭据去列远端。
    if (!isWebdav && syncRoot === null) {
      return;
    }
    void loadPackages();
  }, [config.kind, isWebdav, syncRoot]);
  const exporting = exported.kind === 'exporting';
  const canExport = isDesktop && syncRoot !== null && !exporting;
  const downloading = downloaded.kind === 'downloading';
  const uploading = upload.kind === 'uploading';
  const canUpload = isDesktop && isWebdav && !uploading;
  const checking = planning.kind === 'checking';
  // 检查的是解包后的暂存快照：没有取到包时「同步文件夹」里已经没有目录布局可查。
  const canCheck = isDesktop && planRoot !== null && !checking;

  async function loadPackages() {
    setPackages({ kind: 'loading' });
    try {
      const list = await listSnapshotPackages();
      setPackages({ kind: 'done', list });
      // 默认选中最新的一份；已选的还在列表里就不动。
      setPicked((current) =>
        current !== null && list.packages.some((entry) => entry.name === current)
          ? current
          : (list.packages.find((entry) => entry.isNewest)?.name ?? null),
      );
    } catch (error) {
      const failure = asFailure(error);
      setPackages({
        kind: 'failed',
        code: failure?.code ?? null,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  async function runCheck(root: string | null) {
    setPlanning({ kind: 'checking' });
    // 计划重算后旧的勾选与结果都不再对应，清空。
    setSelected([]);
    setApplying({ kind: 'idle' });
    try {
      const report = await invoke<PlanReport>('verify_and_plan', { root });
      setPlanning({ kind: 'done', report });
    } catch (error) {
      const failure = asPlanFailure(error);
      setPlanning({
        kind: 'failed',
        message: failure?.message ?? errorMessage(error),
        mismatches: failure?.mismatches ?? [],
      });
    }
  }

  async function runDownload(packageName: string) {
    setDownloaded({ kind: 'downloading' });
    setPicked(packageName);
    try {
      const view = await downloadRemoteSnapshot(packageName);
      setDownloaded({ kind: 'done', view });
      // 后面的检查与应用都针对这份暂存快照，不再指向同步文件夹。
      setPlanRoot(view.stagingRoot);
      // 下载写了本机记录：列表里的「云端有 N 份未见」与上次同步时间要跟着刷新。
      void loadPackages();
      await runCheck(view.stagingRoot);
    } catch (error) {
      // 取包失败带设计里的码（远端空、包被删、地址不安全等），原样展示给用户。
      const failure = asFailure(error);
      setDownloaded({
        kind: 'failed',
        code: failure?.code ?? null,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  async function runUpload() {
    setUpload({ kind: 'uploading' });
    try {
      const result = await uploadSnapshot();
      setUpload({ kind: 'done', result });
      // 上传可能写了一个新包并清理了旧包：列表要跟着刷新。
      void loadPackages();
    } catch (error) {
      const failure = asFailure(error);
      setUpload({
        kind: 'failed',
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  async function runCompare() {
    if (comparePicked.length === 0) {
      return;
    }
    setCompare({ kind: 'comparing' });
    try {
      const report = await compareSnapshotPackages(comparePicked);
      setCompare({ kind: 'done', report });
    } catch (error) {
      // 非 HTTPS、同步文件夹没设、凭据缺失都在这里带着失败码上报，原样展示。
      const failure = asFailure(error);
      setCompare({
        kind: 'failed',
        code: failure?.code ?? null,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  function toggleCompare(name: string) {
    setComparePicked((current) =>
      current.includes(name)
        ? current.filter((item) => item !== name)
        : [...current, name],
    );
  }

  async function runCleanup() {
    setCleanup({ kind: 'cleaning' });
    try {
      const result = await cleanupUploadLeftovers();
      setCleanup({ kind: 'done', result });
      // 残留已从存储里没了：列表与「无法识别」区块都要重新读一次。
      void loadPackages();
    } catch (error) {
      // 非 HTTPS、同步文件夹没设、凭据缺失都在这里带着失败码上报，原样展示。
      const failure = asFailure(error);
      setCleanup({
        kind: 'failed',
        code: failure?.code ?? null,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  async function runApply() {
    if (planning.kind !== 'done') {
      return;
    }
    const fingerprint = planning.report.manifestInfo.fingerprint;
    setApplying({ kind: 'applying' });
    try {
      const report = await invoke<ApplyReport>('apply_snapshot', {
        expectedFingerprint: fingerprint,
        deleteRequests: selected,
        root: planRoot,
      });
      setApplying({ kind: 'done', report });
      // 应用前刚生成备份，列表要跟着更新，否则用户看不到可回滚的条目。
      void loadBackups();
    } catch (error) {
      const failure = asApplyFailure(error);
      setApplying({
        kind: 'failed',
        failure,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  const applyBusy = applying.kind === 'applying';
  const restoreBusy = restoring.kind === 'restoring';
  const canApply =
    isDesktop &&
    !applyBusy &&
    applying.kind !== 'done' &&
    planning.kind === 'done' &&
    (planning.report.totals.added > 0 ||
      planning.report.totals.modified > 0 ||
      selected.length > 0);

  function toggleDelete(request: DeleteRequest) {
    const key = deleteKey(request);
    setSelected((current) =>
      current.some((item) => deleteKey(item) === key)
        ? current.filter((item) => deleteKey(item) !== key)
        : [...current, request],
    );
  }

  async function loadBackups() {
    setBackups({ kind: 'loading' });
    try {
      const list = await invoke<BackupList>('list_backups');
      setBackups({ kind: 'done', list });
    } catch (error) {
      setBackups({ kind: 'failed', message: errorMessage(error) });
    }
  }

  async function runRestore(id: string) {
    setRestoring({ kind: 'restoring', id });
    try {
      const report = await invoke<RestoreReport>('restore_backup', {
        backupId: id,
        confirm: true,
      });
      setRestoring({ kind: 'done', report });
      // 回滚前又备份了一次，列表要刷新。
      void loadBackups();
    } catch (error) {
      const failure = asRestoreFailure(error);
      setRestoring({
        kind: 'failed',
        failure,
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  async function runExport() {
    setExported({ kind: 'exporting' });
    try {
      const report = await invoke<SnapshotExport>('export_snapshot');
      setExported({ kind: 'done', report });
      // 刚写了一个包：列表要跟着更新，否则用户看不出存储里已经是最新那份。
      void loadPackages();
    } catch (error) {
      const failure = asFailure(error);
      setExported({
        kind: 'failed',
        message: failure?.message ?? errorMessage(error),
      });
    }
  }

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-lg">
      <header>
        <h1 className="text-lg font-semibold text-foreground">快照</h1>
        <p className="mt-xs text-sm text-muted-foreground">
          把当前 Pi 配置整理成一份可校验的快照，写进云盘同步文件夹；密钥会原样进入快照。
        </p>
      </header>

      <Card className="p-lg">
        <h2 className="text-sm font-semibold text-foreground">同步文件夹</h2>
        <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
          <p className="break-all font-mono text-sm text-foreground">
            {syncRoot ?? '—'}
          </p>
          {config.kind === 'ready' && config.status !== 'ok' && (
            <p className="mt-xs text-xs text-warning">
              {statusText[config.status] ?? '未设置'}
              ：先在设置页选择可写的同步文件夹，否则导出会失败。
            </p>
          )}
        </div>

        {config.kind === 'loading' && (
          <p className="mt-sm text-sm text-muted-foreground">正在读取配置…</p>
        )}
        {config.kind === 'failed' && (
          <p className="mt-sm text-sm text-danger">{config.message}</p>
        )}
        {config.kind === 'browser' && (
          <p className="mt-sm text-sm text-muted-foreground">
            浏览器预览里不读取本机配置，导出仅桌面版可用。
          </p>
        )}
        {config.kind === 'ready' && syncRoot === null && (
          <p className="mt-sm text-sm text-muted-foreground">
            还没有设置同步文件夹：请到「设置」页填写后回到这里导出。
          </p>
        )}
      </Card>

      {isWebdav && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">WebDAV</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            上传会把当前配置打成一个快照包放到云端，并按名字保留最近 20 份；下载在下面的「远端快照」里选一份，解到本机暂存目录后走后面的检查与应用。
          </p>
          <p className="mt-xs text-xs text-warning">
            超出 20 份的旧包会从云端删除。云端只增不改，但删除是真的删除。
          </p>

          <div className="mt-md flex flex-wrap items-center gap-md">
            <Button onClick={() => void runUpload()} disabled={!canUpload}>
              {uploading ? '上传中…' : '上传到 WebDAV'}
            </Button>
            {!isDesktop && (
              <span className="text-sm text-muted-foreground">仅桌面版可用</span>
            )}
          </div>

          {upload.kind === 'done' && <UploadResultView result={upload.result} />}

          {upload.kind === 'failed' && (
            <p className="mt-md flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>上传失败：{upload.message}</span>
            </p>
          )}
        </Card>
      )}

      {!isWebdav && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">导出快照包</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            导出会把当前配置打成一个快照包放进上面的同步文件夹，并按名字保留最近 20 份；另一台机器指向同一个文件夹后，在下面的列表里选一份检查并应用。
          </p>
          <p className="mt-xs flex items-start gap-sm text-xs text-warning">
            <TriangleAlert className="mt-xs h-3.5 w-3.5 shrink-0" aria-hidden="true" />
            <span>
              超出 20 份的旧包会被删除，而这是同步文件夹里的真实删除：云盘会把它同步到其它机器，那些机器上的同一份包也会消失。需要长期留着的一份，请先复制到同步文件夹以外的地方。
            </span>
          </p>

          <div className="mt-md flex flex-wrap items-center gap-md">
            <Button onClick={() => void runExport()} disabled={!canExport}>
              {exporting ? '导出中…' : '导出快照'}
            </Button>
            {!isDesktop && (
              <span className="text-sm text-muted-foreground">导出仅桌面版可用</span>
            )}
            {exporting && (
              <span className="text-sm text-muted-foreground" aria-live="polite">
                正在收集配置、扫描明文密钥并打包…
              </span>
            )}
          </div>
        </Card>
      )}

      {isWebdav && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">远端快照</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            云端的历史包，按名字（UTC 时间）倒序；选一份取到本机暂存目录，再走下面的检查与应用。下载不改动远端。
          </p>

          {packages.kind === 'loading' && (
            <p className="mt-sm text-sm text-muted-foreground">正在读取云端包列表…</p>
          )}
          {packages.kind === 'failed' && (
            <p className="mt-sm flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>
                读取快照包失败：
                {packages.code !== null && (
                  <code className="mr-xs font-mono text-xs">[{packages.code}]</code>
                )}
                {packages.message}
              </span>
            </p>
          )}
          {packages.kind === 'done' && (
            <PackageListBlock
              list={packages.list}
              picked={picked}
              onDownload={(name) => void runDownload(name)}
              downloading={downloading}
              emptyText="云端还没有快照包：先上传一次，或者等另一台机器上传。"
              compare={compare}
              comparePicked={comparePicked}
              onToggleCompare={toggleCompare}
              onCompare={() => void runCompare()}
              cleanup={cleanup}
              onStartCleanup={() => setCleanup({ kind: 'confirming' })}
              onConfirmCleanup={() => void runCleanup()}
              onCancelCleanup={() => setCleanup({ kind: 'idle' })}
            />
          )}
        </Card>
      )}

      {!isWebdav && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">本地存储</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            同步文件夹里的历史包，按名字（UTC 时间）倒序；选一份解到本机暂存目录，再走下面的检查与应用。暂存只保留当前这一份。
          </p>

          {!isDesktop && (
            <p className="mt-sm text-xs leading-relaxed text-muted-foreground">
              浏览器预览里没有 Rust 进程，读不到存储里的包列表；包历史与对比只在桌面版可用。
            </p>
          )}

          {packages.kind === 'loading' && (
            <p className="mt-sm text-sm text-muted-foreground">正在读取存储里的包…</p>
          )}
          {packages.kind === 'failed' && (
            <p className="mt-sm flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>
                读取快照包失败：
                {packages.code !== null && (
                  <code className="mr-xs font-mono text-xs">[{packages.code}]</code>
                )}
                {packages.message}
              </span>
            </p>
          )}
          {packages.kind === 'done' && (
            <PackageListBlock
              list={packages.list}
              picked={picked}
              onDownload={(name) => void runDownload(name)}
              downloading={downloading}
              emptyText="同步文件夹里还没有快照包：先导出一次，或者等云盘把另一台机器的包同步下来。"
              compare={compare}
              comparePicked={comparePicked}
              onToggleCompare={toggleCompare}
              onCompare={() => void runCompare()}
              cleanup={cleanup}
              onStartCleanup={() => setCleanup({ kind: 'confirming' })}
              onConfirmCleanup={() => void runCleanup()}
              onCancelCleanup={() => setCleanup({ kind: 'idle' })}
            />
          )}
        </Card>
      )}

      <Card className="p-lg">
        <h2 className="text-sm font-semibold text-foreground">检查与预览</h2>
        <p className="mt-xs text-sm text-muted-foreground">
          只读校验：重算快照每个文件的大小与哈希，再和本机配置比较，得出新增 / 修改 / 删除候选；不写任何文件。
        </p>
        <div className="mt-md flex flex-wrap items-center gap-md">
          <Button
            variant="secondary"
            onClick={() => void runCheck(planRoot)}
            disabled={!canCheck}
          >
            {checking ? '检查中…' : '检查快照'}
          </Button>
          {!isDesktop && (
            <span className="text-sm text-muted-foreground">检查仅桌面版可用</span>
          )}
          {checking && (
            <span className="text-sm text-muted-foreground" aria-live="polite">
              正在读取 manifest、校验哈希并比较本机配置…
            </span>
          )}
        </div>
        {!isDesktop && (
          <p className="mt-sm text-sm text-muted-foreground">
            浏览器预览里没有本机快照：桌面版会读取 manifest.json、逐文件校验哈希，再列出差异；应用快照也仅桌面版可用，并会先备份再写入。
          </p>
        )}

        {planRoot === null && isDesktop && (
          <p className="mt-sm text-sm text-muted-foreground">
            还没有取到快照：先在「本地存储」或「远端快照」里选一份包下载。
          </p>
        )}

        {downloaded.kind === 'done' && <DownloadResult view={downloaded.view} />}

        {downloaded.kind === 'failed' && (
          <div className="mt-md flex flex-col gap-sm">
            <p className="flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>
                取快照包失败：
                {downloaded.code !== null && (
                  <code className="mr-xs font-mono text-xs">[{downloaded.code}]</code>
                )}
                {downloaded.message}
              </span>
            </p>
            <p className="text-xs text-muted-foreground">
              失败即停：本次暂存目录已删除，本机配置与远端都不动；修正后可以重新取。
            </p>
          </div>
        )}

        {planning.kind === 'done' && (
          <div className="mt-md flex flex-col gap-md">
            <dl className="flex flex-col gap-sm">
              <ResultRow label="创建时间" value={planning.report.manifestInfo.createdAt} />
              <ResultRow label="指纹" value={planning.report.manifestInfo.fingerprint} mono />
              <ResultRow label="文件数" value={String(planning.report.manifestInfo.files)} />
              <ResultRow
                label="来源版本"
                value={sourceText(planning.report.manifestInfo.source)}
              />
              <ResultRow
                label="哈希校验"
                value={`通过（${planning.report.verification.checked} 个文件）`}
              />
            </dl>
            <p className="text-sm text-muted-foreground">
              本机受管文件 {planning.report.totals.liveFiles} 个，其中未改动{' '}
              {planning.report.totals.unchanged} 个。
            </p>
            <PlanGroup title="新增" items={planning.report.plan.added} />
            <PlanGroup title="修改" items={planning.report.plan.modified} />
            {[...planning.report.plan.added, ...planning.report.plan.modified, ...planning.report.plan.deleted].some(
              (item) => item.root === 'agent' && item.path === 'auth.json',
            ) && (
              <p className="flex items-start gap-sm text-sm text-warning">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>
                  本计划会覆盖或删除本机 auth.json（Pi 凭据）；覆盖/删除前会先备份。
                </span>
              </p>
            )}
            <PlanGroup
              title="删除"
              hint="应用时需逐项确认"
              items={planning.report.plan.deleted}
            />
            {planning.report.totals.added === 0 &&
              planning.report.totals.modified === 0 &&
              planning.report.totals.deleted === 0 && (
                <p className="text-sm text-success">本机配置与快照一致，没有待应用的变化。</p>
              )}
            {planning.report.requirements
              .filter((check) => check.satisfied === false)
              .map((check) => (
                <p key={check.tool} className="text-sm text-warning">
                  {check.tool} 版本不满足要求：需要 {check.required}，当前 {check.found ?? '未知'}
                  ；只是警告，不阻止生成计划。
                </p>
              ))}
            {planning.report.requirements.some((check) => check.satisfied === null) && (
              <p className="text-sm text-muted-foreground">
                未能探测到{' '}
                {planning.report.requirements
                  .filter((check) => check.satisfied === null)
                  .map((check) => check.tool)
                  .join('、')}{' '}
                的版本，版本检查记为未知，不影响计划。
              </p>
            )}
          </div>
        )}

        {planning.kind === 'failed' && (
          <div className="mt-md flex flex-col gap-sm">
            <p className="flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>检查未通过：{planning.message}</span>
            </p>
            {planning.mismatches.length > 0 && (
              <ul className="flex flex-col gap-sm">
                {planning.mismatches.map((mismatch) => (
                  <li
                    key={mismatch.path}
                    className="rounded-md bg-surface-muted px-md py-sm"
                  >
                    <p className="break-all font-mono text-xs text-foreground">
                      {mismatch.path}
                    </p>
                    <p className="mt-xs text-xs text-muted-foreground">
                      {mismatchReasonText[mismatch.reason]}
                    </p>
                  </li>
                ))}
              </ul>
            )}
            <p className="text-xs text-muted-foreground">
              哈希不符通常意味着云盘还没传完；本次没有生成计划，也没有写任何文件。
            </p>
          </div>
        )}
      </Card>

      {planning.kind === 'done' && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">应用</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            把快照写进本机：新增 {planning.report.totals.added} 项、修改{' '}
            {planning.report.totals.modified} 项；删除需逐项勾选，未勾选的保留。覆盖与删除前会先备份到本机应用数据目录。
          </p>

          {planning.report.plan.deleted.length === 0 ? (
            <p className="mt-sm text-sm text-muted-foreground">没有删除候选。</p>
          ) : (
            <div className="mt-md">
              <div className="flex flex-wrap items-center justify-between gap-sm">
                <h3 className="text-xs font-semibold text-foreground">
                  删除候选 {planning.report.plan.deleted.length} 项（已勾选 {selected.length} 项）
                </h3>
                <div className="flex gap-sm">
                  <Button
                    variant="secondary"
                    onClick={() =>
                      setSelected(selectableDeletes(planning.report.plan.deleted))
                    }
                    disabled={!isDesktop || applyBusy}
                  >
                    全选
                  </Button>
                  <Button
                    variant="secondary"
                    onClick={() => setSelected([])}
                    disabled={!isDesktop || applyBusy || selected.length === 0}
                  >
                    全不选
                  </Button>
                </div>
              </div>
              <p className="mt-xs text-xs text-muted-foreground">
                “全选”只勾宿主目录的条目；{rootLabels['agents-skills']} 是共享目录，只能逐项确认。
              </p>
              <ul className="mt-xs flex flex-col gap-sm">
                {planning.report.plan.deleted.map((item) => {
                  const request: DeleteRequest = { root: item.root, path: item.path };
                  const checked = selected.some(
                    (chosen) => deleteKey(chosen) === deleteKey(request),
                  );
                  return (
                    <li
                      key={deleteKey(request)}
                      className="rounded-md bg-surface-muted px-md py-sm"
                    >
                      <label className="flex items-start gap-sm">
                        <input
                          type="checkbox"
                          className="mt-xs h-4 w-4"
                          checked={checked}
                          onChange={() => toggleDelete(request)}
                          disabled={!isDesktop || applyBusy}
                        />
                        <span className="flex flex-col gap-xs">
                          <span className="font-mono text-xs text-foreground">
                            <span className="mr-sm rounded bg-surface px-xs py-px font-sans text-muted-foreground">
                              {rootLabels[item.root]}
                            </span>
                            {item.path}
                            {item.kind === 'link' && (
                              <span className="ml-xs font-sans text-muted-foreground">
                                · 链接，只删链接本身
                              </span>
                            )}
                          </span>
                          {item.root !== 'agent' && (
                            <span className="text-xs text-warning">{SHARED_ROOT_HINT}</span>
                          )}
                        </span>
                      </label>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}

          <div className="mt-md flex flex-wrap items-center gap-md">
            <Button onClick={() => setApplying({ kind: 'confirming' })} disabled={!canApply}>
              {applyBusy ? '应用中…' : '应用快照'}
            </Button>
            {!isDesktop && (
              <span className="text-sm text-muted-foreground">应用仅桌面版可用</span>
            )}
          </div>

          {applying.kind === 'confirming' && (
            <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
              <p className="text-sm text-foreground">
                将写入 {planning.report.totals.added + planning.report.totals.modified}{' '}
                个文件，勾选 {selected.length} 项删除（落在链接之下的会跳过）；覆盖与删除前会先备份到本机。
              </p>
              {selected.some((item) => item.root !== 'agent') && (
                <p className="mt-xs text-sm text-warning">
                  其中 {selected.filter((item) => item.root !== 'agent').length} 项在{
                    rootLabels['agents-skills']
                  }（共享目录）：其它 agent 工具也可能在用这些技能，原内容会进备份。
                </p>
              )}
              <div className="mt-sm flex flex-wrap gap-sm">
                <Button onClick={() => void runApply()} disabled={applyBusy}>
                  确认应用
                </Button>
                <Button variant="secondary" onClick={() => setApplying({ kind: 'idle' })}>
                  取消
                </Button>
              </div>
            </div>
          )}

          {applyBusy && (
            <p className="mt-sm text-sm text-muted-foreground" aria-live="polite">
              正在备份并写入本机配置…
            </p>
          )}

          {applying.kind === 'done' && (
            <div className="mt-md flex flex-col gap-sm">
              <dl className="flex flex-col gap-sm">
                <ResultRow label="备份编号" value={applying.report.backupId} mono />
                <ResultRow label="备份目录" value={applying.report.backupDir} mono />
                <ResultRow label="写入" value={`${applying.report.written} 个文件`} />
                <ResultRow label="删除" value={`${applying.report.deleted} 个文件`} />
                <ResultRow label="保留的删除候选" value={`${applying.report.skipped} 个`} />
                <ResultRow label="重建链接" value={`${applying.report.linked} 个`} />
                <ResultRow label="跳过删除" value={`${applying.report.skippedDeletes} 个`} />
              </dl>
              <WarningList warnings={applying.report.warnings} />
              <p className="text-sm text-success">
                已按快照落地：请重启 Pi，或在会话里执行 /reload 让配置生效。
              </p>
              <p className="text-xs text-muted-foreground">
                如需再次应用，请先重新检查快照。
              </p>
            </div>
          )}

          {applying.kind === 'failed' && (
            <div className="mt-md flex flex-col gap-sm">
              <p className="flex items-start gap-sm text-sm text-danger">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>应用失败：{applying.message}</span>
              </p>
              {applying.failure !== null && (
                <dl className="flex flex-col gap-sm">
                  <ResultRow label="失败码" value={applying.failure.code} mono />
                  {applying.failure.backupDir !== null && (
                    <ResultRow label="备份目录" value={applying.failure.backupDir} mono />
                  )}
                </dl>
              )}
              <p className="text-xs text-muted-foreground">
                失败即停：已完成的部分留在原地，备份已保留；不自动重试、不自动回滚。
              </p>
            </div>
          )}
        </Card>
      )}

      <Card className="p-lg">
        <h2 className="text-sm font-semibold text-foreground">备份与回滚</h2>
        <p className="mt-xs text-sm text-muted-foreground">
          应用前会把将被覆盖或删除的文件备份到本机应用数据目录；回滚只从备份还原本机配置，不碰快照。
        </p>

        {!isDesktop && (
          <p className="mt-sm text-sm text-muted-foreground">
            备份与回滚仅桌面版可用：浏览器预览读不到本机应用数据目录。
          </p>
        )}
        {backups.kind === 'loading' && (
          <p className="mt-sm text-sm text-muted-foreground">正在读取本机备份…</p>
        )}
        {backups.kind === 'failed' && (
          <p className="mt-sm text-sm text-danger">读取备份失败：{backups.message}</p>
        )}
        {backups.kind === 'done' && backups.list.backups.length === 0 && (
          <p className="mt-sm text-sm text-muted-foreground">
            本机还没有备份：应用一次快照后这里会出现记录。
          </p>
        )}
        {backups.kind === 'done' && backups.list.backups.length > 0 && (
          <ul className="mt-md flex flex-col gap-sm">
            {backups.list.backups.slice(0, BACKUP_LIST_SHOWN).map((backup) => (
              <li key={backup.id} className="rounded-md bg-surface-muted px-md py-sm">
                <div className="flex flex-wrap items-center justify-between gap-sm">
                  <div>
                    <p className="font-mono text-xs text-foreground">{backup.createdAt}</p>
                    <p className="mt-xs text-xs text-muted-foreground">
                      指纹 <span className="font-mono">{backup.fingerprint}</span> ·{' '}
                      {backup.entries} 个条目
                    </p>
                  </div>
                  <Button
                    variant="secondary"
                    onClick={() => setRestoring({ kind: 'confirming', id: backup.id })}
                    disabled={!isDesktop || restoreBusy}
                  >
                    回滚到此
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        )}
        {backups.kind === 'done' && backups.list.backups.length > BACKUP_LIST_SHOWN && (
          <p className="mt-xs text-xs text-muted-foreground">
            只列出最近 {BACKUP_LIST_SHOWN} 份，共 {backups.list.backups.length} 份。
          </p>
        )}
        {backups.kind === 'done' && backups.list.warnings.length > 0 && (
          <ul className="mt-sm flex flex-col gap-sm">
            {backups.list.warnings.map((warning) => (
              <li key={warning} className="text-xs text-warning">
                {warning}
              </li>
            ))}
          </ul>
        )}

        {restoring.kind === 'confirming' && (
          <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
            <p className="text-sm text-foreground">
              将把本机配置还原成备份 <span className="font-mono">{restoring.id}</span> 的状态；
              还原前会先把当前状态自动备份一次，回滚不碰快照。
            </p>
            <div className="mt-sm flex flex-wrap gap-sm">
              <Button onClick={() => void runRestore(restoring.id)} disabled={restoreBusy}>
                确认回滚
              </Button>
              <Button variant="secondary" onClick={() => setRestoring({ kind: 'idle' })}>
                取消
              </Button>
            </div>
          </div>
        )}
        {restoring.kind === 'restoring' && (
          <p className="mt-sm text-sm text-muted-foreground" aria-live="polite">
            正在备份当前状态并还原…
          </p>
        )}
        {restoring.kind === 'done' && (
          <div className="mt-md flex flex-col gap-sm">
            <dl className="flex flex-col gap-sm">
              <ResultRow label="还原的备份" value={restoring.report.backupId} mono />
              <ResultRow label="还原前备份" value={restoring.report.safetyBackupId} mono />
              <ResultRow label="还原" value={`${restoring.report.restored} 个条目`} />
              <ResultRow label="删除" value={`${restoring.report.removed} 个新增条目`} />
            </dl>
            <WarningList warnings={restoring.report.warnings} />
            <p className="text-sm text-success">
              已从备份还原：请重启 Pi，或在会话里执行 /reload 让配置生效。
            </p>
            <p className="text-xs text-muted-foreground">
              想反悔可以从「还原前备份」再回滚一次。
            </p>
          </div>
        )}
        {restoring.kind === 'failed' && (
          <div className="mt-md flex flex-col gap-sm">
            <p className="flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>回滚失败：{restoring.message}</span>
            </p>
            {restoring.failure !== null && (
              <dl className="flex flex-col gap-sm">
                <ResultRow label="失败码" value={restoring.failure.code} mono />
                <ResultRow label="已还原" value={`${restoring.failure.completed} 个条目`} />
                {restoring.failure.safetyBackupId !== null && (
                  <ResultRow
                    label="还原前备份"
                    value={restoring.failure.safetyBackupId}
                    mono
                  />
                )}
              </dl>
            )}
            <p className="text-xs text-muted-foreground">
              失败即停：已完成的部分留在原地，还原前的备份已保留；不自动重试、不自动回退。
            </p>
          </div>
        )}
      </Card>

      {exported.kind === 'done' && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">
            {exported.report.unchanged ? '内容未变，没有新包' : '导出完成'}
          </h2>
          <ExportResult report={exported.report} />
          <p className="mt-md text-sm text-muted-foreground">
            {exported.report.unchanged
              ? '存储里最新的一份包就是这份内容：没有写新包，历史不会重复堆积。'
              : '包在存储里出现即代表完整：云盘客户端传完后，另一台机器就能在「本地存储」里选到它。'}
          </p>
          <WarningList warnings={exported.report.rootWarnings} />
          {exported.report.warnings.length > 0 && (
            <p className="mt-sm text-sm text-muted-foreground">
              另有 {exported.report.warnings.length} 条密钥提示，文件已原样保留。
            </p>
          )}
        </Card>
      )}

      {exported.kind === 'failed' && (
        <Card className="p-lg">
          <p className="flex items-start gap-sm text-sm text-danger">
            <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
            <span>导出失败：{exported.message}</span>
          </p>
          <p className="mt-sm text-xs text-muted-foreground">
            失败即停：存储里已有的包没被改动，修正后可以重新导出。
          </p>
        </Card>
      )}

    </div>
  );
}

function sourceText(source: PlanReport['manifestInfo']['source']): string {
  return `${source.app} · ${source.os}/${source.arch} · pi ${
    source.pi ?? '未知'
  } · node ${source.node ?? '未知'}`;
}

/** 导出/上传的结果：包名、大小、文件数、指纹、受管根。 */
function ExportResult({ report }: { report: SnapshotExport }) {
  return (
    <>
      <dl className="mt-md flex flex-col gap-sm">
        <ResultRow label="快照包" value={report.packageName} mono />
        <ResultRow label="包体积" value={formatBytes(report.size)} />
        <ResultRow label="文件数" value={String(report.files)} />
        <ResultRow label="指纹" value={report.fingerprint} mono />
        <ResultRow label="创建时间" value={report.createdAt} />
        <ResultRow
          label="受管根"
          value={report.managedRoots.map((root) => rootLabels[root]).join('、')}
        />
        <ResultRow
          label="声明的包"
          value={String(report.requiredPackages.length)}
        />
        <ResultRow label="跳过的条目" value={String(report.skipped)} />
      </dl>
      {report.plaintextSecrets && (
        <p className="mt-sm text-sm text-warning">
          快照含明文密钥（auth.json 或高危命中）：包在云盘/WebDAV 服务端及其版本历史里可见。
          OAuth 多机共享可能互相失效，切换机器前先应用最新快照。
        </p>
      )}
    </>
  );
}

/** 上传结果：包名、是否跳过、清理了哪些旧包。 */
function UploadResultView({ result }: { result: UploadResult }) {
  return (
    <>
      <dl className="mt-md flex flex-col gap-sm">
        <ResultRow
          label="快照包"
          value={result.package === '' ? '—' : result.package}
          mono
        />
      </dl>
      {result.unchanged && (
        <p className="mt-sm text-sm text-muted-foreground">
          跳过（内容未变）：云端已是最新一份，这次没有写新包。
        </p>
      )}
      {result.pruned.length > 0 && (
        <p className="mt-sm text-sm text-muted-foreground">
          已清理 {result.pruned.length} 份旧包：{result.pruned.join('、')}
        </p>
      )}
      {result.rootWarnings.length > 0 && (
        <div className="mt-sm">
          <WarningList warnings={result.rootWarnings} />
        </div>
      )}
      {result.pruneFailed.length > 0 && (
        <div className="mt-sm text-sm text-warning">
          <p>有 {result.pruneFailed.length} 份清理失败（上传已成功，下次上传会重算）：</p>
          <ul className="mt-xs flex flex-col gap-xs">
            {result.pruneFailed.map((failure, index) => (
              <li key={`${failure.name}-${index}`} className="break-all">
                {failure.name === '' ? '整个清理动作' : failure.name}：{failure.reason}
              </li>
            ))}
          </ul>
        </div>
      )}
    </>
  );
}

/** 取包结果：包名、体积、创建时间、文件数、指纹与暂存目录。 */
function DownloadResult({ view }: { view: RemoteDownload }) {
  return (
    <>
      <dl className="mt-md flex flex-col gap-sm">
        <ResultRow label="快照包" value={view.package} mono />
        <ResultRow label="包体积" value={formatBytes(view.size)} />
        <ResultRow label="创建时间" value={view.createdAt} />
        <ResultRow label="文件数" value={String(view.files)} />
        <ResultRow label="指纹" value={view.fingerprint} mono />
        <ResultRow label="暂存目录" value={view.stagingRoot} mono />
      </dl>
      {view.previousSeenAt !== null && (
        <p className="mt-sm text-xs text-muted-foreground">
          上一次记录的云端时间：{view.previousSeenAt}
        </p>
      )}
    </>
  );
}

/** 对比按钮的文案：对比中、还没勾选、已勾选 N 份，三种状态各一句。 */
function compareButtonLabel(comparing: boolean, picked: number): string {
  if (comparing) {
    return '对比中…';
  }
  if (picked === 0) {
    return '对比';
  }

  return `对比选中的 ${picked} 份`;
}

/**
 * 历史包列表：本地文件夹与 WebDAV 共用，差别只在标题、空列表文案与是否展示云端状态。
 *
 * 每行自带「用这个包」：取哪一份就在哪一行点；勾选框只用于挑要对比的候选。
 */
function PackageListBlock({
  list,
  picked,
  onDownload,
  downloading,
  emptyText,
  compare,
  comparePicked,
  onToggleCompare,
  onCompare,
  cleanup,
  onStartCleanup,
  onConfirmCleanup,
  onCancelCleanup,
}: {
  list: PackageList;
  picked: string | null;
  onDownload: (name: string) => void;
  downloading: boolean;
  emptyText: string;
  compare: CompareState;
  comparePicked: string[];
  onToggleCompare: (name: string) => void;
  onCompare: () => void;
  cleanup: CleanupState;
  onStartCleanup: () => void;
  onConfirmCleanup: () => void;
  onCancelCleanup: () => void;
}) {
  const comparing = compare.kind === 'comparing';

  return (
    <>
      {list.unseenCount > 0 && (
        <p className="mt-sm text-sm text-warning">
          云端有 {list.unseenCount} 份本机未见的新快照：多半是另一台机器上传的，选一份就能恢复。
        </p>
      )}
      {list.seenAt !== null && (
        <p className="mt-xs text-xs text-muted-foreground">
          上次记录的云端时间：{list.seenAt}
        </p>
      )}

      {list.packages.length === 0 && (
        <p className="mt-sm text-sm text-muted-foreground">{emptyText}</p>
      )}
      {list.packages.length > 0 && (
        <ul className="mt-md flex flex-col gap-sm">
          {list.packages.map((entry) => (
            <li
              key={entry.name}
              className="flex flex-wrap items-start justify-between gap-sm rounded-md bg-surface-muted px-md py-sm"
            >
              <label className="flex min-w-0 flex-1 items-start gap-sm">
                <input
                  type="checkbox"
                  className="mt-xs h-4 w-4"
                  checked={comparePicked.includes(entry.name)}
                  onChange={() => onToggleCompare(entry.name)}
                  disabled={comparing}
                  aria-label={`选择 ${entry.name} 参与对比`}
                />
                <span className="flex min-w-0 flex-col gap-xs">
                  <span className="break-all font-mono text-xs text-foreground">
                    {entry.name}
                  </span>
                  <span className="break-all text-xs text-muted-foreground">
                    包名时间 {packageTime(entry.createdAt)}（本地时区）·{' '}
                    {formatBytes(entry.size)} · 指纹{' '}
                    <span className="font-mono">{entry.fingerprint8}</span>
                    {entry.isNewest && (
                      <span className="ml-xs text-foreground">· 最新</span>
                    )}
                  </span>
                </span>
              </label>
              <Button
                variant="secondary"
                onClick={() => onDownload(entry.name)}
                disabled={downloading}
              >
                {downloading && picked === entry.name ? '下载中…' : '用这个包'}
              </Button>
            </li>
          ))}
        </ul>
      )}

      {list.packages.length > 0 && (
        <div className="mt-md">
          <div className="flex flex-wrap items-center gap-md">
            <Button
              onClick={onCompare}
              disabled={comparing || comparePicked.length === 0}
            >
              {compareButtonLabel(comparing, comparePicked.length)}
            </Button>
            <span className="text-xs text-muted-foreground">
              先勾选候选包，再看应用它会发生什么。对比只读，不会改动你的配置或云端。
            </span>
          </div>

          {compare.kind === 'failed' && (
            <p className="mt-sm flex items-start gap-sm text-sm text-danger">
              <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
              <span>
                对比失败：
                {compare.code !== null && (
                  <code className="mr-xs font-mono text-xs">[{compare.code}]</code>
                )}
                {compare.message}
              </span>
            </p>
          )}

          {compare.kind === 'done' && (
            <CompareTable
              report={compare.report}
              downloading={downloading}
              onUse={onDownload}
            />
          )}
        </div>
      )}

      {list.unrecognized.length > 0 && (
        <div className="mt-md">
          <h3 className="text-xs font-semibold text-muted-foreground">
            无法识别的文件 {list.unrecognized.length} 个
          </h3>
          <ul className="mt-xs flex flex-col gap-xs">
            {list.unrecognized.map((name) => (
              <li key={name} className="break-all font-mono text-xs text-muted-foreground">
                {name}
              </li>
            ))}
          </ul>
          <p className="mt-xs text-xs text-muted-foreground">
            上传残留、云盘冲突副本或旧格式文件：不会被当成包，不参与「最新」判定，这里只列出。
          </p>
        </div>
      )}

      <CleanupLeftoversBlock
        list={list}
        cleanup={cleanup}
        onStart={onStartCleanup}
        onConfirm={onConfirmCleanup}
        onCancel={onCancelCleanup}
      />
    </>
  );
}

/**
 * 对比结果表：每行一份候选包，横向对比着读。
 *
 * 「将删除」列最醒目：应用一份包真正可能弄丢东西的就是这一列，点开看具体路径。
 */
function CompareTable({
  report,
  downloading,
  onUse,
}: {
  report: CompareReport;
  downloading: boolean;
  onUse: (name: string) => void;
}) {
  const [expanded, setExpanded] = useState<string | null>(null);

  return (
    <div className="mt-md">
      <table className="w-full border-collapse text-sm">
        <thead>
          <tr className="text-left text-xs text-muted-foreground">
            <th className="py-sm pr-md font-medium">包</th>
            <th className="py-sm pr-md font-medium">创建时间</th>
            <th className="py-sm pr-md font-medium">新增</th>
            <th className="py-sm pr-md font-medium">修改</th>
            <th className="py-sm pr-md font-medium text-danger">将删除</th>
            <th className="py-sm pr-md font-medium">与当前相同</th>
            <th className="py-sm font-medium"> </th>
          </tr>
        </thead>
        <tbody>
          {report.rows.map((row) => (
            <Fragment key={row.package}>
              <tr className="border-t border-border align-top">
                <td className="py-sm pr-md break-all font-mono text-xs text-foreground">
                  {row.package}
                </td>
                <td className="py-sm pr-md whitespace-nowrap text-xs text-muted-foreground">
                  {packageTime(row.createdAt)}
                </td>
                {row.error === null ? (
                  <>
                    <td className="py-sm pr-md text-foreground">{row.added ?? 0}</td>
                    <td className="py-sm pr-md text-foreground">{row.modified ?? 0}</td>
                    <td className="py-sm pr-md">
                      {(row.deleted ?? 0) === 0 ? (
                        <span className="text-muted-foreground">0</span>
                      ) : (
                        <button
                          type="button"
                          className="font-semibold text-danger underline"
                          aria-expanded={expanded === row.package}
                          onClick={() =>
                            setExpanded(expanded === row.package ? null : row.package)
                          }
                        >
                          {row.deleted} 项
                        </button>
                      )}
                    </td>
                    <td className="py-sm pr-md text-foreground">{row.unchanged ?? 0}</td>
                  </>
                ) : (
                  <td colSpan={4} className="py-sm pr-md text-xs text-danger">
                    <code className="mr-xs font-mono">[{row.error.code}]</code>
                    {row.error.message}
                  </td>
                )}
                <td className="py-sm">
                  <Button
                    variant="secondary"
                    onClick={() => onUse(row.package)}
                    disabled={downloading || row.error !== null}
                  >
                    {downloading ? '取包中…' : '用这个包'}
                  </Button>
                </td>
              </tr>
              {expanded === row.package && (
                <tr className="border-t border-border">
                  <td colSpan={7} className="py-sm">
                    <PlanPathList
                      title="将被删除"
                      hint="应用时默认不勾选，逐项确认"
                      items={row.deletedItems}
                    />
                  </td>
                </tr>
              )}
            </Fragment>
          ))}
        </tbody>
      </table>

      {report.pair !== null && <PairDiff pair={report.pair} />}
    </div>
  );
}

/** 包间差异：两个包互相比，与当前配置无关。 */
function PairDiff({ pair }: { pair: ComparePair }) {
  return (
    <div className="mt-md rounded-md border border-border px-md py-sm">
      <h3 className="text-xs font-semibold text-foreground">包间差异</h3>
      <p className="mt-xs break-all text-xs text-muted-foreground">
        右相对左：<span className="font-mono">{pair.left}</span> →{' '}
        <span className="font-mono">{pair.right}</span>
      </p>
      <div className="mt-sm flex flex-col gap-sm">
        <PathList title="右侧新增" paths={pair.added} />
        <PathList title="右侧修改" paths={pair.modified} />
        <PathList title="右侧没有" paths={pair.removed} />
      </div>
    </div>
  );
}

/** 计划条目的路径清单：带上所属根，两个根同相对路径也不串。 */
function PlanPathList({
  title,
  hint,
  items,
}: {
  title: string;
  hint?: string;
  items: { root: RootId; path: string }[];
}) {
  if (items.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">
        {title}：0 项
      </p>
    );
  }
  const shown = items.slice(0, PLAN_LIST_LIMIT);
  return (
    <div>
      <h4 className="text-xs font-semibold text-foreground">
        {title} {items.length} 项
        {hint !== undefined && (
          <span className="ml-xs font-normal text-muted-foreground">· {hint}</span>
        )}
      </h4>
      <ul className="mt-xs flex flex-col gap-xs">
        {shown.map((item) => (
          <li key={`${item.root}:${item.path}`} className="flex flex-wrap items-baseline gap-xs text-xs">
            <span className="rounded bg-surface-muted px-xs py-0.5 text-muted-foreground">
              {rootLabels[item.root]}
            </span>
            <span className="break-all font-mono text-foreground">{item.path}</span>
          </li>
        ))}
      </ul>
      {items.length > shown.length && (
        <p className="mt-xs text-xs text-muted-foreground">
          还有 {items.length - shown.length} 项
        </p>
      )}
    </div>
  );
}

/** 路径清单：只列前 20 条，其余只报数量。 */
function PathList({
  title,
  hint,
  paths,
}: {
  title: string;
  hint?: string;
  paths: string[];
}) {
  if (paths.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">
        {title}：0 项
      </p>
    );
  }
  const shown = paths.slice(0, PLAN_LIST_LIMIT);
  return (
    <div>
      <h4 className="text-xs font-semibold text-foreground">
        {title} {paths.length} 项
        {hint !== undefined && (
          <span className="ml-xs font-normal text-muted-foreground">· {hint}</span>
        )}
      </h4>
      <ul className="mt-xs flex flex-col gap-xs">
        {shown.map((path) => (
          <li key={path} className="break-all font-mono text-xs text-foreground">
            {path}
          </li>
        ))}
      </ul>
      {paths.length > shown.length && (
        <p className="mt-xs text-xs text-muted-foreground">
          还有 {paths.length - shown.length} 项
        </p>
      )}
    </div>
  );
}

/**
 * 清理上传残留：只删本应用自己产生过的临时名，其它无法识别的文件一律不动。
 *
 * 按钮总是显示：没有可清理项时置灰并说明原因，比凭空消失更好解释。
 */
function CleanupLeftoversBlock({
  list,
  cleanup,
  onStart,
  onConfirm,
  onCancel,
}: {
  list: PackageList;
  cleanup: CleanupState;
  onStart: () => void;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const cleaning = cleanup.kind === 'cleaning';
  const canClean = isDesktop && !cleaning && list.tempFiles.length > 0;
  // 「不动」就是本轮列表里除上传残留之外的无法识别项：确认框里先说清楚，免得误点。
  const untouched = list.unrecognized.filter((name) => !list.tempFiles.includes(name));

  return (
    <div className="mt-md">
      <div className="flex flex-wrap items-center gap-md">
        <Button variant="secondary" onClick={onStart} disabled={!canClean}>
          {cleaning ? '清理中…' : '清理上传残留'}
        </Button>
        {list.tempFiles.length === 0 && (
          <span className="text-xs text-muted-foreground">
            没有本应用的上传残留可清理
          </span>
        )}
        {!isDesktop && (
          <span className="text-sm text-muted-foreground">清理仅桌面版可用</span>
        )}
      </div>

      {cleanup.kind === 'confirming' && (
        <div className="mt-sm rounded-md bg-surface-muted px-md py-sm">
          <p className="text-sm text-foreground">
            将删除 {list.tempFiles.length} 个上传残留：
          </p>
          <ul className="mt-xs flex flex-col gap-xs">
            {list.tempFiles.map((name) => (
              <li key={name} className="break-all font-mono text-xs text-foreground">
                {name}
              </li>
            ))}
          </ul>
          {untouched.length > 0 && (
            <p className="mt-xs break-all text-xs text-muted-foreground">
              这些不会动（{untouched.length} 个）：{untouched.join('、')}
            </p>
          )}
          <div className="mt-sm flex flex-wrap gap-sm">
            <Button onClick={onConfirm}>确认清理</Button>
            <Button variant="secondary" onClick={onCancel}>
              取消
            </Button>
          </div>
        </div>
      )}

      {cleaning && (
        <p className="mt-sm text-sm text-muted-foreground" aria-live="polite">
          正在删除上传残留…
        </p>
      )}

      {cleanup.kind === 'done' && (
        <div className="mt-sm flex flex-col gap-xs">
          <p className="text-sm text-foreground">
            已清理 {cleanup.result.deleted.length} 项
            {cleanup.result.failed.length > 0 &&
              ` / ${cleanup.result.failed.length} 项失败`}
          </p>
          {cleanup.result.failed.length > 0 && (
            <ul className="flex flex-col gap-xs">
              {cleanup.result.failed.map((item) => (
                <li key={item.name} className="break-all text-xs text-danger">
                  <span className="font-mono">{item.name}</span>：{item.reason}
                </li>
              ))}
            </ul>
          )}
          {cleanup.result.skipped.length > 0 && (
            <p className="break-all text-xs text-muted-foreground">
              没有动的 {cleanup.result.skipped.length} 个：
              {cleanup.result.skipped.join('、')}
            </p>
          )}
        </div>
      )}

      {cleanup.kind === 'failed' && (
        <p className="mt-sm flex items-start gap-sm text-sm text-danger">
          <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
          <span>
            清理失败：
            {cleanup.code !== null && (
              <code className="mr-xs font-mono text-xs">[{cleanup.code}]</code>
            )}
            {cleanup.message}
          </span>
        </p>
      )}
    </div>
  );
}

/**
 * 包名里的时间是 UTC，而用户看的是本地时间：换算后标出来源，
 * 免得和 manifest 里的创建时间混为一谈。
 */
function packageTime(createdAt: string): string {
  const date = new Date(createdAt);
  return Number.isNaN(date.getTime()) ? createdAt : date.toLocaleString();
}

/** 包体积按 KB / MB 显示：几十 KB 是常态，字节数读起来没意义。 */
function formatBytes(size: number): string {
  if (size >= 1024 * 1024) {
    return `${(size / (1024 * 1024)).toFixed(1)} MB`;
  }
  if (size >= 1024) {
    return `${(size / 1024).toFixed(1)} KB`;
  }
  return `${size} B`;
}

/** 一组计划项：只列前 20 条，其余只报数量。 */
function PlanGroup({
  title,
  hint,
  items,
}: {
  title: string;
  hint?: string;
  items: PlanItem[];
}) {
  if (items.length === 0) {
    return null;
  }
  const shown = items.slice(0, PLAN_LIST_LIMIT);
  return (
    <div>
      <h3 className="text-xs font-semibold text-foreground">
        {title} {items.length} 项
        {hint !== undefined && (
          <span className="ml-xs font-normal text-warning">· {hint}</span>
        )}
      </h3>
      <ul className="mt-xs flex flex-col gap-sm">
        {shown.map((item) => (
          <li
            key={`${item.root}:${item.path}`}
            className="rounded-md bg-surface-muted px-md py-sm"
          >
            <p className="break-all font-mono text-xs text-foreground">
              <span className="mr-sm rounded bg-surface px-xs py-px font-sans text-muted-foreground">
                {rootLabels[item.root]}
              </span>
              {item.path}
            </p>
            <p className="mt-xs text-xs text-muted-foreground">
              {item.kind === 'link' ? '链接（只删链接本身）' : `${item.size} B`}
              {item.root !== 'agent' && ` · ${SHARED_ROOT_HINT}`}
            </p>
          </li>
        ))}
      </ul>
      {items.length > shown.length && (
        <p className="mt-xs text-xs text-muted-foreground">
          还有 {items.length - shown.length} 项
        </p>
      )}
    </div>
  );
}

/** 应用提示：只列前 5 条，其余只报数量。 */
function WarningList({ warnings }: { warnings: string[] }) {
  if (warnings.length === 0) {
    return null;
  }
  const shown = warnings.slice(0, APPLY_WARNING_LIMIT);
  return (
    <div>
      <h3 className="text-xs font-semibold text-foreground">
        提示 {warnings.length} 条
      </h3>
      <ul className="mt-xs flex flex-col gap-sm">
        {shown.map((warning, index) => (
          <li
            key={`${index}-${warning}`}
            className="rounded-md bg-surface-muted px-md py-sm"
          >
            <p className="break-all text-xs text-foreground">{warning}</p>
          </li>
        ))}
      </ul>
      {warnings.length > shown.length && (
        <p className="mt-xs text-xs text-muted-foreground">
          还有 {warnings.length - shown.length} 条
        </p>
      )}
    </div>
  );
}

function ResultRow({
  label,
  value,
  mono = false,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="rounded-md bg-surface-muted px-md py-sm">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd
        className={cn(
          'break-all text-sm text-foreground',
          mono && 'font-mono',
        )}
      >
        {value}
      </dd>
    </div>
  );
}
