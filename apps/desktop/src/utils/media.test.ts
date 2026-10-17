import { describe, expect, test } from 'bun:test';
import { isWebviewDisplayable } from './media';

describe('isWebviewDisplayable', () => {
  test('accepts the formats the webview displays, in any case', () => {
    for (const ext of ['png', 'jpg', 'jpeg', 'webp', 'bmp', 'gif', 'svg', 'PNG', 'Jpg']) {
      expect(isWebviewDisplayable(ext)).toBe(true);
    }
  });

  test('rejects formats that need a thumbnail', () => {
    for (const ext of ['tga', 'tif', 'tiff', 'psd', 'exr', 'dds', 'glb', '']) {
      expect(isWebviewDisplayable(ext)).toBe(false);
    }
  });
});
