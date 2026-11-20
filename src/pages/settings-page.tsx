import { FolderCog } from 'lucide-react';
import { EmptyState } from '../components/empty-state';

export function SettingsPage() {
  return (
    <EmptyState
      icon={FolderCog}
      title="还没有选择同步文件夹"
      description="指定云盘里的同步文件夹，快照和校验信息都会保存在那里，之后可以随时更换。"
      actionLabel="选择文件夹"
    />
  );
}
