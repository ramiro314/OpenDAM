import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { useAssetStore, useCollectionStore, useLibraryStore, useTagStore } from '.';
import { applyFolderAction, folderAbsolutePath, revealFolder } from './folderActions';
import type { Collection, Library, Tag } from '../types';

const real = { ...api };
const realRefresh = useAssetStore.getState().refresh;

afterEach(() => {
  Object.assign(api, real);
  useAssetStore.setState({ refresh: realRefresh });
  useLibraryStore.setState({ libraries: [] });
  useCollectionStore.setState({ collections: [] });
  useTagStore.setState({ tags: [], tagMap: new Map() });
});

/** Records every call the action makes, answering with `written` files. */
function record(written: number) {
  const calls: string[] = [];
  api.tagFolder = async (libraryId, path, id) => {
    calls.push(`tag ${libraryId} '${path}' ${id}`);
    return written;
  };
  api.collectFolder = async (libraryId, path, id) => {
    calls.push(`collect ${libraryId} '${path}' ${id}`);
    return written;
  };
  api.listCollections = async () => {
    calls.push('list collections');
    return [{ id: 'c', name: 'cars', assetIds: ['a', 'b'] } as unknown as Collection];
  };
  api.listTags = async () => {
    calls.push('list tags');
    return [{ id: 't', name: 'vehicle' } as Tag];
  };
  useAssetStore.setState({
    refresh: async () => {
      calls.push('refresh grid');
    },
  });
  return calls;
}

describe('applyFolderAction', () => {
  test('tags the folder, then refetches the grid, the tags and the collection counts', async () => {
    const calls = record(4);
    const written = await applyFolderAction({ libraryId: 'kenney', path: 'Kit' }, 'tag', 't');
    expect(written).toBe(4);
    expect(calls[0]).toBe("tag kenney 'Kit' t");
    expect(calls.slice(1).sort()).toEqual(['list collections', 'list tags', 'refresh grid']);
    expect(useCollectionStore.getState().collections[0].assetIds).toEqual(['a', 'b']);
  });

  test("adds the library root's every file to a collection", async () => {
    const calls = record(9);
    expect(await applyFolderAction({ libraryId: 'kenney', path: '' }, 'collection', 'c')).toBe(9);
    expect(calls[0]).toBe("collect kenney '' c");
  });

  test('refetches nothing when the write fails', async () => {
    const calls = record(0);
    api.tagFolder = async () => {
      throw 'The tag no longer exists';
    };
    await expect(applyFolderAction({ libraryId: 'k', path: 'Kit' }, 'tag', 'gone')).rejects.toBe(
      'The tag no longer exists'
    );
    expect(calls).toEqual([]);
  });
});

describe('revealFolder', () => {
  test('reveals the folder below its library root, or the root itself', async () => {
    const revealed: string[] = [];
    api.revealInFileManager = async (path) => {
      revealed.push(path);
    };
    useLibraryStore.setState({
      libraries: [{ id: 'kenney', rootPaths: ['/mnt/kenney/'] } as Library],
    });
    await revealFolder({ libraryId: 'kenney', path: '3D assets/Car Kit' });
    await revealFolder({ libraryId: 'kenney', path: '' });
    expect(revealed).toEqual(['/mnt/kenney/3D assets/Car Kit', '/mnt/kenney/']);
    await expect(revealFolder({ libraryId: 'gone', path: '' })).rejects.toThrow();
  });

  test('folderAbsolutePath joins with one slash', () => {
    expect(folderAbsolutePath('/a', 'b/c')).toBe('/a/b/c');
    expect(folderAbsolutePath('/a//', 'b')).toBe('/a/b');
    expect(folderAbsolutePath('/a', '')).toBe('/a');
  });
});
