/**
 * Top-left corner for a `width` x `height` menu opened at (`x`, `y`), moved back inside the
 * viewport with `margin` to spare.
 */
export function fitInViewport(
  x: number,
  y: number,
  width: number,
  height: number,
  viewportWidth: number,
  viewportHeight: number,
  margin = 4
) {
  return {
    left: Math.max(margin, Math.min(x, viewportWidth - width - margin)),
    top: Math.max(margin, Math.min(y, viewportHeight - height - margin)),
  };
}

/** The assets a grid right-click acts on: the selection, or the clicked asset if it isn't in it. */
export function menuTargets(selected: ReadonlySet<string>, clickedId: string): string[] {
  return selected.has(clickedId) ? Array.from(selected) : [clickedId];
}
