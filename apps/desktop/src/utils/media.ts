import type { Asset } from '../types';

/** Extensions the webview displays itself, so the original can stand in for a thumbnail. */
const WEBVIEW_DISPLAYABLE = new Set(['png', 'jpg', 'jpeg', 'webp', 'bmp', 'gif', 'svg']);

/** Whether the webview can display a file with extension `ext` (case-insensitive, no dot). */
export function isWebviewDisplayable(ext: string): boolean {
  return WEBVIEW_DISPLAYABLE.has(ext.toLowerCase());
}

/**
 * The image that stands for `asset`: the original where the webview can display it, else its
 * rendered thumbnail. `undefined` when it has neither, or its preview is a model.
 */
export function imageOf(
  asset: Pick<Asset, 'extension' | 'filePath' | 'thumbnailPath'>
): string | undefined {
  if (isWebviewDisplayable(asset.extension)) return asset.filePath;
  if (asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb')) return asset.thumbnailPath;
  return undefined;
}
