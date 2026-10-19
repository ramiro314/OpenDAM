import { create } from 'zustand';
import { api } from '../api';
import type { FolderNode } from '../types';

/** Children by parent path; `''` is the library root */
type LoadedNodes = Record<string, FolderNode[]>;

interface FolderTreeStore {
  /** The children of each loaded node, per library id */
  children: Record<string, LoadedNodes>;
  /** The expanded nodes per library id, kept for the session; `''` is the library row itself */
  expanded: Record<string, string[]>;
  /** Expands a node, loading its children the first time, or collapses it */
  toggle: (libraryId: string, path: string) => Promise<void>;
  /**
   * Reloads every expanded node's children, for `library:updated`. Collapsed nodes' children are
   * dropped, so they load fresh when expanded, and expanded nodes whose folder is gone collapse.
   * When refetches overlap, only the latest one's results are applied.
   */
  refetch: () => Promise<void>;
}

/** The folder holding `path`; `''` for a top-level folder. */
function parentOf(path: string): string {
  const slash = path.lastIndexOf('/');
  return slash < 0 ? '' : path.slice(0, slash);
}

/** Whether `path` is the root or a listed child of an expanded node that is itself shown. */
function isShown(nodes: LoadedNodes, expanded: Set<string>, path: string): boolean {
  if (path === '') return true;
  const parent = parentOf(path);
  return (
    expanded.has(parent) &&
    (nodes[parent]?.some((n) => n.path === path) ?? false) &&
    isShown(nodes, expanded, parent)
  );
}

/**
 * The expanded nodes of `libraryId` whose children the tree shows: every ancestor up to the
 * library row (`''`) is expanded too, and each node is listed under its parent. A collapse keeps
 * the expanded nodes below it, which show again when it reopens.
 */
export function openNodes(
  s: Pick<FolderTreeStore, 'children' | 'expanded'>,
  libraryId: string
): string[] {
  const paths = s.expanded[libraryId] ?? [];
  const open = new Set(paths);
  const nodes = s.children[libraryId] ?? {};
  return paths.filter((p) => isShown(nodes, open, p));
}

async function load(libraryId: string, path: string): Promise<FolderNode[] | null> {
  try {
    return await api.getFolders(libraryId, path);
  } catch (e) {
    console.error(`Failed to load the folders in ${libraryId}/${path}:`, e);
    return null;
  }
}

// Bumped by every refetch: only the latest one's results are applied, so an earlier one that
// resolves last, with folders read before a later change, can't overwrite them.
let refetchGeneration = 0;

export const useFolderTreeStore = create<FolderTreeStore>()((set, get) => ({
  children: {},
  expanded: {},

  toggle: async (libraryId, path) => {
    const open = get().expanded[libraryId] ?? [];
    if (open.includes(path)) {
      set((s) => ({
        expanded: { ...s.expanded, [libraryId]: open.filter((p) => p !== path) },
      }));
      return;
    }
    set((s) => ({ expanded: { ...s.expanded, [libraryId]: [...open, path] } }));
    if (get().children[libraryId]?.[path]) return;
    const nodes = await load(libraryId, path);
    if (!nodes) return;
    set((s) => ({
      children: { ...s.children, [libraryId]: { ...s.children[libraryId], [path]: nodes } },
    }));
  },

  refetch: async () => {
    const generation = ++refetchGeneration;
    const jobs = Object.entries(get().expanded).flatMap(([libraryId, paths]) =>
      paths.map((path) => ({ libraryId, path }))
    );
    const results = await Promise.all(
      jobs.map(async (job) => ({ ...job, nodes: await load(job.libraryId, job.path) }))
    );
    if (generation !== refetchGeneration) return;
    set((s) => {
      const children: Record<string, LoadedNodes> = {};
      const expanded: Record<string, string[]> = {};
      for (const [libraryId, paths] of Object.entries(s.expanded)) {
        // A node expanded while this ran keeps what it loaded, or is still loading
        const nodes: LoadedNodes = {};
        const failed = new Set<string>();
        for (const path of paths) {
          const loaded = s.children[libraryId]?.[path];
          if (loaded) nodes[path] = loaded;
        }
        for (const r of results) {
          if (r.libraryId !== libraryId || !paths.includes(r.path)) continue;
          if (r.nodes) nodes[r.path] = r.nodes;
          else failed.add(r.path);
        }
        const open = new Set(paths.filter((p) => !failed.has(p)));
        const shown = paths.filter((p) => open.has(p) && isShown(nodes, open, p));
        expanded[libraryId] = shown;
        children[libraryId] = Object.fromEntries(
          shown.filter((p) => nodes[p]).map((p) => [p, nodes[p]])
        );
      }
      return { children, expanded };
    });
  },
}));
