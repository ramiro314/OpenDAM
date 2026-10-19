import { describe, expect, test } from 'bun:test';
import { CARD_BODY_HEIGHT } from './cardMetrics';
import {
  GRID_GAP,
  LIST_ROW_HEIGHT,
  TILE_MIN,
  anchorRow,
  anchoredScrollTop,
  gridLayout,
  listLayout,
  pagesForIndexRange,
  pagingTarget,
  prefetchPage,
  rowCount,
  rowOfIndex,
  rowTop,
  rowsToIndexRange,
  scrubLabelTop,
  visibleRows,
  wantedPages,
} from './gridGeometry';

/** What `repeat(auto-fill, minmax(min, 1fr))` with `gap` picks: the most tracks that fit. */
function cssAutoFillColumns(width: number, min: number, gap: number): number {
  let n = 1;
  while ((n + 1) * min + n * gap <= width) n++;
  return n;
}

describe('gridLayout', () => {
  test('picks the column count the CSS auto-fill rule picks, at every width and size', () => {
    for (const min of Object.values(TILE_MIN)) {
      for (let width = 100; width <= 4000; width += 7) {
        expect(gridLayout(width, min).columns).toBe(cssAutoFillColumns(width, min, GRID_GAP));
      }
    }
  });

  test('stretches the tiles to fill the width, gaps included', () => {
    for (const width of [500, 1368, 1903, 2528]) {
      const { columns, tileWidth } = gridLayout(width, TILE_MIN.md);
      expect(columns * tileWidth + (columns - 1) * GRID_GAP).toBeCloseTo(width, 6);
      expect(tileWidth).toBeGreaterThanOrEqual(TILE_MIN.md);
    }
  });

  test('a 1400 px window at md has 5 columns of 264 px', () => {
    // The scroll area is 1368 px wide inside its 16 px padding
    const layout = gridLayout(1368, TILE_MIN.md);
    expect(layout).toEqual({
      columns: 5,
      tileWidth: 264,
      rowHeight: 264 + CARD_BODY_HEIGHT,
      rowPitch: 264 + CARD_BODY_HEIGHT + GRID_GAP,
    });
  });

  test('a tile exactly at its minimum fits; one pixel less drops a column', () => {
    const width = 3 * TILE_MIN.md + 2 * GRID_GAP;
    expect(gridLayout(width, TILE_MIN.md).columns).toBe(3);
    expect(gridLayout(width - 1, TILE_MIN.md).columns).toBe(2);
  });

  test('a width narrower than one tile still has one column', () => {
    expect(gridLayout(100, TILE_MIN.lg)).toMatchObject({ columns: 1, tileWidth: 100 });
  });

  test('the list has one 36 px row per asset with a 1 px gap', () => {
    expect(listLayout(800)).toEqual({
      columns: 1,
      tileWidth: 800,
      rowHeight: LIST_ROW_HEIGHT,
      rowPitch: 37,
    });
  });
});

describe('rows and pages', () => {
  test('maps indices to rows and rows to indices', () => {
    expect(rowCount(83_095, 5)).toBe(16_619);
    expect(rowOfIndex(0, 5)).toBe(0);
    expect(rowOfIndex(41_200, 5)).toBe(8_240);
    expect(rowsToIndexRange(2, 3, 6, 100)).toEqual({ first: 12, last: 23 });
    expect(rowsToIndexRange(16, 17, 6, 100)).toEqual({ first: 96, last: 99 });
    expect(rowsToIndexRange(-2, 0, 6, 100)).toEqual({ first: 0, last: 5 });
    expect(rowsToIndexRange(20, 21, 6, 100)).toBeNull();
  });

  test('maps an index range to the pages holding it', () => {
    expect(pagesForIndexRange(0, 29, 200)).toEqual([0]);
    expect(pagesForIndexRange(150, 450, 200)).toEqual([0, 1, 2]);
    expect(pagesForIndexRange(200, 399, 200)).toEqual([1]);
  });

  test('finds the rows the viewport shows, after the 16 px padding', () => {
    // Rows span 16-316, 316-616, 616-916: an 800 px viewport at the top shows three
    expect(visibleRows(0, 800, 300, 100)).toEqual({ first: 0, last: 2 });
    expect(visibleRows(316, 600, 300, 100)).toEqual({ first: 1, last: 2 });
    expect(visibleRows(30_000, 800, 300, 100)).toEqual({ first: 99, last: 99 });
    expect(visibleRows(0, 800, 300, 0)).toBeNull();
    expect(rowTop(3, 300)).toBe(916);
  });

  test('leaves out a row only a sub-pixel sliver or its gap shows of', () => {
    // List rows: 36 px tall, 1 px gap, so row r spans 16 + 37r to 16 + 37r + 36
    const list = (scrollTop: number, height: number) =>
      visibleRows(scrollTop, height, 37, 100, 16, 36);
    // Row 0 ends at 52: 0.4 px of it shows, then 0.6 px
    expect(list(51.6, 370)?.first).toBe(1);
    expect(list(51.4, 370)?.first).toBe(0);
    // The top falls in the gap below row 0 (52-53)
    expect(list(52.5, 370)?.first).toBe(1);
    // Row 10 starts at 386: the bottom edge 0.4 px into it, then 0.6 px
    expect(list(16.4, 370)?.last).toBe(9);
    expect(list(16.6, 370)?.last).toBe(10);
    // Exactly at a row's edges
    expect(list(16, 370)).toEqual({ first: 0, last: 9 });
  });

  test('agrees with a check that counts a row once more than 0.5 px of it shows', () => {
    const [pitch, height, offset, viewport] = [37, 36, 16, 793.4];
    const shown = (scrollTop: number) => {
      const rows: number[] = [];
      for (let r = 0; r < 2_000; r++) {
        const top = offset + r * pitch - scrollTop;
        if (top + height > 0.5 && top < viewport - 0.5) rows.push(r);
      }
      return { first: rows[0], last: rows[rows.length - 1] };
    };
    for (let step = 0; step <= 400; step++) {
      const scrollTop = step * 137.29;
      expect(visibleRows(scrollTop, viewport, pitch, 2_000, offset, height)).toEqual(
        shown(scrollTop)
      );
    }
  });
});

describe('anchorRow', () => {
  test('keeps the first visible asset in the first visible row across column changes', () => {
    for (const first of [0, 5, 6_000, 41_203, 83_094]) {
      for (const columns of [1, 4, 5, 6, 7, 9]) {
        const row = anchorRow(first, columns);
        expect(row * columns).toBeLessThanOrEqual(first);
        expect(first).toBeLessThan((row + 1) * columns);
      }
    }
    expect(anchorRow(6_000, 6)).toBe(1_000);
    expect(anchorRow(6_000, 4)).toBe(1_500);
  });
});

describe('anchoredScrollTop', () => {
  test('shifts the view by as many rows as the top asset moved, keeping its offset in the row', () => {
    // 5 columns of 300 px rows; asset 450 was 40 px into row 90's view, two rows were added above
    expect(anchoredScrollTop(rowTop(90, 300) + 40, 450, 460, 5, 300)).toBe(rowTop(92, 300) + 40);
    expect(anchoredScrollTop(rowTop(90, 300) + 40, 450, 452, 5, 300)).toBe(rowTop(90, 300) + 40);
  });

  test('a shrinking result set near the bottom shifts once, from the scrollTop before the refresh', () => {
    // The view was at 30,000 with asset 990 on top; 100 assets went, so 990 clamps to 899, 19 rows
    // up. The browser has already pulled the live scrollTop up to the shorter end; the result
    // depends only on where the view was.
    expect(anchoredScrollTop(30_000, 990, 899, 5, 300)).toBe(30_000 - 19 * 300);
  });

  test('rounds up, as the browser rounds scrollTop down, and never goes above the top', () => {
    expect(anchoredScrollTop(100, 10, 15, 5, 300.4)).toBe(401);
    expect(anchoredScrollTop(100, 500, 0, 5, 300)).toBe(0);
  });
});

describe('wantedPages', () => {
  // 6 columns of 300 px rows, an 800 px viewport: 3 rows, 18 assets, on screen
  const layout = { columns: 6, tileWidth: 204, rowHeight: 288, rowPitch: 300 };
  const at = (row: number) => rowTop(row, 300);

  test('at the top: the page under the visible and overscan rows only', () => {
    expect(wantedPages(0, 800, layout, 83_095, 1, 200)).toEqual([0]);
  });

  test('prefetches the next page once it is within two screens, in the scroll direction', () => {
    // Rows 26-28 show indices 156-173; overscan reaches 185; page 1 starts 27 assets on
    expect(wantedPages(at(26), 800, layout, 83_095, 1, 200)).toEqual([0, 1]);
    expect(wantedPages(at(26), 800, layout, 83_095, -1, 200)).toEqual([0]);
    // Rows 38-40 show 228-245; page 0 ends 29 assets back
    expect(wantedPages(at(38), 800, layout, 83_095, -1, 200)).toEqual([0, 1]);
    expect(wantedPages(at(38), 800, layout, 83_095, 1, 200)).toEqual([1]);
  });

  test('a jump deep into the results wants only the pages there', () => {
    const row = rowOfIndex(62_321, 6);
    expect(wantedPages(at(row), 800, layout, 83_095, 1, 200)).toEqual([311]);
  });

  test('includes the overscan rows across a page boundary', () => {
    // Rows 32-34 show 192-209: pages 0 and 1 are both on screen
    expect(wantedPages(at(32), 800, layout, 83_095, 1, 200)).toEqual([0, 1]);
  });

  test('never wants a page past the end', () => {
    expect(prefetchPage(190, 199, 1, 18, 200, 200)).toBeNull();
    expect(prefetchPage(0, 17, -1, 18, 200, 200)).toBeNull();
  });
});

describe('pagingTarget', () => {
  const [view, height, pitch] = [900, 100_000, 244];

  test('Home and End go to the top and the bottom', () => {
    expect(pagingTarget('Home', 5_000, view, height, pitch)).toBe(0);
    expect(pagingTarget('End', 5_000, view, height, pitch)).toBe(height - view);
  });

  test('Page Down and Page Up move one viewport minus one row', () => {
    expect(pagingTarget('PageDown', 5_000, view, height, pitch)).toBe(5_000 + view - pitch);
    expect(pagingTarget('PageUp', 5_000, view, height, pitch)).toBe(5_000 - view + pitch);
  });

  test('stays inside the scroll range', () => {
    expect(pagingTarget('PageUp', 100, view, height, pitch)).toBe(0);
    expect(pagingTarget('PageDown', height - view - 10, view, height, pitch)).toBe(height - view);
    expect(pagingTarget('End', 0, view, 500, pitch)).toBe(0);
  });

  test('still moves when a row is taller than the viewport', () => {
    expect(pagingTarget('PageDown', 0, 300, height, 400)).toBe(1);
  });
});

describe('scrubLabelTop', () => {
  test('follows the scroll position down the viewport, label height included', () => {
    expect(scrubLabelTop(0, 900, 10_900, 30)).toBe(0);
    expect(scrubLabelTop(5_000, 900, 10_900, 30)).toBe(435);
    expect(scrubLabelTop(10_000, 900, 10_900, 30)).toBe(870);
  });

  test('stays at the top when nothing scrolls, and inside the viewport past either end', () => {
    expect(scrubLabelTop(0, 900, 900, 30)).toBe(0);
    expect(scrubLabelTop(-50, 900, 10_900, 30)).toBe(0);
    expect(scrubLabelTop(20_000, 900, 10_900, 30)).toBe(870);
  });
});
