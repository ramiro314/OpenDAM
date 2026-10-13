import { afterEach, describe, expect, spyOn, test } from 'bun:test';
import { api } from '../api';
import { useAssetStore, useUIStore } from '.';
import type { Asset, SearchResult } from '../types';

const realSearch = api.searchAssets;
const realDeleteAssets = api.deleteAssets;
const asset = { id: 'a1', fileName: 'tree.glb' } as Asset;
const found: SearchResult = { assets: [asset], total: 1, page: 0, pageSize: 100 };

afterEach(() => {
  api.searchAssets = realSearch;
  api.deleteAssets = realDeleteAssets;
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
  test('after a partial failure, rethrows and deselects only the assets that are gone', async () => {
    const named = (id: string) => ({ id, fileName: `${id}.png` }) as Asset;
    const before = ['a', 'b', 'c', 'd'].map(named);
    useAssetStore.setState({
      assets: before,
      total: before.length,
      assetMap: new Map(before.map((a) => [a.id, a])),
    });
    useUIStore.getState().selectAll(['a', 'b', 'c']);
    useUIStore.getState().focusAsset('a');
    // The backend after deleting a, b and c where only b's trash failed.
    const after = ['b', 'd'].map(named);
    api.deleteAssets = async () => {
      throw 'Could not move b.png to the trash, so it was not deleted: denied';
    };
    api.searchAssets = async () => ({ assets: after, total: after.length, page: 0, pageSize: 100 });

    await expect(useAssetStore.getState().deleteAssets(['a', 'b', 'c'])).rejects.toBe(
      'Could not move b.png to the trash, so it was not deleted: denied'
    );

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['b']);
    expect(useUIStore.getState().focusedAssetId).toBeNull();
    expect(useAssetStore.getState().assets).toEqual(after);
  });
});
