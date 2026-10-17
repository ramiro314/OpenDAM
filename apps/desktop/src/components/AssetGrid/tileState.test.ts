import { describe, expect, test } from 'bun:test';
import { tileMedia, tileState } from './tileState';

const png = { kind: 'texture' as const, filePath: '/lib/rock.png' };
const thumbnailed = { ...png, thumbnailPath: '/lib/.opendam/thumbs/a_thumb.png' };
const video = { kind: 'video' as const, filePath: '/lib/clip.mp4' };
const model = { kind: '3d_model' as const, filePath: '/lib/ship.glb' };

describe('tileMedia', () => {
  test('prefers the thumbnail, then the original image', () => {
    expect(tileMedia(thumbnailed)).toBe('thumbnail');
    expect(tileMedia(png)).toBe('original');
  });

  test('skips a .glb thumbnail and falls back to the icon for a model', () => {
    expect(tileMedia({ ...model, thumbnailPath: '/lib/.opendam/thumbs/a.glb' })).toBe('none');
  });

  test('shows a video frame only where video is allowed', () => {
    expect(tileMedia(video, { withVideo: true })).toBe('video');
    expect(tileMedia(video)).toBe('none');
  });

  test('a failed load falls back to the icon', () => {
    expect(tileMedia(thumbnailed, { failed: true })).toBe('none');
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

  test('a grid card whose image failed is ready', () => {
    expect(tileState(tileMedia(thumbnailed, { failed: true }), false)).toBe('ready');
  });

  test('a video is a placeholder until its metadata loads', () => {
    const media = tileMedia(video, { withVideo: true });
    expect(tileState(media, false)).toBe('placeholder');
    expect(tileState(media, true)).toBe('ready');
  });
});
