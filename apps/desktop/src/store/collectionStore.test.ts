import { afterEach, describe, expect, test } from 'bun:test';
import { selectCollectionNames, useCollectionStore } from '.';
import type { Collection } from '../types';

const rocks = { id: 'c-rocks', name: 'Rocks', assetIds: ['a1'] } as unknown as Collection;
const trees = { id: 'c-trees', name: 'Trees', assetIds: [] } as unknown as Collection;

afterEach(() => useCollectionStore.setState({ collections: [] }));

describe('selectCollectionNames', () => {
  test('maps each collection id to its name', () => {
    useCollectionStore.setState({ collections: [rocks, trees] });
    const names = selectCollectionNames(useCollectionStore.getState());
    expect([...names]).toEqual([
      ['c-rocks', 'Rocks'],
      ['c-trees', 'Trees'],
    ]);
  });

  test('every caller shares one map until the collection list changes', () => {
    useCollectionStore.setState({ collections: [rocks] });
    const first = selectCollectionNames(useCollectionStore.getState());
    // A card rendering later, or a store update that leaves the list alone, gets the same map
    useCollectionStore.setState({});
    expect(selectCollectionNames(useCollectionStore.getState())).toBe(first);

    useCollectionStore.setState({ collections: [{ ...rocks, name: 'Boulders' }, trees] });
    const next = selectCollectionNames(useCollectionStore.getState());
    expect(next).not.toBe(first);
    expect(next.get('c-rocks')).toBe('Boulders');
    expect(next.get('c-trees')).toBe('Trees');
  });
});
