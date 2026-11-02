import { describe, expect, test } from 'bun:test';
import { imageOf, isWebviewDisplayable, previewMessage } from './media';

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

  test('a file with a preview reason never loads its original', () => {
    const previewReason = 'Too large to preview (29027 x 29027 px)';
    expect(imageOf({ extension: 'png', filePath: '/lib/huge.png', previewReason })).toBe(undefined);
  });
});

describe('previewMessage', () => {
  const previewReason = 'Too large to preview (29027 x 29027 px)';

  test('a reason replaces the message and offers no retry', () => {
    const message = previewMessage({ previewStatus: 'error', previewReason });
    expect(message.title).toBe(previewReason);
    expect(message.canGenerate).toBe(false);
  });

  test('a file with no reason keeps the messages it has', () => {
    expect(previewMessage({ previewStatus: 'none' }).title).toBe('No preview available');
    expect(previewMessage({ previewStatus: 'done' }).title).toBe('No preview available');
    expect(previewMessage({ previewStatus: 'error' }).title).toBe(
      'Preview generation failed previously'
    );
    expect(previewMessage({ previewStatus: 'none' }).canGenerate).toBe(true);
  });

  test('a job running again says so, even over a reason', () => {
    const message = previewMessage({ previewStatus: 'pending', previewReason });
    expect(message.title).toBe('Generating preview…');
    expect(message.canGenerate).toBe(true);
  });
});
