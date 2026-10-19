import { useRef, useState } from 'react';
import { useUIStore } from '../../store';
import {
  MAIN_MIN_WIDTH,
  PANEL_LIMITS,
  PANEL_VARS,
  clampPanelWidth,
  dragPanelWidth,
  keyPanelWidth,
  settledPanelWidth,
  type PanelSide,
} from '../../utils/panelSize';
import styles from './PanelResizer.module.css';

const PANEL_ID: Record<PanelSide, string> = { sidebar: 'sidebar', detail: 'detail-panel' };

/** Where a resize starts: the panel's width and the widest the grid lets it be */
interface Start {
  width: number;
  room: number;
}

interface Drag {
  startX: number;
  panel: HTMLElement;
  /** Set on the first move, so a click that doesn't move stores nothing */
  start: Start | null;
  /** The width the drag gives the panel, null when it collapses it */
  width: number | null;
}

/**
 * The drag handle on a side panel's inner edge, between the panel and the grid in the layout
 * body. While it drags it sets the width variable on the body directly and stores the width
 * only on release, so the app doesn't render on every pointer move. Dragging well past the
 * minimum collapses the panel, and a double click gives it its initial width.
 */
export function PanelResizer({ side }: { side: PanelSide }) {
  const width = useUIStore((s) => (side === 'sidebar' ? s.sidebarWidth : s.detailPanelWidth));
  const setPanelWidth = useUIStore((s) => s.setPanelWidth);
  const finishPanelDrag = useUIStore((s) => s.finishPanelDrag);
  const [state, setState] = useState<'resize' | 'collapse' | null>(null);
  const drag = useRef<Drag | null>(null);
  const { min, max, initial } = PANEL_LIMITS[side];
  const label = side === 'sidebar' ? 'sidebar' : 'detail panel';

  const showWidth = (handle: HTMLElement, px: number) =>
    handle.parentElement?.style.setProperty(PANEL_VARS[side].width, `${px}px`);

  const panelOf = (handle: HTMLElement, s = side) =>
    handle.parentElement?.querySelector<HTMLElement>(`:scope > #${PANEL_ID[s]}`) ?? null;

  /**
   * Starts a resize from what the panels show. A window too narrow for both stored widths
   * shrinks the panels, so each shrunk panel first stores the width it shows: the resize then
   * follows the pointer and leaves the other panel where it is. Both are measured before
   * either changes, since settling one lets the other grow back.
   */
  const beginResize = (handle: HTMLElement): Start => {
    const ui = useUIStore.getState();
    const shown = (['sidebar', 'detail'] as const).map(
      (s) => [s, panelOf(handle, s)?.getBoundingClientRect().width] as const
    );
    for (const [s, width] of shown) {
      const stored = s === 'sidebar' ? ui.sidebarWidth : ui.detailPanelWidth;
      const settled = width === undefined ? null : settledPanelWidth(s, width, stored);
      if (settled === null) continue;
      handle.parentElement?.style.setProperty(PANEL_VARS[s].width, `${settled}px`);
      ui.setPanelWidth(s, settled);
    }
    const current = useUIStore.getState();
    const width = clampPanelWidth(
      side,
      side === 'sidebar' ? current.sidebarWidth : current.detailPanelWidth
    );
    const main = handle.parentElement?.querySelector<HTMLElement>(':scope > main');
    return { width, room: main ? width + main.clientWidth - MAIN_MIN_WIDTH : Infinity };
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    const handle = e.currentTarget;
    const panel = panelOf(handle);
    if (!panel) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    drag.current = { startX: e.clientX, panel, start: null, width: null };
    document.body.style.cursor = 'col-resize';
    setState('resize');
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d || (!d.start && e.clientX === d.startX)) return;
    d.start ??= beginResize(e.currentTarget);
    d.width = dragPanelWidth(side, d.start.width, e.clientX - d.startX, d.start.room);
    if (d.width !== null) showWidth(e.currentTarget, d.width);
    d.panel.style.opacity = d.width === null ? '0.4' : '';
    setState(d.width === null ? 'collapse' : 'resize');
  };

  const endDrag = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    drag.current = null;
    d.panel.style.opacity = '';
    document.body.style.cursor = '';
    setState(null);
    if (!d.start) return;
    // A collapse puts back the stored width, which the panel has when it reopens
    showWidth(e.currentTarget, d.width ?? d.start.width);
    finishPanelDrag(side, d.width);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    const start = beginResize(e.currentTarget);
    const next = keyPanelWidth(side, start.width, e.key, start.room);
    if (next !== null) setPanelWidth(side, next);
  };

  return (
    <div
      className={styles.handle}
      id={`${side}-resizer`}
      role="separator"
      aria-orientation="vertical"
      aria-label={`Resize ${label}`}
      aria-valuenow={clampPanelWidth(side, width)}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      title={`Drag to resize the ${label}, or past its edge to collapse it. Double-click to reset.`}
      data-state={state ?? undefined}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onLostPointerCapture={endDrag}
      onDoubleClick={() => setPanelWidth(side, initial)}
      onKeyDown={onKeyDown}
    />
  );
}
