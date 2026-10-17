import type { Asset } from '../../types';

/** What a grid or list tile shows: its thumbnail, the original image, a video frame, or an icon. */
export type TileMedia = 'thumbnail' | 'original' | 'video' | 'none';

/**
 * The media a tile shows for `asset`. `failed` means its image or video failed to load, so it
 * shows the icon instead; `withVideo` lets a video show a frame (grid cards do, list rows don't).
 */
export function tileMedia(
  asset: Pick<Asset, 'thumbnailPath' | 'filePath' | 'kind'>,
  { failed = false, withVideo = false }: { failed?: boolean; withVideo?: boolean } = {}
): TileMedia {
  if (failed) return 'none';
  if (asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb')) return 'thumbnail';
  if (['image', 'texture'].includes(asset.kind) && asset.filePath) return 'original';
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
