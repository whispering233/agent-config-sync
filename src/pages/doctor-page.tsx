import { useState } from 'react';
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

/** 与 Rust 侧 `doctor::DoctorReport` 一一对应。 */
type DoctorReport = {
  generatedAt: string;
  platform: { os: string; arch: string };
  checks: DoctorCheck[];
};

type RunState =
  | { kind: 'idle' }
  | { kind: 'running' }
  | { kind: 'ready'; report: DoctorReport }
  | { kind: 'failed'; message: string };

function runDoctorCheck(): Promise<DoctorReport> {
  return invoke<DoctorReport>('run_check');
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
  'pi.install_kind': '安装方式',
  packages: '扩展包',
  'skills.links': '技能链接',
};

const actionLabels: Record<string, string> = {
  'agent_dir.edit': '在设置里修正宿主目录后重新检查。',
  'node.install': '安装 Node 22.19.0 或更高版本，然后重新检查。',
  'host.install': '安装 Pi CLI，然后重新检查。',
  'host.upgrade': '升级 Pi CLI，然后重新检查。',
  'auth.configure': '在密钥向导里补齐这个 provider 的凭据。',
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

  async function handleRun() {
    setState({ kind: 'running' });
    try {
      const report = await runDoctorCheck();
      setState({ kind: 'ready', report });
    } catch (error) {
      setState({ kind: 'failed', message: errorMessage(error) });
    }
  }

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-lg">
      <header>
        <h1 className="text-lg font-semibold text-foreground">环境医生</h1>
        <p className="mt-xs text-sm text-muted-foreground">
          只读检查 Node.js、Pi CLI、凭据与扩展包是否就绪。检查过程不写任何文件，也不会安装东西。
        </p>
      </header>

      <Card className="p-lg">
        <div className="flex flex-wrap items-center gap-md">
          <Button onClick={handleRun} disabled={!isDesktop || state.kind === 'running'}>
            {state.kind === 'running' ? '检查中…' : '开始检查'}
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
      </Card>

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
