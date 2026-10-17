// The UI checks `VITE_SCROLL_BENCH=checks` runs (U1-U8 in
// docs/plans/fast-collection-browsing/plan.md). Each ticket that delivers a behavior adds its
// check here.

import { useAssetStore, useUIStore } from '../store';
import { rowOfIndex, rowTop } from '../components/AssetGrid/gridGeometry';

export interface UiCheckContext {
  /** The grid's scroll container (`#asset-scroll`). */
  scroll: () => HTMLElement;
  /** Throws, failing the check, unless `condition` holds. */
  assert: (condition: unknown, message: string) => void;
  /** Resolves once `predicate` holds, or throws after `timeoutMs`. */
  waitFor: (predicate: () => boolean, timeoutMs: number, what: string) => Promise<void>;
  /** Logs a named pause and holds still long enough for the runner to take a screenshot. */
  pause: (name: string) => Promise<void>;
  sleep: (ms: number) => Promise<void>;
}

export interface UiCheck {
  /** `U1` to `U8` */
  id: string;
  title: string;
  /** Throws to fail. Starts in grid view at md size, scrolled to the top of a fresh search. */
  run: (ctx: UiCheckContext) => Promise<void>;
}

const nextFrame = () => new Promise<number>((resolve) => requestAnimationFrame(resolve));

async function frames(n: number) {
  for (let i = 0; i < n; i++) await nextFrame();
}

/** Resolves on the first frame `predicate` holds, with the ms it took; throws after `timeoutMs`. */
async function untilFrame(predicate: () => boolean, timeoutMs: number, what: string) {
  const start = performance.now();
  while (!predicate()) {
    if (performance.now() - start > timeoutMs) {
      throw new Error(`Timed out after ${timeoutMs} ms: ${what}`);
    }
    await nextFrame();
  }
  return performance.now() - start;
}

/** The tiles at least partly inside the scroll container's viewport. */
function tilesInView(el: HTMLElement): HTMLElement[] {
  const view = el.getBoundingClientRect();
  return [...el.querySelectorAll<HTMLElement>('[data-tile]')].filter((tile) => {
    const r = tile.getBoundingClientRect();
    return r.bottom > view.top + 0.5 && r.top < view.bottom - 0.5;
  });
}

const indexOf = (tile: HTMLElement) => Number(tile.dataset.index);

function allReady(el: HTMLElement): boolean {
  const tiles = tilesInView(el);
  return tiles.length > 0 && tiles.every((t) => t.dataset.tileState === 'ready');
}

/** The windowed grid's layout, as it publishes it on the scroll container. */
function layoutOf(el: HTMLElement) {
  return { columns: Number(el.dataset.columns), rowPitch: Number(el.dataset.rowPitch) };
}

/** The indices of the first row the viewport shows. */
function firstVisibleRow(el: HTMLElement): number[] {
  const tiles = tilesInView(el);
  const top = Math.min(...tiles.map((t) => t.getBoundingClientRect().top));
  return tiles
    .filter((t) => Math.abs(t.getBoundingClientRect().top - top) < 1)
    .map(indexOf)
    .sort((a, b) => a - b);
}

const FOOTER = /^([\d,]+)-([\d,]+) of ([\d,]+) assets?$/;
const parseCount = (s: string) => Number(s.replace(/,/g, ''));

export const UI_CHECKS: UiCheck[] = [
  {
    id: 'U1',
    title: 'With only page 0 cached, the scrollbar spans every result',
    run: async ({ scroll, assert, pause }) => {
      const el = scroll();
      const { pages, total } = useAssetStore.getState();
      assert(
        pages.size === 1 && pages.has(0),
        `cached pages are [${[...pages.keys()]}], not page 0 alone`
      );
      const { columns, rowPitch } = layoutOf(el);
      const expected = Math.ceil(total / columns) * rowPitch;
      assert(
        Math.abs(el.scrollHeight - expected) <= rowPitch,
        `scrollHeight ${el.scrollHeight} is not within a row (${rowPitch}) of ` +
          `ceil(${total} / ${columns}) x ${rowPitch} = ${expected}`
      );
      // The geometry is the one the cards are laid out in
      const card = (i: number) => el.querySelector<HTMLElement>(`[data-index="${i}"]`);
      const first = card(0)?.getBoundingClientRect();
      const below = card(columns)?.getBoundingClientRect();
      const lastInRow = card(columns - 1)?.getBoundingClientRect();
      assert(first && below && lastInRow, 'the first two rows are not rendered');
      assert(
        Math.abs(below!.top - first!.top - rowPitch) < 1,
        `rows are ${below!.top - first!.top} px apart, the geometry says ${rowPitch}`
      );
      assert(Math.abs(lastInRow!.top - first!.top) < 1, `row 0 does not hold ${columns} cards`);
      assert(
        Math.abs(first!.height + 12 - rowPitch) < 1,
        `a card is ${first!.height} px tall, the row pitch ${rowPitch} wants ${rowPitch - 12}`
      );
      await pause('U1-full-scrollbar');
    },
  },
  {
    id: 'U2',
    title: 'A jump to 75% shows placeholders, then fills every visible tile within 300 ms',
    run: async ({ scroll, assert, pause }) => {
      const el = scroll();
      el.scrollTop = 0.75 * (el.scrollHeight - el.clientHeight);
      const start = performance.now();
      let placeholders = 0;
      for (let f = 0; f < 3 && placeholders === 0; f++) {
        await nextFrame();
        placeholders = tilesInView(el).filter((t) => t.hasAttribute('data-placeholder')).length;
      }
      assert(placeholders > 0, 'no placeholder showed after the jump');
      await untilFrame(() => allReady(el), 5_000, 'visible tiles ready after the jump');
      const ms = Math.round(performance.now() - start);
      assert(ms <= 300, `the visible tiles took ${ms} ms to fill`);
      const { pages, wanted } = useAssetStore.getState();
      const flownPast = [...pages.keys()].filter((p) => p !== 0 && !wanted.includes(p));
      assert(flownPast.length === 0, `pages the jump passed were fetched: [${flownPast}]`);
      await pause('U2-jump-filled');
    },
  },
  {
    id: 'U3',
    title: 'The list view stays under 60 rows top to bottom, and the footer shows the range',
    run: async ({ scroll, assert, pause }) => {
      useUIStore.getState().setViewMode('list');
      await frames(3);
      const el = scroll();
      const total = useAssetStore.getState().total;
      const footer = document.getElementById('asset-range');
      assert(footer, 'no #asset-range footer');
      const steps = 20;
      let most = 0;
      for (let step = 0; step <= steps; step++) {
        el.scrollTop = (step / steps) * (el.scrollHeight - el.clientHeight);
        await frames(3);
        const rows = el.querySelectorAll('[data-tile]').length;
        most = Math.max(most, rows);
        assert(rows < 60, `step ${step}: ${rows} rows rendered`);
        const text = footer!.textContent ?? '';
        const m = FOOTER.exec(text);
        assert(m, `step ${step}: the footer reads "${text}"`);
        const [first, last, count] = [m![1], m![2], m![3]].map(parseCount);
        assert(count === total, `step ${step}: the footer counts ${count} of ${total}`);
        const shown = tilesInView(el).map(indexOf);
        assert(
          first === Math.min(...shown) + 1 && last === Math.max(...shown) + 1,
          `step ${step}: the footer reads ${first}-${last}, rows ${Math.min(...shown) + 1}-` +
            `${Math.max(...shown) + 1} are visible`
        );
        if (step === steps) assert(last === total, `at the bottom the footer ends at ${last}`);
        if (step === steps / 2) await pause('U3-list-middle');
      }
      assert(most > 0, 'no rows rendered');
    },
  },
  {
    id: 'U4',
    title: 'Changing the grid size or the width keeps the first visible asset in the first row',
    run: async ({ scroll, assert, pause }) => {
      const el = scroll();
      el.scrollTop = 0.4 * (el.scrollHeight - el.clientHeight);
      await untilFrame(() => allReady(el), 5_000, 'visible tiles ready at 40%');
      const keeps = async (what: string, change: () => void) => {
        const before = Math.min(...tilesInView(el).map(indexOf));
        const was = { ...layoutOf(el), scrollTop: el.scrollTop };
        change();
        await frames(4);
        const row = firstVisibleRow(el);
        const now = { ...layoutOf(el), scrollTop: el.scrollTop };
        assert(
          row.includes(before),
          `${what}: asset ${before} was first, the first row now holds ${row[0]}-${row.at(-1)}; ` +
            `layout and scrollTop went from ${JSON.stringify(was)} to ${JSON.stringify(now)}, ` +
            `its row's top is ${rowTop(rowOfIndex(before, now.columns), now.rowPitch)}`
        );
      };
      const ui = useUIStore.getState();
      await keeps('md to lg', () => ui.setGridSize('lg'));
      await keeps('lg to sm', () => ui.setGridSize('sm'));
      await keeps('sm to md', () => ui.setGridSize('md'));
      await keeps('narrower', () => (el.style.width = '900px'));
      await untilFrame(() => allReady(el), 5_000, 'visible tiles ready after narrowing');
      await pause('U4-narrowed');
      await keeps('wider again', () => (el.style.width = ''));
    },
  },
  {
    id: 'U5',
    title: 'Right-clicking a card at offset 60,000 opens the menu, and Escape closes it',
    run: async ({ scroll, assert, pause }) => {
      const el = scroll();
      const target = 60_000;
      const { total } = useAssetStore.getState();
      assert(total > target, `only ${total} results`);
      const { columns, rowPitch } = layoutOf(el);
      el.scrollTop = rowTop(rowOfIndex(target, columns), rowPitch);
      const selector = `[data-index="${target}"]:not([data-placeholder])`;
      await untilFrame(() => el.querySelector(selector) !== null, 5_000, `card ${target}`);
      const card = el.querySelector<HTMLElement>(selector)!;
      const r = card.getBoundingClientRect();
      card.dispatchEvent(
        new MouseEvent('contextmenu', {
          bubbles: true,
          cancelable: true,
          button: 2,
          clientX: r.left + r.width / 2,
          clientY: r.top + r.height / 2,
        })
      );
      await frames(2);
      assert(document.getElementById('grid-menu-autotag'), 'no Auto-tag item in the menu');
      const id = card.id.replace(/^asset-/, '');
      assert(
        useUIStore.getState().selectedAssetIds.has(id),
        'the right-clicked card is not selected'
      );
      await pause('U5-context-menu');
      window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
      await frames(2);
      assert(!document.getElementById('grid-menu-autotag'), 'Escape left the menu open');
    },
  },
];

export interface UiCheckResult {
  id: string;
  title: string;
  result: 'PASS' | 'FAIL';
  /** Why a check failed */
  detail?: string;
}

/**
 * Runs `checks` in order, recording PASS when a check resolves and FAIL, with the error as
 * detail, when it throws. `before` runs ahead of each check, `after` gets each result.
 */
export async function runUiChecks(
  checks: UiCheck[],
  ctx: UiCheckContext,
  hooks: { before?: () => Promise<void>; after?: (result: UiCheckResult) => Promise<void> } = {}
): Promise<{ passed: number; failed: number; results: UiCheckResult[] }> {
  const results: UiCheckResult[] = [];
  for (const check of checks) {
    await hooks.before?.();
    let result: UiCheckResult;
    try {
      await check.run(ctx);
      result = { id: check.id, title: check.title, result: 'PASS' };
    } catch (e) {
      result = { id: check.id, title: check.title, result: 'FAIL', detail: String(e) };
    }
    results.push(result);
    await hooks.after?.(result);
  }
  return {
    passed: results.filter((r) => r.result === 'PASS').length,
    failed: results.filter((r) => r.result === 'FAIL').length,
    results,
  };
}
