import path, { posix, win32 } from 'node:path';

/** Lexical containment only; callers retain their own filesystem/permission checks.
 * Inputs must already be absolute, so neither cwd nor environment supplies a root.
 * POSIX treats backslashes literally. Windows accepts both separators, drive-absolute
 * and UNC paths, with Node's case-insensitive semantics; drive-relative and current-
 * drive-rooted paths are rejected because their meaning depends on process state.
 * Windows device namespace paths are outside this contract.
 */
export function insideTaskArtifacts(root: string, candidate: string, {
  allowRoot = false,
  flavor,
}: { allowRoot?: boolean; flavor?: 'posix' | 'win32' } = {}): boolean {
  const paths = flavor === 'posix' ? posix : flavor === 'win32' ? win32 : path;
  const absolute = (value: string) => paths.isAbsolute(value) && (paths !== win32
    || !/^[\\/]{2}[?.][\\/]/.test(value)
      && (/^[a-z]:[\\/]/i.test(value) || /^[\\/]{2}[^\\/]+[\\/][^\\/]+(?:[\\/]|$)/.test(value)));
  if (!absolute(root) || !absolute(candidate)) return false;
  const suffix = paths.relative(root, candidate);
  return suffix === '' ? allowRoot : !paths.isAbsolute(suffix) && suffix.split(paths.sep)[0] !== '..';
}
