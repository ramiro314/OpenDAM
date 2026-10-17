// Dev-only scroll benchmark (docs/plans/fast-collection-browsing/plan.md, "Scroll benchmark").
// `main.tsx` loads it only in a dev build started with `VITE_SCROLL_BENCH`:
//   1       runs the timing scenarios at sm, md and lg
//   checks  runs the UI checks in `uiChecks.ts`
// Either way it writes one JSON report through `bench_report`. How to run it on bench data in a
// nested headless KWin: docs/agents/running-the-app.md, "Scroll benchmark".

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { useAssetStore, useUIStore } from '../store';
import { frameStats, minMax, rssStats, seededRandom, timeStats } from './benchStats';
import { UI_CHECKS, runUiChecks, type UiCheckContext } from './uiChecks';

type GridSize = 'sm' | 'md' | 'lg';
const GRID_SIZES: GridSize[] = ['sm', 'md', 'lg'];

const CONTINUOUS_MS = 60_000;
const CONTINUOUS_PX_PER_S = 3_000;
const END_TO_END_STEPS = 200;
const DWELL_MS = 150;
const BASELINE_TARGET_TILES = 20_000;
const BASELINE_TIMEOUT_MS = 120_000;
const JUMPS = 20;
const JUMP_SEED = 0x5eed;
const JUMP_TIMEOUT_MS = 5_000;
const PAUSE_MS = 3_000;
/** Startup indexing ends with `library:updated`; a big library on a cold cache takes a while. */
const INDEXING_TIMEOUT_MS = 180_000;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const nextFrame = () => new Promise<number>((resolve) => requestAnimationFrame(resolve));

const log = (line: string) => invoke<void>('bench_log', { line }).catch(() => {});
const readRss = () => invoke<number>('bench_rss');
const domNodes = () => document.getElementsByTagName('*').length;

/**
 * The windowed grid (fast-collection-browsing step 3) drops the store's `loadMore`. Until then
 * the grid appends pages forever, and the scenarios run their baseline variants.
 */
function gridIsWindowed(): boolean {
  return !('loadMore' in useAssetStore.getState());
}

function scrollEl(): HTMLElement {
  const el = document.getElementById('asset-scroll');
  if (!el) throw new Error('#asset-scroll is not on the page');
  return el;
}

async function waitFor(predicate: () => boolean, timeoutMs: number, what: string): Promise<void> {
  const deadline = performance.now() + timeoutMs;
  while (!predicate()) {
    if (performance.now() > deadline) throw new Error(`Timed out after ${timeoutMs} ms: ${what}`);
    await sleep(50);
  }
}

/** True when at least one tile is in the viewport and every one there is ready. */
function visibleTilesReady(): boolean {
  const el = document.getElementById('asset-scroll');
  if (!el) return false;
  const view = el.getBoundingClientRect();
  let visible = 0;
  for (const tile of el.querySelectorAll<HTMLElement>('[data-tile]')) {
    const r = tile.getBoundingClientRect();
    if (r.bottom <= view.top || r.top >= view.bottom) continue;
    if (tile.dataset.tileState !== 'ready') return false;
    visible++;
  }
  return visible > 0;
}

/** Records rAF frame intervals until `stop` is called. */
function recordFrames() {
  const intervals: number[] = [];
  let last: number | undefined;
  let running = true;
  const tick = (t: number) => {
    if (!running) return;
    if (last !== undefined) intervals.push(t - last);
    last = t;
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
  return {
    stop: () => {
      running = false;
      return intervals;
    },
  };
}

/** Grid view at `size`, a fresh search, scrolled to the top, visible tiles ready. */
async function resetGrid(size: GridSize): Promise<void> {
  const ui = useUIStore.getState();
  ui.clearSelection();
  ui.setViewMode('grid');
  ui.setGridSize(size);
  await useAssetStore.getState().search();
  await waitFor(() => document.getElementById('asset-scroll') !== null, 10_000, '#asset-scroll');
  scrollEl().scrollTop = 0;
  // A stuck tile shows in the log; the scenario still runs and measures what it finds
  await waitFor(visibleTilesReady, 10_000, 'visible tiles ready after reset').catch((e) =>
    log(`warning: ${String(e)}`)
  );
  await sleep(500);
}

async function continuousScroll() {
  const el = scrollEl();
  const intervals: number[] = [];
  const nodes: number[] = [domNodes()];
  const rss: number[] = [await readRss()];
  let clampedFrames = 0;
  const start = performance.now();
  let last = start;
  let lastSample = start;

  await new Promise<void>((resolve) => {
    const tick = (t: number) => {
      intervals.push(t - last);
      last = t;
      const target = (CONTINUOUS_PX_PER_S * (t - start)) / 1000;
      el.scrollTop = target;
      // The grid ran out of loaded content: the target is past the end
      if (el.scrollTop < target - 1) clampedFrames++;
      if (t - lastSample >= 1000) {
        lastSample = t;
        nodes.push(domNodes());
        readRss().then(
          (b) => rss.push(b),
          () => {}
        );
      }
      if (t - start < CONTINUOUS_MS) requestAnimationFrame(tick);
      else resolve();
    };
    requestAnimationFrame(tick);
  });
  intervals.shift(); // from the start time to the first frame, not a frame interval
  nodes.push(domNodes());
  rss.push(await readRss());

  return {
    durationMs: Math.round(performance.now() - start),
    targetPx: (CONTINUOUS_PX_PER_S * CONTINUOUS_MS) / 1000,
    reachedPx: Math.round(el.scrollTop),
    clampedFrames,
    tiles: el.querySelectorAll('[data-tile]').length,
    frames: frameStats(intervals),
    domNodes: minMax(nodes),
    rss: rssStats(rss),
  };
}

async function endToEnd() {
  const el = scrollEl();
  const frames = recordFrames();
  const nodes: number[] = [domNodes()];
  const rss: number[] = [await readRss()];
  const start = performance.now();
  const sample = async () => {
    await sleep(DWELL_MS);
    nodes.push(domNodes());
    rss.push(await readRss());
  };

  let variant: string;
  let steps = 0;
  let stopReason: string | undefined;
  if (gridIsWindowed()) {
    variant = `${END_TO_END_STEPS} equal steps, ${DWELL_MS} ms dwell`;
    for (; steps < END_TO_END_STEPS; steps++) {
      el.scrollTop = (steps / (END_TO_END_STEPS - 1)) * (el.scrollHeight - el.clientHeight);
      await sample();
    }
  } else {
    variant = `baseline: jump to the bottom until ${BASELINE_TARGET_TILES} tiles or ${BASELINE_TIMEOUT_MS / 1000} s`;
    const total = useAssetStore.getState().total;
    const tiles = () => el.querySelectorAll('[data-tile]').length;
    for (;;) {
      if (tiles() >= Math.min(total, BASELINE_TARGET_TILES)) {
        stopReason = tiles() >= total ? 'all-loaded' : 'target-reached';
        break;
      }
      if (performance.now() - start > BASELINE_TIMEOUT_MS) {
        stopReason = 'timeout';
        break;
      }
      el.scrollTop = el.scrollHeight;
      steps++;
      await sample();
    }
  }

  return {
    variant,
    steps,
    stopReason,
    durationMs: Math.round(performance.now() - start),
    tiles: el.querySelectorAll('[data-tile]').length,
    scrollHeightPx: el.scrollHeight,
    frames: frameStats(frames.stop()),
    domNodes: minMax(nodes),
    rss: rssStats(rss),
  };
}

async function randomJumps() {
  if (!gridIsWindowed()) {
    return {
      supported: false,
      reason: "today's grid can only reach loaded pages, so a jump to any offset is not possible",
    };
  }
  const el = scrollEl();
  const random = seededRandom(JUMP_SEED);
  const frames = recordFrames();
  const times: number[] = [];
  const jumps: { fraction: number; ms: number | null }[] = [];
  for (let i = 0; i < JUMPS; i++) {
    const fraction = random();
    el.scrollTop = fraction * (el.scrollHeight - el.clientHeight);
    const t0 = performance.now();
    await nextFrame();
    let ms: number | null = null;
    while (performance.now() - t0 < JUMP_TIMEOUT_MS) {
      if (visibleTilesReady()) {
        ms = performance.now() - t0;
        break;
      }
      await nextFrame();
    }
    jumps.push({ fraction: Math.round(fraction * 10_000) / 10_000, ms: ms && Math.round(ms) });
    if (ms !== null) times.push(ms);
    await sleep(DWELL_MS);
  }
  return {
    supported: true,
    seed: JUMP_SEED,
    timeouts: jumps.filter((j) => j.ms === null).length,
    timeToReady: timeStats(times),
    frames: frameStats(frames.stop()),
    jumps,
  };
}

async function runTiming() {
  const results: Record<string, unknown> = {};
  for (const size of GRID_SIZES) {
    const scenarios: Record<string, unknown> = {};
    for (const [name, run] of [
      ['continuousScroll', continuousScroll],
      ['endToEnd', endToEnd],
      ['randomJumps', randomJumps],
    ] as const) {
      await resetGrid(size);
      await log(`${size} ${name}: start`);
      scenarios[name] = await run();
      await log(`${size} ${name}: ${JSON.stringify(scenarios[name]).slice(0, 300)}`);
    }
    results[size] = scenarios;
  }
  return results;
}

async function pause(name: string) {
  await log(`pause ${name}`);
  await sleep(PAUSE_MS);
}

async function runChecks() {
  const ctx: UiCheckContext = {
    scroll: scrollEl,
    assert: (condition, message) => {
      if (!condition) throw new Error(message);
    },
    waitFor,
    pause,
    sleep,
  };
  await resetGrid('md');
  // Shows the runner's screenshot step works, and what the grid looked like when checks began
  await pause('checks-start');
  if (UI_CHECKS.length === 0) await log('no UI checks registered yet');
  return runUiChecks(UI_CHECKS, ctx, {
    before: () => resetGrid('md'),
    after: (r) => log(`${r.id} ${r.result}: ${r.title}${r.detail ? ` (${r.detail})` : ''}`),
  });
}

export async function startScrollBench(mode: string): Promise<void> {
  const kind = mode === 'checks' ? 'checks' : 'timing';
  const startedAt = new Date().toISOString();
  // Subscribe first: startup indexing may finish while the page is still loading
  let indexed = false;
  const unlisten = await listen('library:updated', () => {
    indexed = true;
  });
  await log(`${kind}: waiting for startup indexing and the first page`);

  const report: Record<string, unknown> = { kind };
  let renderMode = 'unknown';
  try {
    renderMode = await invoke<string>('bench_render_mode');
    const waitStart = performance.now();
    const indexingSettled = await waitFor(() => indexed, INDEXING_TIMEOUT_MS, 'library:updated')
      .then(() => true)
      .catch(() => false);
    unlisten();
    await waitFor(
      () => {
        const s = useAssetStore.getState();
        return !s.loading && s.total > 0 && document.getElementById('asset-scroll') !== null;
      },
      60_000,
      'the first page of results'
    );
    report.context = {
      renderMode,
      startedAt,
      window: { width: window.innerWidth, height: window.innerHeight },
      devicePixelRatio: window.devicePixelRatio,
      totalAssets: useAssetStore.getState().total,
      gridWindowed: gridIsWindowed(),
      indexingSettled,
      waitedForStartupMs: Math.round(performance.now() - waitStart),
      userAgent: navigator.userAgent,
    };
    await log(`${kind}: context ${JSON.stringify(report.context)}`);
    if (kind === 'checks') report.checks = await runChecks();
    else report.scenarios = await runTiming();
  } catch (e) {
    report.error = String(e);
    report.context ??= { renderMode, startedAt };
  }
  const name = kind === 'checks' ? `${renderMode}-checks` : renderMode;
  try {
    const path = await invoke<string>('bench_report', { name, json: JSON.stringify(report) });
    await log(report.error ? `failed ${path}: ${report.error}` : `done ${path}`);
  } catch (e) {
    await log(`failed: could not write the report: ${String(e)}`);
  }
}
