import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { TriangleAlert } from 'lucide-react';
import { Button } from '../components/ui/button';
import { Card } from '../components/ui/card';
import { loadAppConfig, type PathStatus } from '../lib/app-config';
import { isDesktop } from '../lib/desktop';
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

type ConfigState =
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; syncRoot: string | null; status: PathStatus }
  | { kind: 'failed'; message: string };

type ExportState =
  | { kind: 'idle' }
  | { kind: 'exporting' }
  | { kind: 'done'; report: ExportReport }
  | { kind: 'blocked'; failure: ExportFailure }
  | { kind: 'failed'; message: string };

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

export function SnapshotPage() {
  const [config, setConfig] = useState<ConfigState>({ kind: 'loading' });
  const [exported, setExported] = useState<ExportState>({ kind: 'idle' });

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

  const syncRoot = config.kind === 'ready' ? config.syncRoot : null;
  const exporting = exported.kind === 'exporting';
  const canExport = isDesktop && syncRoot !== null && !exporting;

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
