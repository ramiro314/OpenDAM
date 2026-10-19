import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { openPack, useAssetStore, useFolderTreeStore, useUIStore } from '.';
import type { FolderNode, SearchResult } from '../types';

const realSearch = api.searchAssets;
const realGetFolders = api.getFolders;

const node = (path: string): FolderNode => ({
  name: path.slice(path.lastIndexOf('/') + 1),
  path,
  isPack: false,
  itemCount: 1,
  hiddenCount: 0,
  hasChildren: true,
  shownWithoutHidden: true,
  hasVisibleChildren: true,
});

function serve() {
  api.searchAssets = async (): Promise<SearchResult> => ({
    assets: [],
    total: 0,
    fileTotal: 0,
    page: 0,
    pageSize: 200,
  });
  api.getFolders = async (_id, parent) =>
    parent === '' ? [node('3D assets')] : [node(`${parent}/Car Kit`)];
}

afterEach(() => {
  api.searchAssets = realSearch;
  api.getFolders = realGetFolders;
  useFolderTreeStore.setState({ children: {}, expanded: {} });
  useUIStore.getState().clearLibrarySelection();
  useAssetStore
    .getState()
    .setQuery({ libraryIds: undefined, libraryId: undefined, folder: undefined });
});

describe('openPack', () => {
  test("selects a pack's folder in the tree, expanding the folders above it", async () => {
    serve();
    useUIStore.getState().setActiveLibrary('other');
    await openPack({ libraryId: 'kenney', path: '3D assets/Car Kit' });
    const { query } = useAssetStore.getState();
    expect(query.folder).toEqual({ libraryId: 'kenney', path: '3D assets/Car Kit' });
    expect(query.libraryId).toBeUndefined();
    expect(useUIStore.getState().selectedLibraryIds).toEqual([]);
    expect(useFolderTreeStore.getState().expanded.kenney).toEqual(['', '3D assets']);
  });

  test('selects the library row of a library that is itself one pack', async () => {
    serve();
    useAssetStore.getState().setQuery({ folder: { libraryId: 'kenney', path: '3D assets' } });
    await openPack({ libraryId: 'solo', path: '' });
    const { query } = useAssetStore.getState();
    expect(query.folder).toBeUndefined();
    expect([query.libraryId, query.libraryIds]).toEqual(['solo', ['solo']]);
    expect(useUIStore.getState().selectedLibraryIds).toEqual(['solo']);
    expect(useFolderTreeStore.getState().expanded.solo).toBeUndefined();
  });
});
