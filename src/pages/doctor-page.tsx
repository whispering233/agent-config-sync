import { Stethoscope } from 'lucide-react';
import { EmptyState } from '../components/empty-state';

export function DoctorPage() {
  return (
    <EmptyState
      icon={Stethoscope}
      title="还没有检查这台电脑"
      description="检查 Node.js、Pi CLI 与登录凭据是否就绪，确认这台电脑能正常运行 Pi。"
      actionLabel="开始检查"
    />
  );
}
