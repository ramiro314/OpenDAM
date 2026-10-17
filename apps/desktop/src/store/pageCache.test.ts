import { describe, expect, test } from 'bun:test';
import {
  MAX_PAGES,
  PAGE_SIZE,
  SETTLE_MS,
  assetAt,
  createSettler,
  emptyCache,
  findAnchor,
  insertPage,
  patchAssets,
  refreshPages,
  touch,
  type PageCache,
  type Schedule,
} from './pageCache';
import type { Asset } from '../types';

/** Page `p`'s assets, ids `p<p>-<i>`. */
const page = (p: number, size = PAGE_SIZE) =>
  Array.from({ length: size }, (_, i) => ({ id: `p${p}-${i}` }) as Asset);

function cacheOf(indices: number[], wanted: number[] = []): PageCache {
  return indices.reduce((c, p) => insertPage(c, p, page(p), wanted), emptyCache());
}

describe('insertPage and eviction', () => {
  test(`keeps at most ${MAX_PAGES} pages, dropping the least recently wanted`, () => {
    const cache = cacheOf(Array.from({ length: MAX_PAGES + 3 }, (_, i) => i));
    expect(cache.pages.size).toBe(MAX_PAGES);
    expect(cache.lru).toEqual(Array.from({ length: MAX_PAGES }, (_, i) => i + 3));
    expect(cache.indexById.has('p2-0')).toBe(false);
    expect(cache.indexById.get('p3-5')).toBe(3 * PAGE_SIZE + 5);
  });

  test('never evicts a wanted page, even the least recently wanted', () => {
    const cache = cacheOf(
      Array.from({ length: MAX_PAGES + 1 }, (_, i) => i),
      [0]
    );
    expect(cache.pages.has(0)).toBe(true);
    expect(cache.pages.has(1)).toBe(false);
  });

  test('keeps more than the limit while every page is wanted', () => {
    const all = Array.from({ length: MAX_PAGES + 2 }, (_, i) => i);
    expect(cacheOf(all, all).pages.size).toBe(MAX_PAGES + 2);
  });

  test('touch makes a page the most recently wanted, so it outlives newer ones', () => {
    let cache = cacheOf(Array.from({ length: MAX_PAGES }, (_, i) => i));
    cache = touch(cache, [0]);
    cache = insertPage(cache, 99, page(99), []);
    expect(cache.pages.has(0)).toBe(true);
    expect(cache.pages.has(1)).toBe(false);
  });

  test('replacing a page re-indexes its assets', () => {
    let cache = cacheOf([0]);
    cache = insertPage(cache, 0, [{ id: 'x' } as Asset], [0]);
    expect(cache.indexById.has('p0-0')).toBe(false);
    expect(cache.indexById.get('x')).toBe(0);
    expect(assetAt(cache, 0)?.id).toBe('x');
  });
});

describe('patchAssets', () => {
  test('copies only the page holding the asset, and updates its pinned copy', () => {
    const cache = cacheOf([0, 1, 2]);
    const pinned = new Map([['p1-7', assetAt(cache, PAGE_SIZE + 7)!]]);
    const patched = patchAssets(cache, pinned, ['p1-7'], (a) => ({ ...a, rating: 5 }))!;
    expect(patched.pages.get(0)).toBe(cache.pages.get(0)!);
    expect(patched.pages.get(2)).toBe(cache.pages.get(2)!);
    expect(patched.pages.get(1)![7].rating).toBe(5);
    expect(cache.pages.get(1)![7].rating).toBeUndefined();
    expect(patched.pinned.get('p1-7')).toBe(patched.pages.get(1)![7]);
  });

  test('copies a page once for several of its assets', () => {
    const cache = cacheOf([0]);
    const patched = patchAssets(cache, new Map(), ['p0-1', 'p0-2'], (a) => ({ ...a, rating: 1 }))!;
    expect(patched.pages.get(0)!.filter((a) => a.rating === 1).length).toBe(2);
  });

  test('patches a pinned asset whose page was evicted', () => {
    const pinned = new Map([['gone', { id: 'gone' } as Asset]]);
    const patched = patchAssets(emptyCache(), pinned, ['gone'], (a) => ({ ...a, rating: 3 }))!;
    expect(patched.pinned.get('gone')?.rating).toBe(3);
  });

  test('returns null when no asset is cached or pinned', () => {
    expect(patchAssets(cacheOf([0]), new Map(), ['nope'], (a) => a)).toBeNull();
  });
});

describe('refresh anchor', () => {
  test('refetches the wanted pages and those within 200 of the top', () => {
    expect(refreshPages([2], 450, 1000)).toEqual([1, 2, 3]);
    expect(refreshPages([0], 0, 1000)).toEqual([0, 1]);
    expect(refreshPages([4], 990, 1000)).toEqual([3, 4]);
  });

  test('finds the top asset where it moved to', () => {
    const cache = cacheOf([1, 2]);
    // p2-0 was at 380 before two assets went
    expect(findAnchor(cache.pages, 'p2-0', 380, 1000)).toBe(400);
  });

  test('keeps the old index, clamped to the total, when the top asset is gone', () => {
    const cache = cacheOf([1]);
    expect(findAnchor(cache.pages, 'deleted', 250, 1000)).toBe(250);
    expect(findAnchor(cache.pages, 'deleted', 250, 100)).toBe(99);
    expect(findAnchor(cache.pages, undefined, 0, 0)).toBe(0);
  });
});

describe('createSettler', () => {
  function fakeClock() {
    const timers: { fn: () => void; ms: number; cancelled: boolean }[] = [];
    const schedule: Schedule = (fn, ms) => {
      const timer = { fn, ms, cancelled: false };
      timers.push(timer);
      return () => (timer.cancelled = true);
    };
    const fire = () =>
      timers.filter((t) => !t.cancelled).forEach((t) => ((t.cancelled = true), t.fn()));
    return { timers, schedule, fire };
  }

  test(`settles the wanted pages after ${SETTLE_MS} ms`, () => {
    const clock = fakeClock();
    const settled: number[][] = [];
    const settler = createSettler((p) => settled.push(p), clock.schedule);
    settler.update([0, 1]);
    expect(settled).toEqual([]);
    expect(clock.timers[0].ms).toBe(SETTLE_MS);
    clock.fire();
    expect(settled).toEqual([[0, 1]]);
  });

  test('pages a drag flies past are never settled', () => {
    const clock = fakeClock();
    const settled: number[][] = [];
    const settler = createSettler((p) => settled.push(p), clock.schedule);
    for (const p of [3, 4, 5, 6]) settler.update([p]);
    clock.fire();
    expect(settled).toEqual([[6]]);
  });

  test('a slow scroll within the same pages keeps the timer running', () => {
    const clock = fakeClock();
    const settled: number[][] = [];
    const settler = createSettler((p) => settled.push(p), clock.schedule);
    settler.update([2]);
    settler.update([2]);
    settler.update([2]);
    expect(clock.timers.length).toBe(1);
    clock.fire();
    expect(settled).toEqual([[2]]);
  });

  test('kick settles at once, and dispose stops a pending settle', () => {
    const clock = fakeClock();
    const settled: number[][] = [];
    const settler = createSettler((p) => settled.push(p), clock.schedule);
    settler.kick(); // nothing wanted yet
    settler.update([1]);
    settler.kick();
    expect(settled).toEqual([[1]]);
    settler.update([2]);
    settler.dispose();
    clock.fire();
    expect(settled).toEqual([[1]]);
  });
});
