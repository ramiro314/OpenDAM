import { beforeEach, describe, expect, mock, test } from 'bun:test';
import type { Asset } from '../types';

const asset = (id: string) => ({ id, fileName: `${id}.png` }) as Asset;

// The backend after a multi-delete of a, b and c where only b's trash failed.
const remaining = [asset('b'), asset('d')];
mock.module('../api', () => ({
  api: {
    deleteAssets: async () => {
      throw 'Could not move b.png to the trash, so it was not deleted: denied';
    },
    searchAssets: async () => ({ assets: remaining, total: remaining.length }),
  },
}));

const { useAssetStore, useUIStore } = await import('./index');

describe('deleteAssets', () => {
  beforeEach(() => {
    const assets = [asset('a'), asset('b'), asset('c'), asset('d')];
    useAssetStore.setState({
      assets,
      total: assets.length,
      assetMap: new Map(assets.map((a) => [a.id, a])),
    });
    useUIStore.getState().selectAll(['a', 'b', 'c']);
    useUIStore.getState().focusAsset('a');
  });

  test('after a partial failure, rethrows and deselects only the assets that are gone', async () => {
    await expect(useAssetStore.getState().deleteAssets(['a', 'b', 'c'])).rejects.toBe(
      'Could not move b.png to the trash, so it was not deleted: denied'
    );

    expect(Array.from(useUIStore.getState().selectedAssetIds)).toEqual(['b']);
    expect(useUIStore.getState().focusedAssetId).toBeNull();
    expect(useAssetStore.getState().assets.map((a) => a.id)).toEqual(['b', 'd']);
  });
});
