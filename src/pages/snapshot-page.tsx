import { Archive } from 'lucide-react';
import { EmptyState } from '../components/empty-state';

export function SnapshotPage() {
  return (
    <EmptyState
      icon={Archive}
      title="还没有导出过快照"
      description="把当前的 Pi 配置整理成一份可校验的快照，存进云盘同步文件夹，随时带到另一台电脑。"
      actionLabel="导出快照"
    />
  );
}
