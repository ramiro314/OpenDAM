import type { FolderRef, Library } from '../types';

/** A folder path relative to a library root, without padding or edge slashes. The root is `''`. */
export function normalizeFolderPath(path: string): string {
  return path.trim().replace(/^\/+|\/+$/g, '');
}

/**
 * The library folder at absolute path `picked`: the library whose root holds it (the innermost
 * one, when roots nest) and its path relative to that root. Null when no library holds it.
 */
export function folderInLibraries(
  picked: string,
  libraries: Pick<Library, 'id' | 'rootPaths'>[]
): FolderRef | null {
  const target = picked.replace(/\/+$/, '');
  let best: { ref: FolderRef; rootLength: number } | null = null;
  for (const library of libraries) {
    for (const rootPath of library.rootPaths) {
      const root = rootPath.replace(/\/+$/, '');
      if (target !== root && !target.startsWith(`${root}/`)) continue;
      if (best && best.rootLength >= root.length) continue;
      best = {
        ref: { libraryId: library.id, path: normalizeFolderPath(target.slice(root.length)) },
        rootLength: root.length,
      };
    }
  }
  return best?.ref ?? null;
}
