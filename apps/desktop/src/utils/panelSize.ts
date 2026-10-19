/** The two side panels the user can resize and collapse */
export type PanelSide = 'sidebar' | 'detail';

export const PANEL_LIMITS: Record<PanelSide, { min: number; max: number; initial: number }> = {
  sidebar: { min: 180, max: 480, initial: 240 },
  detail: { min: 260, max: 640, initial: 320 },
};

/** The grid keeps at least this width while a panel is dragged wider */
export const MAIN_MIN_WIDTH = 320;

/** Dragging a panel this far below its minimum collapses it */
export const COLLAPSE_SLACK = 60;

/** How far an arrow key on a panel's handle resizes it */
export const KEY_STEP = 16;

/** The CSS custom properties on the layout body that size each panel */
export const PANEL_VARS: Record<PanelSide, { width: string; min: string }> = {
  sidebar: { width: '--sidebar-width', min: '--sidebar-min-width' },
  detail: { width: '--detail-panel-width', min: '--detail-panel-min-width' },
};

/** The CSS custom property on the layout body that holds the grid's minimum width */
export const MAIN_MIN_WIDTH_VAR = '--main-min-width';

/**
 * The layout body's custom properties, so the CSS reads each panel's width and minimum, and the
 * grid's minimum, from here rather than keeping copies that could drift from these limits.
 */
export function layoutVars(widths: Record<PanelSide, number>): Record<string, string> {
  const vars: Record<string, string> = { [MAIN_MIN_WIDTH_VAR]: `${MAIN_MIN_WIDTH}px` };
  for (const side of ['sidebar', 'detail'] as const) {
    vars[PANEL_VARS[side].width] = `${clampPanelWidth(side, widths[side])}px`;
    vars[PANEL_VARS[side].min] = `${PANEL_LIMITS[side].min}px`;
  }
  return vars;
}

/** `width` within the panel's limits and the `room` the grid leaves it, in whole pixels. */
export function clampPanelWidth(side: PanelSide, width: number, room = Infinity): number {
  const { min, max } = PANEL_LIMITS[side];
  if (!Number.isFinite(width)) return PANEL_LIMITS[side].initial;
  return Math.round(Math.max(min, Math.min(max, room, width)));
}

/**
 * The width a drag of `dx` pixels gives a panel that started at `startWidth`, or null when the
 * drag collapses it. The sidebar's handle is on its right edge and the detail panel's on its
 * left, so dragging toward the grid widens both.
 */
export function dragPanelWidth(
  side: PanelSide,
  startWidth: number,
  dx: number,
  room = Infinity
): number | null {
  const width = side === 'sidebar' ? startWidth + dx : startWidth - dx;
  if (width < PANEL_LIMITS[side].min - COLLAPSE_SLACK) return null;
  return clampPanelWidth(side, width, room);
}

/**
 * The width an arrow key on its handle gives a panel `current` pixels wide, or null for any
 * other key. Like a drag, the key toward the grid widens the panel, and it never collapses it.
 */
export function keyPanelWidth(
  side: PanelSide,
  current: number,
  key: string,
  room = Infinity
): number | null {
  if (key !== 'ArrowLeft' && key !== 'ArrowRight') return null;
  const dx = key === 'ArrowRight' ? KEY_STEP : -KEY_STEP;
  return clampPanelWidth(side, side === 'sidebar' ? current + dx : current - dx, room);
}

/**
 * The width to store for a panel that shows `shown` pixels of its `stored` width before a
 * resize starts from it, or null when it shows its stored width. A window too narrow for both
 * stored widths shrinks the panels, and a resize that started from the stored widths would
 * jump away from the pointer and move the other panel. Rounds down, so the widths still fit.
 */
export function settledPanelWidth(side: PanelSide, shown: number, stored: number): number | null {
  if (shown > clampPanelWidth(side, stored) - 0.5) return null;
  return clampPanelWidth(side, Math.floor(shown));
}
