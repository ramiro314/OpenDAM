import { afterEach, beforeEach, describe, expect, spyOn, test } from 'bun:test';
import { useGLTF } from '@react-three/drei';
import { useAssetStore } from '../store';
import { onPreviewDone } from './previewDone';

const realWindow = globalThis.window;
const model = '/lib/.opendam/thumbs/a_preview.glb';
const toUrl = (path: string) => `asset://localhost/${encodeURIComponent(path)}`;

beforeEach(() => {
  (globalThis as { window: unknown }).window = {
    __TAURI_INTERNALS__: { convertFileSrc: (path: string) => toUrl(path) },
  };
  useAssetStore.setState({ modelRevisions: {} });
});

afterEach(() => {
  (globalThis as { window: unknown }).window = realWindow;
});

describe('onPreviewDone', () => {
  test("clears the model's converted URL, not its raw path, and bumps its revision", () => {
    const clear = spyOn(useGLTF, 'clear');
    try {
      onPreviewDone({ assetId: 'a', thumbnailPath: model, previewModel: model });
      expect(clear.mock.calls).toEqual([[toUrl(model)]]);
      expect(useAssetStore.getState().modelRevisions).toEqual({ [model]: 1 });

      onPreviewDone({ assetId: 'a', thumbnailPath: model, previewModel: model });
      expect(useAssetStore.getState().modelRevisions).toEqual({ [model]: 2 });
    } finally {
      clear.mockRestore();
    }
  });

  test('a preview without a model clears nothing and bumps nothing', () => {
    const clear = spyOn(useGLTF, 'clear');
    try {
      onPreviewDone({ assetId: 'b', thumbnailPath: '/lib/.opendam/thumbs/b_thumb.png' });
      onPreviewDone({ assetId: 'c', thumbnailPath: null, previewModel: null });
      expect(clear).not.toHaveBeenCalled();
      expect(useAssetStore.getState().modelRevisions).toEqual({});
    } finally {
      clear.mockRestore();
    }
  });
});
