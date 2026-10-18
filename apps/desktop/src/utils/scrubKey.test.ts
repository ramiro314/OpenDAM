import { describe, expect, test } from 'bun:test';
import type { Asset } from '../types';
import { scrubKey, scrubLabelText } from './scrubKey';

function asset(fields: Partial<Asset>): Asset {
  return {
    id: 'a',
    filePath: '/lib/a.png',
    fileName: 'a.png',
    extension: 'png',
    kind: 'image',
    sizeBytes: 0,
    modifiedAt: 0,
    indexedAt: 0,
    rating: 0,
    colorLabel: 'none',
    isFavorite: false,
    description: '',
    tags: [],
    previewStatus: 'done',
    metadata: {},
    ...fields,
  };
}

describe('scrubKey', () => {
  test('name: the first letter, uppercased', () => {
    expect(scrubKey(asset({ fileName: 'moss.png' }), 'name')).toBe('M');
    expect(scrubKey(asset({ fileName: 'Rock.png' }), 'name')).toBe('R');
    expect(scrubKey(asset({ fileName: 'élan.png' }), 'name')).toBe('É');
    expect(scrubKey(asset({ fileName: 'ωmega.png' }), 'name')).toBe('Ω');
  });

  test('name: # for digits and anything else that is not a letter', () => {
    for (const fileName of [
      '2d_tile.png',
      '0.png',
      '_hidden.png',
      '.env',
      '-x',
      ' a',
      '😀.png',
      '',
    ]) {
      expect(scrubKey(asset({ fileName }), 'name')).toBe('#');
    }
  });

  test('date: the short month and the year, in local time', () => {
    expect(scrubKey(asset({ modifiedAt: new Date(2024, 2, 15).getTime() }), 'date')).toBe(
      'Mar 2024'
    );
    expect(scrubKey(asset({ modifiedAt: new Date(1999, 11, 31, 23, 59).getTime() }), 'date')).toBe(
      'Dec 1999'
    );
  });

  test('size: the size as the grid shows it', () => {
    expect(scrubKey(asset({ sizeBytes: 12 * 1024 * 1024 }), 'size')).toBe('12.0MB');
    expect(scrubKey(asset({ sizeBytes: 512 }), 'size')).toBe('512B');
  });

  test('rating: filled and empty stars, or Unrated', () => {
    expect(scrubKey(asset({ rating: 3 }), 'rating')).toBe('★★★☆☆');
    expect(scrubKey(asset({ rating: 5 }), 'rating')).toBe('★★★★★');
    expect(scrubKey(asset({ rating: 1 }), 'rating')).toBe('★☆☆☆☆');
    expect(scrubKey(asset({ rating: 0 }), 'rating')).toBe('Unrated');
  });

  test('kind: the kind label', () => {
    expect(scrubKey(asset({ kind: '3d_model' }), 'kind')).toBe('3D Model');
    expect(scrubKey(asset({ kind: 'texture' }), 'kind')).toBe('Texture');
    expect(scrubKey(asset({ kind: 'hdri' }), 'kind')).toBe('HDRI');
    expect(scrubKey(asset({ kind: 'other' }), 'kind')).toBe('Other');
  });

  test('favorite: Favorites or Others', () => {
    expect(scrubKey(asset({ isFavorite: true }), 'favorite')).toBe('Favorites');
    expect(scrubKey(asset({ isFavorite: false }), 'favorite')).toBe('Others');
  });
});

describe('scrubLabelText', () => {
  test('the 1-based position of the total, and the key once there is one', () => {
    expect(scrubLabelText(41_199, 83_095, 'M')).toBe('41,200 of 83,095 - M');
    expect(scrubLabelText(0, 1, null)).toBe('1 of 1');
  });
});
