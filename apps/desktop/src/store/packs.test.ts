import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import type { PackCard } from '../types';
import { scopeKey, type PacksScope } from '../utils/packsView';
import { packKey, usePacksStore } from './packs';

const realGetPacks = api.getPacks;

const card = (path: string): PackCard => ({
  libraryId: 'kenney',
  path,
  name: path.slice(path.lastIndexOf('/') + 1),
  parentPath: path.slice(0, Math.max(0, path.lastIndexOf('/'))),
  itemCount: 1,
  hiddenCount: 0,
});

const root: PacksScope = { libraryIds: ['kenney'], underPath: '' };
const audio: PacksScope = { libraryIds: ['kenney'], underPath: 'Audio' };
const store = () => usePacksStore.getState();

afterEach(() => {
  api.getPacks = realGetPacks;
  usePacksStore.setState({ key: null, scope: null, packs: null, selected: null, mode: 'packs' });
});

describe('usePacksStore', () => {
  test('a failed load leaves no packs, so the node shows Assets', async () => {
    api.getPacks = async () => {
      throw new Error('Library kenney is not open');
    };
    await store().load(root);
    expect(store().packs).toEqual([]);
    expect(store().key).toBe(scopeKey(root));
  });

  test('an earlier, slower load does not overwrite a later scope', async () => {
    let releaseRoot = () => {};
    api.getPacks = (_ids, underPath) =>
      underPath === ''
        ? new Promise((resolve) => (releaseRoot = () => resolve([card('3D assets/Car Kit')])))
        : Promise.resolve([card('Audio/Casino Audio')]);
    const slow = store().load(root);
    await store().load(audio);
    releaseRoot();
    await slow;
    expect(store().key).toBe(scopeKey(audio));
    expect(store().packs?.map((p) => p.path)).toEqual(['Audio/Casino Audio']);
  });

  test('a refetch keeps the selected card only while it is still listed', async () => {
    let paths = ['3D assets/Car Kit', '3D assets/Space Kit'];
    api.getPacks = async () => paths.map(card);
    await store().load(root);
    store().select(packKey(card('3D assets/Space Kit')));

    paths = ['3D assets/Car Kit', '3D assets/Space Kit', '3D assets/Tower Kit'];
    await store().refetch();
    expect(store().selected).toBe(packKey(card('3D assets/Space Kit')));

    paths = ['3D assets/Car Kit'];
    await store().refetch();
    expect(store().packs?.map((p) => p.path)).toEqual(['3D assets/Car Kit']);
    expect(store().selected).toBeNull();
  });

  test('loading the loaded scope again asks for nothing', async () => {
    let calls = 0;
    api.getPacks = async () => {
      calls++;
      return [card('3D assets/Car Kit')];
    };
    await store().load(root);
    await store().load({ ...root });
    expect(calls).toBe(1);
  });
});
