import { describe, expect, test } from 'bun:test';
import { shouldPixelate } from './imageScaling';

const px = (width: number, height = width) => ({ width, height });

describe('shouldPixelate', () => {
  test('pixelates an enlarged image only when the preference is pixelated', () => {
    expect(shouldPixelate('pixelated', px(16), px(160), 'cover', 1)).toBe(true);
    expect(shouldPixelate('smooth', px(16), px(160), 'cover', 1)).toBe(false);
  });

  test('never pixelates a shrunk image or one shown at its own size', () => {
    expect(shouldPixelate('pixelated', px(512), px(160), 'cover', 1)).toBe(false);
    expect(shouldPixelate('pixelated', px(160), px(160), 'contain', 1)).toBe(false);
  });

  test('cover scales by the larger ratio, contain by the smaller', () => {
    // 100x400 into 200x200: cover scales by 2, contain by 0.5
    expect(shouldPixelate('pixelated', px(100, 400), px(200), 'cover', 1)).toBe(true);
    expect(shouldPixelate('pixelated', px(100, 400), px(200), 'contain', 1)).toBe(false);
  });

  test('compares device pixels, so devicePixelRatio can enlarge an image the CSS box shrinks', () => {
    // 256 px into a 160 px box is shrunk at 1x, but 160 x 2 = 320 device px enlarges it
    expect(shouldPixelate('pixelated', px(256), px(160), 'cover', 1)).toBe(false);
    expect(shouldPixelate('pixelated', px(256), px(160), 'cover', 2)).toBe(true);
    // and a fractional ratio can shrink an image the CSS box enlarges
    expect(shouldPixelate('pixelated', px(200), px(160), 'contain', 1.25)).toBe(false);
  });

  test('an image with no natural size yet is not pixelated', () => {
    expect(shouldPixelate('pixelated', px(0), px(160), 'cover', 1)).toBe(false);
  });
});
