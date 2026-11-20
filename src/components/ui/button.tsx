import type { ButtonHTMLAttributes } from 'react';
import { cn } from '../../lib/utils';

const variants = {
  primary: 'enabled:hover:bg-accent-hover bg-accent text-accent-foreground',
  secondary:
    'enabled:hover:bg-surface-muted border border-border bg-surface text-foreground',
  ghost:
    'enabled:hover:bg-surface-muted enabled:hover:text-foreground text-muted-foreground',
} as const;

export type ButtonVariant = keyof typeof variants;

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: ButtonVariant;
};

export function Button({
  className,
  variant = 'primary',
  type = 'button',
  ...props
}: ButtonProps) {
  return (
    <button
      type={type}
      className={cn(
        'inline-flex items-center justify-center gap-sm rounded-md px-md py-sm text-sm font-medium transition-colors',
        'focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent',
        'disabled:cursor-not-allowed disabled:opacity-50',
        variants[variant],
        className,
      )}
      {...props}
    />
  );
}
