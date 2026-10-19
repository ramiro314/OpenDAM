import { describe, expect, test } from 'bun:test';
import type { SearchQuery } from '../types';
import { browseView, isSearching, packsScope, scopeKey } from './packsView';

const query = (patch: Partial<SearchQuery> = {}): SearchQuery => ({
  text: '',
  filter: {},
  sort: { field: 'name', order: 'asc' },
  ...patch,
});

describe('the Packs/Assets switching rule', () => {
  test('a node with packs below it starts on Packs', () => {
    expect(browseView(true, query(), 'packs')).toBe('packs');
  });

  test('a node with no pack below it shows Assets, and waits while its packs load', () => {
    expect(browseView(false, query(), 'packs')).toBe('assets');
    expect(browseView(null, query(), 'packs')).toBe('loading');
  });

  test('the toggle on Assets shows Assets', () => {
    expect(browseView(true, query(), 'assets')).toBe('assets');
    expect(browseView(null, query(), 'assets')).toBe('assets');
  });

  test('search text or any filter shows Assets, and clearing it returns to the toggle', () => {
    const filtered: Partial<SearchQuery>[] = [
      { text: 'tree' },
      { filter: { kinds: ['texture'] } },
      { filter: { extensions: ['glb'] } },
      { filter: { showHidden: true } },
      { filter: { isFavorite: true } },
      { filter: { rating: { min: 3, max: 5 } } },
      { filter: { tags: ['t1'] } },
      { filter: { directory: 'Models' } },
      { filter: { namePrefix: 'tile_' } },
      { filter: { collectionIds: ['c1'] } },
    ];
    for (const patch of filtered) {
      expect(browseView(true, query(patch), 'packs')).toBe('assets');
      expect(browseView(null, query(patch), 'packs')).toBe('assets');
    }
    // Blank text, and a filter set back to nothing, filter nothing
    expect(browseView(true, query({ text: '  ' }), 'packs')).toBe('packs');
    expect(
      browseView(true, query({ filter: { kinds: [], extensions: [], tagLogic: 'or' } }), 'packs')
    ).toBe('packs');
  });

  test('isSearching sees search text and filters only', () => {
    expect(isSearching(query())).toBe(false);
    expect(isSearching(query({ text: 'x' }))).toBe(true);
    expect(isSearching(query({ filter: { showHidden: false } }))).toBe(false);
  });
});

describe('packsScope', () => {
  test('a folder looks below itself in its own library', () => {
    expect(packsScope(query({ folder: { libraryId: 'kenney', path: '3D assets' } }))).toEqual({
      libraryIds: ['kenney'],
      underPath: '3D assets',
    });
  });

  test('selected libraries look below their roots, and none selected below every root', () => {
    expect(packsScope(query({ libraryIds: ['a', 'b'], libraryId: undefined }))).toEqual({
      libraryIds: ['a', 'b'],
      underPath: '',
    });
    expect(packsScope(query({ libraryId: 'a' }))).toEqual({ libraryIds: ['a'], underPath: '' });
    expect(packsScope(query())).toEqual({ libraryIds: null, underPath: '' });
  });

  test('a collection is no tree node, so it has no packs', () => {
    expect(packsScope(query({ collectionId: 'c1' }))).toBeNull();
  });

  test('scopes share a key exactly when they are equal', () => {
    const key = (q: Partial<SearchQuery>) => scopeKey(packsScope(query(q))!);
    expect(key({ libraryId: 'a' })).toBe(key({ libraryIds: ['a'] }));
    expect(key({})).not.toBe(key({ libraryId: 'a' }));
    expect(key({ folder: { libraryId: 'a', path: 'b' } })).not.toBe(
      key({ folder: { libraryId: 'a', path: '' } })
    );
  });
});
