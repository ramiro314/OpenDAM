import { describe, expect, test } from 'bun:test';
import { type TileMedia, tileImage, tileMedia, tileState } from './tileState';

const png = { kind: 'texture' as const, filePath: '/lib/rock.png', extension: 'png' };
const thumbnailed = { ...png, thumbnailPath: '/lib/.opendam/thumbs/a_thumb.png' };
const tga = { kind: 'texture' as const, filePath: '/lib/rock.tga', extension: 'tga' };
const video = { kind: 'video' as const, filePath: '/lib/clip.mp4', extension: 'mp4' };
const model = { kind: '3d_model' as const, filePath: '/lib/ship.glb', extension: 'glb' };
const rendered = {
  ...model,
  bundledPreview: '/lib/Previews/ship.png',
  thumbnailPath: '/lib/.opendam/thumbs/s_thumb.png',
};

describe('tileMedia', () => {
  test('prefers the thumbnail, then the original image', () => {
    expect(tileMedia(thumbnailed)).toBe('thumbnail');
    expect(tileMedia(png)).toBe('original');
  });

  test('chooses the original by extension, not kind', () => {
    expect(tileMedia({ ...png, kind: 'material' })).toBe('original');
    expect(tileMedia({ ...png, kind: 'other', extension: 'SVG' })).toBe('original');
    expect(tileMedia(tga)).toBe('none');
  });

  test('skips a .glb thumbnail and falls back to the icon for a model', () => {
    expect(tileMedia({ ...model, thumbnailPath: '/lib/.opendam/thumbs/a.glb' })).toBe('none');
  });

  test('shows a video frame only where video is allowed', () => {
    expect(tileMedia(video, { withVideo: true })).toBe('video');
    expect(tileMedia(video)).toBe('none');
  });

  test('prefers a bundled preview to the thumbnail and the original', () => {
    expect(tileMedia(rendered)).toBe('bundled');
    expect(tileMedia({ ...png, bundledPreview: '/lib/Previews/rock.png' })).toBe('bundled');
    expect(tileImage(rendered, 'bundled')).toBe('/lib/Previews/ship.png');
  });

  test('a failed bundled preview falls back to the thumbnail, then the original', () => {
    expect(tileMedia(rendered, { failed: 'bundled' })).toBe('thumbnail');
    expect(tileImage(rendered, 'thumbnail')).toBe('/lib/.opendam/thumbs/s_thumb.png');
    expect(tileMedia({ ...rendered, thumbnailPath: undefined }, { failed: 'bundled' })).toBe(
      'none'
    );
    const renderedPng = { ...png, bundledPreview: '/lib/Previews/rock.png' };
    expect(tileMedia(renderedPng, { failed: 'bundled' })).toBe('original');
    expect(tileImage(renderedPng, 'original')).toBe('/lib/rock.png');
  });

  test('shows a small render only after the thumbnail, and before the original', () => {
    const small = '/lib/Previews/ship.png';
    expect(tileMedia({ ...model, fallbackRender: small })).toBe('fallback');
    expect(tileImage({ ...model, fallbackRender: small }, 'fallback')).toBe(small);
    expect(
      tileMedia({ ...model, fallbackRender: small, thumbnailPath: '/lib/.opendam/thumbs/s.png' })
    ).toBe('thumbnail');
    expect(tileMedia({ ...png, fallbackRender: '/lib/Previews/rock.png' })).toBe('fallback');
  });

  test('falls back through a fitting render, the thumbnail, a small render and the original', () => {
    const everything = {
      ...png,
      bundledPreview: '/lib/Previews/big.png',
      thumbnailPath: '/lib/.opendam/thumbs/r_thumb.png',
      fallbackRender: '/lib/Previews/small.png',
    };
    const steps: (TileMedia | undefined)[] = [undefined];
    for (;;) {
      const next = tileMedia(everything, { failed: steps.at(-1) });
      steps.push(next);
      if (next === 'none') break;
    }
    expect(steps.slice(1)).toEqual(['bundled', 'thumbnail', 'fallback', 'original', 'none']);
  });

  test('each failed step skips to the next media the asset has', () => {
    const small = { ...model, fallbackRender: '/lib/Previews/ship.png' };
    expect(tileMedia({ ...small, bundledPreview: '/b.png' }, { failed: 'bundled' })).toBe(
      'fallback'
    );
    expect(tileMedia({ ...small, thumbnailPath: '/t.png' }, { failed: 'thumbnail' })).toBe(
      'fallback'
    );
    expect(tileMedia(small, { failed: 'fallback' })).toBe('none');
    const smallPng = { ...png, fallbackRender: '/lib/Previews/rock.png' };
    expect(tileMedia(smallPng, { failed: 'fallback' })).toBe('original');
    expect(
      tileMedia({ ...video, fallbackRender: '/v.png' }, { failed: 'fallback', withVideo: true })
    ).toBe('video');
  });

  test('a failed thumbnail falls back to the original, then the icon', () => {
    expect(tileMedia(thumbnailed, { failed: 'thumbnail' })).toBe('original');
    expect(tileMedia(thumbnailed, { failed: 'original' })).toBe('none');
    expect(tileMedia({ ...tga, thumbnailPath: '/t.png' }, { failed: 'thumbnail' })).toBe('none');
  });
});

describe('tileState', () => {
  test('a tile with no media is ready at once', () => {
    expect(tileState(tileMedia(model), false)).toBe('ready');
  });

  test('a thumbnail is a placeholder until it loads, then ready', () => {
    const media = tileMedia(thumbnailed);
    expect(tileState(media, false)).toBe('placeholder');
    expect(tileState(media, true)).toBe('ready');
  });

  test('a grid card whose media all failed is ready', () => {
    expect(tileState(tileMedia(thumbnailed, { failed: 'original' }), false)).toBe('ready');
  });

  test('a video is a placeholder until its metadata loads', () => {
    const media = tileMedia(video, { withVideo: true });
    expect(tileState(media, false)).toBe('placeholder');
    expect(tileState(media, true)).toBe('ready');
  });
});
