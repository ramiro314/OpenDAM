/** How an image shown larger than its pixel size renders: hard pixel edges, or smoothed. */
export type ImageScaling = 'pixelated' | 'smooth';

/** The `object-fit` an image is laid out with. */
export type ImageFit = 'cover' | 'contain';

export interface Size {
  width: number;
  height: number;
}

/**
 * Whether an image gets `image-rendering: pixelated`: only when `pref` is pixelated and the
 * image is enlarged in device pixels, that is, its `fit` scale into `box` (CSS px) times `dpr` is
 * over 1. A shrunk image, or one not loaded yet (no natural size), always renders smooth.
 */
export function shouldPixelate(
  pref: ImageScaling,
  natural: Size,
  box: Size,
  fit: ImageFit,
  dpr: number
): boolean {
  if (pref !== 'pixelated' || natural.width <= 0 || natural.height <= 0) return false;
  const sx = box.width / natural.width;
  const sy = box.height / natural.height;
  const scale = fit === 'cover' ? Math.max(sx, sy) : Math.min(sx, sy);
  return scale * dpr > 1;
}
