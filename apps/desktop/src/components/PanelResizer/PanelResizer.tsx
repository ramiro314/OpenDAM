import { useRef, useState } from 'react';
import { useUIStore } from '../../store';
import {
  MAIN_MIN_WIDTH,
  PANEL_LIMITS,
  clampPanelWidth,
  dragPanelWidth,
  type PanelSide,
} from '../../utils/panelSize';
import styles from './PanelResizer.module.css';

const WIDTH_VAR: Record<PanelSide, string> = {
  sidebar: '--sidebar-width',
  detail: '--detail-panel-width',
};

const KEY_STEP = 16;

interface Drag {
  startX: number;
  startWidth: number;
  room: number;
  panel: HTMLElement;
  /** The width the drag gives the panel, null when it collapses it */
  width: number | null;
}

/**
 * The drag handle on a side panel's inner edge, between the panel and the grid in the layout
 * body: right after the sidebar, right before the detail panel. While it drags it sets the
 * width variable on the body directly and stores the width only on release, so the app doesn't
 * render on every pointer move. Dragging well past the minimum collapses the panel, and a
 * double click gives it its initial width.
 */
export function PanelResizer({ side }: { side: PanelSide }) {
  const width = useUIStore((s) => (side === 'sidebar' ? s.sidebarWidth : s.detailPanelWidth));
  const setPanelWidth = useUIStore((s) => s.setPanelWidth);
  const [state, setState] = useState<'resize' | 'collapse' | null>(null);
  const drag = useRef<Drag | null>(null);
  const { min, max, initial } = PANEL_LIMITS[side];
  const label = side === 'sidebar' ? 'sidebar' : 'detail panel';

  const showWidth = (handle: HTMLElement, px: number) =>
    handle.parentElement?.style.setProperty(WIDTH_VAR[side], `${px}px`);

  const panelOf = (handle: HTMLElement) =>
    (side === 'sidebar' ? handle.previousElementSibling : handle.nextElementSibling) as HTMLElement;

  /** The widest the panel can be while the grid keeps its minimum */
  const roomFor = (handle: HTMLElement, panelWidth: number) => {
    const main = handle.parentElement?.querySelector<HTMLElement>(':scope > main');
    return main ? panelWidth + main.clientWidth - MAIN_MIN_WIDTH : Infinity;
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    const handle = e.currentTarget;
    const panel = panelOf(handle);
    if (!panel) return;
    e.preventDefault();
    handle.setPointerCapture(e.pointerId);
    const startWidth = panel.getBoundingClientRect().width;
    drag.current = {
      startX: e.clientX,
      startWidth,
      room: roomFor(handle, startWidth),
      panel,
      width: startWidth,
    };
    document.body.style.cursor = 'col-resize';
    setState('resize');
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    d.width = dragPanelWidth(side, d.startWidth, e.clientX - d.startX, d.room);
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
    if (d.width === null) {
      // Put back the width the app last rendered, which the panel has again when it reopens
      showWidth(e.currentTarget, clampPanelWidth(side, width));
      const ui = useUIStore.getState();
      if (side === 'sidebar') ui.toggleSidebar();
      else ui.toggleDetailPanel();
    } else {
      showWidth(e.currentTarget, d.width);
      setPanelWidth(side, d.width);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    const dx = e.key === 'ArrowRight' ? KEY_STEP : -KEY_STEP;
    const current = panelOf(e.currentTarget)?.getBoundingClientRect().width ?? width;
    const room = roomFor(e.currentTarget, current);
    setPanelWidth(
      side,
      clampPanelWidth(side, side === 'sidebar' ? current + dx : current - dx, room)
    );
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
