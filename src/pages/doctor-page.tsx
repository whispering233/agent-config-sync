import { useEffect, useState } from 'react';
import {
  CircleAlert,
  CircleCheck,
  CircleHelp,
  Stethoscope,
  TriangleAlert,
} from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import type { LucideIcon } from 'lucide-react';
import { Button } from '../components/ui/button';
import { Card } from '../components/ui/card';
import { isDesktop } from '../lib/desktop';
import { cn } from '../lib/utils';

type CheckStatus = 'pass' | 'warn' | 'fail' | 'unknown';

/** 与 Rust 侧 `doctor::Check` 一一对应。 */
type DoctorCheck = {
  id: string;
  status: CheckStatus;
  value: string | null;
  detail: string | null;
  action: string | null;
};

/** 与 Rust 侧 `versions::LatestVersion` 一一对应。 */
type LatestVersion = {
  version: string | null;
  note: string | null;
};

/** 与 Rust 侧 `versions::LatestVersions` 一一对应。 */
type LatestVersions = {
  checkedAt: string | null;
  stale: boolean;
  pi: LatestVersion | null;
  packages: Record<string, LatestVersion>;
  error: string | null;
};

/** 与 Rust 侧 `doctor::DoctorReport` 一一对应。 */
type DoctorReport = {
  generatedAt: string;
  platform: { os: string; arch: string };
  checks: DoctorCheck[];
  latest: LatestVersions;
};

type RunState =
  | { kind: 'idle' }
  | { kind: 'running' }
  | { kind: 'ready'; report: DoctorReport }
  | { kind: 'failed'; message: string };

type InstallKind = 'managed' | 'npm' | 'unknown';
type InstallAction = 'install' | 'upgrade';
type InstallStatus = 'succeeded' | 'failed' | 'timeout' | 'unavailable';

/** 与 Rust 侧 `installer::InstallCommand` 一一对应。 */
type InstallCommand = { action: InstallAction; argv: string[] };

/** 与 Rust 侧 `installer::InstallPreview` 一一对应。 */
type InstallPreview = {
  nodeVersion: string | null;
  npmVersion: string | null;
  piPath: string | null;
  piVersion: string | null;
  installKind: InstallKind | null;
  command: InstallCommand | null;
  blocking: string[];
};

/** 与 Rust 侧 `installer::InstallResult` 一一对应；输出已脱敏。 */
type InstallResult = {
  action: InstallAction;
  status: InstallStatus;
  exitCode: number | null;
  output: string;
  truncated: boolean;
  elapsedMs: number;
};

type PreviewState =
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; preview: InstallPreview }
  | { kind: 'failed'; message: string };

/** 与 Rust 侧 `packages::DeclaredPackage` 一一对应。 */
type DeclaredPackage = { source: string; enabled: boolean };

/** 与 Rust 侧 `packages::InstalledPackage` 一一对应。 */
type InstalledPackage = { source: string; path: string; version: string | null };

/** 与 Rust 侧 `packages::PackageStatus` 一一对应。 */
type PackageStatus = {
  declared: DeclaredPackage[];
  installed: InstalledPackage[];
  missing: string[];
  extra: string[];
};

/** 与 Rust 侧 `packages::AlignResult` 一一对应；输出已截断并脱敏。 */
type AlignResult = {
  source: string;
  status: InstallStatus;
  exitCode: number | null;
  output: string;
  truncated: boolean;
};

/** 与 Rust 侧 `packages::AlignReport` 一一对应。 */
type AlignReport = { results: AlignResult[]; completed: boolean };

type PackageViewState =
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; status: PackageStatus }
  | { kind: 'failed'; message: string };

type AlignRunState =
  | { kind: 'idle' }
  | { kind: 'confirming' }
  | { kind: 'running' }
  | { kind: 'done'; report: AlignReport }
  | { kind: 'failed'; message: string };

/** 与 Rust 侧 `scope::RootId` 一一对应。 */
type SkillRoot = 'agent' | 'agents-skills';

/** 与 Rust 侧 `skills::SkillSource` 一一对应。 */
type SkillSource = 'user' | 'package' | 'project' | 'other';

/** 与 Rust 侧 `skills::SkillEntry` 一一对应。 */
type SkillEntry = {
  name: string;
  description: string | null;
  path: string | null;
  root: SkillRoot | null;
  snapshot: boolean;
  source: SkillSource;
  reason: string | null;
};

/** 与 Rust 侧 `skills::SkillInventory` 一一对应。 */
type SkillInventory = {
  error: string | null;
  skills: SkillEntry[];
  warnings: string[];
  createdFiles: string[];
};

type SkillsState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; report: SkillInventory }
  | { kind: 'failed'; message: string };

type InstallRunState =
  | { kind: 'idle' }
  | { kind: 'confirming'; command: InstallCommand }
  | { kind: 'running'; command: InstallCommand }
  | { kind: 'done'; result: InstallResult }
  | { kind: 'failed'; message: string };

function runDoctorCheck(refreshVersions = false): Promise<DoctorReport> {
  return invoke<DoctorReport>('run_check', { refreshVersions });
}

function loadInstallPreview(): Promise<InstallPreview> {
  return invoke<InstallPreview>('preview_install');
}

function runInstallCommand(action: InstallAction): Promise<InstallResult> {
  return invoke<InstallResult>('install_or_upgrade', { action, confirm: true });
}

function loadPackageStatus(): Promise<PackageStatus> {
  return invoke<PackageStatus>('package_status');
}

function runAlignPackages(sources: string[]): Promise<AlignReport> {
  return invoke<AlignReport>('align_packages', { sources, confirm: true });
}

function loadSkillInventory(): Promise<SkillInventory> {
  return invoke<SkillInventory>('skills_inventory');
}

const groupOrder = ['环境', '宿主与工具', '凭据', '扩展包', '技能'] as const;
type GroupLabel = (typeof groupOrder)[number];

const checkLabels: Record<string, string> = {
  os: '操作系统',
  agent_dir: '宿主目录',
  'node.path': 'Node 路径',
  'node.version': 'Node 版本',
  npm: 'npm',
  'pi.path': 'Pi 路径',
  'pi.version': 'Pi 版本',
  'pi.latest': 'Pi 最新版本',
  'pi.install_kind': '安装方式',
  packages: '扩展包',
  'skills.links': '技能链接',
};

const actionLabels: Record<string, string> = {
  'agent_dir.edit': '在设置里修正宿主目录后重新检查。',
  'node.install': '安装 Node 22.19.0 或更高版本，然后重新检查。',
  'host.install': '安装 Pi CLI，然后重新检查。',
  'host.upgrade': '升级 Pi CLI，然后重新检查。',
  'auth.configure': '在终端执行 pi auth login <provider>，然后重新检查。',
  'packages.install': '按声明安装缺少的扩展包，然后重新检查。',
  'skills.relink': '重建 skills 下的符号链接。',
};

const statusText: Record<CheckStatus, string> = {
  pass: '正常',
  warn: '注意',
  fail: '异常',
  unknown: '未知',
};

const statusTone: Record<CheckStatus, string> = {
  pass: 'text-success',
  warn: 'text-warning',
  fail: 'text-danger',
  unknown: 'text-muted-foreground',
};

const statusIcon: Record<CheckStatus, LucideIcon> = {
  pass: CircleCheck,
  warn: TriangleAlert,
  fail: CircleAlert,
  unknown: CircleHelp,
};

const installKindText: Record<InstallKind, string> = {
  managed: '托管安装',
  npm: 'npm 全局安装',
  unknown: '未知（需手动升级）',
};

const installActionText: Record<InstallAction, string> = {
  install: '安装 Pi CLI',
  upgrade: '升级 Pi CLI',
};

const installStatusText: Record<InstallStatus, string> = {
  succeeded: '安装/升级完成',
  failed: '命令非零退出，请检查输出',
  timeout: '执行超时（10 分钟），进程组已终止',
  unavailable: '命令无法启动',
};

const alignStatusText: Record<InstallStatus, string> = {
  succeeded: '已安装',
  failed: '安装失败，已停止（不自动重试）',
  timeout: '安装超时（10 分钟），进程组已终止',
  unavailable: '命令无法启动',
};

/** 命令以数组返回，展示时给含空格的参数加引号。 */
function commandText(argv: string[]): string {
  return argv.map((part) => (/\s/.test(part) ? `"${part}"` : part)).join(' ');
}

/** 与 Rust 侧 `platform::sh_quote` 一致：后端拼接的单引号形式，展示时才与执行的一致。 */
function shQuote(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

function installCommandText(source: string): string {
  return `pi install ${shQuote(source)}`;
}

function groupOf(id: string): GroupLabel {
  if (id === 'os' || id === 'agent_dir') {
    return '环境';
  }
  if (id.startsWith('auth')) {
    return '凭据';
  }
  if (id.startsWith('packages')) {
    return '扩展包';
  }
  if (id.startsWith('skills')) {
    return '技能';
  }
  return '宿主与工具';
}

function labelOf(id: string): string {
  if (id.startsWith('auth:')) {
    return `凭据 · ${id.slice('auth:'.length)}`;
  }
  return checkLabels[id] ?? id;
}

function groupsOf(checks: DoctorCheck[]) {
  return groupOrder
    .map((label) => ({
      label,
      checks: checks.filter((check) => groupOf(check.id) === label),
    }))
    .filter((group) => group.checks.length > 0);
}

function summaryOf(checks: DoctorCheck[]): { text: string; tone: string } {
  const count = (status: CheckStatus) =>
    checks.filter((check) => check.status === status).length;
  const failed = count('fail');
  const warned = count('warn');
  const unknown = count('unknown');

  if (failed > 0) {
    return { text: `有 ${failed} 项需要处理`, tone: 'text-danger' };
  }
  if (warned > 0) {
    return { text: `有 ${warned} 项需要注意`, tone: 'text-warning' };
  }
  if (unknown > 0) {
    return { text: `有 ${unknown} 项状态未知`, tone: 'text-muted-foreground' };
  }
  return { text: '环境就绪', tone: 'text-success' };
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function DoctorPage() {
  const [state, setState] = useState<RunState>({ kind: 'idle' });
  const [preview, setPreview] = useState<PreviewState>(
    isDesktop ? { kind: 'loading' } : { kind: 'browser' },
  );
  const [installRun, setInstallRun] = useState<InstallRunState>({ kind: 'idle' });

  useEffect(() => {
    if (isDesktop) {
      void refreshPreview();
    }
  }, []);

  async function handleRun(refreshVersions = false) {
    setState({ kind: 'running' });
    try {
      const report = await runDoctorCheck(refreshVersions);
      setState({ kind: 'ready', report });
    } catch (error) {
      setState({ kind: 'failed', message: errorMessage(error) });
    }
  }

  async function refreshPreview() {
    setPreview({ kind: 'loading' });
    try {
      setPreview({ kind: 'ready', preview: await loadInstallPreview() });
    } catch (error) {
      setPreview({ kind: 'failed', message: errorMessage(error) });
    }
  }

  /** 后端会重新探测并校验动作，前端只能提交枚举与确认位。 */
  async function handleInstall(command: InstallCommand) {
    setInstallRun({ kind: 'running', command });
    try {
      const result = await runInstallCommand(command.action);
      setInstallRun({ kind: 'done', result });
      if (result.status === 'succeeded') {
        await handleRun(false);
        await refreshPreview();
      }
    } catch (error) {
      setInstallRun({ kind: 'failed', message: errorMessage(error) });
    }
  }

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-lg">
      <header>
        <h1 className="text-lg font-semibold text-foreground">环境检测</h1>
        <p className="mt-xs text-sm text-muted-foreground">
          只读检查 Node.js、Pi CLI、凭据与扩展包是否就绪。检查过程不写任何文件，也不会安装东西。
        </p>
      </header>

      <Card className="p-lg">
        <div className="flex flex-wrap items-center gap-md">
          <Button
            onClick={() => void handleRun(false)}
            disabled={!isDesktop || state.kind === 'running'}
          >
            {state.kind === 'running' ? '检查中…' : '开始检查'}
          </Button>
          <Button
            variant="secondary"
            onClick={() => void handleRun(true)}
            disabled={!isDesktop || state.kind === 'running'}
          >
            检查更新
          </Button>
          {!isDesktop && (
            <span className="text-sm text-muted-foreground">仅桌面版可用</span>
          )}
          {state.kind === 'ready' && (
            <span className={cn('text-sm', summaryOf(state.report.checks).tone)}>
              {summaryOf(state.report.checks).text}
            </span>
          )}
        </div>

        {!isDesktop && (
          <p className="mt-md text-xs leading-relaxed text-muted-foreground">
            浏览器预览里没有 Rust 进程，无法执行探测命令。这个页面只展示按钮与说明；请在 Tauri
            桌面版里运行检查。
          </p>
        )}

        {state.kind === 'failed' && (
          <p className="mt-md text-sm text-danger">{state.message}</p>
        )}

        {state.kind === 'ready' && (
          <p className="mt-md text-xs text-muted-foreground">
            {state.report.platform.os} · {state.report.platform.arch} · 检查于{' '}
            {state.report.generatedAt}
          </p>
        )}
        {state.kind === 'ready' && state.report.latest.error !== null && (
          <p className="mt-xs text-xs text-warning">{state.report.latest.error}</p>
        )}
        {state.kind === 'ready' && state.report.latest.checkedAt !== null && (
          <p className="mt-xs text-xs text-muted-foreground">
            最新版本缓存于 {state.report.latest.checkedAt}
            {state.report.latest.stale && '（已超过 6 小时，点“检查更新”刷新）'}
          </p>
        )}
      </Card>

      <InstallCard
        preview={preview}
        run={installRun}
        onRefresh={() => void refreshPreview()}
        onStart={(command) => setInstallRun({ kind: 'confirming', command })}
        onCancel={() => setInstallRun({ kind: 'idle' })}
        onConfirm={(command) => void handleInstall(command)}
      />

      <PackagesCard latest={state.kind === 'ready' ? state.report.latest : null} />

      <SkillsCard />

      {state.kind === 'idle' && (
        <Card className="p-lg text-center">
          <div className="mx-auto flex h-12 w-12 items-center justify-center rounded-lg bg-surface-muted text-accent">
            <Stethoscope className="h-6 w-6" aria-hidden="true" />
          </div>
          <p className="mt-lg text-sm text-muted-foreground">
            还没有检查这台电脑。点“开始检查”生成报告。
          </p>
        </Card>
      )}

      {state.kind === 'ready' &&
        groupsOf(state.report.checks).map((group) => (
          <Card key={group.label} className="p-lg">
            <h2 className="text-sm font-semibold text-foreground">{group.label}</h2>
            <ul className="mt-md flex flex-col gap-md">
              {group.checks.map((check) => (
                <CheckRow key={check.id} check={check} />
              ))}
            </ul>
          </Card>
        ))}
    </div>
  );
}

function CheckRow({ check }: { check: DoctorCheck }) {
  const Icon = statusIcon[check.status];

  return (
    <li className="flex gap-sm">
      <Icon
        className={cn('mt-0.5 h-4 w-4 shrink-0', statusTone[check.status])}
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-baseline gap-sm">
          <span className="text-sm font-medium text-foreground">
            {labelOf(check.id)}
          </span>
          <span className={cn('text-xs', statusTone[check.status])}>
            {statusText[check.status]}
          </span>
          {check.value !== null && (
            <span className="break-all text-xs text-muted-foreground">
              {check.value}
            </span>
          )}
        </div>
        {check.detail !== null && (
          <p className="mt-xs break-all text-xs leading-relaxed text-muted-foreground">
            {check.detail}
          </p>
        )}
        {check.action !== null && (
          <p className="mt-xs text-xs leading-relaxed text-muted-foreground">
            建议：{actionLabels[check.action] ?? check.action}
          </p>
        )}
      </div>
    </li>
  );
}

function InstallCard({
  preview,
  run,
  onRefresh,
  onStart,
  onCancel,
  onConfirm,
}: {
  preview: PreviewState;
  run: InstallRunState;
  onRefresh: () => void;
  onStart: (command: InstallCommand) => void;
  onCancel: () => void;
  onConfirm: (command: InstallCommand) => void;
}) {
  const ready = preview.kind === 'ready' ? preview.preview : null;
  const command = ready?.command ?? null;
  const running = run.kind === 'running';

  return (
    <Card className="p-lg">
      <h2 className="text-sm font-semibold text-foreground">宿主安装与升级</h2>
      <p className="mt-xs text-sm text-muted-foreground">
        先展示命令，用户确认后才执行；不会自动安装 Node，也不会改 shell 启动文件。
      </p>

      {preview.kind === 'loading' && (
        <p className="mt-md text-sm text-muted-foreground">正在探测安装条件…</p>
      )}
      {preview.kind === 'browser' && (
        <p className="mt-md text-xs leading-relaxed text-muted-foreground">
          浏览器预览里没有 Rust 进程，探测与安装命令都只在 Tauri 桌面版可用。
        </p>
      )}
      {preview.kind === 'failed' && (
        <p className="mt-md text-sm text-danger">{preview.message}</p>
      )}

      {ready !== null && (
        <>
          <dl className="mt-md flex flex-col gap-sm">
            <InfoRow label="Node 版本" value={ready.nodeVersion ?? '未找到'} />
            <InfoRow label="npm" value={ready.npmVersion ?? '未找到'} />
            <InfoRow
              label="Pi 版本"
              value={ready.piVersion ?? (ready.piPath === null ? '未安装' : '读不到版本')}
            />
            <InfoRow
              label="安装方式"
              value={
                ready.installKind === null
                  ? '未安装'
                  : installKindText[ready.installKind]
              }
            />
            {ready.piPath !== null && (
              <InfoRow label="Pi 入口" value={ready.piPath} mono />
            )}
          </dl>

          {ready.blocking.length > 0 && (
            <>
              <ul className="mt-md flex flex-col gap-sm">
                {ready.blocking.map((reason) => (
                  <li
                    key={reason}
                    className="flex items-start gap-sm text-sm text-warning"
                  >
                    <TriangleAlert
                      className="mt-0.5 h-4 w-4 shrink-0"
                      aria-hidden="true"
                    />
                    <span className="break-all">{reason}</span>
                  </li>
                ))}
              </ul>
              {ready.nodeVersion === null && (
                <p className="mt-sm text-xs leading-relaxed text-muted-foreground">
                  先按官方文档安装 Node 22.19.0 或更高版本（https://nodejs.org，或
                  nvm install 22 / fnm install 22），装完点“重新检测”。本应用不会自动安装
                  Node。
                </p>
              )}
            </>
          )}

          {command !== null && (
            <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
              <p className="text-xs text-muted-foreground">将执行的命令</p>
              <p className="mt-xs break-all font-mono text-sm text-foreground">
                {commandText(command.argv)}
              </p>
              <p className="mt-xs text-xs text-muted-foreground">
                命令由后端按平台与安装方式拼接，前端只能提交“安装/升级”这个动作。
              </p>
            </div>
          )}

          <div className="mt-lg flex flex-wrap gap-sm">
            <Button variant="secondary" onClick={onRefresh} disabled={running}>
              重新检测
            </Button>
            {command !== null && run.kind === 'idle' && (
              <Button onClick={() => onStart(command)}>
                {installActionText[command.action]}
              </Button>
            )}
          </div>
        </>
      )}

      {(run.kind === 'confirming' || run.kind === 'running') && (
        <div className="mt-lg rounded-md bg-surface-muted px-md py-sm">
          <p className="text-sm font-medium text-foreground">确认执行这条命令？</p>
          <p className="mt-xs break-all font-mono text-sm text-foreground">
            {commandText(run.command.argv)}
          </p>
          <p className="mt-xs text-xs leading-relaxed text-muted-foreground">
            最多 10 分钟，输出脱敏后展示。失败不会自动重试，也不会换别的方案。
          </p>
          <div className="mt-md flex flex-wrap gap-sm">
            <Button onClick={() => onConfirm(run.command)} disabled={running}>
              {running ? '执行中…' : '确认执行'}
            </Button>
            <Button variant="secondary" onClick={onCancel} disabled={running}>
              取消
            </Button>
          </div>
        </div>
      )}

      {run.kind === 'done' && (
        <div className="mt-lg rounded-md bg-surface-muted px-md py-sm">
          <p
            className={cn(
              'text-sm font-medium',
              run.result.status === 'succeeded' ? 'text-success' : 'text-danger',
            )}
          >
            {installStatusText[run.result.status]}
          </p>
          <p className="mt-xs text-xs text-muted-foreground">
            退出码 {run.result.exitCode ?? '—'} · 耗时{' '}
            {(run.result.elapsedMs / 1000).toFixed(1)} 秒
            {run.result.truncated && ' · 输出已截断（仅保留最后 100 行、最多 200 KB）'}
          </p>
          {run.result.output !== '' && (
            <pre className="mt-sm max-h-64 overflow-auto whitespace-pre-wrap break-all font-mono text-xs leading-relaxed text-foreground">
              {run.result.output}
            </pre>
          )}
          {run.result.status === 'succeeded' && (
            <p className="mt-sm text-xs leading-relaxed text-muted-foreground">
              已自动重跑环境检测：看上面的 Pi 路径与版本是否更新。若仍提示找不到 pi，请重开终端后再检查。
            </p>
          )}
        </div>
      )}

      {run.kind === 'failed' && (
        <p className="mt-lg text-sm text-danger">执行失败：{run.message}</p>
      )}
    </Card>
  );
}

function PackagesCard({ latest }: { latest: LatestVersions | null }) {
  const [view, setView] = useState<PackageViewState>(
    isDesktop ? { kind: 'loading' } : { kind: 'browser' },
  );
  const [selected, setSelected] = useState<string[]>([]);
  const [run, setRun] = useState<AlignRunState>({ kind: 'idle' });

  useEffect(() => {
    if (isDesktop) {
      void refresh();
    }
  }, []);

  async function refresh() {
    setView({ kind: 'loading' });
    try {
      const status = await loadPackageStatus();
      setView({ kind: 'ready', status });
      // 默认全选缺失项，用户可以取消勾选；取消勾选的不会被提交。
      setSelected(status.missing);
    } catch (error) {
      setView({ kind: 'failed', message: errorMessage(error) });
    }
  }

  /** 后端会重新计算缺失清单并校验包名，校验失败一行命令都不执行。 */
  async function handleAlign() {
    setRun({ kind: 'running' });
    try {
      const report = await runAlignPackages(selected);
      setRun({ kind: 'done', report });
      if (report.completed) {
        await refresh();
      }
    } catch (error) {
      setRun({ kind: 'failed', message: errorMessage(error) });
    }
  }

  function toggle(source: string) {
    setSelected((current) =>
      current.includes(source)
        ? current.filter((item) => item !== source)
        : [...current, source],
    );
  }

  const status = view.kind === 'ready' ? view.status : null;
  const busy = run.kind === 'running';

  return (
    <Card className="p-lg">
      <h2 className="text-sm font-semibold text-foreground">包对齐</h2>
      <p className="mt-xs text-sm text-muted-foreground">
        比对 <span className="font-mono">settings.json</span> 的 packages 声明与{' '}
        <span className="font-mono">pi list</span> 的实装。只安装缺失项；多余包只列出，不会自动删除。
      </p>

      {view.kind === 'loading' && (
        <p className="mt-md text-sm text-muted-foreground">正在读取包状态…</p>
      )}
      {view.kind === 'browser' && (
        <p className="mt-md text-xs leading-relaxed text-muted-foreground">
          浏览器预览里没有 Rust 进程，读不到{' '}
          <span className="font-mono">settings.json</span> 也执行不了{' '}
          <span className="font-mono">pi list</span>
          。包状态与安装命令只在 Tauri 桌面版可用。
        </p>
      )}
      {view.kind === 'failed' && (
        <p className="mt-md text-sm text-danger">{view.message}</p>
      )}

      {status !== null && (
        <>
          <dl className="mt-md grid grid-cols-2 gap-sm">
            <InfoRow label="声明" value={`${status.declared.length} 个`} />
            <InfoRow label="实装" value={`${status.installed.length} 个`} />
            <InfoRow label="缺失" value={`${status.missing.length} 个`} />
            <InfoRow label="多余" value={`${status.extra.length} 个`} />
          </dl>

          {status.declared.length > 0 && (
            <PackageList
              title="声明"
              items={status.declared.map((item, index) => ({
                key: `${index}-${item.source}`,
                text: item.enabled
                  ? item.source
                  : `${item.source}（已停用，不自动安装）`,
              }))}
            />
          )}
          {status.installed.length > 0 && (
            <PackageList
              title="实装"
              items={status.installed.map((item, index) => {
                const remote = latest?.packages[item.source];
                const remoteText =
                  remote === undefined
                    ? ''
                    : remote.version !== null
                      ? ` · 最新 ${remote.version}`
                      : remote.note !== null
                        ? ` · ${remote.note}`
                        : '';
                return {
                  key: `${index}-${item.source}`,
                  text:
                    (item.version === null
                      ? item.source
                      : `${item.source} · ${item.version}`) + remoteText,
                  sub: item.path === '' ? undefined : item.path,
                };
              })}
            />
          )}

          {status.missing.length > 0 ? (
            <div className="mt-md">
              <p className="text-xs text-muted-foreground">
                缺失（勾选要安装的项，命令由后端拼接）
              </p>
              <ul className="mt-sm flex flex-col gap-sm">
                {status.missing.map((source) => (
                  <li key={source}>
                    <label className="flex items-start gap-sm text-sm text-foreground">
                      <input
                        type="checkbox"
                        className="mt-1"
                        checked={selected.includes(source)}
                        disabled={busy || run.kind === 'confirming'}
                        onChange={() => toggle(source)}
                      />
                      <span className="min-w-0 flex-1 break-all">
                        {source}
                        <span className="block font-mono text-xs text-muted-foreground">
                          {installCommandText(source)}
                        </span>
                      </span>
                    </label>
                  </li>
                ))}
              </ul>
            </div>
          ) : (
            <p className="mt-md text-sm text-success">没有缺失的包。</p>
          )}

          {status.extra.length > 0 && (
            <PackageList
              title="多余（不会自动删除，需要时手动 pi remove）"
              items={status.extra.map((source, index) => ({
                key: `${index}-${source}`,
                text: source,
              }))}
            />
          )}

          <div className="mt-lg flex flex-wrap gap-sm">
            <Button variant="secondary" onClick={() => void refresh()} disabled={busy}>
              刷新状态
            </Button>
            {run.kind !== 'confirming' && run.kind !== 'running' && (
              <Button
                onClick={() => setRun({ kind: 'confirming' })}
                disabled={selected.length === 0}
              >
                对齐选中的 {selected.length} 个包
              </Button>
            )}
          </div>
        </>
      )}

      {(run.kind === 'confirming' || run.kind === 'running') && (
        <div className="mt-lg rounded-md bg-surface-muted px-md py-sm">
          <p className="text-sm font-medium text-foreground">确认执行这些命令？</p>
          <ul className="mt-xs flex flex-col gap-xs font-mono text-sm text-foreground">
            {selected.map((source) => (
              <li key={source} className="break-all">
                {installCommandText(source)}
              </li>
            ))}
          </ul>
          <p className="mt-xs text-xs leading-relaxed text-muted-foreground">
            逐个执行，每个最多 10 分钟；失败即停，不自动重试，也不会换别的方案。
          </p>
          <div className="mt-md flex flex-wrap gap-sm">
            <Button onClick={() => void handleAlign()} disabled={busy}>
              {busy ? '安装中…' : '确认执行'}
            </Button>
            <Button
              variant="secondary"
              onClick={() => setRun({ kind: 'idle' })}
              disabled={busy}
            >
              取消
            </Button>
          </div>
        </div>
      )}

      {run.kind === 'done' && (
        <div className="mt-lg rounded-md bg-surface-muted px-md py-sm">
          <p
            className={cn(
              'text-sm font-medium',
              run.report.completed ? 'text-success' : 'text-danger',
            )}
          >
            {run.report.completed
              ? `${run.report.results.length} 个包已安装`
              : '中途失败，后面的包没有执行'}
          </p>
          <ul className="mt-md flex flex-col gap-md">
            {run.report.results.map((result) => (
              <li key={result.source}>
                <p className="break-all text-sm text-foreground">{result.source}</p>
                <p
                  className={cn(
                    'text-xs',
                    result.status === 'succeeded' ? 'text-success' : 'text-danger',
                  )}
                >
                  {alignStatusText[result.status]} · 退出码 {result.exitCode ?? '—'}
                  {result.truncated && ' · 输出已截断（仅保留最后 100 行、最多 200 KB）'}
                </p>
                {result.output !== '' && (
                  <pre className="mt-xs max-h-48 overflow-auto whitespace-pre-wrap break-all font-mono text-xs leading-relaxed text-foreground">
                    {result.output}
                  </pre>
                )}
              </li>
            ))}
          </ul>
          <Button
            variant="secondary"
            className="mt-md"
            onClick={() => {
              setRun({ kind: 'idle' });
              void refresh();
            }}
          >
            重新读取状态
          </Button>
        </div>
      )}

      {run.kind === 'failed' && (
        <p className="mt-lg text-sm text-danger">执行失败：{run.message}</p>
      )}
    </Card>
  );
}

function PackageList({
  title,
  items,
}: {
  title: string;
  /** 同一个包可能出现在两条来源里，展示文本会重复：key 由调用方给，不能拿文本顶替。 */
  items: ReadonlyArray<{ key: string; text: string; sub?: string }>;
}) {
  return (
    <div className="mt-md">
      <p className="text-xs text-muted-foreground">{title}</p>
      <ul className="mt-xs flex flex-col gap-xs">
        {items.map((item) => (
          <li
            key={item.key}
            className="break-all rounded-md bg-surface-muted px-md py-xs font-mono text-xs text-foreground"
          >
            {item.text}
            {item.sub !== undefined && (
              <span className="block font-mono text-xs text-muted-foreground">
                {item.sub}
              </span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

function InfoRow({
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
        className={cn('break-all text-sm text-foreground', mono && 'font-mono')}
      >
        {value}
      </dd>
    </div>
  );
}

const skillRootLabels: Record<SkillRoot, string> = {
  agent: '宿主目录',
  'agents-skills': '~/.agents/skills',
};

/**
 * 技能清单只在用户点击后拉取：RPC 会启动宿主，宿主可能创建空文件，
 * 所以首次点击前先把副作用说清楚，不能用挂载即请求的方式。
 */
function SkillsCard() {
  const [state, setState] = useState<SkillsState>(
    isDesktop ? { kind: 'idle' } : { kind: 'browser' },
  );

  const running = state.kind === 'loading';
  const report = state.kind === 'ready' ? state.report : null;

  async function refresh() {
    setState({ kind: 'loading' });
    try {
      setState({ kind: 'ready', report: await loadSkillInventory() });
    } catch (error) {
      setState({ kind: 'failed', message: errorMessage(error) });
    }
  }

  return (
    <Card className="p-lg">
      <h2 className="text-sm font-semibold text-foreground">技能</h2>
      <p className="mt-xs text-sm text-muted-foreground">
        读宿主实际加载的技能，并标出哪些随快照。只读展示，不修复、不改文件。
      </p>

      {state.kind === 'browser' && (
        <p className="mt-md text-xs leading-relaxed text-muted-foreground">
          浏览器预览里没有 Rust 进程，起不了宿主 RPC；技能清单只在桌面版可用。
        </p>
      )}

      {state.kind === 'idle' && (
        <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
          <p className="text-xs leading-relaxed text-muted-foreground">
            刷新会启动{' '}
            <span className="break-all font-mono">
              pi --mode rpc --offline --no-session --no-extensions --no-approve
            </span>
            ，最多等 15 秒，失败不重试。若宿主目录缺少{' '}
            <span className="font-mono">auth.json</span> 或{' '}
            <span className="font-mono">models-store.json</span>
            ，宿主会创建空文件：本应用不删除它们；新建的空{' '}
            <span className="font-mono">auth.json</span>{' '}
            会被后续导出当作源机凭据状态纳入，并在应用端覆盖目标凭据（覆盖前有备份）。
          </p>
        </div>
      )}

      <div className="mt-md flex flex-wrap items-center gap-sm">
        <Button onClick={() => void refresh()} disabled={!isDesktop || running}>
          {running ? '读取中…' : '刷新技能'}
        </Button>
        {!isDesktop && (
          <span className="text-sm text-muted-foreground">仅桌面版可用</span>
        )}
        {report !== null && report.error === null && (
          <span className="text-sm text-muted-foreground">
            共 {report.skills.length} 个技能
          </span>
        )}
      </div>

      {running && (
        <p className="mt-sm text-xs text-muted-foreground">
          正在启动宿主 RPC，最多 15 秒…
        </p>
      )}

      {state.kind === 'failed' && (
        <p className="mt-md text-sm text-danger">{state.message}</p>
      )}

      {report !== null && (
        <>
          {report.error !== null && (
            <p className="mt-md text-sm text-danger">{report.error}</p>
          )}
          <CreatedFiles files={report.createdFiles} />

          {report.error === null &&
            (report.skills.length === 0 ? (
              <p className="mt-md text-sm text-muted-foreground">
                宿主没有加载技能。
              </p>
            ) : (
              <>
                <SkillGroup
                  title="随快照"
                  hint="导出会带上，应用时按所属根落地"
                  skills={report.skills.filter((skill) => skill.snapshot)}
                />
                <SkillGroup
                  title="包提供"
                  hint="随包重装恢复，不进快照"
                  skills={report.skills.filter(
                    (skill) => !skill.snapshot && skill.source === 'package',
                  )}
                />
                <SkillGroup
                  title="不随快照"
                  skills={report.skills.filter(
                    (skill) => !skill.snapshot && skill.source !== 'package',
                  )}
                />
              </>
            ))}

          {report.warnings.length > 0 && (
            <ul className="mt-md flex flex-col gap-sm">
              {report.warnings.map((warning) => (
                <li
                  key={warning}
                  className="flex items-start gap-sm text-xs leading-relaxed text-warning"
                >
                  <TriangleAlert
                    className="mt-0.5 h-4 w-4 shrink-0"
                    aria-hidden="true"
                  />
                  <span className="break-all">{warning}</span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </Card>
  );
}

function CreatedFiles({ files }: { files: string[] }) {
  if (files.length === 0) {
    return null;
  }

  return (
    <div className="mt-md rounded-md bg-surface-muted px-md py-sm">
      <p className="text-sm font-medium text-warning">
        本次 RPC 在宿主目录新建了 {files.join('、')}
      </p>
      <p className="mt-xs text-xs leading-relaxed text-muted-foreground">
        这些文件不会被自动删除；已存在的文件内容没有变化。空{' '}
        <span className="font-mono">auth.json</span>{' '}
        会被后续导出当作源机凭据状态纳入，并在应用端覆盖目标凭据（覆盖前有备份）。
      </p>
    </div>
  );
}

function SkillGroup({
  title,
  hint,
  skills,
}: {
  title: string;
  hint?: string;
  skills: SkillEntry[];
}) {
  if (skills.length === 0) {
    return null;
  }

  return (
    <div className="mt-md">
      <p className="text-xs text-muted-foreground">
        {title}（{skills.length} 个）{hint !== undefined && ` · ${hint}`}
      </p>
      <ul className="mt-xs flex flex-col gap-xs">
        {skills.map((skill) => (
          <li
            key={`${skill.root ?? skill.source}-${skill.name}-${skill.path ?? ''}`}
            className="rounded-md bg-surface-muted px-md py-xs"
          >
            <div className="flex flex-wrap items-baseline gap-sm">
              <span className="text-sm font-medium text-foreground">
                {skill.name}
              </span>
              {skill.root !== null && (
                <span className="text-xs text-muted-foreground">
                  {skillRootLabels[skill.root]}
                </span>
              )}
            </div>
            {skill.description !== null && (
              <p className="mt-xs text-xs leading-relaxed text-muted-foreground">
                {skill.description}
              </p>
            )}
            {skill.path !== null && (
              <p className="mt-xs break-all font-mono text-xs text-muted-foreground">
                {skill.path}
              </p>
            )}
            {skill.reason !== null && (
              <p className="mt-xs text-xs leading-relaxed text-warning">
                {skill.reason}
              </p>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
