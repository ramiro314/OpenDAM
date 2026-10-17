import { afterEach, describe, expect, test } from 'bun:test';
import { api } from '../api';
import { useLibraryStore } from '.';
import type { Library } from '../types';

const realAddLibrary = api.addLibrary;

function library(id: string, assetCount = 0): Library {
  return { id, name: id, rootPaths: [`/mnt/${id}`], createdAt: 0, assetCount };
}

afterEach(() => {
  api.addLibrary = realAddLibrary;
  useLibraryStore.setState({ libraries: [] });
});

describe('useLibraryStore.addLibrary', () => {
  test('a new library is appended', async () => {
    useLibraryStore.setState({ libraries: [library('kenney')] });
    api.addLibrary = async () => library('sprites');

    await useLibraryStore.getState().addLibrary('sprites', ['/mnt/sprites']);

    expect(useLibraryStore.getState().libraries.map((l) => l.id)).toEqual(['kenney', 'sprites']);
  });

  test('a reopened offline library replaces its entry instead of being listed twice', async () => {
    useLibraryStore.setState({ libraries: [library('nas'), library('kenney')] });
    api.addLibrary = async () => library('nas', 12);

    await useLibraryStore.getState().addLibrary('NAS again', ['/mnt/nas']);

    const libraries = useLibraryStore.getState().libraries;
    expect(libraries.map((l) => l.id)).toEqual(['nas', 'kenney']);
    expect(libraries[0].assetCount).toBe(12);
  });
});
