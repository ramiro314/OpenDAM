import { describe, expect, test } from 'bun:test';
import { imageOf, isWebviewDisplayable } from './media';

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

describe('imageOf', () => {
  const thumb = '/lib/.opendam/thumbs/t_thumb.png';

  test('a raster the webview displays shows itself, even with a thumbnail', () => {
    const file = '/lib/kit/FBX format/Textures/colormap.png';
    expect(imageOf({ extension: 'png', filePath: file, thumbnailPath: thumb })).toBe(file);
  });

  test('another file shows its rendered thumbnail', () => {
    expect(imageOf({ extension: 'tga', filePath: '/lib/rock.tga', thumbnailPath: thumb })).toBe(
      thumb
    );
  });

  test('a model preview or no thumbnail shows no image', () => {
    const glb = '/lib/.opendam/thumbs/t_preview.glb';
    expect(imageOf({ extension: 'fbx', filePath: '/lib/a.fbx', thumbnailPath: glb })).toBe(
      undefined
    );
    expect(imageOf({ extension: 'obj', filePath: '/lib/a.obj' })).toBe(undefined);
  });
});
