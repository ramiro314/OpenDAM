import { create } from 'zustand';
import { api } from '../api';
import type { FolderRef, PackCard } from '../types';
import { scopeKey, type BrowseMode, type PacksScope } from '../utils/packsView';

interface PacksStore {
  /** The toolbar's Packs/Assets toggle, kept for the session; it starts on Packs */
  mode: BrowseMode;
  /** The scope `packs` were loaded for, as `scopeKey`, or null before the first load */
  key: string | null;
  scope: PacksScope | null;
  /** The packs below the scope's node; null until they load */
  packs: PackCard[] | null;
  /** The selected card, as `packKey` */
  selected: string | null;
  setMode: (mode: BrowseMode) => void;
  /** Loads the packs of `scope`, unless they are loaded or loading */
  load: (scope: PacksScope | null) => Promise<void>;
  /** Reloads the current scope's packs, for `library:updated` */
  refetch: () => Promise<void>;
  select: (key: string | null) => void;
}

/** A card's key, unique across libraries. */
export function packKey(pack: Pick<PackCard, 'libraryId' | 'path'>): string {
  return `${pack.libraryId}\u0000${pack.path}`;
}

/** The pack a `packKey` names. */
export function packRef(key: string): FolderRef {
  const [libraryId, path] = key.split('\u0000');
  return { libraryId, path: path ?? '' };
}

// Bumped by every load: only the latest one's packs are applied.
let generation = 0;

async function fetchPacks(scope: PacksScope, key: string) {
  const mine = ++generation;
  let packs: PackCard[];
  try {
    packs = await api.getPacks(scope.libraryIds, scope.underPath);
  } catch (e) {
    console.error('Failed to load the packs:', e);
    packs = [];
  }
  if (mine !== generation) return;
  usePacksStore.setState((s) => ({
    packs,
    selected: packs.some((p) => packKey(p) === s.selected) ? s.selected : null,
    key,
  }));
}

export const usePacksStore = create<PacksStore>()((set, get) => ({
  mode: 'packs',
  key: null,
  scope: null,
  packs: null,
  selected: null,

  setMode: (mode) => set({ mode }),

  load: async (scope) => {
    if (!scope) {
      generation++;
      set({ scope: null, key: null, packs: [], selected: null });
      return;
    }
    const key = scopeKey(scope);
    const current = get().scope;
    if (current && scopeKey(current) === key) return;
    set({ scope, key: null, packs: null, selected: null });
    await fetchPacks(scope, key);
  },

  refetch: async () => {
    const { scope } = get();
    if (scope) await fetchPacks(scope, scopeKey(scope));
  },

  select: (selected) => set({ selected }),
}));
