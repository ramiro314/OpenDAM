import { reloadModel } from '../components/DetailPanel/ThreeViewer';
import { useAssetStore } from '../store';

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
