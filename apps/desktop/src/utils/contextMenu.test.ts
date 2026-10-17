import { describe, expect, test } from 'bun:test';
import { fitInViewport, menuTargets, openLimitReason } from './contextMenu';

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

describe('menuTargets', () => {
  const selected = new Set(['a', 'b']);

  test('a selected asset acts on the whole selection', () => {
    expect(menuTargets(selected, 'b')).toEqual(['a', 'b']);
  });

  test('an asset outside the selection acts on itself alone', () => {
    expect(menuTargets(selected, 'c')).toEqual(['c']);
    expect(menuTargets(new Set(), 'c')).toEqual(['c']);
  });
});

describe('openLimitReason', () => {
  test('Open and Reveal work on up to 10 assets', () => {
    expect(openLimitReason(1)).toBeNull();
    expect(openLimitReason(10)).toBeNull();
  });

  test('past 10 assets they are disabled with the reason', () => {
    expect(openLimitReason(11)).toBe('Works on up to 10 assets at once');
  });
});
