import { useState } from 'react';
import { Archive, Settings, Stethoscope } from 'lucide-react';
import { AppShell, type NavItem } from './components/layout/app-shell';
import { DoctorPage } from './pages/doctor-page';
import { SnapshotPage } from './pages/snapshot-page';
import { SettingsPage } from './pages/settings-page';

type PageId = 'doctor' | 'snapshot' | 'settings';

const navItems: ReadonlyArray<NavItem<PageId>> = [
  { id: 'doctor', label: '医生', icon: Stethoscope },
  { id: 'snapshot', label: '快照', icon: Archive },
  { id: 'settings', label: '设置', icon: Settings },
];

export function App() {
  const [activePage, setActivePage] = useState<PageId>('doctor');

  return (
    <AppShell
      items={navItems}
      activeId={activePage}
      onNavigate={setActivePage}
    >
      {activePage === 'doctor' && <DoctorPage />}
      {activePage === 'snapshot' && <SnapshotPage />}
      {activePage === 'settings' && <SettingsPage />}
    </AppShell>
  );
}
