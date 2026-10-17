import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test';
import { api } from '../api';
import { useAssetStore, useAutotagStore, useCollectionStore, useTagStore, useUIStore } from '.';
import { MAX_PAGES, PAGE_SIZE, emptyCache } from './pageCache';
import type { Asset, DeleteAssetsError, SearchPayload, SearchResult, Tag } from '../types';

const realSearch = api.searchAssets;
const realDeleteAssets = api.deleteAssets;
const realGetAsset = api.getAsset;
const realListTags = api.listTags;
const realAddToCollection = api.addToCollection;

const named = (id: string) => ({ id, fileName: `${id}.png`, collections: [] }) as unknown as Asset;
const library = (n: number) => Array.from({ length: n }, (_, i) => named(`a${i}`));
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Serves `search_assets` pages from `assets` (read on every call), recording each request. */
function serve(assets: () => Asset[]) {
  const calls: SearchPayload[] = [];
  api.searchAssets = async (payload) => {
    calls.push(payload);
    const all = assets();
    const start = payload.page * payload.pageSize;
    const result: SearchResult = {
      assets: all.slice(start, start + payload.pageSize),
      page: payload.page,
      pageSize: payload.pageSize,
    };
    if (payload.includeTotal !== false) result.total = all.length;
    return result;
  };
  return calls;
}

beforeEach(() => {
  useAssetStore.setState({
    ...emptyCache(),
    total: 0,
    pinned: new Map(),
    pending: new Set(),
    wanted: [],
    loading: false,
    searchError: null,
    anchor: null,
  });
  useUIStore.getState().clearSelection();
  useAssetStore.getState().setViewTop(0);
});

afterEach(() => {
  api.searchAssets = realSearch;
  api.deleteAssets = realDeleteAssets;
  api.getAsset = realGetAsset;
  api.listTags = realListTags;
  api.addToCollection = realAddToCollection;
});

describe('useAssetStore search errors', () => {
  const asset = named('a1');
  const found: SearchResult = { assets: [asset], total: 1, page: 0, pageSize: PAGE_SIZE };

  test('a failed search shows the error and clears stale results', async () => {
    api.searchAssets = async () => found;
    await useAssetStore.getState().search();
    expect(useAssetStore.getState().pages.get(0)).toEqual([asset]);

    api.searchAssets = async () => {
      throw 'fts5: syntax error near "."';
    };
    await useAssetStore.getState().search();

    const s = useAssetStore.getState();
    expect(s.searchError).toBe('fts5: syntax error near "."');
    expect(s.pages.size).toBe(0);
    expect(s.total).toBe(0);
    expect(s.loading).toBe(false);
  });

  test('a failed refresh keeps the current results, which are for the same query', async () => {
    api.searchAssets = async () => found;
    await useAssetStore.getState().search();

    api.searchAssets = async () => {
      throw 'database is locked';
    };
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    await useAssetStore.getState().refresh();
    consoleError.mockRestore();

    expect(useAssetStore.getState().searchError).toBeNull();
    expect(useAssetStore.getState().pages.get(0)).toEqual([asset]);
  });

  test('the next successful search clears the error', async () => {
    api.searchAssets = async () => {
      throw 'boom';
    };
    await useAssetStore.getState().search();
    api.searchAssets = async () => found;
    await useAssetStore.getState().search();

    expect(useAssetStore.getState().searchError).toBeNull();
    expect(useAssetStore.getState().getById('a1')).toEqual(asset);
  });
});

describe('useAssetStore page cache', () => {
  test('search fetches page 0 with the total; later pages never count it again', async () => {
    const all = library(1000);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([2, 3]);
    await flush();

    expect(calls.map((c) => [c.page, c.pageSize, c.includeTotal])).toEqual([
      [0, PAGE_SIZE, true],
      [2, PAGE_SIZE, false],
      [3, PAGE_SIZE, false],
    ]);
    const s = useAssetStore.getState();
    expect(s.total).toBe(1000);
    expect(s.assetAt(450)?.id).toBe('a450');
    expect(s.assetAt(250)).toBeUndefined(); // page 1 was never wanted
    expect(s.indexById.get('a799')).toBe(799);
  });

  test('a page already cached or in flight is not fetched again', async () => {
    const all = library(1000);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([0, 1]);
    useAssetStore.getState().ensurePages([1, 2]);
    await flush();
    expect(calls.map((c) => c.page)).toEqual([0, 1, 2]);
    expect(useAssetStore.getState().pending.size).toBe(0);
  });

  test('a page that arrives after a new search is dropped', async () => {
    let all = library(1000);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    let release!: () => void;
    const served = api.searchAssets;
    api.searchAssets = async (payload) => {
      await new Promise<void>((resolve) => (release = resolve));
      return served(payload);
    };
    useAssetStore.getState().ensurePages([3]);
    api.searchAssets = served;
    all = library(1000).map((a) => ({ ...a, id: `new-${a.id}` }));
    await useAssetStore.getState().search();
    release();
    await flush();

    const s = useAssetStore.getState();
    expect(s.pages.has(3)).toBe(false);
    expect(s.assetAt(0)?.id).toBe('new-a0');
    expect(calls.length).toBe(3);
  });

  test(`keeps at most ${MAX_PAGES} pages, evicting the least recently wanted`, async () => {
    const all = library(40 * PAGE_SIZE);
    serve(() => all);
    await useAssetStore.getState().search();
    for (let p = 1; p < 30; p++) {
      useAssetStore.getState().ensurePages([p]);
      await flush();
    }
    const s = useAssetStore.getState();
    expect(s.pages.size).toBe(MAX_PAGES);
    expect([...s.pages.keys()].sort((a, b) => a - b)).toEqual(
      Array.from({ length: MAX_PAGES }, (_, i) => i + 5)
    );
    expect(s.indexById.has('a0')).toBe(false);
    expect(s.indexById.size).toBe(MAX_PAGES * PAGE_SIZE);
  });

  test('a selected asset outlives its page, and getById still finds it', async () => {
    const all = library(40 * PAGE_SIZE);
    serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([1]);
    await flush();
    useUIStore.getState().selectAsset('a250');
    for (let p = 2; p < 30; p++) {
      useAssetStore.getState().ensurePages([p]);
      await flush();
    }
    const s = useAssetStore.getState();
    expect(s.pages.has(1)).toBe(false);
    expect(s.getById('a250')?.id).toBe('a250');

    useUIStore.getState().clearSelection();
    expect(useAssetStore.getState().getById('a250')).toBeUndefined();
  });

  test('patchCached replaces the asset in its one page and in its pinned copy', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([0, 1, 2]);
    await flush();
    useUIStore.getState().focusAsset('a210');
    const before = useAssetStore.getState();

    before.patchCached('a210', (a) => ({ ...a, isFavorite: true }));

    const after = useAssetStore.getState();
    expect(after.pages.get(0)).toBe(before.pages.get(0)!);
    expect(after.pages.get(2)).toBe(before.pages.get(2)!);
    expect(after.pages.get(1)).not.toBe(before.pages.get(1)!);
    expect(after.assetAt(210)?.isFavorite).toBe(true);
    expect(after.pinned.get('a210')?.isFavorite).toBe(true);
    expect(after.getById('a210')).toBe(after.assetAt(210)!);
  });

  test('refresh refetches the wanted pages and the total, then re-anchors on the top asset', async () => {
    let all = library(1000);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([2]);
    await flush();
    useAssetStore.getState().setViewTop(450);
    calls.length = 0;

    // Two new assets sort ahead of the top one
    all = [named('n1'), named('n2'), ...all];
    const stale = useAssetStore.getState().pages.get(2);
    const refreshing = useAssetStore.getState().refresh();
    expect(useAssetStore.getState().pages.get(2)).toBe(stale!); // stays on screen meanwhile
    await refreshing;

    expect(calls.map((c) => [c.page, c.includeTotal])).toEqual([
      [1, true],
      [2, false],
      [3, false],
    ]);
    const s = useAssetStore.getState();
    expect(s.total).toBe(1002);
    expect(s.anchor).toEqual({ from: 450, to: 452 });
    expect(s.assetAt(452)?.id).toBe('a450');
    expect(s.pages.has(0)).toBe(false); // stale, not refetched
  });

  test('a refresh that no longer finds the top asset keeps its index, clamped to the total', async () => {
    let all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([4]);
    await flush();
    useAssetStore.getState().setViewTop(990);

    all = all.slice(0, 900);
    await useAssetStore.getState().refresh();

    const s = useAssetStore.getState();
    expect(s.total).toBe(900);
    expect(s.anchor).toEqual({ from: 990, to: 899 });
  });
});

describe('useAssetStore deleteAssets', () => {
  test('after a partial failure, rethrows, deselects only the deleted assets and refreshes', async () => {
    let all = ['a', 'b', 'c', 'd'].map(named);
    serve(() => all);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAll(['a', 'b', 'c']);
    useUIStore.getState().focusAsset('a');
    const failure: DeleteAssetsError = {
      message: 'Could not move b.png to the trash, so it was not deleted: denied',
      failedIds: ['b'],
    };
    api.deleteAssets = async () => {
      all = all.filter((a) => a.id === 'b' || a.id === 'd');
      throw failure;
    };

    await expect(useAssetStore.getState().deleteAssets(['a', 'b', 'c'])).rejects.toEqual(failure);

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['b']);
    expect(useUIStore.getState().focusedAssetId).toBeNull();
    expect(
      useAssetStore
        .getState()
        .pages.get(0)
        ?.map((a) => a.id)
    ).toEqual(['b', 'd']);
    expect(useAssetStore.getState().total).toBe(2);
  });

  test('a rejection without failed ids keeps every asset', async () => {
    const before = ['a', 'b'].map(named);
    serve(() => before);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAll(['a', 'b']);
    api.deleteAssets = async () => {
      throw 'IPC failed';
    };

    await expect(useAssetStore.getState().deleteAssets(['a', 'b'])).rejects.toBe('IPC failed');

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['a', 'b']);
    expect(useAssetStore.getState().pages.get(0)).toEqual(before);
  });
});

describe('useAssetStore updateAssetPreview', () => {
  const thumbnailed = { ...asset, thumbnailPath: '/lib/.opendam/thumbs/a1_thumb.png' } as Asset;

  async function loadAsset() {
    api.searchAssets = async () => ({ ...found, assets: [thumbnailed] });
    await useAssetStore.getState().search();
  }

  test('a null thumbnail clears the stored path', async () => {
    await loadAsset();

    useAssetStore.getState().updateAssetPreview('a1', null);

    const s = useAssetStore.getState();
    expect(s.assetMap.get('a1')?.thumbnailPath).toBeUndefined();
    expect(s.assets[0].thumbnailPath).toBeUndefined();
    expect(s.assets[0].previewStatus).toBe('done');
  });

  test('a new thumbnail replaces the stored path', async () => {
    await loadAsset();

    useAssetStore.getState().updateAssetPreview('a1', '/lib/.opendam/thumbs/a1_thumb.jpg');

    expect(useAssetStore.getState().assets[0].thumbnailPath).toBe(
      '/lib/.opendam/thumbs/a1_thumb.jpg'
    );
  });
});

describe('useAutotagStore applyUpdate', () => {
  test('re-fetches only the changed assets that are cached', async () => {
    const loaded = ['a', 'b'].map((id) => ({ ...named(id), tags: [] }) as unknown as Asset);
    serve(() => loaded);
    await useAssetStore.getState().search();
    const fetched: string[] = [];
    api.getAsset = async (id) => {
      fetched.push(id);
      return { id, fileName: `${id}.png`, tags: ['t-rock'] } as unknown as Asset;
    };

    useAutotagStore.getState().applyUpdate({
      runs: [],
      changedAssetIds: ['b', 'not-loaded'],
      tagsChanged: false,
    });
    await flush();

    expect(fetched).toEqual(['b']);
    const s = useAssetStore.getState();
    expect(s.pages.get(0)?.map((a) => a.tags)).toEqual([[], ['t-rock']]);
    expect(s.getById('b')?.tags).toEqual(['t-rock']);
  });

  test('re-fetches the tags only when a run created some', async () => {
    let listed = 0;
    const moss = { id: 't-moss', name: 'Moss' } as Tag;
    api.listTags = async () => {
      listed += 1;
      return [moss];
    };
    const update = (tagsChanged: boolean) =>
      useAutotagStore.getState().applyUpdate({ runs: [], changedAssetIds: [], tagsChanged });

    update(false);
    await flush();
    expect(listed).toBe(0);

    update(true);
    await flush();
    expect(listed).toBe(1);
    expect(useTagStore.getState().tagMap.get('t-moss')).toEqual(moss);
  });
});

describe('useCollectionStore addToCollection', () => {
  test('adds the collection to the cached and pinned assets', async () => {
    const all = library(500);
    serve(() => all);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAsset('a3');
    api.addToCollection = async () => {};

    await useCollectionStore.getState().addToCollection('c-rocks', ['a3', 'a4', 'a450']);

    const s = useAssetStore.getState();
    expect(s.assetAt(3)?.collections).toEqual(['c-rocks']);
    expect(s.assetAt(4)?.collections).toEqual(['c-rocks']);
    expect(s.pinned.get('a3')?.collections).toEqual(['c-rocks']);
    expect(s.assetAt(5)?.collections).toEqual([]);
  });
});
