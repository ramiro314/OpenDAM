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
  FolderRef,
  PackCard,
  SortField,
  SortOrder,
  AutotagLastUsed,
  AutotagProviderOption,
  AutotagTarget,
  AutotagUpdate,
  RunSummary,
} from '../types';
import { api } from '../api';
import { useFolderTreeStore } from './folderTree';
import type { ImageScaling } from '../utils/imageScaling';
import {
  PAGE_SIZE,
  assetAt,
  emptyCache,
  findAnchor,
  findCached,
  insertPage,
  pageOf,
  patchAssets,
  refreshPages,
  retryDelay,
  touch,
  type PageCache,
} from './pageCache';

// ── UI State ──────────────────────────────────────────────────────────────
interface UIState {
  viewMode: 'grid' | 'list';
  gridSize: 'sm' | 'md' | 'lg';
  /** How small images shown larger than their pixel size render */
  imageScaling: ImageScaling;
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
  setImageScaling: (scaling: ImageScaling) => void;
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
      imageScaling: 'pixelated',
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
      setImageScaling: (imageScaling) => set({ imageScaling }),
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
        imageScaling: s.imageScaling,
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
interface AssetStore extends PageCache {
  /** Result count for the current query, counted once per search or refresh */
  total: number;
  /** The files the results stand for, hidden support files included, counted with `total` */
  fileTotal: number;
  /** Copies of the selected and focused assets, kept when their pages are evicted (D3) */
  pinned: Map<string, Asset>;
  /**
   * Assets deleted since a search or refresh last replaced the cache: their tiles show as
   * placeholders and lookups skip them until a refresh lands without them
   */
  deleted: Set<string>;
  /** Pages in flight */
  pending: Set<number>;
  /** The pages the grid last asked for; never evicted */
  wanted: number[];
  /** Bumped by every search and refresh; page results from an older one are dropped */
  generation: number;
  /** Bumped each time a search or refresh replaces the cache, so the grid re-sends its pages */
  epoch: number;
  /**
   * Where the last refresh found the asset that was at the top of the view: `from` its old
   * index, `to` its new one. Null after a search, which starts at the top.
   */
  anchor: { from: number; to: number } | null;
  loading: boolean;
  /** Why the last search failed; the grid shows it instead of results */
  searchError: string | null;
  query: SearchQuery;

  setQuery: (q: Partial<SearchQuery>) => void;
  setFilter: (f: Partial<FilterSpec>) => void;
  /** Replaces the filter, keeping "Show hidden files" unless `f` sets it */
  replaceFilter: (f: FilterSpec, collectionId?: string) => void;
  /** Clears the collection and every filter but "Show hidden files" */
  resetFilter: () => void;
  clearFilter: (key: keyof FilterSpec) => void;
  setSort: (field: SortField, order?: SortOrder) => void;
  /** Runs the query: clears the cache and fetches page 0 with the total */
  search: () => Promise<void>;
  /** Fetches the wanted pages that aren't cached or in flight, then evicts beyond the limit */
  ensurePages: (indices: number[]) => void;
  /** The grid reports the first visible index, which a refresh keeps in place */
  setViewTop: (index: number) => void;
  /**
   * Refetches the total and the wanted pages, keeping the stale ones on screen until they
   * arrive, then re-anchors on the asset that was at the top (C5)
   */
  refresh: () => Promise<void>;
  assetAt: (index: number) => Asset | undefined;
  /** A pinned asset, or a cached one */
  getById: (id: string) => Asset | undefined;
  /** Replaces each of `ids` with `fn(asset)` in its page and its pinned copy */
  patchCached: (ids: string | string[], fn: (asset: Asset) => Asset) => void;
  patchAsset: (id: string, patch: Partial<Asset>) => Promise<void>;
  deleteAsset: (id: string, deleteFile?: boolean) => Promise<void>;
  deleteAssets: (ids: string[], deleteFile?: boolean) => Promise<void>;
  renameAsset: (id: string, newName: string) => Promise<Asset>;
  /**
   * Applies a finished preview: `thumbnailPath` replaces the old one, and `null` clears it. A
   * cached model whose bundled preview is this asset, as its own file (`render.filePath`) or its
   * thumbnail before the job (`render.previousThumbnailPath`), shows the new thumbnail instead,
   * or the file itself when the thumbnail was cleared.
   */
  updateAssetPreview: (
    id: string,
    thumbnailPath: string | null,
    previewModel?: string,
    render?: { filePath: string; previousThumbnailPath?: string }
  ) => void;
  setPreviewStatus: (id: string, status: Asset['previewStatus']) => void;
  /** Re-fetches the given assets that are cached or pinned; other ids are ignored */
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

/**
 * `next` with `prev`'s "Show hidden files", unless `next` sets it: a view setting that outlasts
 * picking a collection, a tag or a smart filter, as it does turning it on while one is picked.
 */
function keepView(prev: FilterSpec, next: FilterSpec): FilterSpec {
  return prev.showHidden && next.showHidden === undefined ? { ...next, showHidden: true } : next;
}

/** Clears the grid so a failed search can't leave results from an earlier query on screen */
function failedSearch(e: unknown): Partial<AssetStore> {
  return {
    ...emptyCache(),
    total: 0,
    fileTotal: 0,
    wanted: [],
    deleted: new Set(),
    searchError: String(e),
  };
}

/** The cached asset `id`, if its page is cached. */
function cachedAsset(s: Pick<PageCache, 'pages' | 'indexById'>, id: string): Asset | undefined {
  return findCached(s, id)?.asset;
}

/** A pinned asset, or a cached one; for selectors, which `getById` can't serve. */
export function lookupAsset(
  s: Pick<AssetStore, 'pinned' | 'pages' | 'indexById' | 'deleted'>,
  id: string
): Asset | undefined {
  if (s.deleted.has(id)) return undefined;
  return s.pinned.get(id) ?? cachedAsset(s, id);
}

/** Every cached and pinned asset, for lookups by something other than id. */
export function cachedAssets(s: Pick<AssetStore, 'pinned' | 'pages' | 'deleted'>): Asset[] {
  return [...[...s.pages.values()].flat(), ...s.pinned.values()].filter(
    (a) => !s.deleted.has(a.id)
  );
}

/** `pinned` with the copies `assets` holds newer versions of. */
function repin(pinned: Map<string, Asset>, assets: Iterable<Asset>): Map<string, Asset> {
  let next: Map<string, Asset> | null = null;
  for (const a of assets) {
    if (pinned.has(a.id) && pinned.get(a.id) !== a) (next ??= new Map(pinned)).set(a.id, a);
  }
  return next ?? pinned;
}

// The first visible index the grid last reported. It changes on every row scrolled, and only
// `refresh` reads it, so it stays out of the state that components subscribe to.
let viewTop = 0;

// Consecutive failed refreshes, for their retry backoff; reset by a search or a refresh that lands.
let refreshFailures = 0;

/** Masks deleted assets until the refresh that follows a delete replaces the cache (C4). */
function hideDeleted(ids: string[]) {
  useAssetStore.setState((s) => ({ deleted: new Set([...s.deleted, ...ids]) }));
}

// Consecutive failures per page, for the retry backoff; reset by every search and refresh.
let pageFailures = { generation: -1, counts: new Map<number, number>() };

function failuresOf(generation: number) {
  if (pageFailures.generation !== generation) pageFailures = { generation, counts: new Map() };
  return pageFailures.counts;
}

export const useAssetStore = create<AssetStore>()((set, get) => ({
  ...emptyCache(),
  total: 0,
  fileTotal: 0,
  pinned: new Map(),
  deleted: new Set(),
  pending: new Set(),
  wanted: [],
  generation: 0,
  epoch: 0,
  anchor: null,
  loading: false,
  searchError: null,
  query: defaultQuery,

  setQuery: (q) => set((s) => ({ query: { ...s.query, ...q } })),

  setFilter: (f) => set((s) => ({ query: { ...s.query, filter: { ...s.query.filter, ...f } } })),

  replaceFilter: (f, collectionId) =>
    set((s) => ({ query: { ...s.query, collectionId, filter: keepView(s.query.filter, f) } })),

  resetFilter: () =>
    set((s) => ({
      query: { ...s.query, collectionId: undefined, filter: keepView(s.query.filter, {}) },
    })),

  clearFilter: (key) =>
    set((s) => {
      const filter = { ...s.query.filter };
      delete filter[key];
      return { query: { ...s.query, filter } };
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
      };
    }),

  search: async () => {
    const { query } = get();
    const generation = get().generation + 1;
    refreshFailures = 0;
    // The old results stay on screen until page 0 replaces them
    set({ generation, loading: true, pending: new Set() });
    try {
      const result = await api.searchAssets({
        query,
        page: 0,
        pageSize: PAGE_SIZE,
        includeTotal: true,
      });
      if (generation !== get().generation) return;
      viewTop = 0;
      const cache = insertPage(emptyCache(), 0, result.assets, [0]);
      set((s) => ({
        ...cache,
        total: result.total ?? result.assets.length,
        fileTotal: result.fileTotal ?? result.total ?? result.assets.length,
        wanted: [0],
        pinned: repin(s.pinned, result.assets),
        deleted: new Set(),
        epoch: s.epoch + 1,
        anchor: null,
        searchError: null,
      }));
    } catch (e) {
      if (generation === get().generation) set(failedSearch(e));
    } finally {
      if (generation === get().generation) set({ loading: false });
    }
  },

  ensurePages: (indices) => {
    const s = get();
    const lastPage = pageOf(Math.max(0, s.total - 1));
    const wanted = indices.filter((p) => p >= 0 && p <= lastPage);
    set({ wanted, lru: touch(s, wanted).lru });
    // A search in flight replaces the cache, and the grid asks again once it has
    if (s.loading || s.total === 0) return;
    const missing = wanted.filter((p) => !s.pages.has(p) && !s.pending.has(p));
    if (missing.length === 0) return;
    const { generation, query } = s;
    set({ pending: new Set([...s.pending, ...missing]) });
    for (const page of missing) {
      api
        .searchAssets({ query, page, pageSize: PAGE_SIZE, includeTotal: false })
        .then((result) => {
          if (generation !== get().generation) return;
          failuresOf(generation).delete(page);
          set((cur) => ({
            ...insertPage(cur, page, result.assets, cur.wanted),
            pinned: repin(cur.pinned, result.assets),
          }));
        })
        .catch((e) => {
          console.error(`Failed to load page ${page}:`, e);
          if (generation !== get().generation) return;
          // The grid asks again only when its wanted pages change, so a failed page it still
          // shows would stay placeholders: ask again, waiting longer after each failure
          const counts = failuresOf(generation);
          const failures = (counts.get(page) ?? 0) + 1;
          counts.set(page, failures);
          setTimeout(() => {
            const cur = get();
            if (cur.generation === generation && cur.wanted.includes(page)) {
              cur.ensurePages(cur.wanted);
            }
          }, retryDelay(failures));
        })
        .finally(() => {
          if (generation !== get().generation) return;
          set((cur) => {
            const pending = new Set(cur.pending);
            pending.delete(page);
            return { pending };
          });
        });
    }
  },

  setViewTop: (index) => {
    viewTop = index;
  },

  refresh: async () => {
    const s = get();
    // The search in flight may have read the library before it changed
    if (s.loading) return get().search();
    const generation = s.generation + 1;
    set({ generation, pending: new Set() });
    const from = viewTop;
    const anchorId = assetAt(s, from)?.id;
    const pages = refreshPages(s.wanted.length > 0 ? s.wanted : [pageOf(from)], from, s.total);
    try {
      const results = await Promise.all(
        pages.map((page, i) =>
          api.searchAssets({ query: s.query, page, pageSize: PAGE_SIZE, includeTotal: i === 0 })
        )
      );
      if (generation !== get().generation) return;
      const total = results[0].total ?? s.total;
      const fileTotal = results[0].fileTotal ?? s.fileTotal;
      let cache = emptyCache();
      results.forEach((r, i) => {
        if (r.assets.length > 0) cache = insertPage(cache, pages[i], r.assets, pages);
      });
      const to = findAnchor(cache.pages, anchorId, from, total);
      viewTop = to;
      refreshFailures = 0;
      set((cur) => ({
        ...cache,
        total,
        fileTotal,
        pinned: repin(
          cur.pinned,
          results.flatMap((r) => r.assets)
        ),
        deleted: new Set(),
        epoch: cur.epoch + 1,
        anchor: { from, to },
        searchError: null,
      }));
    } catch (e) {
      // The cached results belong to this same query, so keep them, and ask for the wanted pages
      // that were in flight. Deleted assets stay masked and the rest stale until a refresh lands,
      // so try again, waiting longer after each failure, unless a newer search or refresh ran.
      console.error('Failed to refresh assets:', e);
      get().ensurePages(get().wanted);
      // A query whose search failed (an FTS syntax error) fails every refresh the same way and
      // has nothing cached to keep in place: only a new search or the next event tries again
      if (get().searchError) return;
      refreshFailures += 1;
      setTimeout(() => {
        if (get().generation === generation) void get().refresh();
      }, retryDelay(refreshFailures));
    }
  },

  assetAt: (index) => assetAt(get(), index),

  getById: (id) => lookupAsset(get(), id),

  patchCached: (ids, fn) => {
    const s = get();
    const patched = patchAssets(s, s.pinned, typeof ids === 'string' ? [ids] : ids, fn);
    if (patched) set(patched);
  },

  patchAsset: async (id, patch) => {
    const updated = await api.updateAsset(id, patch);
    get().patchCached(id, () => updated);
  },

  // Deletes shift every later index, so they refetch the wanted pages rather than splice (C4)
  deleteAsset: async (id, deleteFile = true) => {
    await api.deleteAsset(id, deleteFile);
    deselectAssets([id]);
    hideDeleted([id]);
    await get().refresh();
  },

  deleteAssets: async (ids, deleteFile = true) => {
    try {
      await api.deleteAssets(ids, deleteFile);
    } catch (err) {
      // A partial failure names the assets that were not deleted; the rest are gone, so drop
      // them now rather than leave them selected for a second delete to send again.
      const failed = new Set((err as Partial<DeleteAssetsError> | null)?.failedIds ?? ids);
      const gone = ids.filter((id) => !failed.has(id));
      if (gone.length > 0) {
        deselectAssets(gone);
        hideDeleted(gone);
        await get().refresh();
      }
      throw err;
    }
    deselectAssets(ids);
    hideDeleted(ids);
    await get().refresh();
  },

  renameAsset: async (id, newName) => {
    const updated = await api.renameAsset(id, newName);
    get().patchCached(id, () => updated);
    return updated;
  },

  updateAssetPreview: (id, thumbnailPath, previewModel, render) => {
    get().patchCached(id, (asset) => ({
      ...asset,
      thumbnailPath: thumbnailPath ?? undefined,
      previewStatus: 'done',
      metadata: previewModel ? { ...asset.metadata, preview_model: previewModel } : asset.metadata,
    }));
    if (!render) return;
    const shown = new Set([render.filePath, render.previousThumbnailPath].filter(Boolean));
    const picture = thumbnailPath ?? render.filePath;
    const rendered = cachedAssets(get())
      .filter(
        (a) => a.bundledPreview && shown.has(a.bundledPreview) && a.bundledPreview !== picture
      )
      .map((a) => a.id);
    if (rendered.length > 0) {
      get().patchCached(rendered, (asset) => ({ ...asset, bundledPreview: picture }));
    }
  },

  setPreviewStatus: (id, status) => {
    get().patchCached(id, (asset) => ({ ...asset, previewStatus: status }));
  },

  refreshAssets: async (ids) => {
    const loaded = ids.filter((id) => lookupAsset(get(), id));
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
    get().patchCached([...updated.keys()], (a) => updated.get(a.id) ?? a);
  },
}));

// Pins copies of the selected and focused assets, so evicting their pages keeps them (D3)
useUIStore.subscribe((ui, prev) => {
  if (ui.selectedAssetIds === prev.selectedAssetIds && ui.focusedAssetId === prev.focusedAssetId) {
    return;
  }
  const s = useAssetStore.getState();
  const ids = new Set(ui.selectedAssetIds);
  if (ui.focusedAssetId) ids.add(ui.focusedAssetId);
  const pinned = new Map<string, Asset>();
  for (const id of ids) {
    const asset = cachedAsset(s, id) ?? s.pinned.get(id);
    if (asset) pinned.set(id, asset);
  }
  const same =
    pinned.size === s.pinned.size && [...pinned].every(([id, a]) => s.pinned.get(id) === a);
  if (!same) useAssetStore.setState({ pinned });
});

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
    useAssetStore.getState().patchCached(assetIds, (a) => ({
      ...a,
      collections: [...new Set([...(a.collections ?? []), collectionId])],
    }));
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
    useAssetStore.getState().patchCached(assetIds, (a) => ({
      ...a,
      collections: (a.collections ?? []).filter((cid) => cid !== collectionId),
    }));
  },

  deleteCollection: async (id) => {
    await api.deleteCollection(id);
    set((s) => ({ collections: s.collections.filter((c) => c.id !== id) }));
  },
}));

export { useFolderTreeStore } from './folderTree';
export { useGroupDetailStore, shownMember } from './groupDetail';
export { extensionCount, kindCount, useLiveCountsStore } from './liveCounts';

export { usePacksStore, packKey } from './packs';
export {
  useFolderActionStore,
  applyFolderAction,
  revealFolder,
  type FolderTarget,
} from './folderActions';

/**
 * Selects tree node `folder` and searches it. A folder names its own library, so it replaces the
 * library and collection selection. The sidebar also clears its smart filter.
 */
export function selectFolder(folder: FolderRef) {
  const ui = useUIStore.getState();
  ui.setActiveCollection(null);
  ui.clearLibrarySelection();
  const assets = useAssetStore.getState();
  assets.setQuery({
    libraryIds: undefined,
    libraryId: undefined,
    collectionId: undefined,
    folder,
  });
  void assets.search();
}

/** Selects library `libraryId`'s row alone and searches it. */
export function selectLibrary(libraryId: string) {
  useUIStore.getState().setActiveLibrary(libraryId);
  const assets = useAssetStore.getState();
  assets.setQuery({
    libraryIds: [libraryId],
    libraryId,
    collectionId: undefined,
    folder: undefined,
  });
  void assets.search();
}

/**
 * Opens a pack card (D19): selects the pack's node in the tree, expanding the folders above it,
 * so the main area shows the pack's assets. A library that is itself one pack is its row.
 */
export async function openPack(pack: Pick<PackCard, 'libraryId' | 'path'>) {
  if (pack.path === '') {
    selectLibrary(pack.libraryId);
    return;
  }
  selectFolder({ libraryId: pack.libraryId, path: pack.path });
  await useFolderTreeStore.getState().reveal(pack.libraryId, pack.path);
}
