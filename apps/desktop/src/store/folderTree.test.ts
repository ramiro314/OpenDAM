import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { useFolderTreeStore } from './folderTree';
import type { FolderNode } from '../types';

const realGetFolders = api.getFolders;

/** A `get_folders` over `tree` (library id, then parent path, to child paths), recording calls. */
function serve(tree: () => Record<string, Record<string, string[]>>) {
  const calls: string[] = [];
  api.getFolders = async (libraryId, parentPath) => {
    calls.push(`${libraryId}:${parentPath}`);
    const library = tree()[libraryId];
    if (!library) throw new Error(`Library ${libraryId} is not open`);
    const childPaths = library[parentPath] ?? [];
    return childPaths.map((path): FolderNode => ({
      name: path.slice(path.lastIndexOf('/') + 1),
      path,
      isPack: false,
      itemCount: 1,
      hiddenCount: 0,
      hasChildren: (library[path] ?? []).length > 0,
      holdsPack: false,
      hasVisibleChildren: (library[path] ?? []).length > 0,
    }));
  };
  return calls;
}

const tree = () => useFolderTreeStore.getState();
const childPaths = (libraryId: string, path: string) =>
  tree().children[libraryId]?.[path]?.map((n) => n.path);

afterEach(() => {
  api.getFolders = realGetFolders;
  useFolderTreeStore.setState({ children: {}, expanded: {} });
});

const KENNEY = {
  '': ['2D assets', '3D assets'],
  '3D assets': ['3D assets/Nature Kit', '3D assets/Platformer Kit'],
  '3D assets/Platformer Kit': ['3D assets/Platformer Kit/Models'],
};

describe('useFolderTreeStore', () => {
  test('a node loads its children the first time it expands, and only then', async () => {
    const calls = serve(() => ({ kenney: KENNEY }));

    await tree().toggle('kenney', '');
    expect(childPaths('kenney', '')).toEqual(['2D assets', '3D assets']);
    expect(calls).toEqual(['kenney:']);

    await tree().toggle('kenney', '3D assets');
    expect(childPaths('kenney', '3D assets')).toEqual([
      '3D assets/Nature Kit',
      '3D assets/Platformer Kit',
    ]);

    // Collapsing keeps the children, so expanding again asks for nothing
    await tree().toggle('kenney', '3D assets');
    expect(tree().expanded.kenney).toEqual(['']);
    await tree().toggle('kenney', '3D assets');
    expect(tree().expanded.kenney).toEqual(['', '3D assets']);
    expect(calls).toEqual(['kenney:', 'kenney:3D assets']);
  });

  test('each library keeps its own expanded nodes', async () => {
    serve(() => ({ kenney: KENNEY, sprites: { '': ['3D assets'] } }));

    await tree().toggle('kenney', '');
    await tree().toggle('kenney', '3D assets');
    await tree().toggle('sprites', '');

    expect(tree().expanded).toEqual({ kenney: ['', '3D assets'], sprites: [''] });
    expect(childPaths('sprites', '')).toEqual(['3D assets']);
    expect(childPaths('sprites', '3D assets')).toBeUndefined();
  });

  test('library:updated refetches every expanded node and forgets collapsed ones', async () => {
    let current: Record<string, string[]> = KENNEY;
    const calls = serve(() => ({ kenney: current }));
    await tree().toggle('kenney', '');
    await tree().toggle('kenney', '3D assets');
    await tree().toggle('kenney', '3D assets/Platformer Kit');
    await tree().toggle('kenney', '3D assets/Platformer Kit'); // collapsed again
    calls.length = 0;

    // A delete emptied `Nature Kit`
    current = { ...KENNEY, '3D assets': ['3D assets/Platformer Kit'] };
    await tree().refetch();

    expect(calls.sort()).toEqual(['kenney:', 'kenney:3D assets']);
    expect(childPaths('kenney', '3D assets')).toEqual(['3D assets/Platformer Kit']);
    expect(childPaths('kenney', '3D assets/Platformer Kit')).toBeUndefined();

    calls.length = 0;
    await tree().toggle('kenney', '3D assets/Platformer Kit');
    expect(calls).toEqual(['kenney:3D assets/Platformer Kit']);
  });

  test('a refetch collapses expanded nodes whose folder is gone, and their descendants', async () => {
    let current: Record<string, string[]> = KENNEY;
    serve(() => ({ kenney: current }));
    await tree().toggle('kenney', '');
    await tree().toggle('kenney', '3D assets');
    await tree().toggle('kenney', '3D assets/Platformer Kit');

    current = { '': ['2D assets'] };
    await tree().refetch();

    expect(tree().expanded.kenney).toEqual(['']);
    expect(Object.keys(tree().children.kenney)).toEqual(['']);
    expect(childPaths('kenney', '')).toEqual(['2D assets']);
  });

  test('a library that closed drops its tree on the next refetch', async () => {
    let libraries: Record<string, Record<string, string[]>> = {
      kenney: KENNEY,
      nas: { '': ['models'] },
    };
    serve(() => libraries);
    await tree().toggle('kenney', '');
    await tree().toggle('nas', '');

    libraries = { kenney: KENNEY };
    await tree().refetch();

    expect(tree().expanded).toEqual({ kenney: [''], nas: [] });
    expect(childPaths('nas', '')).toBeUndefined();
    expect(childPaths('kenney', '')).toEqual(['2D assets', '3D assets']);
  });

  test('when refetches overlap, an earlier one resolving last does not overwrite the latest', async () => {
    serve(() => ({ kenney: KENNEY }));
    await tree().toggle('kenney', '');

    // The first refetch read the folders before a delete emptied `2D assets`, and answers last
    let releaseStale = () => {};
    api.getFolders = () =>
      new Promise<FolderNode[]>((resolve) => {
        releaseStale = () =>
          resolve(
            ['2D assets', '3D assets'].map((path) => ({
              name: path,
              path,
              isPack: false,
              itemCount: 1,
              hiddenCount: 0,
              hasChildren: true,
              holdsPack: false,
              hasVisibleChildren: true,
            }))
          );
      });
    const stale = tree().refetch();
    serve(() => ({ kenney: { ...KENNEY, '': ['3D assets'] } }));
    await tree().refetch();
    releaseStale();
    await stale;

    expect(childPaths('kenney', '')).toEqual(['3D assets']);
  });

  test('a node expanded during a refetch stays expanded with what it loaded', async () => {
    serve(() => ({ kenney: KENNEY }));
    await tree().toggle('kenney', '');

    const refetching = tree().refetch();
    const expanding = tree().toggle('kenney', '3D assets');
    await Promise.all([refetching, expanding]);

    expect(tree().expanded.kenney).toEqual(['', '3D assets']);
    expect(childPaths('kenney', '3D assets')).toEqual([
      '3D assets/Nature Kit',
      '3D assets/Platformer Kit',
    ]);
  });
});
