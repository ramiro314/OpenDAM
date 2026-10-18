// The UI checks `VITE_SCROLL_BENCH=checks` runs (U1-U8 in
// docs/plans/fast-collection-browsing/plan.md). Each ticket that delivers a behavior adds its
// check here.

import { useAssetStore, useUIStore } from '../store';
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
        small = shown
          .filter((img) => isRaster(img) && isEnlarged(img, 'cover'))
          .sort((a, b) => a.naturalWidth - b.naturalWidth)[0];
        large = shown.find((img) => !isEnlarged(img, 'cover'));
      }
      assert(small && large, 'no viewport showed both an enlarged raster and a shrunk image');
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
