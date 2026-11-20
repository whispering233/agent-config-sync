import type { LucideIcon } from 'lucide-react';
import { Button } from './ui/button';
import { Card } from './ui/card';

type EmptyStateProps = {
  icon: LucideIcon;
  title: string;
  description: string;
  actionLabel: string;
};

export function EmptyState({
  icon: Icon,
  title,
  description,
  actionLabel,
}: EmptyStateProps) {
  return (
    <div className="flex min-h-full items-center justify-center">
      <Card className="w-full max-w-2xl px-xl py-2xl text-center">
        <div className="mx-auto flex h-12 w-12 items-center justify-center rounded-lg bg-surface-muted text-accent">
          <Icon className="h-6 w-6" aria-hidden="true" />
        </div>
        <h1 className="mt-lg text-lg font-semibold text-foreground">{title}</h1>
        <p className="mx-auto mt-sm max-w-md text-sm leading-relaxed text-muted-foreground">
          {description}
        </p>
        <div className="mt-xl">
          <Button disabled>{actionLabel}</Button>
        </div>
      </Card>
    </div>
  );
}
