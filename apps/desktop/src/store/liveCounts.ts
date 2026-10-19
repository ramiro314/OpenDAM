import { create } from 'zustand';
import { api } from '../api';
import { showsLiveCounts } from '../components/Sidebar/sidebarHelpers';
import type { FacetCounts, FolderCounts, SearchQuery } from '../types';
import { openNodes, useFolderTreeStore } from './folderTree';

interface LiveCountsStore {
  /** Whether the current results show live counts (`showsLiveCounts`) */
  live: boolean;
  /** The kind and extension counts of the current query, null until they arrive */
  facets: FacetCounts | null;
  /** Child folder counts per library id, then per node path, for the nodes counted so far */
  folders: Record<string, FolderCounts>;
  /**
   * Counts `query`, the one whose results the grid now shows, and the children of every node
   * the tree shows expanded (`openNodes`). Runs once per search or refresh, like the total. Counts that `query` can't change
   * stay on screen until the new ones arrive: all of them on a refresh, and the folder counts
   * when only the selected folder or collection changed, which they leave out.
   */
  recount: (query: SearchQuery) => void;
  /** Counts the children of `parentPaths` for the current query, unless already asked for */
  countFolders: (libraryId: string, parentPaths: string[]) => void;
}

/** The grid's total once `kind` is the only kind (the sidebar's Types replace the kind filter). */
export function kindCount(facets: FacetCounts, kind: string): number {
  return facets.kinds[kind] ?? 0;
}

/**
 * The grid's total once `extension` joins the active ones (a format pill adds to them). Read
 * every extension count through this: `facets.extensions` holds only those that differ from
 * `extensionBase`, so a missing one counts the active extensions' total, not 0.
 */
export function extensionCount(facets: FacetCounts, extension: string): number {
  return facets.extensions[extension] ?? facets.extensionBase;
}

// Bumped by every recount: only the latest one's results are applied.
let generation = 0;
let counted: SearchQuery | null = null;
/** `library id \n parent path` of each node asked for since the last recount */
let asked = new Set<string>();

/** What the kind and extension counts depend on: the query but its sort. */
const facetKey = (query: SearchQuery | null) => JSON.stringify(query && { ...query, sort: null });

/** What the folder counts depend on: the query but its sort, folder and collection. */
const folderKey = (query: SearchQuery | null) =>
  JSON.stringify(query && { ...query, sort: null, folder: null, collectionId: null });

export const useLiveCountsStore = create<LiveCountsStore>()((set, get) => ({
  live: false,
  facets: null,
  folders: {},

  recount: (query) => {
    const current = ++generation;
    const { live } = get();
    const sameFacets = live && facetKey(query) === facetKey(counted);
    const sameFolders = live && folderKey(query) === folderKey(counted);
    asked = new Set();
    counted = query;
    if (!showsLiveCounts(query)) {
      set({ live: false, facets: null, folders: {} });
      return;
    }
    set((s) => ({
      live: true,
      facets: sameFacets ? s.facets : null,
      folders: sameFolders ? s.folders : {},
    }));
    api
      .getFacetCounts(query)
      .then((facets) => {
        if (current === generation) set({ facets });
      })
      .catch((e) => console.error('Failed to count kinds and extensions:', e));
    // Only the nodes the tree shows: one under a collapsed ancestor asks for its own counts
    // when its tree mounts again
    const tree = useFolderTreeStore.getState();
    for (const libraryId of Object.keys(tree.expanded)) {
      get().countFolders(libraryId, openNodes(tree, libraryId));
    }
  },

  countFolders: (libraryId, parentPaths) => {
    const query = counted;
    if (!get().live || !query) return;
    const key = (path: string) => `${libraryId}\n${path}`;
    const wanted = parentPaths.filter((p) => !asked.has(key(p)));
    if (wanted.length === 0) return;
    for (const p of wanted) asked.add(key(p));
    const current = generation;
    api
      .getFolderCounts(query, libraryId, wanted)
      .then((counts) => {
        if (current !== generation) return;
        set((s) => ({
          folders: { ...s.folders, [libraryId]: { ...s.folders[libraryId], ...counts } },
        }));
      })
      .catch((e) => {
        console.error(`Failed to count the folders in ${libraryId}:`, e);
        // Asked again by the next recount, or when the node's tree next mounts
        if (current === generation) for (const p of wanted) asked.delete(key(p));
      });
  },
}));
