import type { Asset } from '../types';

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

/**
 * A group's badge on its card and list row (plan C19). A model's format group names the main
 * item's format and how many other files the group holds, `GLB +2`, with every format in its
 * tooltip, `3 files: GLB, FBX, OBJ`. Any other group, such as a sprite's 2D stack, counts its
 * files, `x3`, with the same tooltip. `null` for a file on its own.
 */
export function formatGroupBadge(
  asset: Pick<Asset, 'kind' | 'groupFormats'>
): { label: string; title: string } | null {
  const { groupFormats } = asset;
  if (!groupFormats || groupFormats.length < 2) return null;
  const formats = groupFormats.map((f) => f.toUpperCase());
  return {
    label:
      asset.kind === '3d_model' ? `${formats[0]} +${formats.length - 1}` : `x${formats.length}`,
    title: `${formats.length} files: ${[...new Set(formats)].join(', ')}`,
  };
}

/**
 * How many files deleting or renaming an item reaches (plan C21): each member of its format
 * group, or the file alone, plus its render, whether it fits a card or not.
 */
export function itemFileCount(
  asset: Pick<Asset, 'groupFormats' | 'bundledPreview' | 'fallbackRender'>
): number {
  return (asset.groupFormats?.length ?? 1) + (asset.bundledPreview || asset.fallbackRender ? 1 : 0);
}

/** A delete dialog's action for `files` files, `Move 4 files to the trash`. */
export function formatTrashAction(files: number): string {
  return `Move ${files === 1 ? '1 file' : `${files.toLocaleString('en-US')} files`} to the trash`;
}
