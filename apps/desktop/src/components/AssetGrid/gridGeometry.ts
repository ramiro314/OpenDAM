// Row geometry of the windowed grid and list (docs/plans/fast-collection-browsing/plan.md,
// "Windowed grid and list"). Pure, so the virtualizer, the page cache and the UI checks share it.

import { CARD_BODY_HEIGHT } from './cardMetrics';

/** Smallest tile width per grid size, as `minmax(<tile>, 1fr)` had it. */
export const TILE_MIN: Record<'sm' | 'md' | 'lg', number> = { sm: 180, md: 220, lg: 280 };
// The grid sets these as CSS variables (--grid-gap, --scroll-padding, --list-row-height), so the
// stylesheets lay out with the same values the virtualizer positions rows by.
/** Gap between cards, both ways. */
export const GRID_GAP = 12;
/** The scroll area's padding on every side. */
export const SCROLL_PADDING = 16;
/** A list row's height, and the gap below it. */
export const LIST_ROW_HEIGHT = 36;
export const LIST_GAP = 1;

export interface RowLayout {
  columns: number;
  /** A card's width; the list's rows span the whole width. */
  tileWidth: number;
  /** A row's height without the gap below it. */
  rowHeight: number;
  /** One row's height plus the gap, the distance from one row's top to the next. */
  rowPitch: number;
}

/**
 * The grid's rows, as `repeat(auto-fill, minmax(tileMin, 1fr))` with `gap` lays them out in
 * `contentWidth`: as many columns as fit at `tileMin`, stretched to fill the width. A card is
 * square above its fixed-height body; its 1 px border sits inside the tile (border-box), so the
 * thumbnail is `tileWidth - 2` tall and the card `tileWidth + CARD_BODY_HEIGHT`.
 */
export function gridLayout(contentWidth: number, tileMin: number, gap = GRID_GAP): RowLayout {
  const columns = Math.max(1, Math.floor((contentWidth + gap) / (tileMin + gap)));
  const tileWidth = Math.max(0, (contentWidth - gap * (columns - 1)) / columns);
  const rowHeight = tileWidth + CARD_BODY_HEIGHT;
  return { columns, tileWidth, rowHeight, rowPitch: rowHeight + gap };
}

/** The list view's rows: one asset each, a fixed height. */
export function listLayout(contentWidth: number): RowLayout {
  return {
    columns: 1,
    tileWidth: contentWidth,
    rowHeight: LIST_ROW_HEIGHT,
    rowPitch: LIST_ROW_HEIGHT + LIST_GAP,
  };
}

export function rowCount(total: number, columns: number): number {
  return Math.ceil(total / columns);
}

export function rowOfIndex(index: number, columns: number): number {
  return Math.floor(index / columns);
}

/** The asset indices rows `firstRow` to `lastRow` (inclusive) hold, clamped to `total`. */
export function rowsToIndexRange(
  firstRow: number,
  lastRow: number,
  columns: number,
  total: number
): { first: number; last: number } | null {
  const first = Math.max(0, firstRow) * columns;
  const last = Math.min(total - 1, (lastRow + 1) * columns - 1);
  return first <= last ? { first, last } : null;
}

/** The pages (of `pageSize` assets) holding indices `first` to `last`, inclusive. */
export function pagesForIndexRange(first: number, last: number, pageSize: number): number[] {
  const pages: number[] = [];
  for (let p = Math.floor(first / pageSize); p <= Math.floor(last / pageSize); p++) pages.push(p);
  return pages;
}

/**
 * The rows the viewport shows, partly or whole, when the content is scrolled to `scrollTop`.
 * Row 0 starts `offset` px down (the scroll area's padding).
 */
export function visibleRows(
  scrollTop: number,
  viewportHeight: number,
  rowPitch: number,
  rows: number,
  offset = SCROLL_PADDING
): { first: number; last: number } | null {
  if (rows === 0 || rowPitch <= 0) return null;
  const clamp = (r: number) => Math.min(rows - 1, Math.max(0, r));
  const top = scrollTop - offset;
  return {
    first: clamp(Math.floor(top / rowPitch)),
    // The row the bottom edge falls in, unless it falls in the gap below the row before
    last: clamp(Math.ceil((top + viewportHeight) / rowPitch) - 1),
  };
}

/** The `scrollTop` that puts `row`'s top at the top of the viewport. */
export function rowTop(row: number, rowPitch: number, offset = SCROLL_PADDING): number {
  return offset + row * rowPitch;
}

/**
 * The row to scroll to after the column count changes from showing `firstIndex` first: the row
 * that now holds it, so the first visible asset stays in the first visible row.
 */
export function anchorRow(firstIndex: number, columns: number): number {
  return rowOfIndex(Math.max(0, firstIndex), columns);
}

/**
 * The `scrollTop` after a refresh moved the asset at the top from index `from` to `to` (C5): the
 * `scrollTop` the view had before the refresh, shifted by as many rows. It starts from the old
 * value, because a shorter result set has already pulled the live `scrollTop` up to its new end,
 * and shifting that would move the view twice. Rounded up, as the browser rounds down.
 */
export function anchoredScrollTop(
  before: number,
  from: number,
  to: number,
  columns: number,
  rowPitch: number
): number {
  const rows = rowOfIndex(to, columns) - rowOfIndex(from, columns);
  return Math.max(0, Math.ceil(before + rows * rowPitch));
}

/**
 * The page to prefetch (C3): the next page in the scroll direction, once the viewport, showing
 * indices `first` to `last`, is within two screens (`screenAssets` assets each) of it. Null when
 * that page is out of range or still far away.
 */
export function prefetchPage(
  first: number,
  last: number,
  direction: 1 | -1,
  screenAssets: number,
  total: number,
  pageSize: number
): number | null {
  const reach = 2 * screenAssets;
  if (direction > 0) {
    const page = Math.floor(last / pageSize) + 1;
    const start = page * pageSize;
    return start < total && start - last <= reach ? page : null;
  }
  const page = Math.floor(first / pageSize) - 1;
  const end = (page + 1) * pageSize - 1;
  return page >= 0 && first - end <= reach ? page : null;
}

/** Rows rendered beyond each edge of the viewport. */
export const OVERSCAN_ROWS = 2;

/**
 * The pages the grid wants at `scrollTop`: those under the visible rows plus `OVERSCAN_ROWS`
 * either side, and the prefetch page in the scroll direction (C3).
 */
export function wantedPages(
  scrollTop: number,
  viewportHeight: number,
  layout: RowLayout,
  total: number,
  direction: 1 | -1,
  pageSize: number
): number[] {
  const { columns, rowPitch } = layout;
  const visible = visibleRows(scrollTop, viewportHeight, rowPitch, rowCount(total, columns));
  if (!visible) return [];
  const shown = rowsToIndexRange(visible.first, visible.last, columns, total);
  const rendered = rowsToIndexRange(
    visible.first - OVERSCAN_ROWS,
    visible.last + OVERSCAN_ROWS,
    columns,
    total
  );
  if (!shown || !rendered) return [];
  const pages = pagesForIndexRange(rendered.first, rendered.last, pageSize);
  const screenAssets = (visible.last - visible.first + 1) * columns;
  const prefetch = prefetchPage(shown.first, shown.last, direction, screenAssets, total, pageSize);
  if (prefetch !== null && !pages.includes(prefetch)) pages.push(prefetch);
  return pages.sort((a, b) => a - b);
}

/** The keys the scroll container pages with (D17). */
export type PagingKey = 'Home' | 'End' | 'PageUp' | 'PageDown';

/**
 * The `scrollTop` `key` scrolls to: the top, the bottom, or one viewport minus one row down or
 * up, so the row cut by the viewport's edge stays in view. Clamped to the scroll range.
 */
export function pagingTarget(
  key: PagingKey,
  scrollTop: number,
  viewportHeight: number,
  scrollHeight: number,
  rowPitch: number
): number {
  const max = Math.max(0, scrollHeight - viewportHeight);
  const step = Math.max(1, viewportHeight - rowPitch);
  const target =
    key === 'Home' ? 0 : key === 'End' ? max : scrollTop + (key === 'PageDown' ? step : -step);
  return Math.min(max, Math.max(0, target));
}

/**
 * The scrub label's top within the viewport, at the scrollbar thumb's height: as far down the
 * viewport as `scrollTop` is down the scroll range, so it is at the top at the top and touches the
 * bottom at the bottom.
 */
export function scrubLabelTop(
  scrollTop: number,
  viewportHeight: number,
  scrollHeight: number,
  labelHeight: number
): number {
  const range = scrollHeight - viewportHeight;
  const fraction = range > 0 ? Math.min(1, Math.max(0, scrollTop / range)) : 0;
  return fraction * Math.max(0, viewportHeight - labelHeight);
}
