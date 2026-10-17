import type { Asset } from '../../types';
import { isWebviewDisplayable } from '../../utils/media';

/** What a grid or list tile shows: its thumbnail, the original image, a video frame, or an icon. */
export type TileMedia = 'thumbnail' | 'original' | 'video' | 'none';

/**
 * The media a tile shows for `asset`. `failed` is the media that failed to load: a failed
 * thumbnail falls back to the original if the webview can display it, anything else to the icon.
 * `withVideo` lets a video show a frame (grid cards do, list rows don't).
 */
export function tileMedia(
  asset: Pick<Asset, 'thumbnailPath' | 'filePath' | 'extension' | 'kind'>,
  { failed, withVideo = false }: { failed?: TileMedia; withVideo?: boolean } = {}
): TileMedia {
  if (failed && failed !== 'thumbnail') return 'none';
  if (!failed && asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb')) return 'thumbnail';
  if (isWebviewDisplayable(asset.extension) && asset.filePath) return 'original';
  if (withVideo && asset.kind === 'video' && asset.filePath) return 'video';
  return 'none';
}

/**
 * The tile's `data-tile-state`, which the scroll benchmark reads: "ready" once its media has
 * loaded or failed (`settled`), or at once when it shows only an icon.
 */
export function tileState(media: TileMedia, settled: boolean): 'ready' | 'placeholder' {
  return media === 'none' || settled ? 'ready' : 'placeholder';
}
