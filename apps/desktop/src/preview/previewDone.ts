import { reloadModel } from '../components/DetailPanel/ThreeViewer';
import { useAssetStore } from '../store';
import type { PendingModelThumbnail, SavedModelThumbnail } from '../types';

/** A `preview:done` event's payload; every path is absolute */
export interface PreviewDone {
  assetId: string;
  filePath?: string | null;
  previousThumbnailPath?: string | null;
  thumbnailPath: string | null;
  previewModel?: string | null;
}

/** Applies a finished preview job to the cached assets and to open 3D viewers */
export function onPreviewDone(done: PreviewDone) {
  useAssetStore.getState().updateAssetPreview(
    done.assetId,
    done.thumbnailPath,
    done.previewModel ?? undefined,
    done.filePath
      ? {
          filePath: done.filePath,
          previousThumbnailPath: done.previousThumbnailPath ?? undefined,
        }
      : undefined
  );
  if (done.previewModel) reloadModel(done.previewModel);
}

/**
 * Applies a saved model render. A render replaces the file it follows, so the group's card, which
 * may show it as a stand-in, is refreshed too.
 */
export async function onModelRendered(assetId: string, saved: SavedModelThumbnail) {
  const store = useAssetStore.getState();
  store.updateAssetPreview(assetId, saved.thumbnailPath);
  if (saved.groupId && saved.groupId !== assetId) await store.refreshAssets([saved.groupId]);
}

/** What `skipped` remembers a failed render by: a new GLB deserves a new try. */
export function renderKey({ assetId, revision }: PendingModelThumbnail) {
  return `${assetId}@${revision}`;
}

/**
 * The models to render now. `skipped` holds this session's failed first previews (`renderKey`),
 * whose rows can't show that they failed. A model at `generating` is always tried: its failures
 * leave that state.
 */
export function modelsToRender(batch: PendingModelThumbnail[], skipped: Set<string>) {
  return batch.filter((item) => item.generating || !skipped.has(renderKey(item)));
}
