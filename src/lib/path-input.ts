import type { ConfigField, ValidationFinding } from './app-config';

const WINDOWS_ABSOLUTE = /^[a-zA-Z]:[\\/]/;
const UNC_ABSOLUTE = /^\\\\/;

function isAbsolutePath(value: string): boolean {
  return (
    value.startsWith('/') || WINDOWS_ABSOLUTE.test(value) || UNC_ABSOLUTE.test(value)
  );
}

/**
 * 浏览器预览用的最小检查：只覆盖不依赖文件系统的两条规则（绝对路径、不含 `..`）。
 *
 * 桌面版一律以 Rust 侧 `validate` 的结构化结果为准；这里存在的原因只是普通
 * 浏览器里没有 command 可调用，而设置页仍要能提示明显写错的路径。
 */
export function previewPathFindings(
  field: ConfigField,
  value: string,
): ValidationFinding[] {
  const trimmed = value.trim();
  if (trimmed === '') {
    return [];
  }

  const findings: ValidationFinding[] = [];

  if (!isAbsolutePath(trimmed)) {
    findings.push({
      level: 'error',
      field,
      code: 'path_not_absolute',
      message:
        field === 'syncRoot'
          ? '同步文件夹必须是绝对路径，可用“浏览…”选择'
          : '宿主目录必须是绝对路径，可用“浏览…”选择',
    });
  }

  if (trimmed.split(/[\\/]+/).includes('..')) {
    findings.push({
      level: 'error',
      field,
      code: 'path_has_parent_component',
      message: '路径不能包含 .. 片段，请填写规范化的绝对路径',
    });
  }

  return findings;
}
