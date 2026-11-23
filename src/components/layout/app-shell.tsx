import type { ReactNode } from 'react';
import type { LucideIcon } from 'lucide-react';
import { cn } from '../../lib/utils';

export type NavItem<Id extends string> = {
  id: Id;
  label: string;
  icon: LucideIcon;
};

type AppShellProps<Id extends string> = {
  items: ReadonlyArray<NavItem<Id>>;
  activeId: Id;
  onNavigate: (id: Id) => void;
  /** 顶部非模态提示条：没有提示时调用方传 null，不占位。 */
  banner?: ReactNode;
  children: ReactNode;
};

export function AppShell<Id extends string>({
  items,
  activeId,
  onNavigate,
  banner,
  children,
}: AppShellProps<Id>) {
  return (
    <div className="flex h-full min-h-screen flex-col bg-background">
      {banner}
      <div className="flex min-h-0 flex-1">
        <aside className="flex w-sidebar shrink-0 flex-col border-r border-border bg-surface px-md py-lg">
          <div className="px-sm">
            <p className="text-sm font-semibold text-foreground">
              Agent Config Sync
            </p>
            <p className="mt-xs text-xs text-muted-foreground">
              Pi 配置的快照与落地
            </p>
          </div>

          <nav className="mt-xl flex flex-col gap-xs" aria-label="主导航">
            {items.map((item) => {
              const Icon = item.icon;
              const isActive = item.id === activeId;

              return (
                <button
                  key={item.id}
                  type="button"
                  onClick={() => onNavigate(item.id)}
                  aria-current={isActive ? 'page' : undefined}
                  className={cn(
                    'flex w-full items-center gap-sm rounded-md px-md py-sm text-left text-sm transition-colors',
                    'focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent',
                    isActive
                      ? 'bg-nav-active font-medium text-nav-active-foreground'
                      : 'text-muted-foreground enabled:hover:bg-surface-muted enabled:hover:text-foreground',
                  )}
                >
                  <Icon className="h-4 w-4" aria-hidden="true" />
                  {item.label}
                </button>
              );
            })}
          </nav>
        </aside>

        <main className="flex-1 overflow-y-auto px-2xl py-2xl">{children}</main>
      </div>
    </div>
  );
}
