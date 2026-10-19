// The UI checks `VITE_SCROLL_BENCH=checks` runs (U1-U8 in
// docs/plans/fast-collection-browsing/plan.md, M1 for the web process's memory, and U9-U12 in
// docs/plans/smarter-big-collections/plan.md). Each ticket that delivers a behavior adds its
// check here.

import { convertFileSrc } from '@tauri-apps/api/core';
import { api } from '../api';
import { useAssetStore, useFolderTreeStore, useLibraryStore, useUIStore } from '../store';
import type { FolderNode, SortField, SortOrder } from '../types';
import { isFolderShown } from '../components/Sidebar/sidebarHelpers';
import { formatItemsAndFiles } from '../utils/format';
import { assetAt } from '../store/pageCache';
import { scrubKey } from '../utils/scrubKey';
import {
  GRID_GAP,
  SCROLL_PADDING,
  TILE_MIN,
  rowOfIndex,
  rowTop,
} from '../components/AssetGrid/gridGeometry';
import { shouldPixelate, type ImageFit, type ImageScaling } from '../utils/imageScaling';

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
  /** The web process's RSS in bytes (`bench_rss`). */
  rss: () => Promise<number>;
}

export interface UiCheck {
  /** `U1`, `M1` and so on */
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

/**
 * Sorts the results and searches again. `setSort` alone only changes the query, so the grid
 * would keep the old order while the store and the filter bar named the new one.
 */
async function sortResults(field: SortField, order: SortOrder) {
  useAssetStore.getState().setSort(field, order);
  await useAssetStore.getState().search();
  await frames(3);
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

/** The loaded images under `root`. */
function loadedImages(root: ParentNode): HTMLImageElement[] {
  return [...root.querySelectorAll('img')].filter((img) => img.complete && img.naturalWidth > 0);
}

/** Whether `img`, laid out with `fit`, is shown larger than its pixel size in device pixels. */
function isEnlarged(img: HTMLImageElement, fit: ImageFit): boolean {
  return shouldPixelate(
    'pixelated',
    { width: img.naturalWidth, height: img.naturalHeight },
    { width: img.clientWidth, height: img.clientHeight },
    fit,
    window.devicePixelRatio
  );
}

const rendering = (img: HTMLImageElement) => getComputedStyle(img).imageRendering;

/**
 * Whether `img` is a raster. An SVG is drawn at the size it's shown, so pixelated and smooth look
 * the same, and it can't show the toggle's effect.
 */
const isRaster = (img: HTMLImageElement) =>
  !/\.svg$/i.test(decodeURIComponent(new URL(img.src).pathname));

/**
 * Asserts every loaded image under `root` renders `pixelated` when `pref` is pixelated and it is
 * enlarged, `auto` otherwise. Images still finish loading during the check, and one can be
 * complete a moment before its load handler applies the rule, so a mismatch gets up to 1 s to
 * settle.
 */
async function assertScaling(
  assert: UiCheckContext['assert'],
  where: string,
  root: ParentNode,
  fit: ImageFit,
  pref: ImageScaling
) {
  const mismatches = () =>
    loadedImages(root).filter((img) => {
      const want = isEnlarged(img, fit) && pref === 'pixelated' ? 'pixelated' : 'auto';
      return rendering(img) !== want;
    });
  await untilFrame(() => mismatches().length === 0, 1_000, '').catch(() => {});
  const img = mismatches()[0];
  assert(
    !img,
    img &&
      `${where}, ${pref}: a ${img.naturalWidth}x${img.naturalHeight} image in a ` +
        `${img.clientWidth}x${img.clientHeight} box renders ${rendering(img)}`
  );
}

const FOOTER = /^([\d,]+)-([\d,]+) of ([\d,]+) items?$/;
const SCRUB_LABEL = /^([\d,]+) of ([\d,]+)(?: - (.+))?$/;
const parseCount = (s: string) => Number(s.replace(/,/g, ''));

const scrubLabel = () => document.getElementById('scrub-label');

/** The library the bench registers its run copy as (`OPENDAM_BENCH_LIBRARY`). */
const BENCH_LIBRARY = 'bench';

/**
 * The grid items at the bench library's root once its format groups and 2D stacks are formed
 * (docs/plans/smarter-big-collections/plan.md, A3), from the golden snapshot's 83,095 files.
 */
const ROOT_ITEMS = 51_139;

/** How long U9 waits for the folder tree, and for a write to get through. */
const STRUCTURE_TIMEOUT_MS = 900_000;

/** The sidebar row of folder `path` in the bench library, if it is shown. */
function folderRow(path: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(
    `[data-library-id="${BENCH_LIBRARY}"][data-folder-path="${CSS.escape(path)}"]`
  );
}

/** The item count a folder row's badge shows. */
const shownCount = (row: HTMLElement) => parseCount(row.lastElementChild?.textContent ?? '');

/** The folder holding `path`; `''` for a top-level folder. */
const parentFolder = (path: string) => path.slice(0, Math.max(0, path.lastIndexOf('/')));

/**
 * Expands the bench library's row and returns its top-level folders. The checks can start before
 * the startup index run, and the first structure pass it ends with, are done. The expanded row
 * then fills on that run's `library:updated`.
 */
async function expandBenchLibrary({
  assert,
  sleep,
}: Pick<UiCheckContext, 'assert' | 'sleep'>): Promise<FolderNode[]> {
  const toggle = document.getElementById(`lib-toggle-${BENCH_LIBRARY}`);
  assert(toggle, `no #lib-toggle-${BENCH_LIBRARY} on the library row`);
  if (!useFolderTreeStore.getState().expanded[BENCH_LIBRARY]?.includes('')) toggle!.click();
  const deadline = performance.now() + STRUCTURE_TIMEOUT_MS;
  let top = await api.getFolders(BENCH_LIBRARY, '');
  while (top.length === 0) {
    assert(
      performance.now() < deadline,
      `the library has no top-level folders after ${STRUCTURE_TIMEOUT_MS / 1000} s`
    );
    await sleep(1_000);
    top = await api.getFolders(BENCH_LIBRARY, '');
  }
  return top;
}

/** Expands the bench library's folder `path`, unless it is already expanded. */
async function expandFolder(path: string) {
  if (!useFolderTreeStore.getState().expanded[BENCH_LIBRARY]?.includes(path)) {
    await useFolderTreeStore.getState().toggle(BENCH_LIBRARY, path);
  }
}

/** Runs the store's query with `patch` applied, and waits for its first page and totals. */
async function searchWith(
  patch: Parameters<ReturnType<typeof useAssetStore.getState>['setQuery']>[0],
  waitFor: UiCheckContext['waitFor']
) {
  const store = useAssetStore.getState();
  store.setQuery(patch);
  await store.search();
  await waitFor(() => !useAssetStore.getState().loading, 30_000, 'the search');
}

/**
 * The shallowest folder at least two levels deep that holds exactly one file, a visible one that
 * is no format group's, and is no pack, found breadth first through `get_folders`, with the
 * slowest node load on the way.
 */
async function singleFileFolder(): Promise<{ folder: FolderNode; slowestMs: number }> {
  let slowestMs = 0;
  let level = [''];
  while (level.length > 0) {
    const next: string[] = [];
    for (const parent of level) {
      const start = performance.now();
      const children = await api.getFolders(BENCH_LIBRARY, parent);
      slowestMs = Math.max(slowestMs, performance.now() - start);
      for (const c of children) {
        if (c.itemCount !== 1 || c.hiddenCount !== 0 || c.isPack || !c.path.includes('/')) {
          continue;
        }
        // One item could be a group, with files elsewhere or several here
        const { assets, fileTotal } = await api.searchAssets({
          query: {
            ...useAssetStore.getState().query,
            folder: { libraryId: BENCH_LIBRARY, path: c.path },
          },
          page: 0,
          pageSize: 2,
          includeTotal: true,
        });
        if (fileTotal === 1 && !assets[0]?.groupFormats) return { folder: c, slowestMs };
      }
      next.push(...children.filter((c) => c.hasChildren).map((c) => c.path));
    }
    level = next;
  }
  throw new Error('no folder holds exactly one file');
}

/**
 * Records, from DOM mutations rather than frames, each time the scrub label shows or hides and the
 * last text it showed, so a stalled frame can't hide what happened between two frames.
 */
function watchLabel(frame: HTMLElement) {
  const watch = {
    changes: [] as { at: number; visible: boolean }[],
    text: scrubLabel()?.textContent ?? '',
    /** False once the label showed without `aria-hidden="true"` */
    ariaHidden: true,
    shownSince: (t: number) => watch.changes.filter((c) => c.visible && c.at >= t).length,
    hiddenAt: () => watch.changes.filter((c) => !c.visible).at(-1)?.at ?? -1,
    stop: () => observer.disconnect(),
  };
  let visible = scrubLabel() !== null;
  const observer = new MutationObserver(() => {
    const label = scrubLabel();
    if (label) {
      watch.text = label.textContent ?? '';
      watch.ariaHidden &&= label.getAttribute('aria-hidden') === 'true';
    }
    if ((label !== null) !== visible) {
      visible = label !== null;
      watch.changes.push({ at: performance.now(), visible });
    }
  });
  observer.observe(frame, { childList: true, subtree: true, characterData: true });
  return watch;
}

/**
 * Scrolls `el` down at `viewportsPerSecond` viewport heights a second, a step every frame, until
 * `during` settles. Reports the time of the last scroll event, and the frames for diagnostics.
 */
async function scrollAt(el: HTMLElement, viewportsPerSecond: number, during: Promise<unknown>) {
  let done = false;
  void during.finally(() => (done = true));
  let lastScroll = 0;
  const onScroll = () => (lastScroll = performance.now());
  el.addEventListener('scroll', onScroll, { passive: true });
  const from = el.scrollTop;
  const start = performance.now();
  let [frameCount, longest, prev] = [0, 0, start];
  while (!done) {
    const t = await nextFrame();
    frameCount++;
    longest = Math.max(longest, t - prev);
    prev = t;
    el.scrollTop = from + (viewportsPerSecond * el.clientHeight * (t - start)) / 1000;
  }
  // The last step's scroll event
  await frames(2);
  el.removeEventListener('scroll', onScroll);
  return { lastScroll, summary: `${frameCount} frames, longest ${Math.round(longest)} ms` };
}

/** A cancelable `keydown` on `el`, as a real press reaches the grid; returns the event. */
function press(el: HTMLElement, key: string, code = key): KeyboardEvent {
  const event = new KeyboardEvent('keydown', { key, code, bubbles: true, cancelable: true });
  el.dispatchEvent(event);
  return event;
}

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
        Math.abs(first!.height + GRID_GAP - rowPitch) < 1,
        `a card is ${first!.height} px tall, the row pitch ${rowPitch} wants ${rowPitch - GRID_GAP}`
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
            `${Math.max(...shown) + 1} are visible (scrollTop ${el.scrollTop}, viewport ` +
            `${el.getBoundingClientRect().height} px)`
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
        return { was: was.columns, now: now.columns };
      };
      const ui = useUIStore.getState();
      await keeps('md to lg', () => ui.setGridSize('lg'));
      await keeps('lg to sm', () => ui.setGridSize('sm'));
      await keeps('sm to md', () => ui.setGridSize('md'));
      // Narrow the scroll area to a width that fits one column fewer at md, as a narrower window
      // would, then let it fill the window again
      const { columns } = layoutOf(el);
      assert(columns > 1, `md shows ${columns} column`);
      const scrollbar = el.offsetWidth - el.clientWidth;
      const fewer = columns - 1;
      const width =
        fewer * TILE_MIN.md + (fewer - 1) * GRID_GAP + 2 * SCROLL_PADDING + scrollbar + 40;
      const narrowed = await keeps('narrower', () => (el.style.width = `${width}px`));
      assert(
        narrowed.now === fewer,
        `narrowing to ${width} px went from ${narrowed.was} to ${narrowed.now} columns`
      );
      await untilFrame(() => allReady(el), 5_000, 'visible tiles ready after narrowing');
      await pause('U4-narrowed');
      const widened = await keeps('wider again', () => (el.style.width = ''));
      assert(
        widened.now === columns,
        `widening went from ${widened.was} to ${widened.now} columns, not back to ${columns}`
      );
    },
  },
  {
    id: 'U5',
    title:
      'Right-clicking a card, then a list row, at offset 60,000 opens the menu, and Escape closes it',
    run: async ({ scroll, assert, pause }) => {
      const target = 60_000;
      const { total } = useAssetStore.getState();
      assert(total > target, `only ${total} results`);
      const rightClick = async (what: string, idPrefix: string, shot: string) => {
        const el = scroll();
        const { columns, rowPitch } = layoutOf(el);
        el.scrollTop = Math.ceil(rowTop(rowOfIndex(target, columns), rowPitch));
        const selector = `[data-index="${target}"]:not([data-placeholder])`;
        await untilFrame(() => el.querySelector(selector) !== null, 5_000, `${what} ${target}`);
        const tile = el.querySelector<HTMLElement>(selector)!;
        const r = tile.getBoundingClientRect();
        tile.dispatchEvent(
          new MouseEvent('contextmenu', {
            bubbles: true,
            cancelable: true,
            button: 2,
            clientX: r.left + r.width / 2,
            clientY: r.top + r.height / 2,
          })
        );
        await frames(2);
        assert(document.getElementById('grid-menu-autotag'), `${what}: no Auto-tag item`);
        const id = tile.id.slice(idPrefix.length);
        assert(
          tile.id.startsWith(idPrefix) && useUIStore.getState().selectedAssetIds.has(id),
          `${what}: the right-clicked ${what} is not selected`
        );
        await pause(shot);
        window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
        await frames(2);
        assert(!document.getElementById('grid-menu-autotag'), `${what}: Escape left the menu open`);
      };
      await rightClick('card', 'asset-', 'U5-context-menu');
      useUIStore.getState().setViewMode('list');
      await frames(3);
      await rightClick('list row', 'asset-row-', 'U5-list-menu');
    },
  },
  {
    id: 'U6',
    title:
      'A scroll faster than 2 viewports a second shows the scrub label under name and date sort, ' +
      'and it hides 0.6 s after the scroll stops',
    run: async ({ scroll, assert, pause, sleep }) => {
      const { field, order } = useAssetStore.getState().query.sort;
      try {
        for (const sort of ['name', 'date'] as const) {
          await sortResults(sort, 'asc');
          const el = scroll();
          el.scrollTop = 0.3 * (el.scrollHeight - el.clientHeight);
          await frames(3);
          // The jump is a fast scroll too: start once its label has gone
          await untilFrame(() => !scrubLabel(), 30_000, `${sort}: the label from the jump hides`);
          const watch = watchLabel(el.parentElement!);
          try {
            // Slower than the threshold: no label
            const slowStart = performance.now();
            const slow = await scrollAt(el, 1, sleep(700));
            assert(
              watch.shownSince(slowStart) === 0,
              `${sort}: the label showed at 1 viewport a second (${slow.summary})`
            );
            // Faster: the label, and for name sort the screenshot while it shows
            const fastStart = performance.now();
            const fast = await scrollAt(
              el,
              4,
              sort === 'name' ? pause('U6-scrub-label') : sleep(1_000)
            );
            assert(
              watch.shownSince(fastStart) > 0,
              `${sort}: no label at 4 viewports a second (${fast.summary})`
            );
            assert(watch.ariaHidden, `${sort}: the label is not aria-hidden`);
            // It hides 0.6 s after the last scroll event. A stalled webview runs the timer late:
            // a hide past 900 ms passes only when the main thread was stalled across the 600 ms
            // deadline and the label hid as soon as it resumed.
            const frameTimes = [fast.lastScroll];
            await untilFrame(
              () => {
                frameTimes.push(performance.now());
                return !scrubLabel() && watch.hiddenAt() > fast.lastScroll;
              },
              30_000,
              `${sort}: the label hides after the last scroll event`
            );
            const deadline = fast.lastScroll + 600;
            const after = frameTimes.findIndex((t) => t >= deadline);
            const stalledUntil =
              after > 0 && frameTimes[after] - frameTimes[after - 1] > 300
                ? frameTimes[after]
                : null;
            const hidAfter = Math.round(watch.hiddenAt() - fast.lastScroll);
            assert(
              hidAfter >= 550 &&
                (hidAfter <= 900 ||
                  (stalledUntil !== null && watch.hiddenAt() <= stalledUntil + 200)),
              `${sort}: the label hid ${hidAfter} ms after the last scroll event ` +
                `(${fast.summary}; ` +
                (stalledUntil === null
                  ? 'no stall over the deadline)'
                  : `stalled until ${Math.round(stalledUntil - fast.lastScroll)} ms)`)
            );
            // What it showed last: the first visible asset's position and, once its page
            // arrived, its key
            const m = SCRUB_LABEL.exec(watch.text);
            assert(m && m[3] !== undefined, `${sort}: the label last read "${watch.text}"`);
            const [, position, count, key] = m!;
            const { total, pages } = useAssetStore.getState();
            assert(parseCount(count) === total, `${sort}: the label counts ${count} of ${total}`);
            // The first visible asset, or the first of the row above when only its gap shows,
            // as the footer counts it
            const index = parseCount(position) - 1;
            const row = firstVisibleRow(el);
            const { columns } = layoutOf(el);
            const rowTopPx = el
              .querySelector<HTMLElement>(`[data-index="${row[0]}"]`)!
              .getBoundingClientRect().top;
            const gapShows = rowTopPx > el.getBoundingClientRect().top + 0.5;
            assert(
              index === row[0] || (gapShows && index === row[0] - columns),
              `${sort}: the label reads ${position}, asset ${row[0] + 1} is the first visible`
            );
            const asset = assetAt({ pages }, index);
            assert(asset, `${sort}: asset ${index} is not cached`);
            const expected = scrubKey(asset!, sort);
            assert(
              key === expected,
              `${sort}: the key is "${key}", asset ${index} has "${expected}"`
            );
          } finally {
            watch.stop();
          }
        }
      } finally {
        await sortResults(field, order);
      }
    },
  },
  {
    id: 'U7',
    title:
      'With the container focused, End, Home, Page Down and Page Up page through the results, ' +
      'and they and Space are prevented, Space still opening QuickLook',
    run: async ({ scroll, assert }) => {
      const el = scroll();
      const { total } = useAssetStore.getState();
      const { rowPitch } = layoutOf(el);
      const atTop = () => el.scrollTop;
      const inView = (index: number) => tilesInView(el).some((t) => indexOf(t) === index);

      // A pointerdown on a card focuses the grid, and keeps it there when the browser's own
      // mousedown then focuses the card; a control in the card keeps its focus, and so does the
      // background
      const down = (target: Element) =>
        target.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }));
      const blur = () => (document.activeElement as HTMLElement | null)?.blur();
      const focused = () => document.activeElement?.id || document.activeElement?.tagName;
      // Card 0 at the top, once its page has arrived: a stalled webview can outlast the reset's
      // wait, and the steps before may have paged away
      const card0 = async () => {
        const selector = '[data-index="0"]:not([data-placeholder])';
        if (el.scrollTop !== 0) el.scrollTop = 0;
        await untilFrame(
          () => el.querySelector(selector) !== null,
          30_000,
          `card 0 rendered (scrollTop ${el.scrollTop})`
        );
        return el.querySelector<HTMLElement>(selector)!;
      };
      const card = await card0();
      const favorite = card.querySelector('button');
      assert(favorite, "card 0's favorite button is not rendered");
      blur();
      down(favorite!);
      assert(
        document.activeElement !== el,
        'a pointerdown on the favorite button focused the grid'
      );
      blur();
      down(card!.firstElementChild!);
      card!.focus();
      assert(document.activeElement === el, `a click on a card left focus on ${focused()}`);
      blur();
      await frames(2);
      down(el);
      assert(document.activeElement === el, 'a pointerdown on the background did not focus it');

      for (const [key, code] of [
        ['Home', 'Home'],
        ['End', 'End'],
        ['PageUp', 'PageUp'],
        ['PageDown', 'PageDown'],
        [' ', 'Space'],
      ]) {
        const event = press(el, key, code);
        assert(event.defaultPrevented, `${code} was not prevented`);
        const up = new KeyboardEvent('keyup', { key, code, bubbles: true });
        if (code === 'Space') el.dispatchEvent(up);
      }
      assert(!press(el, 'ArrowDown').defaultPrevented, 'ArrowDown was prevented');

      // Space on a control is left to it: a favorite button focused with Tab keeps its activation
      const fav = (await card0()).querySelector<HTMLElement>('button');
      assert(fav, "card 0's favorite button is not rendered");
      fav!.focus();
      assert(
        !press(fav!, ' ', 'Space').defaultPrevented,
        'Space on the favorite button was prevented'
      );
      fav!.dispatchEvent(new KeyboardEvent('keyup', { key: ' ', code: 'Space', bubbles: true }));
      el.focus();

      // Space, though prevented, still reaches App's QuickLook handler: holding it opens
      // QuickLook on the focused asset, releasing it closes it
      const quickLook = () => document.querySelector('[role="dialog"][aria-modal="true"]');
      const asset = assetAt(useAssetStore.getState(), 0);
      assert(asset, 'asset 0 is not cached');
      assert(!quickLook(), 'a dialog was open before Space');
      useUIStore.getState().focusAsset(asset!.id);
      try {
        await frames(2);
        // With an asset focused too, Space on the favorite button is still left to the button
        const button = (await card0()).querySelector<HTMLElement>('button')!;
        button.focus();
        assert(
          !press(button, ' ', 'Space').defaultPrevented,
          'Space on the favorite button was prevented while an asset was focused'
        );
        await frames(2);
        assert(!quickLook(), 'Space on the favorite button opened QuickLook');
        button.dispatchEvent(
          new KeyboardEvent('keyup', { key: ' ', code: 'Space', bubbles: true })
        );
        el.focus();
        await frames(2);
        assert(press(el, ' ', 'Space').defaultPrevented, 'Space was not prevented');
        await frames(2);
        assert(quickLook(), 'Space on the focused grid did not open QuickLook');
        el.dispatchEvent(new KeyboardEvent('keyup', { key: ' ', code: 'Space', bubbles: true }));
        await frames(2);
        assert(!quickLook(), 'releasing Space left QuickLook open');
      } finally {
        useUIStore.getState().focusAsset(null);
      }

      el.scrollTop = 0;
      await frames(2);
      press(el, 'End');
      await frames(3);
      assert(
        Math.abs(atTop() - (el.scrollHeight - el.clientHeight)) <= 1,
        `End scrolled to ${atTop()}, not ${el.scrollHeight - el.clientHeight}`
      );
      assert(inView(total - 1), `End: the last asset (${total - 1}) is not in view`);
      press(el, 'Home');
      await frames(3);
      assert(atTop() === 0 && inView(0), `Home scrolled to ${atTop()}`);
      press(el, 'PageDown');
      await frames(3);
      const step = el.clientHeight - rowPitch;
      assert(
        Math.abs(atTop() - step) <= 1,
        `Page Down moved ${atTop()} px, one viewport minus one row is ${step}`
      );
      press(el, 'PageDown');
      await frames(3);
      assert(Math.abs(atTop() - 2 * step) <= 2, `a second Page Down reached ${atTop()}`);
      press(el, 'PageUp');
      await frames(3);
      assert(Math.abs(atTop() - step) <= 2, `Page Up went back to ${atTop()}, not ${step}`);

      // A card focused with Tab scrolls out of the window and unmounts on End, handing focus to the
      // grid, and the keys still page from there
      el.scrollTop = 0;
      await frames(3);
      const tabbed = await card0();
      tabbed.focus();
      assert(document.activeElement === tabbed, `Tab focus went to ${focused()}, not card 0`);
      // WebKit reveals a newly focused element on its next layout; a real key press comes later
      await frames(3);
      press(document.activeElement as HTMLElement, 'End');
      await frames(3);
      assert(
        !tabbed!.isConnected,
        `card 0 is still mounted after End: scrollTop ${atTop()} of ` +
          `${el.scrollHeight - el.clientHeight}, focus on ${focused()}`
      );
      assert(document.activeElement === el, `card 0 unmounted, focus went to ${focused()}`);
      press(document.activeElement as HTMLElement, 'Home');
      await frames(3);
      assert(
        atTop() === 0,
        `Home after End from a card left scrollTop at ${atTop()} (${focused()})`
      );
      press(document.activeElement as HTMLElement, 'PageDown');
      await frames(3);
      assert(Math.abs(atTop() - step) <= 1, `Page Down from ${focused()} moved ${atTop()} px`);

      // A clicked favorite button keeps its focus; scrolled away with no key, as a wheel would, it
      // hands focus to the grid too
      el.scrollTop = 0;
      await frames(3);
      const button = (await card0()).querySelector<HTMLElement>('button');
      assert(button, "card 0's favorite button is not rendered");
      button!.focus();
      assert(document.activeElement === button, `the favorite button left focus on ${focused()}`);
      await frames(3);
      el.scrollTop = el.scrollHeight - el.clientHeight;
      await frames(3);
      assert(!button!.isConnected, 'the favorite button is still mounted at the bottom');
      assert(document.activeElement === el, `the button unmounted, focus went to ${focused()}`);
      press(document.activeElement as HTMLElement, 'Home');
      await frames(3);
      assert(atTop() === 0, `Home after the button unmounted left scrollTop at ${atTop()}`);

      // The keys belong to a text field while one has focus
      const input = document.querySelector<HTMLInputElement>('#toolbar-search');
      assert(input, 'no text field to focus');
      input!.focus();
      const before = atTop();
      assert(!press(el, 'End').defaultPrevented, 'End was prevented while a text field had focus');
      assert(atTop() === before, 'End scrolled while a text field had focus');
      input!.blur();
    },
  },
  {
    id: 'U8',
    title:
      'The image scaling toggle flips enlarged small images between pixelated and smooth in the ' +
      'grid, the list, the detail panel and QuickLook, and survives a reload',
    run: async ({ scroll, assert, pause }) => {
      const toggle = () => document.getElementById('image-scaling-toggle');
      const ui = () => useUIStore.getState();
      assert(toggle(), 'no #image-scaling-toggle in grid view');
      assert(
        ui().imageScaling === 'pixelated',
        `a fresh profile starts ${ui().imageScaling}, not pixelated`
      );
      assert(
        toggle()!.title === 'Image scaling: Pixelated',
        `the toggle is titled "${toggle()!.title}"`
      );
      // The hook re-checks on a devicePixelRatio change through this media query, so the webview
      // must evaluate it
      const resolution = `(resolution: ${window.devicePixelRatio}dppx)`;
      assert(window.matchMedia(resolution).matches, `${resolution} doesn't match`);

      // Name order interleaves packs, so one viewport mixes small textures shown at their own size
      // with 512 px model thumbnails. Date order keeps a pack together: once the thumbnail cleanup
      // pass has run, no viewport of 41 holds both kinds, and the search below finds nothing.
      const { field, order } = useAssetStore.getState().query.sort;
      try {
        await sortResults('name', 'asc');
        // Scroll through the results until the viewport shows an enlarged raster and a shrunk image
        const el = scroll();
        let small: HTMLImageElement | undefined;
        let large: HTMLImageElement | undefined;
        const steps = 40;
        for (let step = 0; step <= steps && !(small && large); step++) {
          el.scrollTop = (step / steps) * (el.scrollHeight - el.clientHeight);
          // On a fresh run copy the thumbnail cleanup pass slows tile loads, so look at whatever
          // has loaded rather than fail on how fast it loads
          await untilFrame(() => allReady(el), 20_000, '').catch(() => {});
          const shown = tilesInView(el).flatMap(loadedImages);
          // A card cut off by the viewport edge would hide the toggle's effect in the screenshots
          const view = el.getBoundingClientRect();
          const whole = (img: HTMLImageElement) => {
            const r = img.closest('[data-tile]')!.getBoundingClientRect();
            return r.top >= view.top && r.bottom <= view.bottom;
          };
          small = shown
            .filter((img) => whole(img) && isRaster(img) && isEnlarged(img, 'cover'))
            .sort((a, b) => a.naturalWidth - b.naturalWidth)[0];
          large = shown.find((img) => !isEnlarged(img, 'cover'));
        }
        assert(
          small && large,
          'no viewport showed both a whole card with an enlarged raster and a shrunk image'
        );
        const tile = small!.closest<HTMLElement>('[data-tile]')!;
        const id = tile.id.slice('asset-'.length);
        const index = Number(tile.dataset.index);
        const what = `asset ${index} (${small!.naturalWidth}x${small!.naturalHeight})`;

        const quickLook = async (open: boolean) => {
          const type = open ? 'keydown' : 'keyup';
          window.dispatchEvent(new KeyboardEvent(type, { code: 'Space', key: ' ', bubbles: true }));
          await frames(4);
        };
        const quickLookImage = () =>
          document.querySelector<HTMLImageElement>('[role="dialog"][aria-modal="true"] img');

        /** Checks the grid, the detail panel and QuickLook against `pref`, with screenshots. */
        const everywhere = async (pref: ImageScaling) => {
          assert(
            rendering(small!) === (pref === 'pixelated' ? 'pixelated' : 'auto'),
            `grid, ${pref}: ${what} renders ${rendering(small!)}`
          );
          assert(
            rendering(large!) === 'auto',
            `grid, ${pref}: a shrunk image renders ${rendering(large!)}`
          );
          await assertScaling(assert, 'grid', el, 'cover', pref);
          const panel = document.getElementById('detail-panel');
          assert(panel, 'no detail panel');
          await untilFrame(() => loadedImages(panel!).length > 0, 5_000, 'the detail preview');
          await assertScaling(assert, 'detail panel', panel!, 'contain', pref);
          await pause(`U8-${pref}`);
          // What the screenshot shows must still be the state asserted before it
          await assertScaling(assert, 'grid after the screenshot', el, 'cover', pref);
          await assertScaling(assert, 'detail panel after the screenshot', panel!, 'contain', pref);
          await quickLook(true);
          await untilFrame(
            () => !!quickLookImage()?.complete && quickLookImage()!.naturalWidth > 0,
            5_000,
            'the QuickLook image'
          );
          const big = quickLookImage()!;
          assert(isEnlarged(big, 'contain'), `QuickLook shows ${what} at ${big.clientWidth} px`);
          await assertScaling(assert, 'QuickLook', big.parentElement!, 'contain', pref);
          await pause(`U8-quicklook-${pref}`);
          await quickLook(false);
          assert(!quickLookImage(), 'releasing Space left QuickLook open');
        };

        // Select the small image, so the detail panel and QuickLook show it too
        ui().selectAsset(id);
        if (!ui().detailPanelOpen) ui().toggleDetailPanel();
        await frames(3);
        await everywhere('pixelated');

        // A resize alone re-checks: shrink the loaded small image's box below its pixel size, then
        // give it back. The src doesn't change, so no load event fires.
        const box = small!.parentElement!;
        const shrunkPx = Math.floor(
          Math.min(small!.naturalWidth, small!.naturalHeight) / window.devicePixelRatio / 2
        );
        assert(shrunkPx >= 1, `${what} is too small to shrink`);
        box.style.width = box.style.height = `${shrunkPx}px`;
        await untilFrame(
          () => rendering(small!) === 'auto',
          1_000,
          `${what} shrunk to ${small!.clientWidth}x${small!.clientHeight} to render auto, ` +
            `it renders ${rendering(small!)}`
        );
        assert(!isEnlarged(small!, 'cover'), `${what} is still enlarged at ${shrunkPx} px`);
        box.style.width = box.style.height = '';
        await untilFrame(
          () => rendering(small!) === 'pixelated',
          1_000,
          `${what} back at ${small!.clientWidth}x${small!.clientHeight} to render pixelated, ` +
            `it renders ${rendering(small!)}`
        );

        toggle()!.click();
        await frames(3);
        assert(ui().imageScaling === 'smooth', 'the toggle did not switch to smooth');
        assert(
          toggle()!.title === 'Image scaling: Smooth',
          `after a click the toggle is titled "${toggle()!.title}"`
        );
        await everywhere('smooth');

        // The list shows the same small image, in both states
        ui().setViewMode('list');
        await frames(3);
        assert(toggle(), 'no #image-scaling-toggle in list view');
        const list = scroll();
        const { columns, rowPitch } = layoutOf(list);
        list.scrollTop = Math.max(
          0,
          rowTop(rowOfIndex(index, columns), rowPitch) - list.clientHeight / 2
        );
        const row = `[data-index="${index}"]:not([data-placeholder])`;
        await untilFrame(
          () => {
            const r = list.querySelector(row);
            return r !== null && loadedImages(r).length > 0;
          },
          20_000,
          `list row ${index}'s image`
        );
        for (const pref of ['smooth', 'pixelated'] as const) {
          if (ui().imageScaling !== pref) {
            toggle()!.click();
            await frames(3);
          }
          await assertScaling(assert, 'list', list, 'cover', pref);
        }
        ui().setViewMode('grid');
        await frames(3);

        // A reload reads the persisted preference back: save what smooth persists, clobber the
        // store, put the saved value back and rehydrate, as a new page load would
        toggle()!.click();
        await frames(3);
        const KEY = 'opendam-ui';
        const saved = localStorage.getItem(KEY);
        const persisted = saved ? JSON.parse(saved).state?.imageScaling : undefined;
        assert(persisted === 'smooth', `localStorage holds imageScaling ${persisted}, not smooth`);
        useUIStore.setState({ imageScaling: 'pixelated' });
        localStorage.setItem(KEY, saved!);
        await useUIStore.persist.rehydrate();
        await frames(3);
        assert(
          ui().imageScaling === 'smooth',
          `after a reload the preference is ${ui().imageScaling}`
        );
        assert(
          toggle()!.title === 'Image scaling: Smooth',
          `after a reload the toggle is titled "${toggle()!.title}"`
        );

        // Leave the default for whatever runs next
        toggle()!.click();
        await frames(3);
        ui().clearSelection();
      } finally {
        await sortResults(field, order);
      }
    },
  },
  {
    id: 'M1',
    title:
      'Held still 60% down the grid, the tiles fill and the web process grows under 200 MB in 10 s',
    // Each backdrop-filter in a card was a compositing layer that WebKitGTK's GPU compositor
    // re-rendered without end once the grid scrolled, at about 500 MB/s (gridStyles.test.ts).
    run: async ({ scroll, assert, waitFor, pause, sleep, rss }) => {
      const el = scroll();
      el.scrollTop = 0.6 * (el.scrollHeight - el.clientHeight);
      await waitFor(() => allReady(el), 10_000, 'visible tiles ready 60% down');
      await sleep(1000);
      const start = await rss();
      let most = start;
      for (let s = 0; s < 10; s++) {
        await sleep(1000);
        most = Math.max(most, await rss());
      }
      const grewMb = Math.round((most - start) / 2 ** 20);
      assert(grewMb < 200, `the web process grew ${grewMb} MB in 10 s of holding still`);
      await pause('M1-held-still');
    },
  },
  {
    id: 'U9',
    title:
      'The library row expands into its folders with their counts, a folder shows exactly ' +
      'its files, and a delete updates the counts and drops a folder it empties',
    run: async ({ assert, waitFor, pause, sleep }) => {
      const top = (await expandBenchLibrary({ assert, sleep })).filter((f) =>
        isFolderShown(f, false)
      );
      await waitFor(() => top.every((f) => folderRow(f.path)), 10_000, 'the top-level folders');
      const rows = [
        ...document.querySelectorAll<HTMLElement>(`[data-library-id="${BENCH_LIBRARY}"]`),
      ]
        .filter((row) => !row.dataset.folderPath!.includes('/'))
        .map((row) => `${row.dataset.folderPath} (${shownCount(row)})`);
      const expected = top.map((f) => `${f.path} (${f.itemCount})`);
      assert(
        rows.join(', ') === expected.join(', '),
        `the library row shows [${rows.join(', ')}], get_folders lists [${expected.join(', ')}]`
      );
      await pause('U9-top-level-folders');

      // Selecting a kit shows exactly the files below it, as many as its node counts
      const kitPath = '3D assets/Platformer Kit';
      folderRow('3D assets')!.querySelector<HTMLElement>('[data-folder-toggle]')!.click();
      await waitFor(() => folderRow(kitPath) !== null, 5_000, kitPath);
      const kitCount = shownCount(folderRow(kitPath)!);
      folderRow(kitPath)!.click();
      await waitFor(
        () => {
          const s = useAssetStore.getState();
          return s.query.folder?.path === kitPath && !s.loading && s.total > 0;
        },
        10_000,
        `the search for ${kitPath}`
      );
      const { query, total } = useAssetStore.getState();
      assert(query.folder?.libraryId === BENCH_LIBRARY, 'the folder names another library');
      assert(total === kitCount, `the grid holds ${total} assets, the node counts ${kitCount}`);
      const all = await api.searchAssets({
        query,
        page: 0,
        pageSize: total + 1,
        includeTotal: true,
      });
      const root = useLibraryStore.getState().libraries.find((l) => l.id === BENCH_LIBRARY)!
        .rootPaths[0];
      const outside = all.assets.filter((a) => !a.filePath.startsWith(`${root}/${kitPath}/`));
      assert(all.assets.length === kitCount, `the search returned ${all.assets.length} assets`);
      assert(outside.length === 0, `${outside[0]?.filePath} is not below ${kitPath}`);
      const status = document.getElementById('status-folder')?.textContent ?? '';
      assert(status.includes(kitPath), `the status bar reads "${status}"`);
      await pause('U9-platformer-kit');

      // A rename keeps a one-file folder's counts, and deleting the file drops the folder and
      // takes one from every folder above it, with no manual refresh
      const { folder, slowestMs } = await singleFileFolder();
      assert(slowestMs < 50, `a node took ${Math.round(slowestMs)} ms to load`);
      const ancestors: string[] = [];
      for (let p = parentFolder(folder.path); p !== ''; p = parentFolder(p)) ancestors.unshift(p);
      for (const path of ancestors) {
        if (!useFolderTreeStore.getState().expanded[BENCH_LIBRARY]?.includes(path)) {
          await useFolderTreeStore.getState().toggle(BENCH_LIBRARY, path);
        }
      }
      await waitFor(() => folderRow(folder.path) !== null, 5_000, folder.path);
      const before = ancestors.map((path) => shownCount(folderRow(path)!));
      const inFolder = await api.searchAssets({
        query: { ...query, folder: { libraryId: BENCH_LIBRARY, path: folder.path } },
        page: 0,
        pageSize: 2,
        includeTotal: true,
      });
      assert(inFolder.total === 1, `${folder.path} holds ${inFolder.total} assets, not 1`);
      const asset = inFolder.assets[0];
      // On a fresh run copy the one-time thumbnail cleanup holds a long read, and no write
      // commits until it ends: a rename it blocks fails and is undone, so try until one lands
      const writeDeadline = performance.now() + STRUCTURE_TIMEOUT_MS;
      for (;;) {
        const failed = await useAssetStore
          .getState()
          .renameAsset(asset.id, 'u9-renamed')
          .then(() => null)
          .catch((e: unknown) => String(e));
        if (!failed) break;
        assert(performance.now() < writeDeadline, `the rename kept failing: ${failed}`);
        await sleep(5_000);
      }
      const renamed = await api.getAsset(asset.id);
      assert(renamed.fileName.startsWith('u9-renamed.'), `the rename gave ${renamed.fileName}`);
      assert(shownCount(folderRow(folder.path)!) === 1, `${folder.path} lost its file`);

      // Rows only: the file stays on the run copy rather than going to the trash
      await useAssetStore.getState().deleteAsset(asset.id, false);
      await waitFor(() => folderRow(folder.path) === null, 30_000, `${folder.path} to drop out`);
      await waitFor(
        () => ancestors.every((path, i) => shownCount(folderRow(path)!) === before[i] - 1),
        30_000,
        `the counts above ${folder.path} to drop by one, from ${before.join(', ')}`
      );
      await pause('U9-after-delete');

      useAssetStore.getState().setQuery({ folder: undefined });
    },
  },
  {
    id: 'U10',
    title:
      'Packs show a pack icon in the tree, their licenses and previews leave the grid, ' +
      '"Show hidden files" brings them back, and the status bar counts items and files',
    run: async ({ assert, waitFor, pause, sleep }) => {
      await expandBenchLibrary({ assert, sleep });
      const kitPath = '3D assets/Car Kit';
      await expandFolder('3D assets');
      await expandFolder('Audio');
      await waitFor(() => folderRow(kitPath) !== null, 10_000, kitPath);
      const kitRow = folderRow(kitPath)!;
      assert(kitRow.dataset.pack === 'true', `${kitPath} is not marked as a pack`);
      // A pack whose files are all hidden still shows, with no visible item
      const audio = await api.getFolders(BENCH_LIBRARY, 'Audio');
      const allHidden = audio.find((f) => f.isPack && f.itemCount === 0);
      assert(allHidden, 'no pack in Audio/ has every file hidden');
      await waitFor(() => folderRow(allHidden!.path) !== null, 5_000, allHidden!.path);
      assert(shownCount(folderRow(allHidden!.path)!) === 0, `${allHidden!.path} counts items`);
      const kit = (await api.getFolders(BENCH_LIBRARY, '3D assets')).find(
        (f) => f.path === kitPath
      )!;
      assert(kit.hiddenCount >= 2, `${kitPath} has ${kit.hiddenCount} hidden files`);
      assert(shownCount(kitRow) === kit.itemCount, `${kitPath}'s badge isn't its item count`);

      // The kit's grid holds no support file, and the status bar counts them as files only
      const support = ['License.txt', 'Preview.png'];
      const status = () => document.getElementById('status-count')?.textContent ?? '';
      const results = async () => {
        const { query, total } = useAssetStore.getState();
        return (await api.searchAssets({ query, page: 0, pageSize: total + 1 })).assets;
      };
      const names = async () => (await results()).map((a) => a.fileName);
      kitRow.click();
      await waitFor(
        () => {
          const s = useAssetStore.getState();
          return s.query.folder?.path === kitPath && !s.loading && s.total > 0;
        },
        30_000,
        `the search for ${kitPath}`
      );
      let s = useAssetStore.getState();
      assert(
        s.total === kit.itemCount,
        `the grid holds ${s.total}, the node counts ${kit.itemCount}`
      );
      // The files are the items, the hidden files and the format groups' other members
      const shownItems = await results();
      const members = shownItems.reduce((n, a) => n + (a.groupFormats?.length ?? 1) - 1, 0);
      assert(
        s.fileTotal === kit.itemCount + kit.hiddenCount + members,
        `the search counts ${s.fileTotal} files, the node ${kit.itemCount} + ${kit.hiddenCount} ` +
          `hidden, with ${members} other group members`
      );
      const visible = shownItems.map((a) => a.fileName);
      assert(!support.some((n) => visible.includes(n)), `the grid shows ${support.join(' or ')}`);
      const expected = formatItemsAndFiles(s.total, s.fileTotal);
      assert(status() === expected, `the status bar reads "${status()}", not "${expected}"`);

      // A search finding only the license shows no item, and one file
      await searchWith({ text: 'license' }, waitFor);
      s = useAssetStore.getState();
      assert(s.total === 0 && s.fileTotal === 1, `"license" found ${s.total} (${s.fileTotal})`);
      await waitFor(() => status() === '0 items (1 file)', 5_000, 'the status bar');
      await pause('U10-license-hidden');

      // "Show hidden files" brings it back, and the kit's badge counts its hidden files
      const checkbox = document.querySelector<HTMLInputElement>(
        '#sidebar-filter-show-hidden input'
      );
      assert(checkbox, 'no "Show hidden files" checkbox');
      checkbox!.click();
      await waitFor(
        () => {
          const st = useAssetStore.getState();
          return st.query.filter.showHidden === true && !st.loading && st.total === 1;
        },
        30_000,
        'the license to show'
      );
      await waitFor(
        () =>
          document
            .querySelector('[data-tile][data-index="0"]')
            ?.textContent?.includes('License.txt') ?? false,
        10_000,
        'the License.txt tile'
      );
      assert(status() === '1 item (1 file)', `the status bar reads "${status()}"`);
      assert(
        shownCount(folderRow(kitPath)!) === kit.itemCount + kit.hiddenCount,
        `${kitPath}'s badge doesn't count its hidden files`
      );
      await pause('U10-license-shown');

      await searchWith({ text: '' }, waitFor);
      s = useAssetStore.getState();
      // The hidden files join the items, while group members stay out of the grid
      assert(
        s.total === kit.itemCount + kit.hiddenCount,
        `with hidden files, ${s.total} items, the node ${kit.itemCount} + ${kit.hiddenCount}`
      );
      const all = await names();
      assert(
        support.every((n) => all.includes(n)),
        `"Show hidden files" shows ${support.filter((n) => all.includes(n)).join(', ') || 'none'} of ${support.join(', ')}`
      );

      checkbox!.click();
      await waitFor(
        () => !useAssetStore.getState().query.filter.showHidden,
        5_000,
        'hidden files off'
      );
      await waitFor(() => !useAssetStore.getState().loading, 30_000, 'the search');
      useAssetStore.getState().setQuery({ folder: undefined });
    },
  },
  {
    id: 'U11',
    title:
      'A Kenney kit model shows once, as its GLB with a GLB +2 badge, its FBX format folder and ' +
      'an FBX filter still list it, and each format folder counts every model',
    run: async ({ assert, waitFor, pause, sleep }) => {
      await expandBenchLibrary({ assert, sleep });
      const kitPath = '3D assets/Car Kit';
      const modelsPath = `${kitPath}/Models`;
      await expandFolder('3D assets');
      await expandFolder(kitPath);
      await expandFolder(modelsPath);
      const formatPaths = ['GLB format', 'FBX format', 'OBJ format'].map(
        (f) => `${modelsPath}/${f}`
      );
      await waitFor(
        () => formatPaths.every((p) => folderRow(p) !== null),
        10_000,
        "the kit's format folders"
      );

      // `Models/` counts each model once, and every format folder counts all of them
      const models = (await api.getFolders(BENCH_LIBRARY, kitPath)).find(
        (f) => f.path === modelsPath
      )!;
      const formats = await api.getFolders(BENCH_LIBRARY, modelsPath);
      for (const path of formatPaths) {
        const node = formats.find((f) => f.path === path);
        assert(node, `no ${path} node`);
        assert(
          node!.itemCount === models.itemCount,
          `${path} counts ${node!.itemCount}, ${modelsPath} ${models.itemCount}`
        );
        assert(shownCount(folderRow(path)!) === node!.itemCount, `${path}'s badge`);
      }
      assert(shownCount(folderRow(modelsPath)!) === models.itemCount, `${modelsPath}'s badge`);

      /** The current search's every result, with its total. */
      const results = async () => {
        const { query, total } = useAssetStore.getState();
        const page = await api.searchAssets({ query, page: 0, pageSize: total + 1 });
        return { total, assets: page.assets };
      };
      const selectFolder = async (path: string) => {
        folderRow(path)!.click();
        await waitFor(
          () => {
            const st = useAssetStore.getState();
            return st.query.folder?.path === path && !st.loading && st.total > 0;
          },
          30_000,
          `the search for ${path}`
        );
      };

      // The grid lists each model once, as its GLB with a solid `GLB +2` badge
      await selectFolder(modelsPath);
      let found = await results();
      assert(
        found.total === models.itemCount,
        `the grid holds ${found.total}, the node counts ${models.itemCount}`
      );
      const modelItems = found.assets.filter((a) => a.kind === '3d_model');
      const notGlb = modelItems.filter((a) => a.extension !== 'glb');
      assert(notGlb.length === 0, `${notGlb[0]?.fileName} shows in the grid`);
      const sedan = found.assets.find((a) => a.fileName === 'sedan.glb');
      assert(sedan, 'no sedan.glb in the grid');
      assert(
        sedan!.groupFormats?.join(',') === 'glb,fbx,obj',
        `sedan.glb's group is ${sedan!.groupFormats?.join(', ')}`
      );
      await waitFor(
        () => document.querySelectorAll('[data-tile] [data-group-badge]').length > 0,
        10_000,
        'a group badge on a card'
      );
      const badges = [...document.querySelectorAll<HTMLElement>('[data-tile] [data-group-badge]')];
      const modelBadge = badges.find((b) => b.dataset.groupBadge === 'GLB +2');
      assert(modelBadge, `the badges read ${badges.map((b) => b.dataset.groupBadge).join(', ')}`);
      assert(
        modelBadge!.title === '3 files: GLB, FBX, OBJ',
        `the badge's tooltip reads "${modelBadge!.title}"`
      );
      const background = getComputedStyle(modelBadge!).backgroundColor;
      assert(background.startsWith('rgb('), `the badge's background is ${background}, not solid`);

      // Each model's card shows Kenney's render, from `Previews/`, which lists no item of its own
      assert(sedan!.bundledPreview, 'sedan.glb has no bundled preview');
      const byId = new Map(found.assets.map((a) => [a.id, a]));
      const renderedCards = () =>
        [...document.querySelectorAll<HTMLElement>('[data-tile]')].flatMap((tile) => {
          const asset = byId.get(tile.id.replace(/^asset-/, ''));
          return asset?.bundledPreview ? [{ tile, asset }] : [];
        });
      await waitFor(() => renderedCards().length > 0, 10_000, 'a card with a bundled preview');
      for (const { tile, asset } of renderedCards()) {
        const src = tile.querySelector('img')?.getAttribute('src');
        assert(
          src === convertFileSrc(asset.bundledPreview!),
          `${asset.fileName}'s card shows ${src}, not ${asset.bundledPreview}`
        );
      }
      await searchWith({ folder: { libraryId: BENCH_LIBRARY, path: kitPath } }, waitFor);
      const kitItems = await results();
      const renders = kitItems.assets.filter((a) => a.filePath.includes('/Previews/'));
      assert(renders.length === 0, `${kitPath} lists ${renders[0]?.fileName} from Previews/`);
      await selectFolder(modelsPath);
      found = await results();
      await waitFor(
        () =>
          renderedCards().length > 0 && !document.querySelector('[data-tile-state="placeholder"]'),
        10_000,
        "the kit's pictures"
      );
      await pause('U11-kit-models');

      // A list row shows the same badge, solid and whole, in its extension column
      useUIStore.getState().setViewMode('list');
      const rowBadge = () =>
        [...document.querySelectorAll<HTMLElement>('[data-tile] [data-group-badge]')].find(
          (b) => b.dataset.groupBadge === 'GLB +2'
        );
      await waitFor(() => rowBadge() !== undefined, 10_000, "a list row's GLB +2 badge");
      const listBadge = rowBadge()!;
      assert(
        listBadge.textContent === 'GLB +2',
        `the row's badge reads "${listBadge.textContent}"`
      );
      assert(
        listBadge.title === '3 files: GLB, FBX, OBJ',
        `the row badge's tooltip reads "${listBadge.title}"`
      );
      const rowBackground = getComputedStyle(listBadge).backgroundColor;
      assert(
        rowBackground.startsWith('rgb('),
        `the row badge's background is ${rowBackground}, not solid`
      );
      assert(
        listBadge.scrollWidth <= listBadge.clientWidth,
        `the row badge is clipped: ${listBadge.scrollWidth} px of text in ${listBadge.clientWidth} px`
      );
      await pause('U11-list-badge');
      useUIStore.getState().setViewMode('grid');

      // `FBX format/` still lists the models, as their GLBs, and holds no GLB itself
      const fbxPath = `${modelsPath}/FBX format`;
      await selectFolder(fbxPath);
      found = await results();
      assert(found.total === models.itemCount, `${fbxPath} lists ${found.total} items`);
      const shown = new Set(found.assets.map((a) => a.extension));
      assert(!shown.has('fbx'), `${fbxPath} shows FBX files: ${[...shown].join(', ')}`);
      await pause('U11-fbx-format');
      await searchWith(
        { filter: { ...useAssetStore.getState().query.filter, extensions: ['glb'] } },
        waitFor
      );
      assert(
        useAssetStore.getState().total === 0,
        `${fbxPath} with .glb matches ${useAssetStore.getState().total}`
      );

      // Filtering the kit by FBX, or searching it for "fbx", finds the models through their FBX
      await searchWith(
        {
          folder: { libraryId: BENCH_LIBRARY, path: kitPath },
          filter: { ...useAssetStore.getState().query.filter, extensions: ['fbx'] },
        },
        waitFor
      );
      found = await results();
      const glbs = found.assets.filter((a) => a.extension === 'glb').length;
      assert(found.total > 0 && glbs === found.total, `FBX finds ${found.total}, ${glbs} as GLB`);
      assert(
        found.total === modelItems.length,
        `FBX finds ${found.total} of the kit's ${modelItems.length} models`
      );
      await searchWith(
        {
          text: 'fbx',
          filter: { ...useAssetStore.getState().query.filter, extensions: undefined },
        },
        waitFor
      );
      found = await results();
      assert(
        found.assets.some((a) => a.fileName === 'sedan.glb'),
        `"fbx" finds ${found.total} items, sedan.glb not among them`
      );

      await searchWith({ text: '', folder: undefined }, waitFor);
    },
  },
  {
    id: 'U12',
    title:
      'UI Pack sprites show once each with a solid x3 badge, same-named tiles and color or ' +
      'style variants stay apart, and the library root shows 51,139 items',
    run: async ({ assert, waitFor, pause, sleep }) => {
      await expandBenchLibrary({ assert, sleep });
      const results = async () => {
        const { query, total } = useAssetStore.getState();
        return (await api.searchAssets({ query, page: 0, pageSize: total + 1 })).assets;
      };
      const inFolder = (path: string) =>
        searchWith({ text: '', folder: { libraryId: BENCH_LIBRARY, path } }, waitFor);
      const status = () => document.getElementById('status-count')?.textContent ?? '';

      // The library root: plan A3's item count, for the golden snapshot's 83,095 files
      await inFolder('');
      let s = useAssetStore.getState();
      assert(s.total === ROOT_ITEMS, `the library root shows ${s.total} items, not ${ROOT_ITEMS}`);
      const rootStatus = formatItemsAndFiles(s.total, s.fileTotal);
      await waitFor(() => status() === rootStatus, 5_000, `the status bar to read ${rootStatus}`);
      await pause('U12-library-root');

      // Each UI Pack sprite shows once, as its default-scale PNG, standing for its Double PNG
      // and its vector
      const packPath = 'UI assets/UI Pack';
      await expandFolder('UI assets');
      await waitFor(() => folderRow(packPath) !== null, 10_000, packPath);
      const pack = (await api.getFolders(BENCH_LIBRARY, 'UI assets')).find(
        (f) => f.path === packPath
      )!;
      folderRow(packPath)!.click();
      await waitFor(
        () => {
          const st = useAssetStore.getState();
          return st.query.folder?.path === packPath && !st.loading && st.total > 0;
        },
        30_000,
        `the search for ${packPath}`
      );
      s = useAssetStore.getState();
      assert(s.total === pack.itemCount, `the grid holds ${s.total}, the node ${pack.itemCount}`);
      const sprites = (await results()).filter((a) => a.filePath.includes('/UI Pack/PNG/'));
      assert(sprites.length > 0, `${packPath} shows no sprite`);
      const copies = (await results()).filter((a) => /\/(Double|Vector)\//.test(a.filePath));
      assert(copies.length === 0, `${packPath} shows ${copies[0]?.filePath}`);
      const unstacked = sprites.filter(
        (a) => !a.filePath.includes('/Default/') || a.groupFormats?.join(',') !== 'png,png,svg'
      );
      assert(
        unstacked.length === 0,
        `${unstacked[0]?.filePath} stacks ${unstacked[0]?.groupFormats?.join(', ') ?? 'nothing'}`
      );
      // Blue and Red sprites of one name stay two items
      const button = sprites.filter((a) => a.fileName === 'button_rectangle_depth_flat.png');
      const colors = button.map((a) => a.filePath.split('/PNG/')[1]?.split('/')[0]).sort();
      assert(
        colors.join(',') === 'Blue,Green,Grey,Red,Yellow',
        `button_rectangle_depth_flat.png shows in ${colors.join(', ')}`
      );

      // The card's badge counts the stack's files, on a solid background
      const stackBadge = () =>
        [...document.querySelectorAll<HTMLElement>('[data-tile] [data-group-badge]')].find(
          (b) => b.dataset.groupBadge === 'x3'
        );
      await waitFor(() => stackBadge() !== undefined, 10_000, 'an x3 badge on a card');
      const badge = stackBadge()!;
      assert(badge.textContent === 'x3', `the badge reads "${badge.textContent}"`);
      assert(badge.title === '3 files: PNG, SVG', `the badge's tooltip reads "${badge.title}"`);
      const background = getComputedStyle(badge).backgroundColor;
      assert(background.startsWith('rgb('), `the badge's background is ${background}, not solid`);
      await waitFor(
        () => !document.querySelector('[data-tile-state="placeholder"]'),
        10_000,
        "the pack's pictures"
      );
      await pause('U12-ui-pack');

      // Two tiles sharing a name in different folders are two pictures
      await inFolder('2D assets/Pixel Platformer/Tiles');
      const tiles = (await results()).filter((a) => a.fileName === 'tile_0000.png');
      assert(
        tiles.length === 2 && tiles.every((a) => !a.groupFormats),
        `tile_0000.png shows ${tiles.length} times: ${tiles.map((a) => a.filePath).join(', ')}`
      );

      // A crosshair's Dark and Light styles stay apart, each with its 2x and vector copies
      await inFolder('2D assets/Crosshair Pack/PNG');
      const crosshairs = (await results()).filter((a) => a.fileName === 'crosshair-000.png');
      const styles = crosshairs.map((a) => a.filePath.split('/PNG/')[1]?.split('/')[0]).sort();
      assert(
        styles.join(',') === 'Dark,Glow,Light,Outline',
        `crosshair-000.png shows in ${styles.join(', ')}`
      );
      const dark = crosshairs.find((a) => a.filePath.includes('/PNG/Dark/'));
      assert(
        dark?.groupFormats?.join(',') === 'png,png,svg',
        `the Dark crosshair stacks ${dark?.groupFormats?.join(', ')}`
      );

      await searchWith({ text: '', folder: undefined }, waitFor);
    },
  },
];

/**
 * The checks `only` names, as comma-separated ids (`U8` or `U3,U8`), in registry order, or every
 * check when `only` is null. Throws on an id no check has.
 */
export function selectChecks(checks: UiCheck[], only: string | null): UiCheck[] {
  if (!only) return checks;
  const ids = only.split(',').map((id) => id.trim());
  const unknown = ids.filter((id) => !checks.some((c) => c.id === id));
  if (unknown.length > 0) throw new Error(`No UI check is named ${unknown.join(', ')}`);
  return checks.filter((c) => ids.includes(c.id));
}

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
