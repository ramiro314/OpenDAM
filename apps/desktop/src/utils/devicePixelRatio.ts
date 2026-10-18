// One `matchMedia` query shared by every subscriber, for changes of `devicePixelRatio` alone:
// zooming the webview, or moving the window to a monitor with another scale. Neither changes a
// CSS box, so a ResizeObserver doesn't see them.

const listeners = new Set<() => void>();
let query: MediaQueryList | undefined;

/** Matches only the current ratio, so it reports a change once, then is replaced. */
function arm() {
  query?.removeEventListener('change', changed);
  query = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
  query.addEventListener('change', changed);
}

function changed() {
  arm();
  for (const listener of listeners) listener();
}

/** Calls `listener` whenever `devicePixelRatio` changes; returns the unsubscribe. */
export function onDevicePixelRatioChange(listener: () => void): () => void {
  if (listeners.size === 0) arm();
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      query?.removeEventListener('change', changed);
      query = undefined;
    }
  };
}
