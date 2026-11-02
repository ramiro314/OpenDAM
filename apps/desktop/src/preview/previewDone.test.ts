import { afterEach, beforeEach, describe, expect, mock, spyOn, test } from 'bun:test';
import { useGLTF } from '@react-three/drei';
import { useAssetStore } from '../store';
import { modelsToRender, onModelRendered, onPreviewDone, renderKey } from './previewDone';

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

describe('onModelRendered', () => {
  const thumbnailPath = '/lib/.opendam/thumbs/m_thumb.new.png';

  /** Runs `onModelRendered` and returns the ids refreshed and the asset ids patched. */
  async function rendered(assetId: string, groupId: string | null) {
    const refreshAssets = mock(async (_ids: string[]) => {});
    const updateAssetPreview = mock((_id: string, _path: string) => {});
    const real = useAssetStore.getState();
    useAssetStore.setState({ refreshAssets, updateAssetPreview } as never);
    try {
      await onModelRendered(assetId, { thumbnailPath, groupId });
    } finally {
      useAssetStore.setState({
        refreshAssets: real.refreshAssets,
        updateAssetPreview: real.updateAssetPreview,
      });
    }
    return {
      refreshed: refreshAssets.mock.calls.map(([ids]) => ids),
      patched: updateAssetPreview.mock.calls,
    };
  }

  test("refreshes the group's card, which may show a stand-in's old file", async () => {
    expect(await rendered('member', 'main')).toEqual({
      refreshed: [['main']],
      patched: [['member', thumbnailPath]],
    });
  });

  test('a file on its own or a main item has no other card to refresh', async () => {
    expect((await rendered('alone', null)).refreshed).toEqual([]);
    expect((await rendered('main', 'main')).refreshed).toEqual([]);
  });
});

describe('modelsToRender', () => {
  const item = (assetId: string, generating: boolean, modifiedAt = 1) => ({
    assetId,
    modelPath: `/lib/${assetId}.glb`,
    generating,
    modifiedAt,
  });

  test('skips a failed first preview but never a model at generating', () => {
    const skipped = new Set([item('first', false), item('changed-again', true)].map(renderKey));
    const batch = [item('first', false), item('changed-again', true), item('new', false)];
    expect(modelsToRender(batch, skipped).map((m) => m.assetId)).toEqual(['changed-again', 'new']);
  });

  test('a first preview that failed gets a new try once its model changes', () => {
    const skipped = new Set([renderKey(item('first', false, 1))]);
    expect(modelsToRender([item('first', false, 2)], skipped)).toHaveLength(1);
  });
});
