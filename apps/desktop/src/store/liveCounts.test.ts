import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { useFolderTreeStore } from './folderTree';
import { useLiveCountsStore } from './liveCounts';
import type { FacetCounts, FolderCounts, SearchQuery } from '../types';

const realFacets = api.getFacetCounts;
const realFolders = api.getFolderCounts;

const query = (text: string, filter: SearchQuery['filter'] = {}): SearchQuery => ({
  text,
  filter,
  sort: { field: 'date', order: 'desc' },
});

/** Serves counts that name the query's text, recording each call; `hold` keeps them pending. */
function serve() {
  const calls: string[] = [];
  const pending: (() => void)[] = [];
  let hold = false;
  const answer = <T>(value: T) =>
    new Promise<T>((resolve) => (hold ? pending.push(() => resolve(value)) : resolve(value)));
  api.getFacetCounts = (q) => {
    calls.push(`facets:${q.text}`);
    return answer<FacetCounts>({
      kinds: { texture: q.text.length },
      extensions: {},
      extensionBase: 0,
    });
  };
  api.getFolderCounts = (q, libraryId, parentPaths) => {
    calls.push(`folders:${q.text}:${libraryId}:${parentPaths.join('|')}`);
    const counts: FolderCounts = Object.fromEntries(
      parentPaths.map((p) => [p, { [`${p}/${q.text}`]: q.text.length }])
    );
    return answer(counts);
  };
  return {
    calls,
    hold: (on: boolean) => (hold = on),
    release: () => pending.splice(0).forEach((resolve) => resolve()),
  };
}

const counts = () => useLiveCountsStore.getState();
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  api.getFacetCounts = realFacets;
  api.getFolderCounts = realFolders;
  counts().recount(query(''));
  useFolderTreeStore.setState({ children: {}, expanded: {} });
});

describe('useLiveCountsStore', () => {
  test('counts the kinds, extensions and every expanded node while search text is on', async () => {
    const { calls } = serve();
    useFolderTreeStore.setState({ expanded: { kenney: ['', '3D assets'] } });

    counts().recount(query('tree'));
    await settle();
    expect(counts().live).toBe(true);
    expect(counts().facets?.kinds).toEqual({ texture: 4 });
    expect(counts().folders.kenney['3D assets']).toEqual({ '3D assets/tree': 4 });
    expect(calls).toEqual(['facets:tree', 'folders:tree:kenney:|3D assets']);

    // A node expanded later asks for its own; one already counted asks for nothing
    counts().countFolders('kenney', ['3D assets/Platformer Kit', '3D assets']);
    await settle();
    expect(calls.at(-1)).toBe('folders:tree:kenney:3D assets/Platformer Kit');
    expect(Object.keys(counts().folders.kenney)).toHaveLength(3);
  });

  test('asks for nothing and clears the counts with no search text or filter', async () => {
    const { calls } = serve();
    useFolderTreeStore.setState({ expanded: { kenney: [''] } });
    counts().recount(query('', { kinds: ['texture'] }));
    await settle();
    expect(counts().facets).not.toBeNull();

    counts().recount(query(''));
    counts().countFolders('kenney', ['']);
    await settle();
    expect(counts()).toMatchObject({ live: false, facets: null, folders: {} });
    expect(calls).toHaveLength(2);
  });

  test('a new query clears the counts it changes, and a refresh keeps them until it lands', async () => {
    const server = serve();
    useFolderTreeStore.setState({ expanded: { kenney: [''] } });
    counts().recount(query('tree'));
    await settle();

    server.hold(true);
    counts().recount({ ...query('tree'), sort: { field: 'name', order: 'asc' } });
    expect(counts().folders.kenney['']).toEqual({ '/tree': 4 });
    server.release();

    // Selecting a folder changes the kind counts, not the folder counts
    counts().recount({ ...query('tree'), folder: { libraryId: 'kenney', path: '3D assets' } });
    expect(counts().facets).toBeNull();
    expect(counts().folders.kenney['']).toEqual({ '/tree': 4 });
    server.release();

    counts().recount(query('rock'));
    expect(counts()).toMatchObject({ facets: null, folders: {} });
    server.release();
    await settle();
    expect(counts().facets?.kinds).toEqual({ texture: 4 });
    expect(counts().folders.kenney['']).toEqual({ '/rock': 4 });
  });

  test('counts from an earlier query that land late are dropped', async () => {
    const server = serve();
    useFolderTreeStore.setState({ expanded: { kenney: [''] } });
    server.hold(true);
    counts().recount(query('tree'));
    server.hold(false);
    counts().recount(query('rocks'));
    await settle();
    server.release();
    await settle();
    expect(counts().facets?.kinds).toEqual({ texture: 5 });
    expect(counts().folders.kenney['']).toEqual({ '/rocks': 5 });
  });
});
