// The asset store's page cache (docs/plans/fast-collection-browsing/plan.md, "Page cache"): pages
// of search results by index, least recently wanted evicted first. Pure, so `useAssetStore` and
// its tests share it.

import type { Asset } from '../types';

/** Assets per page. */
export const PAGE_SIZE = 200;
/** Pages kept, about 5,000 assets (D3); more only while they are all wanted. */
export const MAX_PAGES = 25;
/** How long the wanted pages must stay the same before they are fetched (C2). */
export const SETTLE_MS = 80;
/** How far around its old index a refresh looks for the asset that was at the top (C5). */
export const ANCHOR_REACH = 200;

export interface PageCache {
  /** Page index to its assets, up to `PAGE_SIZE` each */
  pages: Map<number, Asset[]>;
  /** Cached page indices, least recently wanted first */
  lru: number[];
  /** Global index of every cached asset */
  indexById: Map<string, number>;
}

export function emptyCache(): PageCache {
  return { pages: new Map(), lru: [], indexById: new Map() };
}

export function pageOf(index: number): number {
  return Math.floor(index / PAGE_SIZE);
}

export function assetAt(cache: Pick<PageCache, 'pages'>, index: number): Asset | undefined {
  return cache.pages.get(pageOf(index))?.[index % PAGE_SIZE];
}

/** Moves `indices` that are cached to the most recently wanted end, in their order. */
export function touch(cache: PageCache, indices: number[]): PageCache {
  const wanted = indices.filter((i) => cache.pages.has(i));
  if (wanted.length === 0) return cache;
  const set = new Set(wanted);
  return { ...cache, lru: [...cache.lru.filter((i) => !set.has(i)), ...wanted] };
}

/**
 * Adds page `index`, most recently wanted, replacing any copy already cached, then evicts the
 * least recently wanted pages beyond `MAX_PAGES` that aren't in `wanted`.
 */
export function insertPage(
  cache: PageCache,
  index: number,
  assets: Asset[],
  wanted: Iterable<number>
): PageCache {
  const pages = new Map(cache.pages);
  const indexById = new Map(cache.indexById);
  const old = pages.get(index);
  if (old) dropIds(indexById, old, index);
  pages.set(index, assets);
  assets.forEach((a, i) => indexById.set(a.id, index * PAGE_SIZE + i));
  const lru = [...cache.lru.filter((i) => i !== index), index];
  return evict({ pages, lru, indexById }, wanted);
}

/** Drops least recently wanted pages until at most `MAX_PAGES` remain, never one in `wanted`. */
export function evict(cache: PageCache, wanted: Iterable<number>): PageCache {
  if (cache.pages.size <= MAX_PAGES) return cache;
  const keep = new Set(wanted);
  const pages = new Map(cache.pages);
  const indexById = new Map(cache.indexById);
  const lru: number[] = [];
  let excess = pages.size - MAX_PAGES;
  for (const index of cache.lru) {
    if (excess > 0 && !keep.has(index)) {
      dropIds(indexById, pages.get(index) ?? [], index);
      pages.delete(index);
      excess--;
    } else {
      lru.push(index);
    }
  }
  return { pages, lru, indexById };
}

/** Removes `assets`' ids from `indexById` where they still point into page `index`. */
function dropIds(indexById: Map<string, number>, assets: Asset[], index: number) {
  assets.forEach((a, i) => {
    if (indexById.get(a.id) === index * PAGE_SIZE + i) indexById.delete(a.id);
  });
}

/**
 * Replaces each asset of `ids` with `fn(asset)` in its page and in `pinned`. Only the pages
 * holding one of them get a new array, so only their rows re-render. Ids in neither are skipped.
 */
export function patchAssets(
  cache: PageCache,
  pinned: Map<string, Asset>,
  ids: string[],
  fn: (asset: Asset) => Asset
): { pages: Map<number, Asset[]>; pinned: Map<string, Asset> } | null {
  let pages: Map<number, Asset[]> | null = null;
  let nextPinned: Map<string, Asset> | null = null;
  const copied = new Set<number>();
  for (const id of ids) {
    const index = cache.indexById.get(id);
    const page = index === undefined ? undefined : (pages ?? cache.pages).get(pageOf(index));
    const slot = index === undefined ? -1 : index % PAGE_SIZE;
    const cached = page?.[slot]?.id === id ? page[slot] : undefined;
    const current = cached ?? pinned.get(id);
    if (!current) continue;
    const updated = fn(current);
    if (cached && page && index !== undefined) {
      pages ??= new Map(cache.pages);
      const p = pageOf(index);
      const copy = copied.has(p) ? page : [...page];
      copied.add(p);
      copy[slot] = updated;
      pages.set(p, copy);
    }
    if (pinned.has(id)) {
      nextPinned ??= new Map(pinned);
      nextPinned.set(id, updated);
    }
  }
  if (!pages && !nextPinned) return null;
  return { pages: pages ?? cache.pages, pinned: nextPinned ?? pinned };
}

/** The pages a refresh refetches: the wanted ones, and those within `ANCHOR_REACH` of the top. */
export function refreshPages(wanted: number[], anchorIndex: number, total: number): number[] {
  const pages = new Set(wanted);
  const last = pageOf(Math.max(0, total - 1));
  for (
    let p = pageOf(Math.max(0, anchorIndex - ANCHOR_REACH));
    p <= pageOf(anchorIndex + ANCHOR_REACH);
    p++
  ) {
    if (p <= last) pages.add(p);
  }
  return [...pages].sort((a, b) => a - b);
}

/**
 * Where the asset that was at the top, `id` at `oldIndex`, is after a refresh (C5): its new index
 * if the refetched pages hold it within `ANCHOR_REACH`, otherwise `oldIndex` clamped to `total`.
 */
export function findAnchor(
  pages: Map<number, Asset[]>,
  id: string | undefined,
  oldIndex: number,
  total: number
): number {
  if (id !== undefined) {
    for (let i = Math.max(0, oldIndex - ANCHOR_REACH); i <= oldIndex + ANCHOR_REACH; i++) {
      if (assetAt({ pages }, i)?.id === id) return i;
    }
  }
  return Math.max(0, Math.min(oldIndex, total - 1));
}

export type Schedule = (fn: () => void, ms: number) => () => void;

/**
 * Calls `onSettle` with the wanted pages once they have stayed the same for `SETTLE_MS` (C2), so
 * pages a drag flies past are never fetched. Updates with the same pages keep the timer running,
 * so a slow scroll within a page's span settles; `kick` settles at once, for when the cache
 * was replaced under unchanged pages.
 */
export function createSettler(onSettle: (pages: number[]) => void, schedule: Schedule) {
  let key: string | null = null;
  let pages: number[] = [];
  let cancel: (() => void) | null = null;
  return {
    update(next: number[]) {
      const nextKey = next.join(',');
      if (nextKey === key) return;
      key = nextKey;
      pages = next;
      cancel?.();
      cancel = schedule(() => {
        cancel = null;
        onSettle(pages);
      }, SETTLE_MS);
    },
    kick() {
      cancel?.();
      cancel = null;
      if (key !== null) onSettle(pages);
    },
    dispose() {
      cancel?.();
      cancel = null;
    },
  };
}
