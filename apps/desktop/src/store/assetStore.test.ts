import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test';
import { api } from '../api';
import {
  cachedAssets,
  useAssetStore,
  useAutotagStore,
  useCollectionStore,
  useTagStore,
  useUIStore,
} from '.';
import { MAX_PAGES, PAGE_SIZE, RETRY_MS, emptyCache, retryDelay } from './pageCache';
import type { Asset, DeleteAssetsError, SearchPayload, SearchResult, Tag } from '../types';

const realSearch = api.searchAssets;
const realDeleteAssets = api.deleteAssets;
const realGetAsset = api.getAsset;
const realListTags = api.listTags;
const realAddToCollection = api.addToCollection;
const realRemoveFromCollection = api.removeFromCollection;
const realDeleteAsset = api.deleteAsset;

const named = (id: string) => ({ id, fileName: `${id}.png`, collections: [] }) as unknown as Asset;
const library = (n: number) => Array.from({ length: n }, (_, i) => named(`a${i}`));
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

async function until(predicate: () => boolean, timeoutMs: number) {
  const deadline = performance.now() + timeoutMs;
  while (!predicate()) {
    if (performance.now() > deadline) throw new Error(`Timed out after ${timeoutMs} ms`);
    await sleep(10);
  }
}

/** A `search_assets` that never answers until `release`, recording each request. */
function hold() {
  const calls: SearchPayload[] = [];
  const waiting: (() => void)[] = [];
  api.searchAssets = (payload) => {
    calls.push(payload);
    return new Promise((_, reject) => waiting.push(() => reject('released')));
  };
  return { calls, release: () => waiting.forEach((w) => w()) };
}

/** A `search_assets` answering pages from `assets` (read on every call). */
function pagesOf(assets: () => Asset[]): typeof api.searchAssets {
  return async (payload) => {
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
}

/** Serves `search_assets` pages from `assets`, recording each request. */
function serve(assets: () => Asset[]) {
  const calls: SearchPayload[] = [];
  const answer = pagesOf(assets);
  api.searchAssets = (payload) => {
    calls.push(payload);
    return answer(payload);
  };
  return calls;
}

beforeEach(() => {
  useAssetStore.setState({
    ...emptyCache(),
    total: 0,
    fileTotal: 0,
    pinned: new Map(),
    deleted: new Set(),
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
  // Stops any retry a test left scheduled: retries only run for the generation that failed
  useAssetStore.setState((s) => ({ generation: s.generation + 1 }));
  api.searchAssets = realSearch;
  api.deleteAssets = realDeleteAssets;
  api.getAsset = realGetAsset;
  api.listTags = realListTags;
  api.addToCollection = realAddToCollection;
  api.removeFromCollection = realRemoveFromCollection;
  api.deleteAsset = realDeleteAsset;
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

describe('useAssetStore filters', () => {
  test('picking a collection, a tag or a smart filter keeps "Show hidden files"', () => {
    const store = useAssetStore.getState();
    store.replaceFilter({ showHidden: true, isFavorite: true });

    store.resetFilter();
    expect(useAssetStore.getState().query.filter).toEqual({ showHidden: true });
    store.replaceFilter({ tags: ['t1'] });
    expect(useAssetStore.getState().query.filter).toEqual({ tags: ['t1'], showHidden: true });
    // A smart filter that sets it wins
    store.replaceFilter({ kinds: ['texture'], showHidden: false });
    expect(useAssetStore.getState().query.filter).toEqual({
      kinds: ['texture'],
      showHidden: false,
    });
    store.replaceFilter({});
  });

  test('with hidden files off, a reset clears every filter', () => {
    const store = useAssetStore.getState();
    store.replaceFilter({ isFavorite: true });
    store.resetFilter();
    expect(useAssetStore.getState().query.filter).toEqual({});
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

  test('search and refresh keep the files behind the items, counted with the total', async () => {
    let counts = { total: 2, fileTotal: 5 };
    api.searchAssets = async (payload) => ({
      assets: library(2),
      page: payload.page,
      pageSize: payload.pageSize,
      ...(payload.includeTotal !== false ? counts : {}),
    });
    await useAssetStore.getState().search();
    expect(useAssetStore.getState()).toMatchObject({ total: 2, fileTotal: 5 });

    counts = { total: 2, fileTotal: 4 };
    await useAssetStore.getState().refresh();
    expect(useAssetStore.getState()).toMatchObject({ total: 2, fileTotal: 4 });
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

describe('useAssetStore page cache guards and failures', () => {
  test('a page that fails to load stays uncached, leaves pending, and is asked for again', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    api.searchAssets = async () => {
      throw 'database is locked';
    };
    useAssetStore.getState().ensurePages([2]);
    await flush();
    consoleError.mockRestore();

    expect(useAssetStore.getState().pages.has(2)).toBe(false);
    expect(useAssetStore.getState().pending.size).toBe(0);

    const calls = serve(() => all);
    useAssetStore.getState().ensurePages([2]);
    await flush();
    expect(calls.map((c) => c.page)).toEqual([2]);
    expect(useAssetStore.getState().assetAt(400)?.id).toBe('a400');
  });

  test('a failed page still wanted is asked for again on its own, waiting longer each time', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    // The first two requests for page 2 fail, the third succeeds
    const at: number[] = [];
    const answer = pagesOf(() => all);
    api.searchAssets = async (payload) => {
      at.push(performance.now());
      if (at.length <= 2) throw 'database is locked';
      return answer(payload);
    };
    useAssetStore.getState().ensurePages([2]);
    await until(() => useAssetStore.getState().pages.has(2), 5_000);
    consoleError.mockRestore();

    expect(at.length).toBe(3);
    // Timers never fire early; allow 1 ms for their rounding
    expect(at[1] - at[0]).toBeGreaterThanOrEqual(retryDelay(1) - 1);
    expect(at[2] - at[1]).toBeGreaterThanOrEqual(retryDelay(2) - 1);
    expect(useAssetStore.getState().assetAt(400)?.id).toBe('a400');
  });

  test('a failed page is not asked for again once the grid no longer wants it', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    const calls: number[] = [];
    api.searchAssets = async (payload) => {
      calls.push(payload.page);
      throw 'database is locked';
    };
    useAssetStore.getState().ensurePages([2]);
    await flush();
    useAssetStore.getState().ensurePages([0]); // scrolled back to the top, which is cached
    await sleep(RETRY_MS * 2);
    consoleError.mockRestore();

    expect(calls).toEqual([2]);
  });

  test('a failed page is not asked for again after a new search', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    api.searchAssets = async () => {
      throw 'database is locked';
    };
    useAssetStore.getState().ensurePages([2]);
    await flush();
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    await sleep(RETRY_MS * 2);
    consoleError.mockRestore();

    expect(calls.map((c) => c.page)).toEqual([0]);
  });

  test('a failed refresh keeps the stale pages and total, then asks for the wanted pages again', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    // Page 2 is in flight when the refresh starts, which drops its answer
    hold();
    useAssetStore.getState().ensurePages([2]);
    useAssetStore.getState().setViewTop(400);
    // The refresh's first request, which counts the total, fails; page requests succeed
    const calls: SearchPayload[] = [];
    const answer = pagesOf(() => all);
    api.searchAssets = async (payload) => {
      calls.push(payload);
      if (payload.includeTotal !== false) throw 'database is locked';
      return answer(payload);
    };
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    await useAssetStore.getState().refresh();
    await flush();
    consoleError.mockRestore();

    const s = useAssetStore.getState();
    expect(s.total).toBe(1000);
    expect(s.anchor).toBeNull();
    expect(s.assetAt(0)?.id).toBe('a0');
    expect(calls.at(-1)).toMatchObject({ page: 2, includeTotal: false });
    expect(s.assetAt(400)?.id).toBe('a400');
    expect(s.pending.size).toBe(0);
  });

  test('a refresh during a search runs the search again instead', async () => {
    const first = hold();
    const searching = useAssetStore.getState().search();
    const all = library(1000);
    const calls = serve(() => all);
    useAssetStore.getState().setViewTop(400);
    await useAssetStore.getState().refresh();
    first.release();
    await searching;

    expect(calls.map((c) => [c.page, c.includeTotal])).toEqual([[0, true]]);
    const s = useAssetStore.getState();
    expect(s.total).toBe(1000);
    expect(s.anchor).toBeNull();
    expect(s.searchError).toBeNull();
    expect(s.loading).toBe(false);
  });

  test('ensurePages asks for nothing before the first page or past the last one', async () => {
    const all = library(1000);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    calls.length = 0;

    useAssetStore.getState().ensurePages([-1, 5, 9]);
    await flush();

    expect(calls).toEqual([]);
    expect(useAssetStore.getState().wanted).toEqual([]);
  });

  test('ensurePages asks for nothing while a search is loading or with no results', async () => {
    const all = library(1000);
    serve(() => all);
    await useAssetStore.getState().search();
    const pending = hold();
    void useAssetStore.getState().search();
    useAssetStore.getState().ensurePages([1, 2]);
    expect(pending.calls.map((c) => c.page)).toEqual([0]);
    expect(useAssetStore.getState().wanted).toEqual([1, 2]);
    pending.release();
    await flush();

    // The failed search left no results
    const calls = serve(() => all);
    useAssetStore.getState().ensurePages([0]);
    await flush();
    expect(useAssetStore.getState().total).toBe(0);
    expect(calls).toEqual([]);
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

  test('a successful delete deselects the assets and refetches the page without them', async () => {
    let all = ['a', 'b', 'c', 'd'].map(named);
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAll(['a', 'c']);
    api.deleteAssets = async (ids) => {
      all = all.filter((a) => !ids.includes(a.id));
    };
    calls.length = 0;

    await useAssetStore.getState().deleteAssets(['a', 'c']);

    expect(calls.map((c) => [c.page, c.includeTotal])).toEqual([[0, true]]);
    expect(useUIStore.getState().selectedAssetIds.size).toBe(0);
    expect(
      useAssetStore
        .getState()
        .pages.get(0)
        ?.map((a) => a.id)
    ).toEqual(['b', 'd']);
    expect(useAssetStore.getState().total).toBe(2);
  });

  test('deleting one asset unfocuses it and refetches the page without it', async () => {
    let all = ['a', 'b'].map(named);
    serve(() => all);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAsset('b');
    api.deleteAsset = async (id) => {
      all = all.filter((a) => a.id !== id);
    };

    await useAssetStore.getState().deleteAsset('b');

    expect(useUIStore.getState().focusedAssetId).toBeNull();
    expect(useAssetStore.getState().getById('b')).toBeUndefined();
    expect(useAssetStore.getState().total).toBe(1);
  });

  test('a delete whose refresh fails masks the deleted assets, and the retried refresh drops them', async () => {
    let all = ['a', 'b', 'c', 'd'].map(named);
    const answer = pagesOf(() => all);
    let refreshes = 0;
    api.searchAssets = async (payload) => {
      // The first refresh after the delete times out, as when the pool is starved
      if (payload.includeTotal !== false && refreshes++ === 1) {
        throw 'pool timed out while waiting for an open connection';
      }
      return answer(payload);
    };
    await useAssetStore.getState().search();
    useUIStore.getState().selectAll(['a']);
    api.deleteAssets = async (ids) => {
      all = all.filter((a) => !ids.includes(a.id));
    };
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});

    await useAssetStore.getState().deleteAssets(['a']);

    let s = useAssetStore.getState();
    expect(s.pages.get(0)?.map((a) => a.id)).toEqual(['a', 'b', 'c', 'd']); // still stale
    expect(s.deleted).toEqual(new Set(['a']));
    expect(s.getById('a')).toBeUndefined();
    expect(cachedAssets(s).map((a) => a.id)).toEqual(['b', 'c', 'd']);

    await until(() => useAssetStore.getState().total === 3, 5_000);
    consoleError.mockRestore();
    s = useAssetStore.getState();
    expect(s.pages.get(0)?.map((a) => a.id)).toEqual(['b', 'c', 'd']);
    expect(s.deleted.size).toBe(0);
  });

  test('a refresh of a query whose search failed is not retried', async () => {
    let calls = 0;
    api.searchAssets = async () => {
      calls += 1;
      throw 'fts5: syntax error near "."';
    };
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    await useAssetStore.getState().search();
    await useAssetStore.getState().refresh(); // library:updated still tries once
    await sleep(RETRY_MS * 2);
    consoleError.mockRestore();

    expect(calls).toBe(2);
    expect(useAssetStore.getState().searchError).toBe('fts5: syntax error near "."');
  });

  test('a search resets the refresh backoff', async () => {
    const all = library(10);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    let refreshes = 0;
    api.searchAssets = async () => {
      refreshes += 1;
      throw 'database is locked';
    };
    // Three failures in a row: the next retry would wait retryDelay(4)
    void useAssetStore.getState().refresh();
    await until(() => refreshes === 3, retryDelay(1) + retryDelay(2) + 2_000);
    serve(() => all);
    await useAssetStore.getState().search();
    api.searchAssets = async () => {
      refreshes += 1;
      throw 'database is locked';
    };
    const before = refreshes;
    await useAssetStore.getState().refresh();
    // Retried from the first step of the backoff, well before retryDelay(4)
    await until(() => refreshes === before + 2, retryDelay(4) - 500);
    consoleError.mockRestore();

    expect(refreshes).toBe(before + 2);
  });

  test('a failed refresh is not retried once a newer search ran', async () => {
    const all = library(10);
    serve(() => all);
    await useAssetStore.getState().search();
    const consoleError = spyOn(console, 'error').mockImplementation(() => {});
    api.searchAssets = async () => {
      throw 'database is locked';
    };
    await useAssetStore.getState().refresh();
    const calls = serve(() => all);
    await useAssetStore.getState().search();
    await sleep(RETRY_MS * 2);
    consoleError.mockRestore();

    expect(calls.map((c) => c.page)).toEqual([0]);
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
  const thumbnailed = { ...named('a1'), thumbnailPath: '/lib/.opendam/thumbs/a1_thumb.png' };

  async function loadAsset() {
    serve(() => [thumbnailed]);
    await useAssetStore.getState().search();
    // Focused, so it has a pinned copy too
    useUIStore.getState().focusAsset('a1');
  }

  test('a null thumbnail clears the stored path', async () => {
    await loadAsset();

    useAssetStore.getState().updateAssetPreview('a1', null);

    const s = useAssetStore.getState();
    expect(s.pinned.get('a1')?.thumbnailPath).toBeUndefined();
    expect(s.assetAt(0)?.thumbnailPath).toBeUndefined();
    expect(s.assetAt(0)?.previewStatus).toBe('done');
  });

  test('a new thumbnail replaces the stored path', async () => {
    await loadAsset();

    useAssetStore.getState().updateAssetPreview('a1', '/lib/.opendam/thumbs/a1_thumb.jpg');

    expect(useAssetStore.getState().assetAt(0)?.thumbnailPath).toBe(
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

describe('useCollectionStore membership', () => {
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

  test('removes the collection from the cached and pinned assets', async () => {
    const all = library(500).map((a) => ({ ...a, collections: ['c-rocks'] }));
    serve(() => all);
    await useAssetStore.getState().search();
    useUIStore.getState().selectAsset('a3');
    api.removeFromCollection = async () => {};

    await useCollectionStore.getState().removeFromCollection('c-rocks', ['a3', 'a4', 'a450']);

    const s = useAssetStore.getState();
    expect(s.assetAt(3)?.collections).toEqual([]);
    expect(s.assetAt(4)?.collections).toEqual([]);
    expect(s.pinned.get('a3')?.collections).toEqual([]);
    expect(s.assetAt(5)?.collections).toEqual(['c-rocks']);
  });
});
