import { useCallback, useRef } from 'react';
import { useUIStore } from '../store';
import { shouldPixelate, type ImageFit } from '../utils/imageScaling';

/**
 * A callback ref for an `<img>` laid out with `fit`, that sets `image-rendering: pixelated` on
 * it when the image scaling preference is pixelated and the image is enlarged in device pixels
 * (`shouldPixelate`). It re-checks when the image loads, when its box resizes and when the
 * preference changes.
 */
export function useImageScaling(fit: ImageFit): (img: HTMLImageElement | null) => void {
  const pref = useUIStore((s) => s.imageScaling);
  const cleanup = useRef<() => void>();
  return useCallback(
    (img: HTMLImageElement | null) => {
      cleanup.current?.();
      cleanup.current = undefined;
      if (!img) return;
      const apply = () => {
        const pixelated = shouldPixelate(
          pref,
          { width: img.naturalWidth, height: img.naturalHeight },
          { width: img.clientWidth, height: img.clientHeight },
          fit,
          window.devicePixelRatio
        );
        img.style.imageRendering = pixelated ? 'pixelated' : '';
      };
      apply();
      img.addEventListener('load', apply);
      const observer = new ResizeObserver(apply);
      observer.observe(img);
      cleanup.current = () => {
        img.removeEventListener('load', apply);
        observer.disconnect();
      };
    },
    [pref, fit]
  );
}
