import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import type {
  Asset,
  DeleteAssetsError,
  Library,
  Collection,
  Tag,
  TagPatch,
  SearchQuery,
  FilterSpec,
  SortField,
  SortOrder,
  AutotagLastUsed,
  AutotagProviderOption,
  AutotagTarget,
  AutotagUpdate,
  RunSummary,
} from '../types';
import { api } from '../api';

// ── UI State ──────────────────────────────────────────────────────────────
interface UIState {
  viewMode: 'grid' | 'list';
  gridSize: 'sm' | 'md' | 'lg';
  sidebarOpen: boolean;
  detailPanelOpen: boolean;
  selectedAssetIds: Set<string>;
  focusedAssetId: string | null;
  activeLibraryId: string | null;
  activeCollectionId: string | null;
  /** Assets waiting for the delete confirmation, empty when none */
  assetsToDelete: string[];

  setViewMode: (mode: 'grid' | 'list') => void;
  setGridSize: (size: 'sm' | 'md' | 'lg') => void;
  toggleSidebar: () => void;
  toggleDetailPanel: () => void;
  selectAsset: (id: string, multi?: boolean) => void;
  selectAll: (ids: string[]) => void;
  clearSelection: () => void;
  focusAsset: (id: string | null) => void;
  selectedLibraryIds: string[];
  setSelectedLibraryIds: (ids: string[]) => void;
  toggleSelectedLibrary: (id: string, multi?: boolean) => void;
  clearLibrarySelection: () => void;
  setActiveLibrary: (id: string | null) => void;
  setActiveCollection: (id: string | null) => void;
  /** Asks to delete `ids`, behind the confirmation dialog; [] dismisses it */
  requestDelete: (ids: string[]) => void;
}

export const useUIStore = create<UIState>()(
  persist(
    (set) => ({
      viewMode: 'grid',
      gridSize: 'md',
      sidebarOpen: true,
      detailPanelOpen: true,
      selectedAssetIds: new Set(),
      focusedAssetId: null,
      activeLibraryId: null,
      selectedLibraryIds: [],
      activeCollectionId: null,
      assetsToDelete: [],

      setViewMode: (viewMode) => set({ viewMode }),
      setGridSize: (gridSize) => set({ gridSize }),
      toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
      toggleDetailPanel: () => set((s) => ({ detailPanelOpen: !s.detailPanelOpen })),

      selectAsset: (id, multi = false) =>
        set((s) => {
          if (multi) {
            const next = new Set(s.selectedAssetIds);
            if (next.has(id)) next.delete(id);
            else next.add(id);
            return { selectedAssetIds: next, focusedAssetId: id };
          }
          return { selectedAssetIds: new Set([id]), focusedAssetId: id };
        }),

      selectAll: (ids) => set({ selectedAssetIds: new Set(ids) }),

      clearSelection: () => set({ selectedAssetIds: new Set(), focusedAssetId: null }),

      focusAsset: (id) => set({ focusedAssetId: id }),

      setSelectedLibraryIds: (ids) =>
        set({ selectedLibraryIds: ids, activeLibraryId: ids[0] ?? null, activeCollectionId: null }),

      toggleSelectedLibrary: (id, multi = false) =>
        set((s) => {
          let next: string[];
          if (multi) {
            next = s.selectedLibraryIds.includes(id)
              ? s.selectedLibraryIds.filter((i) => i !== id)
              : [...s.selectedLibraryIds, id];
          } else {
            next = s.selectedLibraryIds.length === 1 && s.selectedLibraryIds[0] === id ? [] : [id];
          }
          return {
            selectedLibraryIds: next,
            activeLibraryId: next.length === 1 ? next[0] : null,
            activeCollectionId: null,
          };
        }),

      clearLibrarySelection: () => set({ selectedLibraryIds: [], activeLibraryId: null }),

      setActiveLibrary: (id) =>
        set({ activeLibraryId: id, selectedLibraryIds: id ? [id] : [], activeCollectionId: null }),
      setActiveCollection: (id) =>
        set({ activeCollectionId: id, activeLibraryId: null, selectedLibraryIds: [] }),
      requestDelete: (assetsToDelete) => set({ assetsToDelete }),
    }),
    {
      name: 'opendam-ui',
      partialize: (s) => ({
        viewMode: s.viewMode,
        gridSize: s.gridSize,
        sidebarOpen: s.sidebarOpen,
        detailPanelOpen: s.detailPanelOpen,
      }),
    }
  )
);

// ── Library Store ─────────────────────────────────────────────────────────
interface LibraryStore {
  libraries: Library[];
  loading: boolean;
  fetchLibraries: () => Promise<void>;
  addLibrary: (name: string, rootPaths: string[]) => Promise<void>;
  removeLibrary: (id: string) => Promise<void>;
}

export const useLibraryStore = create<LibraryStore>()((set) => ({
  libraries: [],
  loading: false,

  fetchLibraries: async () => {
    set({ loading: true });
    try {
      const libraries = await api.listLibraries();
      set({ libraries });
    } finally {
      set({ loading: false });
    }
  },

  addLibrary: async (name, rootPaths) => {
    const lib = await api.addLibrary({ name, rootPaths });
    // Adding the folder of an offline library reopens that library: replace it, don't list it twice
    set((s) => ({
      libraries: s.libraries.some((l) => l.id === lib.id)
        ? s.libraries.map((l) => (l.id === lib.id ? lib : l))
        : [...s.libraries, lib],
    }));
  },

  removeLibrary: async (id) => {
    await api.removeLibrary(id);
    set((s) => ({ libraries: s.libraries.filter((l) => l.id !== id) }));
  },
}));

// ── Asset / Search Store ──────────────────────────────────────────────────
interface AssetStore {
  assets: Asset[];
  total: number;
  /** Last page loaded into `assets` (pages are appended as the grid scrolls) */
  page: number;
  pageSize: number;
  loading: boolean;
  loadingMore: boolean;
  /** Why the last search failed; the grid shows it instead of results */
  searchError: string | null;
  query: SearchQuery;
  assetMap: Map<string, Asset>;

  setQuery: (q: Partial<SearchQuery>) => void;
  setFilter: (f: Partial<FilterSpec>) => void;
  replaceFilter: (f: FilterSpec, collectionId?: string) => void;
  resetFilter: () => void;
  clearFilter: (key: keyof FilterSpec) => void;
  setSort: (field: SortField, order?: SortOrder) => void;
  /** Runs the query from the first page, replacing the loaded assets */
  search: () => Promise<void>;
  /** Appends the next page, if there is one */
  loadMore: () => Promise<void>;
  /** Re-fetches every page loaded so far, keeping the grid's scroll position */
  refresh: () => Promise<void>;
  patchAsset: (id: string, patch: Partial<Asset>) => Promise<void>;
  deleteAsset: (id: string, deleteFile?: boolean) => Promise<void>;
  deleteAssets: (ids: string[], deleteFile?: boolean) => Promise<void>;
  renameAsset: (id: string, newName: string) => Promise<Asset>;
  updateAssetPreview: (id: string, thumbnailPath?: string, previewModel?: string) => void;
  setPreviewStatus: (id: string, status: Asset['previewStatus']) => void;
  getById: (id: string) => Asset | undefined;
  /** Re-fetches the given assets that are loaded; ids not loaded are ignored */
  refreshAssets: (ids: string[]) => Promise<void>;
}

/** Drops deleted assets from the selection and the focus. */
function deselectAssets(ids: string[]) {
  const idSet = new Set(ids);
  const ui = useUIStore.getState();
  if (ui.focusedAssetId && idSet.has(ui.focusedAssetId)) {
    ui.focusAsset(null);
  }
  ui.selectAll(Array.from(ui.selectedAssetIds).filter((id) => !idSet.has(id)));
}

const defaultQuery: SearchQuery = {
  text: '',
  filter: {},
  sort: { field: 'date', order: 'desc' },
  includeSubcollections: true,
};

/** Clears the grid so a failed search can't leave results from an earlier query on screen */
function failedSearch(e: unknown): Partial<AssetStore> {
  return { assets: [], total: 0, page: 0, assetMap: new Map(), searchError: String(e) };
}

// Bumped by every search/refresh so a slower loadMore can't append results from an older query.
let queryGeneration = 0;

export const useAssetStore = create<AssetStore>()((set, get) => ({
  assets: [],
  total: 0,
  page: 0,
  pageSize: 100,
  loading: false,
  loadingMore: false,
  searchError: null,
  query: defaultQuery,
  assetMap: new Map(),

  setQuery: (q) => set((s) => ({ query: { ...s.query, ...q }, page: 0 })),

  setFilter: (f) =>
    set((s) => ({
      query: { ...s.query, filter: { ...s.query.filter, ...f } },
      page: 0,
    })),

  replaceFilter: (f, collectionId) =>
    set((s) => ({
      query: { ...s.query, collectionId, filter: f },
      page: 0,
    })),

  resetFilter: () =>
    set((s) => ({
      query: { ...s.query, collectionId: undefined, filter: {} },
      page: 0,
    })),

  clearFilter: (key) =>
    set((s) => {
      const filter = { ...s.query.filter };
      delete filter[key];
      return { query: { ...s.query, filter }, page: 0 };
    }),

  setSort: (field, order) =>
    set((s) => {
      const currentOrder = s.query.sort.order;
      return {
        query: {
          ...s.query,
          sort: {
            field,
            order:
              order ?? (field === s.query.sort.field && currentOrder === 'desc' ? 'asc' : 'desc'),
          },
        },
        page: 0,
      };
    }),

  search: async () => {
    const { query, pageSize } = get();
    const generation = ++queryGeneration;
    set({ loading: true, page: 0 });
    try {
      const result = await api.searchAssets({ query, page: 0, pageSize });
      if (generation !== queryGeneration) return;
      const assetMap = new Map(result.assets.map((a) => [a.id, a]));
      set({ assets: result.assets, total: result.total, assetMap, searchError: null });
    } catch (e) {
      if (generation === queryGeneration) set(failedSearch(e));
    } finally {
      if (generation === queryGeneration) set({ loading: false });
    }
  },

  loadMore: async () => {
    const { query, page, pageSize, assets, total, loading, loadingMore } = get();
    if (loading || loadingMore || assets.length >= total) return;
    const generation = queryGeneration;
    set({ loadingMore: true });
    try {
      const result = await api.searchAssets({ query, page: page + 1, pageSize });
      if (generation !== queryGeneration) return;
      set((s) => {
        // Pages can shift if assets change between requests; skip ones already shown.
        const assetMap = new Map(s.assetMap);
        const added = result.assets.filter((a) => !assetMap.has(a.id));
        for (const a of added) assetMap.set(a.id, a);
        return { assets: [...s.assets, ...added], total: result.total, page: page + 1, assetMap };
      });
    } finally {
      set({ loadingMore: false });
    }
  },

  refresh: async () => {
    const { query, page, pageSize } = get();
    const generation = ++queryGeneration;
    // One request covering every loaded page, so the grid keeps its length and scroll position.
    try {
      const result = await api.searchAssets({ query, page: 0, pageSize: (page + 1) * pageSize });
      if (generation !== queryGeneration) return;
      const assetMap = new Map(result.assets.map((a) => [a.id, a]));
      set({ assets: result.assets, total: result.total, assetMap, searchError: null });
    } catch (e) {
      // The loaded results belong to this same query, so keep them; the next event or search retries.
      console.error('Failed to refresh assets:', e);
    }
  },

  patchAsset: async (id, patch) => {
    const updated = await api.updateAsset(id, patch);
    set((s) => ({
      assets: s.assets.map((a) => (a.id === id ? updated : a)),
      assetMap: new Map(s.assetMap).set(id, updated),
    }));
  },

  deleteAsset: async (id, deleteFile = true) => {
    await api.deleteAsset(id, deleteFile);
    set((s) => {
      const assets = s.assets.filter((a) => a.id !== id);
      const assetMap = new Map(s.assetMap);
      assetMap.delete(id);
      return { assets, total: Math.max(0, s.total - 1), assetMap };
    });
    const ui = useUIStore.getState();
    if (ui.focusedAssetId === id) {
      ui.focusAsset(null);
    }
    if (ui.selectedAssetIds.has(id)) {
      const next = new Set(ui.selectedAssetIds);
      next.delete(id);
      ui.selectAll(Array.from(next));
    }
  },

  deleteAssets: async (ids, deleteFile = true) => {
    const remove = (gone: string[]) => {
      const goneSet = new Set(gone);
      set((s) => {
        const assets = s.assets.filter((a) => !goneSet.has(a.id));
        const assetMap = new Map(s.assetMap);
        for (const id of gone) assetMap.delete(id);
        return { assets, total: Math.max(0, s.total - gone.length), assetMap };
      });
      deselectAssets(gone);
    };
    try {
      await api.deleteAssets(ids, deleteFile);
    } catch (err) {
      // A partial failure names the assets that were not deleted; the rest are gone, so drop
      // them now rather than leave them selected for a second delete to send again.
      const failed = new Set((err as Partial<DeleteAssetsError> | null)?.failedIds ?? ids);
      remove(ids.filter((id) => !failed.has(id)));
      throw err;
    }
    remove(ids);
  },

  renameAsset: async (id, newName) => {
    const updated = await api.renameAsset(id, newName);
    set((s) => ({
      assets: s.assets.map((a) => (a.id === id ? updated : a)),
      assetMap: new Map(s.assetMap).set(id, updated),
    }));
    return updated;
  },

  updateAssetPreview: (id, thumbnailPath, previewModel) => {
    set((s) => {
      const asset = s.assetMap.get(id);
      if (!asset) return s;
      const updated: Asset = {
        ...asset,
        thumbnailPath: thumbnailPath || asset.thumbnailPath,
        previewStatus: 'done',
        metadata: previewModel
          ? { ...asset.metadata, preview_model: previewModel }
          : asset.metadata,
      };
      return {
        assets: s.assets.map((a) => (a.id === id ? updated : a)),
        assetMap: new Map(s.assetMap).set(id, updated),
      };
    });
  },

  setPreviewStatus: (id, status) => {
    set((s) => {
      const asset = s.assetMap.get(id);
      if (!asset) return s;
      const updated: Asset = { ...asset, previewStatus: status };
      return {
        assets: s.assets.map((a) => (a.id === id ? updated : a)),
        assetMap: new Map(s.assetMap).set(id, updated),
      };
    });
  },

  getById: (id) => get().assetMap.get(id),

  refreshAssets: async (ids) => {
    const loaded = ids.filter((id) => get().assetMap.has(id));
    const fetched = await Promise.all(
      loaded.map((id) =>
        api.getAsset(id).catch((e) => {
          console.error(`Failed to refresh asset ${id}:`, e);
          return null;
        })
      )
    );
    const updated = new Map(fetched.filter((a): a is Asset => a !== null).map((a) => [a.id, a]));
    if (updated.size === 0) return;
    set((s) => {
      const assetMap = new Map(s.assetMap);
      for (const [id, asset] of updated) if (assetMap.has(id)) assetMap.set(id, asset);
      return { assets: s.assets.map((a) => updated.get(a.id) ?? a), assetMap };
    });
  },
}));

// ── Auto-tagging Store ────────────────────────────────────────────────────
interface AutotagStore {
  runs: RunSummary[];
  /** null until loaded */
  providers: AutotagProviderOption[] | null;
  /** What "Suggest tags" uses; null until loaded */
  lastUsed: AutotagLastUsed | null;
  runsPanelOpen: boolean;
  /** What the open run dialog would tag, or null when it is closed */
  dialogTarget: AutotagTarget | null;
  /** The run whose review modal is open */
  reviewRunId: string | null;
  fetchRuns: () => Promise<void>;
  fetchProviders: () => Promise<void>;
  /** Reads the last-used provider and mode, which every run start saves */
  fetchLastUsed: () => Promise<void>;
  /**
   * Takes an `autotag:updated` event: the runs, and re-fetches the loaded assets that changed,
   * and the tags when a run created some
   */
  applyUpdate: (update: AutotagUpdate) => void;
  setRunsPanelOpen: (open: boolean) => void;
  openAutotagDialog: (target: AutotagTarget | null) => void;
  /** Opens a run's review in place of the Runs panel, or closes it with null */
  openReview: (runId: string | null) => void;
}

export const useAutotagStore = create<AutotagStore>()((set) => ({
  runs: [],
  providers: null,
  lastUsed: null,
  runsPanelOpen: false,
  dialogTarget: null,
  reviewRunId: null,

  fetchRuns: async () => {
    set({ runs: await api.listAutotagRuns() });
  },

  fetchProviders: async () => {
    set({ providers: await api.listAutotagProviders() });
  },

  fetchLastUsed: async () => {
    set({ lastUsed: await api.getAutotagLastUsed() });
  },

  applyUpdate: ({ runs, changedAssetIds, tagsChanged }) => {
    set({ runs });
    if (tagsChanged) useTagStore.getState().fetchTags().catch(console.error);
    if (changedAssetIds.length > 0) useAssetStore.getState().refreshAssets(changedAssetIds);
  },

  setRunsPanelOpen: (runsPanelOpen) => set({ runsPanelOpen }),
  openAutotagDialog: (dialogTarget) => set({ dialogTarget }),
  openReview: (reviewRunId) =>
    set(reviewRunId ? { reviewRunId, runsPanelOpen: false } : { reviewRunId }),
}));

// ── Tag Store ─────────────────────────────────────────────────────────────
interface TagStore {
  tags: Tag[];
  tagMap: Map<string, Tag>;
  fetchTags: () => Promise<void>;
  createTag: (name: string, parentId?: string) => Promise<void>;
  updateTag: (id: string, patch: TagPatch) => Promise<void>;
  deleteTag: (id: string) => Promise<void>;
}

export const useTagStore = create<TagStore>()((set) => ({
  tags: [],
  tagMap: new Map(),

  fetchTags: async () => {
    const tags = await api.listTags();
    set({ tags, tagMap: new Map(tags.map((t) => [t.id, t])) });
  },

  createTag: async (name, parentId) => {
    const tag = await api.createTag(name, parentId);
    set((s) => ({
      tags: [...s.tags, tag],
      tagMap: new Map(s.tagMap).set(tag.id, tag),
    }));
  },

  updateTag: async (id, patch) => {
    const tag = await api.updateTag(id, patch);
    set((s) => ({
      tags: s.tags.map((t) => (t.id === id ? tag : t)),
      tagMap: new Map(s.tagMap).set(id, tag),
    }));
  },

  deleteTag: async (id) => {
    await api.deleteTag(id);
    set((s) => {
      const tagMap = new Map(s.tagMap);
      tagMap.delete(id);
      return { tags: s.tags.filter((t) => t.id !== id), tagMap };
    });
  },
}));

// ── Collection Store ──────────────────────────────────────────────────────
interface CollectionStore {
  collections: Collection[];
  fetchCollections: () => Promise<void>;
  createCollection: (
    name: string,
    description?: string,
    parentId?: string,
    isSmart?: boolean,
    filterSpec?: string
  ) => Promise<void>;
  addToCollection: (collectionId: string, assetIds: string[]) => Promise<void>;
  removeFromCollection: (collectionId: string, assetIds: string[]) => Promise<void>;
  deleteCollection: (id: string) => Promise<void>;
}

let namedCollections: Collection[] | undefined;
let collectionNames = new Map<string, string>();

/**
 * Collection id to name, for `useCollectionStore`. It rebuilds the map only when the collection
 * list changes, so every grid card shares one map instead of building its own.
 */
export function selectCollectionNames(s: { collections: Collection[] }): Map<string, string> {
  if (s.collections !== namedCollections) {
    namedCollections = s.collections;
    collectionNames = new Map(s.collections.map((c) => [c.id, c.name]));
  }
  return collectionNames;
}

export const useCollectionStore = create<CollectionStore>()((set) => ({
  collections: [],

  fetchCollections: async () => {
    const collections = await api.listCollections();
    set({ collections });
  },

  createCollection: async (name, description, parentId, isSmart, filterSpec) => {
    const col = await api.createCollection(name, description, parentId, isSmart, filterSpec);
    set((s) => ({ collections: [...s.collections, col] }));
  },

  addToCollection: async (collectionId, assetIds) => {
    await api.addToCollection(collectionId, assetIds);
    set((s) => ({
      collections: s.collections.map((c) =>
        c.id === collectionId ? { ...c, assetIds: [...new Set([...c.assetIds, ...assetIds])] } : c
      ),
    }));
    useAssetStore.setState((s) => {
      const assets = s.assets.map((a) => {
        if (assetIds.includes(a.id)) {
          const cur = a.collections ?? [];
          return { ...a, collections: [...new Set([...cur, collectionId])] };
        }
        return a;
      });
      const assetMap = new Map(s.assetMap);
      for (const aid of assetIds) {
        const a = assetMap.get(aid);
        if (a) {
          const cur = a.collections ?? [];
          assetMap.set(aid, { ...a, collections: [...new Set([...cur, collectionId])] });
        }
      }
      return { assets, assetMap };
    });
  },

  removeFromCollection: async (collectionId, assetIds) => {
    await api.removeFromCollection(collectionId, assetIds);
    set((s) => ({
      collections: s.collections.map((c) =>
        c.id === collectionId
          ? { ...c, assetIds: c.assetIds.filter((id) => !assetIds.includes(id)) }
          : c
      ),
    }));
    useAssetStore.setState((s) => {
      const assets = s.assets.map((a) => {
        if (assetIds.includes(a.id)) {
          const cur = a.collections ?? [];
          return { ...a, collections: cur.filter((cid) => cid !== collectionId) };
        }
        return a;
      });
      const assetMap = new Map(s.assetMap);
      for (const aid of assetIds) {
        const a = assetMap.get(aid);
        if (a) {
          const cur = a.collections ?? [];
          assetMap.set(aid, { ...a, collections: cur.filter((cid) => cid !== collectionId) });
        }
      }
      return { assets, assetMap };
    });
  },

  deleteCollection: async (id) => {
    await api.deleteCollection(id);
    set((s) => ({ collections: s.collections.filter((c) => c.id !== id) }));
  },
}));
