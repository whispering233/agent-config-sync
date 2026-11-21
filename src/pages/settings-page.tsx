import { useEffect, useState, type ReactNode } from 'react';
import { getVersion } from '@tauri-apps/api/app';
import { CircleAlert, CircleCheck, TriangleAlert } from 'lucide-react';
import { Button } from '../components/ui/button';
import { Card } from '../components/ui/card';
import { Input } from '../components/ui/input';
import {
  AGENT_DIR_ENV_VAR,
  loadAppConfig,
  saveAppConfig,
  validateAppConfig,
  type AgentDirSource,
  type AppConfig,
  type AppConfigView,
  type ConfigField,
  type ConfigValidation,
  type PathStatus,
  type Transport,
  type ValidationFinding,
} from '../lib/app-config';
import {
  isDesktop,
  loadWebdavSummary,
  pickDirectory,
  saveWebdavCredentials,
  testWebdavConnection,
  type WebdavProbe,
  type WebdavSummary,
} from '../lib/desktop';
import { previewPathFindings } from '../lib/path-input';
import { cn } from '../lib/utils';

type SettingsForm = {
  syncRoot: string;
  agentDirOverride: string;
  transport: Transport;
};

type LoadState =
  | { kind: 'loading' }
  | { kind: 'browser' }
  | { kind: 'ready'; view: AppConfigView }
  | { kind: 'failed'; message: string };

const EMPTY_FORM: SettingsForm = {
  syncRoot: '',
  agentDirOverride: '',
  transport: 'local',
};

/** 坚果云是文档里唯一给出预填的服务商；其他家靠用户粘贴地址。 */
const JIANGUOYUN_URL = 'https://dav.jianguoyun.com/dav';

const transportOptions: { value: Transport; label: string; hint: string }[] = [
  {
    value: 'local',
    label: '本地文件夹',
    hint: '把快照写进云盘同步文件夹，由云盘客户端负责送到另一台机器。',
  },
  {
    value: 'webdav',
    label: 'WebDAV',
    hint: '应用自己直连远端搬运快照；凭据只记在本机，不进快照。',
  },
];

const statusText: Record<PathStatus, string> = {
  unset: '未设置',
  ok: '可用',
  invalid: '路径无效',
  missing: '目录不存在',
  not_writable: '不可写',
};

const statusTone: Record<PathStatus, string> = {
  unset: 'text-muted-foreground',
  ok: 'text-success',
  invalid: 'text-danger',
  missing: 'text-warning',
  not_writable: 'text-danger',
};

const sourceText: Record<AgentDirSource, string> = {
  override: '来自自定义的宿主目录',
  env: `来自环境变量 ${AGENT_DIR_ENV_VAR}`,
  default: '默认目录 ~/.pi/agent',
  unresolved: '无法确定，请填写宿主目录',
};

function formFromConfig(config: AppConfig): SettingsForm {
  return {
    syncRoot: config.syncRoot ?? '',
    agentDirOverride: config.agentDirOverride ?? '',
    transport: config.transport,
  };
}

function trimToNull(value: string): string | null {
  const trimmed = value.trim();
  return trimmed === '' ? null : trimmed;
}

function configFromForm(base: AppConfig, form: SettingsForm): AppConfig {
  return {
    version: base.version,
    syncRoot: trimToNull(form.syncRoot),
    agentDirOverride: trimToNull(form.agentDirOverride),
    backupRetention: base.backupRetention,
    transport: form.transport,
  };
}

function isDirty(base: AppConfig, form: SettingsForm): boolean {
  return (
    trimToNull(form.syncRoot) !== (base.syncRoot ?? null) ||
    trimToNull(form.agentDirOverride) !== (base.agentDirOverride ?? null) ||
    form.transport !== base.transport
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** 命令失败时 Rust 侧返回 `{code, message}`，不是 Error 实例。 */
function failureMessage(error: unknown): string {
  if (typeof error === 'object' && error !== null && 'message' in error) {
    return String((error as { message: unknown }).message);
  }
  return errorMessage(error);
}

export function SettingsPage() {
  const [loadState, setLoadState] = useState<LoadState>({ kind: 'loading' });
  const [form, setForm] = useState<SettingsForm>(EMPTY_FORM);
  const [validation, setValidation] = useState<ConfigValidation | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [justSaved, setJustSaved] = useState(false);

  useEffect(() => {
    if (!isDesktop) {
      setLoadState({ kind: 'browser' });
      return;
    }

    let cancelled = false;

    loadAppConfig()
      .then((view) => {
        if (cancelled) {
          return;
        }
        setLoadState({ kind: 'ready', view });
        setForm(formFromConfig(view.config));
        setValidation(view.validation);
      })
      .catch((error: unknown) => {
        if (cancelled) {
          return;
        }
        setLoadState({ kind: 'failed', message: errorMessage(error) });
      });

    return () => {
      cancelled = true;
    };
  }, []);

  const base = loadState.kind === 'ready' ? loadState.view.config : null;
  const dirty = base !== null && isDirty(base, form);

  // 有改动时请求 Rust 侧校验，规则只在核心层实现一次。
  useEffect(() => {
    if (loadState.kind !== 'ready') {
      return;
    }

    if (!dirty) {
      setValidation(loadState.view.validation);
      return;
    }

    const baseConfig = loadState.view.config;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      validateAppConfig(configFromForm(baseConfig, form))
        .then((view) => {
          if (!cancelled) {
            setValidation(view.validation);
          }
        })
        .catch((error: unknown) => {
          if (!cancelled) {
            setSaveError(errorMessage(error));
          }
        });
    }, 250);

    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [dirty, form, loadState]);

  useEffect(() => {
    if (!justSaved) {
      return;
    }
    const timer = window.setTimeout(() => setJustSaved(false), 2500);
    return () => window.clearTimeout(timer);
  }, [justSaved]);

  const findings: ValidationFinding[] =
    loadState.kind === 'browser'
      ? [
          ...previewPathFindings('syncRoot', form.syncRoot),
          ...previewPathFindings('agentDir', form.agentDirOverride),
        ]
      : validation?.findings ?? [];

  const findingsFor = (field: ConfigField) =>
    findings.filter((finding) => finding.field === field);

  const hasErrors = findings.some((finding) => finding.level === 'error');
  const canSave =
    isDesktop &&
    loadState.kind === 'ready' &&
    dirty &&
    !hasErrors &&
    validation?.ok === true &&
    !saving;

  function updateField<K extends keyof SettingsForm>(field: K, value: SettingsForm[K]) {
    setForm((current) => ({ ...current, [field]: value }));
    setSaveError(null);
    setJustSaved(false);
  }

  async function handleBrowse(field: keyof SettingsForm) {
    const title = field === 'syncRoot' ? '选择同步文件夹' : '选择宿主目录';
    try {
      const selected = await pickDirectory(title);
      if (selected !== null) {
        updateField(field, selected);
      }
    } catch (error) {
      setSaveError(errorMessage(error));
    }
  }

  async function handleSave() {
    if (loadState.kind !== 'ready' || !canSave) {
      return;
    }

    setSaving(true);
    setSaveError(null);
    try {
      const view = await saveAppConfig(configFromForm(loadState.view.config, form));
      setLoadState({ kind: 'ready', view });
      setForm(formFromConfig(view.config));
      setValidation(view.validation);
      setJustSaved(true);
    } catch (error) {
      setSaveError(errorMessage(error));
    } finally {
      setSaving(false);
    }
  }

  const activeValidation = loadState.kind === 'ready' ? validation : null;
  const showForm = loadState.kind === 'browser' || loadState.kind === 'ready';

  let statusToneClass = 'text-muted-foreground';
  let statusTextValue = '';

  if (justSaved) {
    statusToneClass = 'text-success';
    statusTextValue = '已保存';
  } else if (saveError) {
    statusToneClass = 'text-danger';
    statusTextValue = saveError;
  } else if (!isDesktop) {
    statusTextValue = '保存仅在桌面版可用';
  } else if (hasErrors) {
    statusToneClass = 'text-danger';
    statusTextValue = '校验未通过，先修正路径再保存';
  } else if (base !== null && !dirty) {
    statusTextValue = '没有改动';
  } else if (dirty) {
    statusTextValue = '有未保存的改动';
  }

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-lg">
      <header>
        <h1 className="text-lg font-semibold text-foreground">设置</h1>
        <p className="mt-xs text-sm text-muted-foreground">
          选择快照的传输方式与同步文件夹，并确认 Pi 的宿主目录。配置与 WebDAV 凭据只保存在本机，不会进入快照。
        </p>
      </header>

      {loadState.kind === 'loading' && (
        <Card className="p-lg">
          <p className="text-sm text-muted-foreground">正在读取配置…</p>
        </Card>
      )}

      {loadState.kind === 'failed' && (
        <Card className="p-lg">
          <p className="text-sm text-danger">{loadState.message}</p>
        </Card>
      )}

      {showForm && (
        <>
          <Card className="p-lg">
            <h2 className="text-sm font-semibold text-foreground">位置</h2>
            <div className="mt-lg flex flex-col gap-lg">
              <div>
                <span id="transport-label" className="text-sm font-medium text-foreground">
                  传输方式
                </span>
                <div
                  className="mt-sm flex flex-wrap gap-sm"
                  role="group"
                  aria-labelledby="transport-label"
                >
                  {transportOptions.map((option) => (
                    <Button
                      key={option.value}
                      variant={form.transport === option.value ? 'primary' : 'secondary'}
                      aria-pressed={form.transport === option.value}
                      onClick={() => updateField('transport', option.value)}
                    >
                      {option.label}
                    </Button>
                  ))}
                </div>
                <p className="mt-xs text-xs text-muted-foreground">
                  {
                    transportOptions.find((option) => option.value === form.transport)
                      ?.hint
                  }
                </p>
              </div>

              <Field
                id="sync-root"
                label="同步文件夹"
                hint="快照与 manifest 保存在这里；留空表示还没有选择。"
              >
                <div className="mt-sm flex gap-sm">
                  <Input
                    id="sync-root"
                    value={form.syncRoot}
                    onChange={(event) => updateField('syncRoot', event.target.value)}
                    placeholder="输入云盘同步文件夹的路径"
                    spellCheck={false}
                    aria-invalid={findingsFor('syncRoot').some(
                      (finding) => finding.level === 'error',
                    )}
                    aria-describedby={
                      findingsFor('syncRoot').length > 0 ? 'sync-root-feedback' : undefined
                    }
                  />
                  <BrowseButton onBrowse={() => handleBrowse('syncRoot')} />
                </div>
                <FieldFeedback id="sync-root-feedback" findings={findingsFor('syncRoot')} />
              </Field>

              <Field
                id="agent-dir"
                label="宿主目录覆盖"
                hint="留空则按环境变量与 ~/.pi/agent 决定；只有目录不常见时才需要填写。"
              >
                <div className="mt-sm flex gap-sm">
                  <Input
                    id="agent-dir"
                    value={form.agentDirOverride}
                    onChange={(event) =>
                      updateField('agentDirOverride', event.target.value)
                    }
                    placeholder="留空使用默认宿主目录"
                    spellCheck={false}
                    aria-invalid={findingsFor('agentDir').some(
                      (finding) => finding.level === 'error',
                    )}
                    aria-describedby={
                      findingsFor('agentDir').length > 0 ? 'agent-dir-feedback' : undefined
                    }
                  />
                  <BrowseButton onBrowse={() => handleBrowse('agentDirOverride')} />
                </div>
                <FieldFeedback id="agent-dir-feedback" findings={findingsFor('agentDir')} />
              </Field>

              {!isDesktop && (
                <p className="text-xs text-muted-foreground">
                  “浏览…”仅桌面版可用；浏览器预览里只能手动输入路径。
                </p>
              )}
            </div>
          </Card>

          {form.transport === 'webdav' && <WebdavCard />}

          <Card className="p-lg">
            <h2 className="text-sm font-semibold text-foreground">生效路径</h2>
            <dl className="mt-md flex flex-col gap-sm">
              <PathRow
                label="同步文件夹"
                value={trimToNull(form.syncRoot)}
                status={activeValidation?.syncRootStatus}
              />
              <PathRow
                label="宿主目录"
                value={activeValidation?.effectiveAgentDir ?? null}
                status={activeValidation?.agentDirStatus}
                note={
                  activeValidation ? sourceText[activeValidation.agentDirSource] : undefined
                }
              />
              <PathRow
                label="配置文件"
                value={loadState.kind === 'ready' ? loadState.view.configFilePath : null}
                note={loadState.kind === 'browser' ? '浏览器预览不读取本机配置' : undefined}
              />
            </dl>
          </Card>

          <Card className="p-lg">
            <h2 className="text-sm font-semibold text-foreground">校验结果</h2>
            <div className="mt-md flex flex-col gap-sm" aria-live="polite">
              {findings.length === 0 &&
                (loadState.kind === 'browser' ? (
                  <p className="text-sm text-muted-foreground">
                    浏览器预览只检查路径格式；目录存在性、可写性与目录关系由桌面版校验。
                  </p>
                ) : (
                  <p className="flex items-center gap-sm text-sm text-success">
                    <CircleCheck className="h-4 w-4" aria-hidden="true" />
                    配置有效
                  </p>
                ))}
              {findings.map((finding, index) => (
                <FindingRow key={`${finding.code}-${index}`} finding={finding} />
              ))}
            </div>
          </Card>

          <div className="flex flex-wrap items-center gap-md">
            <Button onClick={handleSave} disabled={!canSave}>
              {saving ? '保存中…' : '保存'}
            </Button>
            {statusTextValue !== '' && (
              <span className={cn('text-sm', statusToneClass)}>{statusTextValue}</span>
            )}
          </div>
        </>
      )}

      <AboutCard />
    </div>
  );
}

type VersionState =
  | { kind: 'browser' }
  | { kind: 'loading' }
  | { kind: 'ready'; version: string }
  | { kind: 'failed' };

/** 版本来自打包时写入的元数据；浏览器预览里没有壳可问。 */
function AboutCard() {
  const [state, setState] = useState<VersionState>(
    isDesktop ? { kind: 'loading' } : { kind: 'browser' },
  );

  useEffect(() => {
    if (!isDesktop) {
      return;
    }
    let cancelled = false;
    getVersion()
      .then((version) => {
        if (!cancelled) {
          setState({ kind: 'ready', version });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setState({ kind: 'failed' });
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const versionText =
    state.kind === 'ready'
      ? state.version
      : state.kind === 'loading'
        ? '读取中…'
        : state.kind === 'failed'
          ? '无法读取'
          : '仅桌面版';

  return (
    <Card className="p-lg">
      <h2 className="text-sm font-semibold text-foreground">关于</h2>
      <dl className="mt-md flex flex-col gap-sm">
        <PathRow label="应用" value="Agent Config Sync" />
        <PathRow label="版本" value={versionText} />
        <PathRow
          label="运行时"
          value={isDesktop ? 'Tauri 2 + React 18' : '浏览器预览（无桌面壳）'}
        />
      </dl>
      <p className="mt-md text-xs text-muted-foreground">
        桌面版使用系统 WebView（Linux 为 WebKitGTK 4.1），缺失时应用无法启动。
      </p>
    </Card>
  );
}

type FieldProps = {
  id: string;
  label: string;
  hint: string;
  children: ReactNode;
};

function Field({ id, label, hint, children }: FieldProps) {
  return (
    <div>
      <label htmlFor={id} className="text-sm font-medium text-foreground">
        {label}
      </label>
      {children}
      <p className="mt-xs text-xs text-muted-foreground">{hint}</p>
    </div>
  );
}

function BrowseButton({ onBrowse }: { onBrowse: () => void }) {
  return (
    <Button
      variant="secondary"
      className="shrink-0"
      onClick={onBrowse}
      disabled={!isDesktop}
      title={isDesktop ? '选择文件夹' : '仅桌面版可用'}
    >
      浏览…
    </Button>
  );
}

type PathRowProps = {
  label: string;
  value: string | null;
  status?: PathStatus;
  note?: string;
};

function PathRow({ label, value, status, note }: PathRowProps) {
  return (
    <div className="rounded-md bg-surface-muted px-md py-sm">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="flex flex-wrap items-center gap-sm">
        <span className="break-all font-mono text-sm text-foreground">
          {value ?? '—'}
        </span>
        {status !== undefined && (
          <span className={cn('text-xs', statusTone[status])}>{statusText[status]}</span>
        )}
      </dd>
      {note !== undefined && (
        <dd className="mt-xs text-xs text-muted-foreground">{note}</dd>
      )}
    </div>
  );
}

function FieldFeedback({
  id,
  findings,
}: {
  id: string;
  findings: ValidationFinding[];
}) {
  if (findings.length === 0) {
    return null;
  }

  return (
    <div id={id} className="mt-xs flex flex-col gap-xs">
      {findings.map((finding, index) => (
        <p
          key={`${finding.code}-${index}`}
          className={cn(
            'flex items-start gap-xs text-xs',
            finding.level === 'error' ? 'text-danger' : 'text-warning',
          )}
        >
          <span aria-hidden="true">•</span>
          <span>{finding.message}</span>
        </p>
      ))}
    </div>
  );
}

function FindingRow({ finding }: { finding: ValidationFinding }) {
  const isError = finding.level === 'error';
  const Icon = isError ? CircleAlert : TriangleAlert;

  return (
    <p
      className={cn(
        'flex items-start gap-sm text-sm',
        isError ? 'text-danger' : 'text-warning',
      )}
    >
      <Icon className="mt-xs h-4 w-4 shrink-0" aria-hidden="true" />
      <span>{finding.message}</span>
    </p>
  );
}

type WebdavForm = {
  url: string;
  username: string;
  password: string;
};

const EMPTY_WEBDAV_FORM: WebdavForm = { url: '', username: '', password: '' };

/** 探测结果的一句话；`warn` 用来把「能连、删不掉」与真正失败分开。 */
type ProbeNote = { tone: 'ok' | 'warn' | 'error'; text: string };

function probeNote(probe: WebdavProbe): ProbeNote {
  if (probe.leftoverWriteTestFile) {
    return {
      tone: 'warn',
      text: '连接正常，但测试文件 .tmp-writetest 删不掉（账号可能只有读权限），请到网盘里手动删除',
    };
  }
  if (probe.created) {
    return { tone: 'ok', text: '连接正常，已创建 PiAgentSync 目录' };
  }
  return { tone: 'ok', text: '连接正常' };
}

/**
 * 凭据单独保存：它不参与配置校验，也不进快照。
 * 密码永远是空白输入框，只在输入非空时覆盖已保存的值。
 */
function WebdavCard() {
  const [summary, setSummary] = useState<WebdavSummary | null>(null);
  const [form, setForm] = useState<WebdavForm>(EMPTY_WEBDAV_FORM);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [probe, setProbe] = useState<ProbeNote | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    if (!isDesktop) {
      return;
    }

    let cancelled = false;
    loadWebdavSummary()
      .then((value) => {
        if (cancelled) {
          return;
        }
        setSummary(value);
        setForm({ url: value.url, username: value.username, password: '' });
      })
      .catch((failure: unknown) => {
        if (!cancelled) {
          setError(failureMessage(failure));
        }
      });

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!saved) {
      return;
    }
    const timer = window.setTimeout(() => setSaved(false), 2500);
    return () => window.clearTimeout(timer);
  }, [saved]);

  function update(field: keyof WebdavForm, value: string) {
    setForm((current) => ({ ...current, [field]: value }));
    setError(null);
    setProbe(null);
    setSaved(false);
  }

  const canSave =
    isDesktop &&
    !saving &&
    !testing &&
    form.url.trim() !== '' &&
    form.username.trim() !== '';

  // 测试与保存前置条件相同，而且同一时刻只跑一个，所以两个按钮共用一套判断。
  const canTest = canSave;

  async function handleSave() {
    if (!canSave) {
      return;
    }

    setSaving(true);
    setError(null);
    try {
      const next = await saveWebdavCredentials({
        url: form.url,
        username: form.username,
        // 空串代表"不修改"，命令层按缺省处理。
        password: form.password === '' ? undefined : form.password,
      });
      setSummary(next);
      setForm({ url: next.url, username: next.username, password: '' });
      setSaved(true);
    } catch (failure) {
      setError(failureMessage(failure));
    } finally {
      setSaving(false);
    }
  }

  async function handleTest() {
    if (!canTest) {
      return;
    }

    setTesting(true);
    setError(null);
    setProbe(null);
    try {
      const result = await testWebdavConnection({
        url: form.url,
        username: form.username,
        // 空串代表"用已保存的密码"：后端按同一语义取值，不必先保存再测。
        password: form.password === '' ? undefined : form.password,
      });
      setProbe(probeNote(result));
    } catch (failure) {
      setProbe({ tone: 'error', text: failureMessage(failure) });
    } finally {
      setTesting(false);
    }
  }

  return (
    <Card className="p-lg">
      <h2 className="text-sm font-semibold text-foreground">WebDAV 凭据</h2>
      <p className="mt-xs text-xs text-muted-foreground">
        保存在本机应用数据目录（仅本用户可读），不进入快照、备份、日志与同步文件夹。
        明文落盘，建议使用可单独吊销的应用密码，而不是账号登录密码。
      </p>

      {!isDesktop && (
        <p className="mt-md text-sm text-muted-foreground">
          浏览器预览不读写本机凭据；桌面版才能测试与保存。
        </p>
      )}

      <div className="mt-lg flex flex-col gap-lg">
        <Field
          id="webdav-url"
          label="服务地址"
          hint="只接受 http:// 或 https://；应用会在该地址下建 PiAgentSync/ 目录。"
        >
          <div className="mt-sm flex gap-sm">
            <Input
              id="webdav-url"
              value={form.url}
              onChange={(event) => update('url', event.target.value)}
              placeholder="https://dav.example.com/dav"
              spellCheck={false}
              autoComplete="off"
            />
            <Button
              variant="secondary"
              className="shrink-0"
              onClick={() => update('url', JIANGUOYUN_URL)}
            >
              坚果云
            </Button>
          </div>
        </Field>

        <Field
          id="webdav-username"
          label="用户名"
          hint="按服务商要求填写；坚果云填注册邮箱。"
        >
          <Input
            id="webdav-username"
            className="mt-sm"
            value={form.username}
            onChange={(event) => update('username', event.target.value)}
            placeholder="you@example.com"
            spellCheck={false}
            autoComplete="off"
          />
        </Field>

        <Field
          id="webdav-password"
          label="应用密码"
          hint="留空表示保留已保存的密码；修改看不到原值。"
        >
          <Input
            id="webdav-password"
            type="password"
            className="mt-sm"
            value={form.password}
            onChange={(event) => update('password', event.target.value)}
            placeholder={
              summary?.hasPassword
                ? '已保存，留空则不修改'
                : '应用密码（不是账号登录密码）'
            }
            autoComplete="new-password"
          />
        </Field>
      </div>

      <div className="mt-lg flex flex-wrap items-center gap-md">
        <Button onClick={handleSave} disabled={!canSave}>
          {saving ? '保存中…' : '保存凭据'}
        </Button>
        <Button variant="secondary" onClick={handleTest} disabled={!canTest}>
          {testing ? '测试中…' : '测试连接'}
        </Button>
        {saved && <span className="text-sm text-success">已保存</span>}
        {probe !== null && (
          <span
            className={cn(
              'text-sm',
              probe.tone === 'ok' && 'text-success',
              probe.tone === 'warn' && 'text-warning',
              probe.tone === 'error' && 'text-danger',
            )}
          >
            {probe.text}
          </span>
        )}
        {error !== null && <span className="text-sm text-danger">{error}</span>}
      </div>
    </Card>
  );
}
