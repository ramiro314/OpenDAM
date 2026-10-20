import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import type { PackDetail } from '../types';
import { usePackDetailStore } from './packDetail';

const realGetPackDetail = api.getPackDetail;

const pack = (path: string, name = path): PackDetail => ({
  libraryId: 'kenney',
  path,
  name,
  parentPath: '3D assets',
  itemCount: 1,
  hiddenCount: 2,
  gallery: [],
  kinds: { texture: 3 },
  sizeBytes: 3,
  authors: [],
  edits: {},
});

const store = () => usePackDetailStore.getState();
const kit = { libraryId: 'kenney', path: '3D assets/Car Kit' };

afterEach(() => {
  api.getPackDetail = realGetPackDetail;
  usePackDetailStore.setState({ target: null, detail: null });
});

describe('usePackDetailStore', () => {
  test('loads the pack holding the folder once, until another folder or a refetch', async () => {
    const calls: string[] = [];
    api.getPackDetail = async (_id, path) => {
      calls.push(path);
      return path.startsWith('3D assets/Car Kit') ? pack('3D assets/Car Kit') : null;
    };
    await store().load({ libraryId: 'kenney', path: '3D assets/Car Kit/Models' });
    expect(store().detail?.path).toBe('3D assets/Car Kit');
    await store().load({ libraryId: 'kenney', path: '3D assets/Car Kit/Models' });
    expect(calls).toEqual(['3D assets/Car Kit/Models']);

    // A folder outside every pack shows none.
    await store().load({ libraryId: 'kenney', path: 'Goodies' });
    expect(store().detail).toBeNull();

    // `library:updated` asks again, so a renamed pack shows its new name.
    await store().load(kit);
    api.getPackDetail = async () => pack('3D assets/Car Kit', 'Car Kit Deluxe');
    await store().refetch();
    expect(store().detail?.name).toBe('Car Kit Deluxe');
  });

  test("an earlier, slower load doesn't overwrite a later folder's pack", async () => {
    let releaseKit = () => {};
    api.getPackDetail = (_id, path) =>
      path === kit.path
        ? new Promise((resolve) => (releaseKit = () => resolve(pack(kit.path))))
        : Promise.resolve(null);
    const slow = store().load(kit);
    await store().load({ libraryId: 'kenney', path: 'Goodies' });
    releaseKit();
    await slow;
    expect(store().target?.path).toBe('Goodies');
    expect(store().detail).toBeNull();
  });

  test('no folder clears the pack, and a failed load shows none', async () => {
    api.getPackDetail = async () => pack(kit.path);
    await store().load(kit);
    await store().load(null);
    expect(store().detail).toBeNull();
    api.getPackDetail = async () => {
      throw new Error('Library kenney is not open');
    };
    await store().load(kit);
    expect(store().detail).toBeNull();
  });
});
