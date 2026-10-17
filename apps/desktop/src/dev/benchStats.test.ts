import { describe, expect, test } from 'bun:test';
import { frameStats, minMax, percentile, rssStats, seededRandom } from './benchStats';

describe('percentile', () => {
  test('uses the nearest rank, in any input order', () => {
    const values = [5, 1, 4, 2, 3, 10, 9, 8, 7, 6];
    expect(percentile(values, 50)).toBe(5);
    expect(percentile(values, 95)).toBe(10);
    expect(percentile(values, 0)).toBe(1);
  });

  test('is 0 with no values', () => {
    expect(percentile([], 95)).toBe(0);
  });
});

describe('frameStats', () => {
  test('counts frames over 50 and over 100 ms', () => {
    const stats = frameStats([16, 17, 16, 60, 120, 16]);
    expect(stats.frames).toBe(6);
    expect(stats.over50Ms).toBe(2);
    expect(stats.over100Ms).toBe(1);
    expect(stats.maxMs).toBe(120);
  });
});

describe('minMax and rssStats', () => {
  test('are null with no samples', () => {
    expect(minMax([])).toEqual({ min: null, max: null });
    expect(rssStats([])).toEqual({ startMB: null, endMB: null, maxMB: null });
  });

  test('rssStats reports start, end and max in MB', () => {
    const mb = 1024 * 1024;
    expect(rssStats([100 * mb, 300 * mb, 200 * mb])).toEqual({
      startMB: 100,
      endMB: 200,
      maxMB: 300,
    });
  });
});

describe('seededRandom', () => {
  test('repeats for the same seed and stays in [0, 1)', () => {
    const a = seededRandom(42);
    const b = seededRandom(42);
    const run = Array.from({ length: 20 }, () => a());
    expect(Array.from({ length: 20 }, () => b())).toEqual(run);
    expect(run.every((x) => x >= 0 && x < 1)).toBe(true);
  });
});
