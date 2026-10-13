import { describe, expect, test } from 'bun:test';
import { formatSize } from './format';

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
