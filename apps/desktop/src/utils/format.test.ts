import { describe, expect, test } from 'bun:test';
import {
  formatGroupBadge,
  formatItemsAndFiles,
  formatSize,
  formatTrashAction,
  formatVisibleRange,
  itemFileCount,
} from './format';

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
    expect(formatVisibleRange({ first: 41_200, last: 41_235 }, 82_292)).toBe(
      '41,201-41,236 of 82,292 items'
    );
    expect(formatVisibleRange({ first: 0, last: 0 }, 1)).toBe('1-1 of 1 item');
  });

  test('shows only the total when no row is visible', () => {
    expect(formatVisibleRange(null, 0)).toBe('0 items');
  });
});

describe('formatItemsAndFiles', () => {
  test('shows the items, then the files they stand for', () => {
    expect(formatItemsAndFiles(82_292, 83_095)).toBe('82,292 items (83,095 files)');
    expect(formatItemsAndFiles(1, 1)).toBe('1 item (1 file)');
    expect(formatItemsAndFiles(0, 3)).toBe('0 items (3 files)');
  });
});

describe('formatGroupBadge', () => {
  test("names the main item's format and counts the other files", () => {
    expect(formatGroupBadge(['glb', 'fbx', 'obj'])).toEqual({
      label: 'GLB +2',
      title: '3 files: GLB, FBX, OBJ',
    });
  });

  test('lists a format the copies share once', () => {
    expect(formatGroupBadge(['png', 'png', 'png'])).toEqual({
      label: 'PNG +2',
      title: '3 files: PNG',
    });
  });

  test('gives a file on its own no badge', () => {
    expect(formatGroupBadge(undefined)).toBeNull();
    expect(formatGroupBadge(['glb'])).toBeNull();
  });
});

describe('itemFileCount', () => {
  test('counts a file on its own as one', () => {
    expect(itemFileCount({})).toBe(1);
  });

  test("counts a group's members and its bundled preview", () => {
    expect(
      itemFileCount({ groupFormats: ['glb', 'fbx', 'obj'], bundledPreview: '/kit/Previews/a.png' })
    ).toBe(4);
    expect(itemFileCount({ groupFormats: ['glb', 'fbx'] })).toBe(2);
    expect(itemFileCount({ bundledPreview: '/kit/Previews/a.png' })).toBe(2);
  });
});

describe('formatTrashAction', () => {
  test('says how many files go to the trash', () => {
    expect(formatTrashAction(1)).toBe('Move 1 file to the trash');
    expect(formatTrashAction(4)).toBe('Move 4 files to the trash');
    expect(formatTrashAction(1200)).toBe('Move 1,200 files to the trash');
  });
});
