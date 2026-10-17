import { useCallback, useEffect, useRef, useState } from 'react';
import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import { api } from '../../api';
import { useUIStore, useAssetStore, useLibraryStore, useAutotagStore } from '../../store';
import { autotagAvailability } from '../../utils/autotag';
import { menuTargets, openLimitReason } from '../../utils/contextMenu';
import { ContextMenu } from '../ContextMenu/ContextMenu';
import { IconEye, IconExternal, IconTag, IconTrash } from '../Icons';
import { AssetCard } from './AssetCard';
import { AssetListRow } from './AssetListRow';
import { FilterBar } from './FilterBar';
import type { Asset } from '../../types';
import styles from './AssetGrid.module.css';

const GRID_COLS: Record<'sm' | 'md' | 'lg', number> = {
  sm: 180,
  md: 220,
  lg: 280,
};

export function AssetGrid() {
  const { viewMode, gridSize, selectedAssetIds, selectAsset, clearSelection } = useUIStore();
  const { assets, loading, total, loadMore, searchError } = useAssetStore();
  const scrollRef = useRef<HTMLDivElement>(null);
  const sentinelRef = useRef<HTMLDivElement>(null);
  const hasMore = assets.length < total;
  const providers = useAutotagStore((s) => s.providers);
  const openAutotagDialog = useAutotagStore((s) => s.openAutotagDialog);
  const requestDelete = useUIStore((s) => s.requestDelete);
  const [menu, setMenu] = useState<{ x: number; y: number; assetIds: string[] } | null>(null);

  const tileSize = GRID_COLS[gridSize];

  // Infinite scroll: load the next page when the end of the grid comes within reach.
  // Re-observing after each page fires again if the sentinel is still in view (tall windows).
  useEffect(() => {
    const sentinel = sentinelRef.current;
    if (!sentinel || !hasMore) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries[0].isIntersecting) loadMore();
      },
      { root: scrollRef.current, rootMargin: '0px 0px 1200px 0px' }
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [assets.length, hasMore, loadMore]);

  const handleBackdropClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.target === e.currentTarget) clearSelection();
    },
    [clearSelection]
  );

  const handleAssetClick = useCallback(
    (asset: Asset, e: React.MouseEvent) => {
      selectAsset(asset.id, e.metaKey || e.ctrlKey);
    },
    [selectAsset]
  );

  // Acts on the selection, or on the clicked asset alone if it isn't selected (it becomes the
  // selection, so you see what the menu acts on).
  const handleAssetContextMenu = useCallback(
    (asset: Asset, e: React.MouseEvent) => {
      const selected = useUIStore.getState().selectedAssetIds;
      if (!selected.has(asset.id)) selectAsset(asset.id);
      setMenu({ x: e.clientX, y: e.clientY, assetIds: menuTargets(selected, asset.id) });
    },
    [selectAsset]
  );

  if (searchError && !loading) {
    return (
      <div className={styles.container}>
        <FilterBar />
        <div className={styles.empty} role="alert">
          <div className={styles.emptyTitle}>Search failed</div>
          <div className={styles.emptyText}>{searchError}</div>
        </div>
      </div>
    );
  }

  if (loading && assets.length === 0) {
    return (
      <div className={styles.container}>
        <FilterBar />
        <div
          className={styles.loadingGrid}
          style={{ '--tile-size': `${tileSize}px` } as React.CSSProperties}
        >
          {Array.from({ length: 24 }).map((_, i) => (
            <div key={i} className={clsx(styles.skeletonCard, 'skeleton')} />
          ))}
        </div>
      </div>
    );
  }

  if (!loading && assets.length === 0) {
    return (
      <div className={styles.container}>
        <FilterBar />
        <EmptyState />
      </div>
    );
  }

  return (
    <div className={styles.container}>
      <FilterBar />
      <div className={styles.scrollArea} onClick={handleBackdropClick} ref={scrollRef}>
        {viewMode === 'grid' ? (
          <div
            className={styles.grid}
            style={{ '--tile-size': `${tileSize}px` } as React.CSSProperties}
          >
            {assets.map((asset) => (
              <AssetCard
                key={asset.id}
                asset={asset}
                selected={selectedAssetIds.has(asset.id)}
                onClick={handleAssetClick}
                onContextMenu={handleAssetContextMenu}
              />
            ))}
          </div>
        ) : (
          <div className={styles.list}>
            {assets.map((asset) => (
              <AssetListRow
                key={asset.id}
                asset={asset}
                selected={selectedAssetIds.has(asset.id)}
                onClick={handleAssetClick}
                onContextMenu={handleAssetContextMenu}
              />
            ))}
          </div>
        )}
        {hasMore && <div ref={sentinelRef} aria-hidden />}
      </div>
      <div className={styles.footer}>
        <span className={styles.footerCount}>
          {assets.length.toLocaleString()} of {total.toLocaleString()} assets
        </span>
      </div>
      {menu && (
        <AssetContextMenu
          {...menu}
          autotagReason={autotagAvailability(providers, menu.assetIds.length).reason}
          onAutotag={() => openAutotagDialog({ kind: 'assets', assetIds: menu.assetIds })}
          onDelete={() => requestDelete(menu.assetIds)}
          onClose={() => setMenu(null)}
        />
      )}
    </div>
  );
}

interface AssetContextMenuProps {
  x: number;
  y: number;
  assetIds: string[];
  autotagReason: string | null;
  onAutotag: () => void;
  onDelete: () => void;
  onClose: () => void;
}

/** The grid's right-click menu: Auto-tag..., Reveal in file manager, Open, Delete. */
function AssetContextMenu({
  x,
  y,
  assetIds,
  autotagReason,
  onAutotag,
  onDelete,
  onClose,
}: AssetContextMenuProps) {
  const tooMany = openLimitReason(assetIds.length);
  const forEachPath = (action: (path: string) => Promise<void>, what: string) => {
    const getById = useAssetStore.getState().getById;
    for (const id of assetIds) {
      const path = getById(id)?.filePath;
      if (path) action(path).catch((err) => console.error(`Failed to ${what}:`, err));
    }
  };

  return (
    <ContextMenu
      x={x}
      y={y}
      onClose={onClose}
      items={[
        {
          id: 'grid-menu-autotag',
          label: 'Auto-tag…',
          icon: <IconTag size={12} />,
          disabledReason: autotagReason,
          onSelect: onAutotag,
        },
        {
          id: 'grid-menu-reveal',
          label: 'Reveal in file manager',
          icon: <IconEye size={12} />,
          disabledReason: tooMany,
          onSelect: () => forEachPath(api.revealInFileManager, 'reveal in file manager'),
        },
        {
          id: 'grid-menu-open',
          label: 'Open',
          icon: <IconExternal size={12} />,
          disabledReason: tooMany,
          onSelect: () => forEachPath(api.openWithDefault, 'open with default app'),
        },
        {
          id: 'grid-menu-delete',
          label: 'Delete…',
          icon: <IconTrash size={12} />,
          danger: true,
          onSelect: onDelete,
        },
      ]}
    />
  );
}

function EmptyState() {
  const libraries = useLibraryStore((s) => s.libraries);
  const addLibrary = useLibraryStore((s) => s.addLibrary);
  const resetFilter = useAssetStore((s) => s.resetFilter);
  const search = useAssetStore((s) => s.search);

  const handleAddLibrary = async () => {
    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        title: 'Select Library Folder',
      });
      if (selectedPath && typeof selectedPath === 'string') {
        const name = selectedPath.split(/[/\\]/).filter(Boolean).pop() || 'New Library';
        await addLibrary(name, [selectedPath]);
        search();
      }
    } catch (err) {
      console.error('Failed to add library:', err);
    }
  };

  const handleClearFilters = () => {
    resetFilter();
    search();
  };

  const hasLibraries = libraries.length > 0;

  return (
    <div className={styles.empty}>
      <div className={styles.emptyIcon}>
        <svg viewBox="0 0 80 80" fill="none" xmlns="http://www.w3.org/2000/svg">
          <rect
            x="10"
            y="20"
            width="60"
            height="48"
            rx="6"
            fill="hsl(220 13% 16%)"
            stroke="hsl(220 10% 25%)"
            strokeWidth="1.5"
          />
          <path
            d="M28 36l8 8 8-8"
            stroke="hsl(220 85% 50%)"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
          <circle
            cx="40"
            cy="12"
            r="8"
            fill="hsl(220 85% 50% / 0.15)"
            stroke="hsl(220 85% 50%)"
            strokeWidth="1.5"
          />
          <path
            d="M40 8v8M36 12h8"
            stroke="hsl(220 85% 50%)"
            strokeWidth="1.5"
            strokeLinecap="round"
          />
        </svg>
      </div>
      <h2 className={styles.emptyTitle}>
        {hasLibraries ? 'No matching assets found' : 'No libraries yet'}
      </h2>
      <p className={styles.emptyText}>
        {hasLibraries
          ? 'No assets match your current search or filter criteria.'
          : 'Add a library folder to start browsing your 3D assets, materials, textures, and videos.'}
      </p>
      <div className={styles.emptyActions}>
        {hasLibraries && (
          <button
            type="button"
            className={styles.emptyBtn}
            id="empty-clear-filters"
            onClick={handleClearFilters}
          >
            Clear Filters
          </button>
        )}
        <button
          type="button"
          className={hasLibraries ? styles.emptyBtnSecondary : styles.emptyBtn}
          id="empty-add-library"
          onClick={handleAddLibrary}
        >
          + Add Library
        </button>
      </div>
    </div>
  );
}
