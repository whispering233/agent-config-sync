import { useEffect, useState } from 'react';
import { Archive, CircleArrowUp, Settings, Stethoscope } from 'lucide-react';
import { AppShell, type NavItem } from './components/layout/app-shell';
import { Button } from './components/ui/button';
import {
  dismissAppUpdate,
  startAppUpdate,
  useAppUpdate,
} from './lib/app-update';
import { DoctorPage } from './pages/doctor-page';
import { SnapshotPage } from './pages/snapshot-page';
import { SettingsPage } from './pages/settings-page';

type PageId = 'doctor' | 'snapshot' | 'settings';

const navItems: ReadonlyArray<NavItem<PageId>> = [
  { id: 'doctor', label: '环境检测', icon: Stethoscope },
  { id: 'snapshot', label: '快照', icon: Archive },
  { id: 'settings', label: '设置', icon: Settings },
];

export function App() {
  const [activePage, setActivePage] = useState<PageId>('doctor');

  // 启动检查只发一个请求，结果与设置页共用；开发态与浏览器里什么都不做。
  useEffect(startAppUpdate, []);

  return (
    <AppShell
      items={navItems}
      activeId={activePage}
      onNavigate={setActivePage}
      banner={<AppUpdateBanner onOpen={() => setActivePage('settings')} />}
    >
      {activePage === 'doctor' && <DoctorPage />}
      {activePage === 'snapshot' && <SnapshotPage />}
      {activePage === 'settings' && <SettingsPage />}
    </AppShell>
  );
}

/** 发现新版本时的一条非模态提示：没有更新、已选稍后或正在安装都不占位。 */
function AppUpdateBanner({ onOpen }: { onOpen: () => void }) {
  const update = useAppUpdate();

  if (update.available === null || update.dismissed || update.installing) {
    return null;
  }

  return (
    <div className="flex flex-wrap items-center gap-md border-b border-border bg-surface-muted px-2xl py-sm text-sm">
      <CircleArrowUp className="h-4 w-4 text-accent" aria-hidden="true" />
      <span className="text-foreground">发现新版本 {update.available.version}</span>
      <span className="text-xs text-muted-foreground">
        当前 {update.available.currentVersion}
      </span>
      <div className="ml-auto flex items-center gap-sm">
        <Button variant="secondary" onClick={onOpen}>
          查看
        </Button>
        <Button variant="ghost" onClick={dismissAppUpdate}>
          稍后
        </Button>
      </div>
    </div>
  );
}
