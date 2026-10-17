import { describe, expect, test } from 'bun:test';
import {
  datePresets,
  fitInViewport,
  isFilterEmpty,
  isNameTaken,
  libraryNameFromPath,
  parentCandidates,
  resolveDroppedAssetId,
} from './sidebarHelpers';

describe('fitInViewport', () => {
  test('keeps a menu that fits at the cursor', () => {
    expect(fitInViewport(100, 200, 150, 70, 1400, 900)).toEqual({ left: 100, top: 200 });
  });

  test('moves a menu opened near the bottom or right edge back inside', () => {
    expect(fitInViewport(1350, 880, 150, 70, 1400, 900)).toEqual({ left: 1246, top: 826 });
  });

  test('pins a menu larger than the viewport to the top-left margin', () => {
    expect(fitInViewport(50, 50, 500, 500, 300, 300)).toEqual({ left: 4, top: 4 });
  });
});

describe('parentCandidates', () => {
  const tags = [
    { id: 'nature' },
    { id: 'rock', parentId: 'nature' },
    { id: 'granite', parentId: 'rock' },
    { id: 'basalt', parentId: 'rock' },
    { id: 'props' },
  ];
  const ids = (id: string) => parentCandidates(tags, id).map((t) => t.id);

  test('leaves out the tag itself and every tag nested in it', () => {
    expect(ids('nature')).toEqual(['props']);
    expect(ids('rock')).toEqual(['nature', 'props']);
  });

  test('keeps every other tag for a leaf', () => {
    expect(ids('granite')).toEqual(['nature', 'rock', 'basalt', 'props']);
  });
});

describe('libraryNameFromPath', () => {
  test('uses the last path segment', () => {
    expect(libraryNameFromPath('/home/me/Assets/Kenney')).toBe('Kenney');
    expect(libraryNameFromPath('/home/me/Assets/Kenney/')).toBe('Kenney');
    expect(libraryNameFromPath('C:\\Assets\\Props')).toBe('Props');
  });

  test('falls back to "New Library" when there is no segment', () => {
    expect(libraryNameFromPath('/')).toBe('New Library');
  });
});

describe('isNameTaken', () => {
  const existing = [{ name: 'Props' }, { name: '  Terrain ' }];

  test('matches ignoring case and padding on existing names', () => {
    expect(isNameTaken(existing, 'props')).toBe(true);
    expect(isNameTaken(existing, 'TERRAIN')).toBe(true);
  });

  test('is false for a new name', () => {
    expect(isNameTaken(existing, 'Crates')).toBe(false);
    expect(isNameTaken([], 'Props')).toBe(false);
  });
});

describe('resolveDroppedAssetId', () => {
  const assets = [
    { id: 'a1', filePath: '/lib/props/crate.png' },
    { id: 'a2', filePath: '/lib/props/barrel.png' },
  ];

  test('passes an asset id through', () => {
    expect(resolveDroppedAssetId('a2', assets)).toBe('a2');
  });

  test('resolves a file path to the matching asset, ignoring case and slash style', () => {
    expect(resolveDroppedAssetId('/lib/props/crate.png', assets)).toBe('a1');
    expect(resolveDroppedAssetId('\\LIB\\props\\Barrel.png', assets)).toBe('a2');
  });

  test('keeps an unknown path as given', () => {
    expect(resolveDroppedAssetId('/elsewhere/rock.png', assets)).toBe('/elsewhere/rock.png');
  });
});

describe('isFilterEmpty', () => {
  test('is true with no filters or only empty lists', () => {
    expect(isFilterEmpty({})).toBe(true);
    expect(isFilterEmpty({ kinds: [], colorLabels: [], tags: [] })).toBe(true);
  });

  test('is false when any sidebar filter is set', () => {
    expect(isFilterEmpty({ isFavorite: true })).toBe(false);
    expect(isFilterEmpty({ unorganized: true })).toBe(false);
    expect(isFilterEmpty({ kinds: ['texture'] })).toBe(false);
    expect(isFilterEmpty({ rating: { min: 4, max: 5 } })).toBe(false);
    expect(isFilterEmpty({ colorLabels: ['red'] })).toBe(false);
    expect(isFilterEmpty({ dateRange: { from: 0 } })).toBe(false);
    expect(isFilterEmpty({ sizeRange: { max: 1 } })).toBe(false);
    expect(isFilterEmpty({ previewStatus: 'done' })).toBe(false);
    expect(isFilterEmpty({ tags: ['t1'] })).toBe(false);
  });

  test('ignores filters the sidebar does not offer', () => {
    expect(isFilterEmpty({ directory: 'props', extensions: ['png'] })).toBe(true);
  });
});

describe('datePresets', () => {
  const now = new Date(2026, 9, 12, 15, 30);
  const todayStart = new Date(2026, 9, 12).getTime();
  const day = 86400000;
  const active = (from: number | undefined) =>
    datePresets(from, now)
      .filter((p) => p.active)
      .map((p) => p.label);

  test('offers today, 7 and 30 days back from now', () => {
    expect(datePresets(undefined, now).map((p) => [p.label, p.from])).toEqual([
      ['Today', todayStart],
      ['Last 7 Days', now.getTime() - 7 * day],
      ['Last 30 Days', now.getTime() - 30 * day],
    ]);
  });

  test('marks none active without a date filter', () => {
    expect(active(undefined)).toEqual([]);
  });

  test('marks exactly the narrowest preset that covers the current from', () => {
    expect(active(todayStart)).toEqual(['Today']);
    expect(active(now.getTime() - 7 * day)).toEqual(['Last 7 Days']);
    expect(active(now.getTime() - 30 * day)).toEqual(['Last 30 Days']);
  });

  test('allows 10 seconds of slack, so a preset picked a moment ago stays active', () => {
    expect(active(now.getTime() - 7 * day - 9000)).toEqual(['Last 7 Days']);
    expect(active(now.getTime() - 30 * day - 11000)).toEqual([]);
  });
});
