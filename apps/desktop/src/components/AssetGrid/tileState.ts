import type { Asset } from '../../types';
import { isWebviewDisplayable } from '../../utils/media';

/**
 * What a grid or list tile shows: its bundled preview (a model's render shipped beside it that
 * fits the tile), its thumbnail, its fallback render (one too small to fit), the original image,
 * a video frame, or an icon.
 */
export type TileMedia = 'bundled' | 'thumbnail' | 'fallback' | 'original' | 'video' | 'none';

type TileAsset = Pick<
  Asset,
  'bundledPreview' | 'fallbackRender' | 'thumbnailPath' | 'filePath' | 'extension' | 'kind'
>;

/** The image media in the order a tile tries them. */
const ORDER: TileMedia[] = ['bundled', 'thumbnail', 'fallback', 'original'];

/**
 * The media a tile shows for `asset`: its bundled preview, then its thumbnail, then its fallback
 * render, then the original if the webview can display it. `failed` is the media that failed to
 * load, and the tile falls back to the next one in that order, or from the original or a video to
 * the icon. `withVideo` lets a video show a frame (grid cards do, list rows don't).
 */
export function tileMedia(
  asset: TileAsset,
  { failed, withVideo = false }: { failed?: TileMedia; withVideo?: boolean } = {}
): TileMedia {
  if (failed === 'original' || failed === 'video' || failed === 'none') return 'none';
  const after = failed ? ORDER.indexOf(failed) + 1 : 0;
  const thumbnail = asset.thumbnailPath && !asset.thumbnailPath.endsWith('.glb');
  const shown = ORDER.slice(after).find(
    (media) =>
      (media === 'bundled' && asset.bundledPreview) ||
      (media === 'thumbnail' && thumbnail) ||
      (media === 'fallback' && asset.fallbackRender) ||
      (media === 'original' && isWebviewDisplayable(asset.extension) && asset.filePath)
  );
  if (shown) return shown;
  if (withVideo && asset.kind === 'video' && asset.filePath) return 'video';
  return 'none';
}

/** The image file a tile showing `media` loads, if `media` is an image. */
export function tileImage(asset: TileAsset, media: TileMedia): string | undefined {
  if (media === 'bundled') return asset.bundledPreview;
  if (media === 'thumbnail') return asset.thumbnailPath;
  if (media === 'fallback') return asset.fallbackRender;
  if (media === 'original') return asset.filePath;
  return undefined;
}

/**
 * The tile's `data-tile-state`, which the scroll benchmark reads: "ready" once its media has
 * loaded or failed (`settled`), or at once when it shows only an icon.
 */
export function tileState(media: TileMedia, settled: boolean): 'ready' | 'placeholder' {
  return media === 'none' || settled ? 'ready' : 'placeholder';
}
