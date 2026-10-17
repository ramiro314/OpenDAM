import { afterEach, describe, expect, spyOn, test } from 'bun:test';
import { api } from '../api';
import { useAssetStore, useAutotagStore, useUIStore } from '.';
import type { Asset, DeleteAssetsError, SearchResult } from '../types';

const realSearch = api.searchAssets;
const realDeleteAssets = api.deleteAssets;
const realGetAsset = api.getAsset;
const asset = { id: 'a1', fileName: 'tree.glb' } as Asset;
const found: SearchResult = { assets: [asset], total: 1, page: 0, pageSize: 100 };

afterEach(() => {
  api.searchAssets = realSearch;
  api.deleteAssets = realDeleteAssets;
  api.getAsset = realGetAsset;
});

describe('useAssetStore search errors', () => {
  test('a failed search shows the error and clears stale results', async () => {
    api.searchAssets = async () => found;
    await useAssetStore.getState().search();
    expect(useAssetStore.getState().assets).toEqual([asset]);

    api.searchAssets = async () => {
      throw 'fts5: syntax error near "."';
    };
    await useAssetStore.getState().search();

    const s = useAssetStore.getState();
    expect(s.searchError).toBe('fts5: syntax error near "."');
    expect(s.assets).toEqual([]);
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
    expect(useAssetStore.getState().assets).toEqual([asset]);
  });

  test('the next successful search clears the error', async () => {
    api.searchAssets = async () => {
      throw 'boom';
    };
    await useAssetStore.getState().search();
    api.searchAssets = async () => found;
    await useAssetStore.getState().search();

    expect(useAssetStore.getState().searchError).toBeNull();
    expect(useAssetStore.getState().assets).toEqual([asset]);
  });
});

describe('useAssetStore deleteAssets', () => {
  test('after a partial failure, rethrows and drops only the assets that were deleted', async () => {
    const named = (id: string) => ({ id, fileName: `${id}.png` }) as Asset;
    const before = ['a', 'b', 'c', 'd'].map(named);
    useAssetStore.setState({
      assets: before,
      total: before.length,
      assetMap: new Map(before.map((a) => [a.id, a])),
    });
    useUIStore.getState().selectAll(['a', 'b', 'c']);
    useUIStore.getState().focusAsset('a');
    const failure: DeleteAssetsError = {
      message: 'Could not move b.png to the trash, so it was not deleted: denied',
      failedIds: ['b'],
    };
    api.deleteAssets = async () => {
      throw failure;
    };

    await expect(useAssetStore.getState().deleteAssets(['a', 'b', 'c'])).rejects.toEqual(failure);

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['b']);
    expect(useUIStore.getState().focusedAssetId).toBeNull();
    expect(useAssetStore.getState().assets.map((a) => a.id)).toEqual(['b', 'd']);
    expect(useAssetStore.getState().total).toBe(2);
  });

  test('a rejection without failed ids keeps every asset', async () => {
    const named = (id: string) => ({ id, fileName: `${id}.png` }) as Asset;
    const before = ['a', 'b'].map(named);
    useAssetStore.setState({
      assets: before,
      total: before.length,
      assetMap: new Map(before.map((a) => [a.id, a])),
    });
    useUIStore.getState().selectAll(['a', 'b']);
    api.deleteAssets = async () => {
      throw 'IPC failed';
    };

    await expect(useAssetStore.getState().deleteAssets(['a', 'b'])).rejects.toBe('IPC failed');

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['a', 'b']);
    expect(useAssetStore.getState().assets).toEqual(before);
  });
});

describe('useAutotagStore applyUpdate', () => {
  test('re-fetches only the changed assets that are loaded', async () => {
    const loaded = ['a', 'b'].map(
      (id) => ({ id, fileName: `${id}.png`, tags: [] }) as unknown as Asset
    );
    useAssetStore.setState({ assets: loaded, assetMap: new Map(loaded.map((a) => [a.id, a])) });
    const fetched: string[] = [];
    api.getAsset = async (id) => {
      fetched.push(id);
      return { id, fileName: `${id}.png`, tags: ['t-rock'] } as unknown as Asset;
    };

    useAutotagStore.getState().applyUpdate({ runs: [], changedAssetIds: ['b', 'not-loaded'] });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(fetched).toEqual(['b']);
    const s = useAssetStore.getState();
    expect(s.assets.map((a) => a.tags)).toEqual([[], ['t-rock']]);
    expect(s.assetMap.get('b')?.tags).toEqual(['t-rock']);
  });
});
