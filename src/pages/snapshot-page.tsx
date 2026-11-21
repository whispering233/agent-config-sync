import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { TriangleAlert } from 'lucide-react';
import { Button } from '../components/ui/button';
import { Card } from '../components/ui/card';
import { loadAppConfig, type PathStatus, type Transport } from '../lib/app-config';
import {
  downloadRemoteSnapshot,
  isDesktop,
  uploadSnapshot,
  type RemoteDownload,
  type RemoteUpload,
} from '../lib/desktop';
import { cn } from '../lib/utils';

/** 与 Rust 侧 `SecretFinding` 一一对应；只含位置，不含密钥值。 */
type SecretFinding = {
  path: string;
  field: string | null;
  line: number;
  level: 'high' | 'low';
  rule: string;
};

type RequiredSecret = {
  name: string;
  usedBy: string[];
  hint: string | null;
};

type RequiredPackage = {
  source: string;
  enabled: boolean;
};

/** 与 Rust 侧 `ExportReport` 一一对应。 */
type ExportReport = {
  files: number;
  fingerprint: string;
  requiredSecrets: RequiredSecret[];
  requiredPackages: RequiredPackage[];
  skipped: number;
  warnings: SecretFinding[];
  plaintextSecrets: boolean;
  snapshotDir: string;
  manifestPath: string;
};

/** 与 Rust 侧 `ExportFailure` 一一对应。 */
type ExportFailure = {
  code: string;
  message: string;
  findings: SecretFinding[];
};

/** 计划项的差异类别，与 Rust 侧 `PlanCategory` 一致。 */
type PlanCategory = 'added' | 'modified' | 'deleted';

/** 与 Rust 侧 `PlanItem` 一一对应；`deleted` 时 `snapshotSha256` 为 null。 */
type PlanItem = {
  path: string;
  category: PlanCategory;
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
  | { kind: 'failed'; message: string };

/** 上传失败的结构与导出失败一致：`code` 区分明文密钥阻断，`findings` 只因此才有内容。 */
type UploadFailure = ExportFailure;

type UploadState =
  | { kind: 'idle' }
  | { kind: 'uploading' }
  | { kind: 'uploaded'; view: Extract<RemoteUpload, { kind: 'uploaded' }> }
  | { kind: 'conflict'; view: Extract<RemoteUpload, { kind: 'conflict' }> }
  /** `overwrite` 记下用户当时的选择，确认明文后接着原路走。 */
  | { kind: 'blocked'; failure: UploadFailure; overwrite: boolean }
  | { kind: 'failed'; message: string };

type ExportState =
  | { kind: 'idle' }
  | { kind: 'exporting' }
  | { kind: 'done'; report: ExportReport }
  | { kind: 'blocked'; failure: ExportFailure }
  | { kind: 'failed'; message: string };

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

/** 冲突清单最多列出的条数；其余只计数。 */
const CONFLICT_LIST_LIMIT = 10;

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
    findings: Array.isArray(candidate.findings) ? candidate.findings : [],
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
  const [selected, setSelected] = useState<string[]>([]);
  const [applying, setApplying] = useState<ApplyState>({ kind: 'idle' });
  const [backups, setBackups] = useState<BackupsState>({ kind: 'idle' });
  const [restoring, setRestoring] = useState<RestoreState>({ kind: 'idle' });

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
  const exporting = exported.kind === 'exporting';
  const canExport = isDesktop && syncRoot !== null && !exporting;
  const downloading = downloaded.kind === 'downloading';
  const canDownload = isDesktop && isWebdav && !downloading;
  const uploading = upload.kind === 'uploading';
  const canUpload = isDesktop && isWebdav && !uploading;
  const checking = planning.kind === 'checking';
  // 下载之后检查的是暂存快照，此时没有同步文件夹也能走完预览与应用。
  const canCheck = isDesktop && (syncRoot !== null || planRoot !== null) && !checking;

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

  async function runDownload() {
    setDownloaded({ kind: 'downloading' });
    try {
      const view = await downloadRemoteSnapshot();
      setDownloaded({ kind: 'done', view });
      // 刚对齐过远端，之前那份冲突提示已经过时。
      setUpload({ kind: 'idle' });
      // 后面的检查与应用都针对这份暂存快照，不再指向同步文件夹。
      setPlanRoot(view.stagingRoot);
      await runCheck(view.stagingRoot);
    } catch (error) {
      setDownloaded({ kind: 'failed', message: errorMessage(error) });
    }
  }

  async function runUpload(overwrite: boolean, allowPlaintext = false) {
    setUpload({ kind: 'uploading' });
    try {
      const view = await uploadSnapshot(overwrite, allowPlaintext);
      setUpload(
        view.kind === 'conflict'
          ? { kind: 'conflict', view }
          : { kind: 'uploaded', view },
      );
    } catch (error) {
      const failure = asFailure(error);
      if (failure !== null && failure.code === 'secrets_unconfirmed') {
        setUpload({ kind: 'blocked', failure, overwrite });
        return;
      }
      setUpload({
        kind: 'failed',
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
        deletePaths: selected,
        root: planRoot,
      });
      setApplying({ kind: 'done', report });
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

  function toggleDelete(path: string) {
    setSelected((current) =>
      current.includes(path) ? current.filter((item) => item !== path) : [...current, path],
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

  async function runExport(allowPlaintext: boolean) {
    setExported({ kind: 'exporting' });
    try {
      const report = await invoke<ExportReport>('export_snapshot', {
        allowPlaintext,
      });
      setExported({ kind: 'done', report });
    } catch (error) {
      const failure = asFailure(error);
      if (failure !== null && failure.code === 'secrets_unconfirmed') {
        setExported({ kind: 'blocked', failure });
        return;
      }
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
          把当前 Pi 配置整理成一份可校验的快照，写进云盘同步文件夹；密钥会换成环境变量引用。
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
            上传把当前配置导出成快照推到远端；下载按远端 manifest 取回本机暂存目录。两边共用同一套快照格式与校验。
          </p>

          <div className="mt-md flex flex-wrap items-center gap-md">
            <Button onClick={() => void runUpload(false)} disabled={!canUpload}>
              {uploading ? '上传中…' : '上传到 WebDAV'}
            </Button>
            {!isDesktop && (
              <span className="text-sm text-muted-foreground">上传仅桌面版可用</span>
            )}
            {uploading && (
              <span className="text-sm text-muted-foreground" aria-live="polite">
                正在导出快照、比对远端清单并上传变化的文件…
              </span>
            )}
          </div>

          {upload.kind === 'uploaded' && (
            <div className="mt-md flex flex-col gap-sm">
              <dl className="flex flex-col gap-sm">
                <ResultRow label="远端指纹" value={upload.view.fingerprint} mono />
                <ResultRow label="本次上传" value={`${upload.view.uploaded} 个文件`} />
                <ResultRow
                  label="已一致跳过"
                  value={`${upload.view.skipped} 个文件`}
                />
                <ResultRow label="上次同步时间" value={upload.view.seenAt} />
              </dl>
              <p className="text-sm text-success">
                上传完成：manifest 最后写入，远端现在指向这份快照。
              </p>
              <p className="text-xs text-muted-foreground">
                旧快照里的多余文件不会自动删除；需要清理时在网盘里手动处理。
              </p>
            </div>
          )}

          {upload.kind === 'conflict' && (
            <div className="mt-md flex flex-col gap-md rounded-md bg-surface-muted px-md py-sm">
              <p className="flex items-start gap-sm text-sm text-warning">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>
                  {upload.view.remoteCorrupt ? (
                    '远端 manifest 已损坏：还没有往远端写入任何内容，只能覆盖重建。'
                  ) : (
                    <>
                      云端快照与本机记录不一致（远端指纹{' '}
                      <span className="font-mono">{upload.view.remoteFingerprint}</span>
                      ）：还没有往远端写入任何内容，先选一边再继续。
                    </>
                  )}
                </span>
              </p>
              <ConflictDiff view={upload.view} />
              <div className="flex flex-wrap gap-sm">
                <Button onClick={() => void runUpload(true)} disabled={uploading}>
                  覆盖云端
                </Button>
                {!upload.view.remoteCorrupt && (
                  <Button
                    variant="secondary"
                    onClick={() => void runDownload()}
                    disabled={!canDownload}
                  >
                    {downloading ? '下载中…' : '从云端更新'}
                  </Button>
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                覆盖云端 = 用当前配置导出新快照替换远端；从云端更新 = 走下面的下载 → 检查 → 应用。
              </p>
            </div>
          )}

          {upload.kind === 'blocked' && (
            <div className="mt-md flex flex-col gap-sm">
              <p className="flex items-start gap-sm text-sm text-warning">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>
                  {upload.failure.message}；这些位置不会自动改写，确认后才会把明文带进远端快照。
                </span>
              </p>
              <ul className="flex flex-col gap-sm">
                {upload.failure.findings.map((finding) => (
                  <li
                    key={`${finding.path}-${finding.line}-${finding.field ?? ''}`}
                    className="rounded-md bg-surface-muted px-md py-sm"
                  >
                    <p className="break-all font-mono text-xs text-foreground">
                      {finding.path}:{finding.line}
                    </p>
                    <p className="mt-xs text-xs text-muted-foreground">
                      {finding.field ?? '未识别字段'} ·{' '}
                      {finding.level === 'high' ? '高危' : '提示'} · {finding.rule}
                    </p>
                  </li>
                ))}
              </ul>
              <div className="flex flex-wrap gap-sm">
                <Button
                  onClick={() => void runUpload(upload.overwrite, true)}
                  disabled={uploading}
                >
                  仍然上传
                </Button>
                <Button
                  variant="secondary"
                  onClick={() => setUpload({ kind: 'idle' })}
                  disabled={uploading}
                >
                  取消
                </Button>
              </div>
              <p className="text-xs text-muted-foreground">
                更稳妥的做法是先把这些值改成 $NAME / ${'${NAME}'} 形式的引用，再重新上传。
              </p>
            </div>
          )}

          {upload.kind === 'failed' && (
            <div className="mt-md flex flex-col gap-sm">
              <p className="flex items-start gap-sm text-sm text-danger">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>上传失败：{upload.message}</span>
              </p>
              <p className="text-xs text-muted-foreground">
                失败停在原地：已上传的文件留在远端，但旧 manifest 仍指向旧快照；本机远端状态没有更新。
              </p>
            </div>
          )}

          <h3 className="mt-lg text-sm font-semibold text-foreground">从 WebDAV 下载</h3>
          <p className="mt-xs text-sm text-muted-foreground">
            按远端 manifest 把快照取到本机暂存目录：逐文件核对字节数与 sha256，全部通过才写盘；不写本机配置。
          </p>
          <div className="mt-md flex flex-wrap items-center gap-md">
            <Button onClick={() => void runDownload()} disabled={!canDownload}>
              {downloading ? '下载中…' : '从 WebDAV 下载'}
            </Button>
            {!isDesktop && (
              <span className="text-sm text-muted-foreground">下载仅桌面版可用</span>
            )}
            {downloading && (
              <span className="text-sm text-muted-foreground" aria-live="polite">
                正在读取远端 manifest 并下载快照文件…
              </span>
            )}
          </div>

          {downloaded.kind === 'failed' && (
            <div className="mt-md flex flex-col gap-sm">
              <p className="flex items-start gap-sm text-sm text-danger">
                <TriangleAlert className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
                <span>下载失败：{downloaded.message}</span>
              </p>
              <p className="text-xs text-muted-foreground">
                失败即停：本次暂存目录已删除，本机配置与远端状态都保持原样；修正后可以重新下载。
              </p>
            </div>
          )}

          {downloaded.kind === 'done' && (
            <div className="mt-md flex flex-col gap-sm">
              <dl className="flex flex-col gap-sm">
                <ResultRow label="远端指纹" value={downloaded.view.fingerprint} mono />
                <ResultRow label="文件数" value={String(downloaded.view.fileCount)} />
                <ResultRow label="快照创建时间" value={downloaded.view.createdAt} />
                <ResultRow
                  label="上次同步时间"
                  value={
                    downloaded.view.previousSeenAt ?? '首次下载：本机还没有远端记录'
                  }
                />
                <ResultRow label="暂存目录" value={downloaded.view.stagingRoot} mono />
              </dl>
              <p className="text-xs text-muted-foreground">
                下面的检查与应用都针对这份暂存快照；只保留最新的暂存目录。
              </p>
            </div>
          )}
        </Card>
      )}

      <div className="flex flex-wrap items-center gap-md">
        <Button onClick={() => void runExport(false)} disabled={!canExport}>
          {exporting ? '导出中…' : '导出快照'}
        </Button>
        {!isDesktop && (
          <span className="text-sm text-muted-foreground">导出仅桌面版可用</span>
        )}
        {exporting && (
          <span className="text-sm text-muted-foreground" aria-live="polite">
            正在收集配置、引用化密钥并写入临时目录…
          </span>
        )}
      </div>

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
                    onClick={() => setSelected(planning.report.plan.deleted.map((item) => item.path))}
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
              <ul className="mt-xs flex flex-col gap-sm">
                {planning.report.plan.deleted.map((item) => (
                  <li key={item.path} className="rounded-md bg-surface-muted px-md py-sm">
                    <label className="flex items-start gap-sm">
                      <input
                        type="checkbox"
                        className="mt-xs h-4 w-4"
                        checked={selected.includes(item.path)}
                        onChange={() => toggleDelete(item.path)}
                        disabled={!isDesktop || applyBusy}
                      />
                      <span className="break-all font-mono text-xs text-foreground">
                        {item.path}
                      </span>
                    </label>
                  </li>
                ))}
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
                个文件、删除 {selected.length} 个文件；覆盖与删除前会先备份到本机。
              </p>
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
          <h2 className="text-sm font-semibold text-foreground">导出完成</h2>
          <dl className="mt-md flex flex-col gap-sm">
            <ResultRow label="文件数" value={String(exported.report.files)} />
            <ResultRow label="指纹" value={exported.report.fingerprint} mono />
            <ResultRow
              label="需要的环境变量"
              value={String(exported.report.requiredSecrets.length)}
            />
            <ResultRow
              label="声明的包"
              value={String(exported.report.requiredPackages.length)}
            />
            <ResultRow label="跳过的条目" value={String(exported.report.skipped)} />
            <ResultRow label="manifest" value={exported.report.manifestPath} mono />
          </dl>
          <p className="mt-md text-sm text-muted-foreground">
            等待云盘上传完成后再在另一台机器上导入；manifest 最后写入，可作为上传完成的信号。
          </p>
          {exported.report.requiredSecrets.length > 0 && (
            <ul className="mt-sm flex flex-wrap gap-sm">
              {exported.report.requiredSecrets.map((secret) => (
                <li
                  key={secret.name}
                  className="rounded-md bg-surface-muted px-sm py-xs font-mono text-xs text-foreground"
                >
                  {secret.name}
                </li>
              ))}
            </ul>
          )}
          {exported.report.plaintextSecrets && (
            <p className="mt-sm text-sm text-warning">
              本次保留了明文密钥，manifest 已记录；导入端会再提醒一次。
            </p>
          )}
          {exported.report.warnings.length > 0 && (
            <p className="mt-sm text-sm text-muted-foreground">
              另有 {exported.report.warnings.length} 条低置信提示，未自动改写。
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
            原快照与 manifest 保持不动，修正后可以重新导出。
          </p>
        </Card>
      )}

      {exported.kind === 'blocked' && (
        <Card className="p-lg">
          <h2 className="text-sm font-semibold text-foreground">发现明文密钥</h2>
          <p className="mt-xs text-sm text-muted-foreground">
            {exported.failure.message}
            ；这些位置不会自动改写，确认要原样带进快照吗？
          </p>
          <ul className="mt-md flex flex-col gap-sm">
            {exported.failure.findings.map((finding) => (
              <li
                key={`${finding.path}-${finding.line}-${finding.field ?? ''}`}
                className="rounded-md bg-surface-muted px-md py-sm"
              >
                <p className="break-all font-mono text-xs text-foreground">
                  {finding.path}:{finding.line}
                </p>
                <p className="mt-xs text-xs text-muted-foreground">
                  {finding.field ?? '未识别字段'} ·{' '}
                  {finding.level === 'high' ? '高危' : '提示'} · {finding.rule}
                </p>
              </li>
            ))}
          </ul>
          <div className="mt-lg flex flex-wrap gap-sm">
            <Button onClick={() => void runExport(true)} disabled={exporting}>
              仍然导出
            </Button>
            <Button
              variant="secondary"
              onClick={() => setExported({ kind: 'idle' })}
              disabled={exporting}
            >
              取消
            </Button>
          </div>
          <p className="mt-sm text-xs text-muted-foreground">
            更稳妥的做法是先把这些值改成 $NAME / ${'${NAME}'} 形式的引用，再重新导出。
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

/** 冲突清单：只列文件名，完整路径放在悬停提示里。 */
/** 冲突时把“差在哪”讲清楚：清单坏了、没有基准、或逐项列出三类变化。 */
function ConflictDiff({ view }: { view: Extract<RemoteUpload, { kind: 'conflict' }> }) {
  if (view.remoteCorrupt) {
    return (
      <p className="text-xs text-danger">
        远端清单无法解析（多半是上次写入中断）。这里只能覆盖：会用当前配置重建整份快照。
      </p>
    );
  }
  if (view.added.length === 0 && view.modified.length === 0 && view.removed.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">
        本机没有上次同步的记录（换机器或换过地址），算不出文件级差异；覆盖会把云端快照整份换成当前配置。
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-sm">
      <ConflictList title="云端新增" paths={view.added} />
      <ConflictList title="云端修改" paths={view.modified} />
      <ConflictList title="云端删除" paths={view.removed} />
    </div>
  );
}

function ConflictList({ title, paths }: { title: string; paths: string[] }) {
  if (paths.length === 0) {
    return null;
  }
  const shown = paths.slice(0, CONFLICT_LIST_LIMIT);
  return (
    <div>
      <h3 className="text-xs font-semibold text-foreground">
        {title} {paths.length} 项
      </h3>
      <ul className="mt-xs flex flex-col gap-xs">
        {shown.map((path) => (
          <li
            key={path}
            title={path}
            className="truncate font-mono text-xs text-foreground"
          >
            {path.split('/').pop() ?? path}
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
          <li key={item.path} className="rounded-md bg-surface-muted px-md py-sm">
            <p className="break-all font-mono text-xs text-foreground">{item.path}</p>
            <p className="mt-xs text-xs text-muted-foreground">{item.size} B</p>
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
