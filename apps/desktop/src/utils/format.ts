/** Compact file size for grid cards and list rows, e.g. `512B`, `1.5KB`, `2.00GB`. */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)}MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)}GB`;
}

/**
 * The grid footer: the visible range out of every result, `41,201-41,236 of 82,292 items` (C12),
 * from 0-based `first` and `last`. Just the total when nothing is visible.
 */
export function formatVisibleRange(
  range: { first: number; last: number } | null,
  total: number
): string {
  const n = (x: number) => x.toLocaleString('en-US');
  const items = `${n(total)} ${total === 1 ? 'item' : 'items'}`;
  return range ? `${n(range.first + 1)}-${n(range.last + 1)} of ${items}` : items;
}

/** The status bar's result count: grid items, then the files they stand for, hidden ones included. */
export function formatItemsAndFiles(items: number, files: number): string {
  const n = (x: number) => x.toLocaleString('en-US');
  return `${n(items)} ${items === 1 ? 'item' : 'items'} (${n(files)} ${files === 1 ? 'file' : 'files'})`;
}
