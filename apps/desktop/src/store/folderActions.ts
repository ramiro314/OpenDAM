import { create } from 'zustand';
import { api } from '../api';
import type { FolderRef } from '../types';
import { useAssetStore, useCollectionStore, useLibraryStore, useTagStore } from '.';

/** A tree node or pack card a folder action acts on; the library root is `path: ''` */
export interface FolderTarget extends FolderRef {
  /** Shown in the dialog's title */
  name: string;
}

export type FolderBulkAction = 'tag' | 'collection';

interface FolderActionStore {
  /** The open "Add a tag to everything" or "Add everything to a collection" dialog */
  dialog: { target: FolderTarget; action: FolderBulkAction } | null;
  openDialog: (target: FolderTarget, action: FolderBulkAction) => void;
  closeDialog: () => void;
}

export const useFolderActionStore = create<FolderActionStore>()((set) => ({
  dialog: null,
  openDialog: (target, action) => set({ dialog: { target, action } }),
  closeDialog: () => set({ dialog: null }),
}));

/**
 * Writes tag `id` or collection `id` to every file below `target` and every member of their
 * groups (plan C22), then refetches what shows the result: the grid, which also recounts the
 * live counts, and the collection counts. It refetches after a failed write too, since the
 * batches before the failure stay written. Returns how many files were written.
 */
export async function applyFolderAction(
  target: FolderRef,
  action: FolderBulkAction,
  id: string
): Promise<number> {
  try {
    return action === 'tag'
      ? await api.tagFolder(target.libraryId, target.path, id)
      : await api.collectFolder(target.libraryId, target.path, id);
  } finally {
    await Promise.all([
      useAssetStore.getState().refresh(),
      useCollectionStore.getState().fetchCollections(),
      useTagStore.getState().fetchTags(),
    ]).catch((err) => console.error('Failed to refetch after a folder action:', err));
  }
}

/** `path` below a library root, which may end with a slash; the root itself for `''`. */
export function folderAbsolutePath(root: string, path: string): string {
  return path === '' ? root : `${root.replace(/\/+$/, '')}/${path}`;
}

/** Shows `target` in the file manager, through the same opener as the grid's assets. */
export async function revealFolder(target: FolderRef): Promise<void> {
  const library = useLibraryStore.getState().libraries.find((l) => l.id === target.libraryId);
  const root = library?.rootPaths[0];
  if (!root) throw new Error(`Library ${target.libraryId} is not open`);
  await api.revealInFileManager(folderAbsolutePath(root, target.path));
}
