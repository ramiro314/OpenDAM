import type { FilterSpec, SearchQuery } from '../types';
import { isFilterEmpty } from '../components/Sidebar/sidebarHelpers';

/** Which of the toolbar's Packs/Assets toggle is picked; it lasts the session. */
export type BrowseMode = 'packs' | 'assets';

/** Where the Packs view looks for packs: the libraries (null for all) and the folder below. */
export interface PacksScope {
  libraryIds: string[] | null;
  underPath: string;
}

/**
 * The tree node `query` selects, as the packs below it: a folder of one library, the selected
 * libraries' roots, or every library's root. A collection is no tree node, so it has none.
 */
export function packsScope(query: SearchQuery): PacksScope | null {
  if (query.collectionId) return null;
  if (query.folder) return { libraryIds: [query.folder.libraryId], underPath: query.folder.path };
  if (query.libraryIds && query.libraryIds.length > 0) {
    return { libraryIds: query.libraryIds, underPath: '' };
  }
  if (query.libraryId) return { libraryIds: [query.libraryId], underPath: '' };
  return { libraryIds: null, underPath: '' };
}

/** A key that differs exactly when two scopes do. */
export function scopeKey(scope: PacksScope): string {
  return JSON.stringify([scope.libraryIds, scope.underPath]);
}

/** Whether any filter is on, "Show hidden files" and the filter bar's included. */
export function isFiltering(filter: FilterSpec): boolean {
  return (
    !isFilterEmpty(filter) ||
    Boolean(filter.extensions?.length) ||
    Boolean(filter.namePrefix?.trim()) ||
    Boolean(filter.nameSuffix?.trim()) ||
    Boolean(filter.collectionId) ||
    Boolean(filter.collectionIds?.length)
  );
}

/** Whether search text or a filter is on, which shows Assets whatever the toggle says (D23). */
export function isSearching(query: Pick<SearchQuery, 'text' | 'filter'>): boolean {
  return query.text.trim() !== '' || isFiltering(query.filter);
}

/**
 * What the main area shows: Packs when the selected node has packs below it, nothing searches
 * or filters and the toggle is on Packs; Assets otherwise. `hasPacks` is null while the node's
 * packs load.
 */
export function browseView(
  hasPacks: boolean | null,
  query: Pick<SearchQuery, 'text' | 'filter'>,
  mode: BrowseMode
): 'packs' | 'assets' | 'loading' {
  if (mode === 'assets' || isSearching(query)) return 'assets';
  if (hasPacks === null) return 'loading';
  return hasPacks ? 'packs' : 'assets';
}
