import type { Asset } from '../types';

/** Extensions the webview displays itself, so the original can stand in for a thumbnail. */
const WEBVIEW_DISPLAYABLE = new Set(['png', 'jpg', 'jpeg', 'webp', 'bmp', 'gif', 'svg']);

/** Whether the webview can display a file with extension `ext` (case-insensitive, no dot). */
export function isWebviewDisplayable(ext: string): boolean {
  return WEBVIEW_DISPLAYABLE.has(ext.toLowerCase());
}

/**
 * The image that stands for `asset`: the original where the webview can display it, else its
 * rendered thumbnail. `undefined` when it has neither, its preview is a model, or it has a
 * `previewReason`, since an image too large to decode would stall the webview too.
 */
export function imageOf(
  asset: Pick<Asset, 'extension' | 'filePath' | 'thumbnailPath' | 'previewReason'>
): string | undefined {
  if (asset.previewReason) return undefined;
  if (isWebviewDisplayable(asset.extension)) return asset.filePath;
  if (asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb')) return asset.thumbnailPath;
  return undefined;
}

/** What Quick Look says about `asset` when it has nothing to show. */
export function quickLookNote(asset: Pick<Asset, 'previewReason'>): string {
  return asset.previewReason ?? 'No preview generated yet';
}

/** What the detail panel says about `asset` when it has nothing to show. */
export function previewMessage(asset: Pick<Asset, 'previewStatus' | 'previewReason'>): {
  title: string;
  sub: string;
  /** Whether running the preview engine again can help */
  canGenerate: boolean;
} {
  if (asset.previewStatus === 'pending' || asset.previewStatus === 'generating') {
    return {
      title: 'Generating preview…',
      sub: 'Please wait while the worker renders the model',
      canGenerate: true,
    };
  }
  if (asset.previewReason) {
    return {
      title: asset.previewReason,
      sub: 'It gets a new try when the file changes',
      canGenerate: false,
    };
  }
  return {
    title:
      asset.previewStatus === 'error'
        ? 'Preview generation failed previously'
        : 'No preview available',
    sub: 'Click below to run the preview engine worker for this file',
    canGenerate: true,
  };
}
