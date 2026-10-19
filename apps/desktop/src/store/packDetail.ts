import { create } from 'zustand';
import { api } from '../api';
import type { FolderRef, PackDetail } from '../types';

/**
 * The detail panel's pack (plan C18): the pack holding the folder the panel asks about, which is
 * a selected pack card's own folder or the selected tree node (`panelPackTarget`).
 */
interface PackDetailStore {
  /** The folder asked about, or `null` */
  target: FolderRef | null;
  /** The pack holding it; `null` when no pack does, or until it loads */
  detail: PackDetail | null;
  /**
   * Loads the pack holding `target`, unless it is the folder already asked about. When loads
   * overlap, only the latest one's reply applies.
   */
  load: (target: FolderRef | null) => Promise<void>;
  /** Loads the current folder's pack again, for `library:updated` */
  refetch: () => Promise<void>;
}

let latestLoad = 0;

const sameFolder = (a: FolderRef | null, b: FolderRef | null) =>
  a?.libraryId === b?.libraryId && a?.path === b?.path;

async function fetchDetail(target: FolderRef | null) {
  const load = ++latestLoad;
  if (!target) return;
  let detail: PackDetail | null;
  try {
    detail = await api.getPackDetail(target.libraryId, target.path);
  } catch (err) {
    console.error('Failed to load the pack:', err);
    detail = null;
  }
  if (load === latestLoad) usePackDetailStore.setState({ detail });
}

export const usePackDetailStore = create<PackDetailStore>()((set, get) => ({
  target: null,
  detail: null,

  load: async (target) => {
    if (sameFolder(target, get().target)) return;
    set({ target, detail: null });
    await fetchDetail(target);
  },

  refetch: () => fetchDetail(get().target),
}));
