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

/** 与 Rust 侧 `PlanFailure` 一一对应。 */
type PlanFailure = {
  code: string;
  message: string;
  mismatches: HashMismatch[];
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

/** 每个分组最多列出的条数；其余只计数。 */
const PLAN_LIST_LIMIT = 20;

/** 应用提示最多列出的条数；其余只计数。 */
const APPLY_WARNING_LIMIT = 5;

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

export function SnapshotPage() {
  const [config, setConfig] = useState<ConfigState>({ kind: 'loading' });
  const [exported, setExported] = useState<ExportState>({ kind: 'idle' });
  const [planning, setPlanning] = useState<PlanState>({ kind: 'idle' });
  const [selected, setSelected] = useState<string[]>([]);
  const [applying, setApplying] = useState<ApplyState>({ kind: 'idle' });

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
  const checking = planning.kind === 'checking';
  const canCheck = isDesktop && syncRoot !== null && !checking;

  async function runCheck() {
    setPlanning({ kind: 'checking' });
    // 计划重算后旧的勾选与结果都不再对应，清空。
    setSelected([]);
    setApplying({ kind: 'idle' });
    try {
      const report = await invoke<PlanReport>('verify_and_plan');
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

      <Card className="p-lg">
        <h2 className="text-sm font-semibold text-foreground">检查与预览</h2>
        <p className="mt-xs text-sm text-muted-foreground">
          只读校验：重算快照每个文件的大小与哈希，再和本机配置比较，得出新增 / 修改 / 删除候选；不写任何文件。
        </p>
        <div className="mt-md flex flex-wrap items-center gap-md">
          <Button variant="secondary" onClick={() => void runCheck()} disabled={!canCheck}>
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
