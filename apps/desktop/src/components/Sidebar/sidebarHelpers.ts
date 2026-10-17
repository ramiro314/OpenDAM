import type { Asset, FilterSpec } from '../../types';

const DAY_MS = 86400000;

/** Library name for a picked folder: its last path segment. */
export function libraryNameFromPath(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() || 'New Library';
}

/** Whether `name` (already trimmed) matches an existing name, ignoring case and padding. */
export function isNameTaken(existing: { name: string }[], name: string): boolean {
  return existing.some((item) => item.name.trim().toLowerCase() === name.toLowerCase());
}

/** Tags `id` may be nested under: every tag but itself and the tags nested in it. */
export function parentCandidates<T extends { id: string; parentId?: string | null }>(
  tags: T[],
  id: string
): T[] {
  const excluded = new Set([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const tag of tags) {
      if (tag.parentId && excluded.has(tag.parentId) && !excluded.has(tag.id)) {
        excluded.add(tag.id);
        grew = true;
      }
    }
  }
  return tags.filter((tag) => !excluded.has(tag.id));
}

/**
 * Asset id carried by a drop. A drag from outside the grid may carry a file path instead of an
 * id; it resolves to the loaded asset with that path, or stays as given if none matches.
 */
export function resolveDroppedAssetId(rawData: string, assets: Pick<Asset, 'id' | 'filePath'>[]) {
  if (!rawData.includes('/') && !rawData.includes('\\')) return rawData;
  const norm = (p: string) => p.replace(/\\/g, '/').toLowerCase();
  return assets.find((a) => norm(a.filePath) === norm(rawData))?.id ?? rawData;
}

/** Whether none of the filters the sidebar offers is set ("All assets" is active). */
export function isFilterEmpty(filter: FilterSpec): boolean {
  return (
    !filter.isFavorite &&
    !filter.unorganized &&
    (!filter.kinds || filter.kinds.length === 0) &&
    !filter.rating &&
    (!filter.colorLabels || filter.colorLabels.length === 0) &&
    !filter.dateRange &&
    !filter.sizeRange &&
    !filter.previewStatus &&
    (!filter.tags || filter.tags.length === 0)
  );
}

/**
 * The "Date Added" presets, each with the `from` timestamp it sets and whether the current
 * `from` matches it. A preset stays active for 10 seconds of slack after it was picked.
 */
export function datePresets(currentFrom: number | undefined, now: Date = new Date()) {
  const startOfDay = new Date(now);
  startOfDay.setHours(0, 0, 0, 0);
  const todayStart = startOfDay.getTime();
  const sevenDaysAgo = now.getTime() - 7 * DAY_MS;
  const thirtyDaysAgo = now.getTime() - 30 * DAY_MS;

  const isToday = currentFrom !== undefined && currentFrom >= todayStart;
  const is7Days = currentFrom !== undefined && !isToday && currentFrom >= sevenDaysAgo - 10000;
  const is30Days =
    currentFrom !== undefined && !isToday && !is7Days && currentFrom >= thirtyDaysAgo - 10000;

  return [
    { label: 'Today', from: todayStart, active: isToday },
    { label: 'Last 7 Days', from: sevenDaysAgo, active: is7Days },
    { label: 'Last 30 Days', from: thirtyDaysAgo, active: is30Days },
  ];
}
