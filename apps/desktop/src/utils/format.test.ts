import { describe, expect, test } from 'bun:test';
import { formatSize, formatVisibleRange } from './format';

describe('formatSize', () => {
  test('shows bytes below 1 KB', () => {
    expect(formatSize(0)).toBe('0B');
    expect(formatSize(1023)).toBe('1023B');
  });

  test('switches unit at each power of 1024', () => {
    expect(formatSize(1024)).toBe('1.0KB');
    expect(formatSize(1024 * 1024)).toBe('1.0MB');
    expect(formatSize(1024 * 1024 * 1024)).toBe('1.00GB');
  });

  test('rounds to one decimal for KB and MB, two for GB', () => {
    expect(formatSize(1536)).toBe('1.5KB');
    expect(formatSize(2.25 * 1024 * 1024)).toBe('2.3MB');
    expect(formatSize(3.456 * 1024 * 1024 * 1024)).toBe('3.46GB');
  });
});

describe('formatVisibleRange', () => {
  test('shows the visible range of every result, 1-based', () => {
    expect(formatVisibleRange({ first: 41_200, last: 41_235 }, 83_095)).toBe(
      '41,201-41,236 of 83,095 assets'
    );
    expect(formatVisibleRange({ first: 0, last: 0 }, 1)).toBe('1-1 of 1 asset');
  });

  test('shows only the total when no row is visible', () => {
    expect(formatVisibleRange(null, 0)).toBe('0 assets');
  });
});
