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
