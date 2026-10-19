// The scrub label's text (docs/plans/fast-collection-browsing/plan.md, "Navigation"): the
// position in the results and the sort key of the asset there, `41,200 of 83,095 - M`.

import type { Asset, AssetKind, SortField } from '../types';
import { formatSize } from './format';

export const KIND_LABEL: Record<AssetKind, string> = {
  '3d_model': '3D Model',
  material: 'Material',
  texture: 'Texture',
  hdri: 'HDRI',
  ies: 'IES',
  video: 'Video',
  image: 'Image',
  document: 'Document',
  other: 'Other',
};

const MONTH_YEAR = new Intl.DateTimeFormat('en-US', { month: 'short', year: 'numeric' });

/** What `asset` sorts by under `sortField`, short enough for the scrub label. */
export function scrubKey(asset: Asset, sortField: SortField): string {
  switch (sortField) {
    case 'name': {
      // The first code point, so a letter outside the BMP isn't split in two
      const first = [...asset.fileName][0] ?? '';
      return /^\p{L}$/u.test(first) ? first.toUpperCase() : '#';
    }
    case 'date':
      return MONTH_YEAR.format(new Date(asset.modifiedAt));
    case 'size':
      return formatSize(asset.sizeBytes);
    case 'rating': {
      const stars = Math.max(0, Math.min(5, Math.round(asset.rating)));
      return stars === 0 ? 'Unrated' : '★'.repeat(stars) + '☆'.repeat(5 - stars);
    }
    case 'kind':
      return KIND_LABEL[asset.kind] ?? asset.kind;
    case 'favorite':
      return asset.isFavorite ? 'Favorites' : 'Others';
  }
}

/**
 * The label for 0-based `index` of `total`: `41,200 of 83,095 - M` with the asset's key, or just
 * the position until its page is cached.
 */
export function scrubLabelText(index: number, total: number, key: string | null): string {
  const n = (x: number) => x.toLocaleString('en-US');
  const position = `${n(index + 1)} of ${n(total)}`;
  return key === null ? position : `${position} - ${key}`;
}
