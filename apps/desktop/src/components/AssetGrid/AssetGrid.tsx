import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import { useVirtualizer } from '@tanstack/react-virtual';
import { api } from '../../api';
import { useUIStore, useAssetStore, useLibraryStore, useAutotagStore } from '../../store';
import { PAGE_SIZE, assetAt, createSettler } from '../../store/pageCache';
import { autotagAvailability } from '../../utils/autotag';
import { menuTargets, openLimitReason } from '../../utils/contextMenu';
import { formatVisibleRange } from '../../utils/format';
import { ContextMenu } from '../ContextMenu/ContextMenu';
import { IconEye, IconExternal, IconTag, IconTrash } from '../Icons';
import { AssetCard } from './AssetCard';
import { CARD_BODY_HEIGHT } from './cardMetrics';
import { AssetListRow } from './AssetListRow';
import { FilterBar } from './FilterBar';
import {
  OVERSCAN_ROWS,
  SCROLL_PADDING,
  TILE_MIN,
  anchorRow,
  gridLayout,
  listLayout,
  rowCount,
  rowOfIndex,
  rowTop,
  rowsToIndexRange,
  visibleRows,
  wantedPages,
} from './gridGeometry';
import type { Asset } from '../../types';
import styles from './AssetGrid.module.css';

type AssetHandler = (asset: Asset, e: React.MouseEvent) => void;

export function AssetGrid() {
  const viewMode = useUIStore((s) => s.viewMode);
  const gridSize = useUIStore((s) => s.gridSize);
  const selectAsset = useUIStore((s) => s.selectAsset);
  const clearSelection = useUIStore((s) => s.clearSelection);
  const loading = useAssetStore((s) => s.loading);
  const total = useAssetStore((s) => s.total);
  const cachedPages = useAssetStore((s) => s.pages.size);
  const searchError = useAssetStore((s) => s.searchError);
  const providers = useAutotagStore((s) => s.providers);
  const openAutotagDialog = useAutotagStore((s) => s.openAutotagDialog);
  const requestDelete = useUIStore((s) => s.requestDelete);
  const [menu, setMenu] = useState<{ x: number; y: number; assetIds: string[] } | null>(null);

  const handleBackdropClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.target === e.currentTarget) clearSelection();
    },
    [clearSelection]
  );

  const handleAssetClick = useCallback<AssetHandler>(
    (asset, e) => {
      selectAsset(asset.id, e.metaKey || e.ctrlKey);
    },
    [selectAsset]
  );

  // Acts on the selection, or on the clicked asset alone if it isn't selected (it becomes the
  // selection, so you see what the menu acts on).
  const handleAssetContextMenu = useCallback<AssetHandler>(
    (asset, e) => {
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

  if (loading && cachedPages === 0) {
    return (
      <div className={styles.container}>
        <FilterBar />
        <div
          className={styles.loadingGrid}
          style={{ '--tile-size': `${TILE_MIN[gridSize]}px` } as React.CSSProperties}
        >
          {Array.from({ length: 24 }).map((_, i) => (
            <div
              key={i}
              className={clsx(styles.skeletonCard, 'skeleton')}
              data-tile
              data-tile-state="placeholder"
            />
          ))}
        </div>
      </div>
    );
  }

  if (!loading && total === 0) {
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
      <WindowedResults
        viewMode={viewMode}
        gridSize={gridSize}
        onBackdropClick={handleBackdropClick}
        onAssetClick={handleAssetClick}
        onAssetContextMenu={handleAssetContextMenu}
      />
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

interface WindowedResultsProps {
  viewMode: 'grid' | 'list';
  gridSize: 'sm' | 'md' | 'lg';
  onBackdropClick: (e: React.MouseEvent) => void;
  onAssetClick: AssetHandler;
  onAssetContextMenu: AssetHandler;
}

/**
 * The scroll area and its footer. The scrollbar spans every result; only the visible rows plus
 * overscan are in the DOM, as cards (or list rows) where their page is cached and placeholders
 * where it isn't. Pages are fetched once the wanted set settles (C2).
 */
function WindowedResults({
  viewMode,
  gridSize,
  onBackdropClick,
  onAssetClick,
  onAssetContextMenu,
}: WindowedResultsProps) {
  const pages = useAssetStore((s) => s.pages);
  const total = useAssetStore((s) => s.total);
  const epoch = useAssetStore((s) => s.epoch);
  const anchor = useAssetStore((s) => s.anchor);
  const selectedAssetIds = useUIStore((s) => s.selectedAssetIds);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, []);

  const contentWidth = Math.max(0, width - 2 * SCROLL_PADDING);
  const layout =
    viewMode === 'grid' ? gridLayout(contentWidth, TILE_MIN[gridSize]) : listLayout(contentWidth);
  const { columns, rowHeight, rowPitch } = layout;
  const rows = rowCount(total, columns);

  const virtualizer = useVirtualizer({
    count: rows,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowPitch,
    overscan: OVERSCAN_ROWS,
    scrollMargin: SCROLL_PADDING,
  });

  // The first visible index, kept across layout changes and reported for refresh anchoring
  const topIndex = useRef(0);
  const lastScrollTop = useRef(0);
  const direction = useRef<1 | -1>(1);
  const appliedEpoch = useRef(epoch);
  const settler = useMemo(
    () =>
      createSettler(
        (wanted) => useAssetStore.getState().ensurePages(wanted),
        (fn, ms) => {
          const timer = setTimeout(fn, ms);
          return () => clearTimeout(timer);
        }
      ),
    []
  );
  useEffect(() => () => settler.dispose(), [settler]);

  // A new grid size, window width or view mode: keep the first visible asset in the first
  // visible row
  useLayoutEffect(() => {
    virtualizer.measure();
    const el = scrollRef.current;
    if (!el) return;
    const row = anchorRow(topIndex.current, columns);
    // scrollTop holds whole pixels and rounds down; rounding up keeps it inside the anchor row
    el.scrollTop = row === 0 ? 0 : Math.ceil(rowTop(row, rowPitch));
  }, [columns, rowPitch, virtualizer]);

  // A search starts at the top; a refresh shifts by as many rows as the top asset moved (C5)
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || appliedEpoch.current === epoch) return;
    if (!anchor) el.scrollTop = 0;
    else {
      const rows = rowOfIndex(anchor.to, columns) - rowOfIndex(anchor.from, columns);
      el.scrollTop = Math.ceil(el.scrollTop + rows * rowPitch);
    }
  }, [epoch, anchor, columns, rowPitch]);

  // After every render, which every scroll causes: track the top and the wanted pages
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const scrollTop = el.scrollTop;
    if (scrollTop !== lastScrollTop.current) {
      direction.current = scrollTop > lastScrollTop.current ? 1 : -1;
      lastScrollTop.current = scrollTop;
    }
    const visible = visibleRows(scrollTop, el.clientHeight, rowPitch, rows);
    topIndex.current = visible ? visible.first * columns : 0;
    useAssetStore.getState().setViewTop(topIndex.current);
    settler.update(
      wantedPages(scrollTop, el.clientHeight, layout, total, direction.current, PAGE_SIZE)
    );
    // The cache was replaced under pages that may not have changed: ask for them now
    if (appliedEpoch.current !== epoch) {
      appliedEpoch.current = epoch;
      settler.kick();
    }
  });

  const visible = visibleRows(
    virtualizer.scrollOffset ?? 0,
    virtualizer.scrollRect?.height ?? 0,
    rowPitch,
    rows
  );
  const range = visible && rowsToIndexRange(visible.first, visible.last, columns, total);

  return (
    <>
      <div
        id="asset-scroll"
        className={styles.scrollArea}
        onClick={onBackdropClick}
        ref={scrollRef}
        data-columns={columns}
        data-row-pitch={rowPitch}
      >
        <div
          className={styles.rows}
          style={
            {
              height: rows > 0 ? (rows - 1) * rowPitch + rowHeight : 0,
              '--card-body-height': `${CARD_BODY_HEIGHT}px`,
            } as React.CSSProperties
          }
        >
          {virtualizer.getVirtualItems().map((item) => {
            const first = item.index * columns;
            const items: (Asset | undefined)[] = [];
            for (let i = first; i < Math.min(total, first + columns); i++) {
              items.push(assetAt({ pages }, i));
            }
            return (
              <ResultRow
                key={item.key}
                viewMode={viewMode}
                first={first}
                items={items}
                top={item.index * rowPitch}
                height={rowHeight}
                columns={columns}
                selected={selectedAssetIds}
                onClick={onAssetClick}
                onContextMenu={onAssetContextMenu}
              />
            );
          })}
        </div>
      </div>
      <div className={styles.footer}>
        <span id="asset-range" className={styles.footerCount}>
          {formatVisibleRange(range, total)}
        </span>
      </div>
    </>
  );
}

interface ResultRowProps {
  viewMode: 'grid' | 'list';
  /** Index of the row's first asset */
  first: number;
  /** The row's assets, undefined where their page isn't cached */
  items: (Asset | undefined)[];
  top: number;
  height: number;
  columns: number;
  selected: Set<string>;
  onClick: AssetHandler;
  onContextMenu: AssetHandler;
}

/** One virtual row: `columns` cards, or one list row, with placeholders for uncached assets. */
const ResultRow = memo(function ResultRow({
  viewMode,
  first,
  items,
  top,
  height,
  columns,
  selected,
  onClick,
  onContextMenu,
}: ResultRowProps) {
  const grid = viewMode === 'grid';
  return (
    <div
      className={grid ? styles.gridRow : styles.listRow}
      style={{
        transform: `translateY(${top}px)`,
        height,
        gridTemplateColumns: grid ? `repeat(${columns}, minmax(0, 1fr))` : undefined,
      }}
    >
      {items.map((asset, i) =>
        !asset ? (
          <div
            key={`placeholder-${first + i}`}
            className={grid ? styles.placeholderCard : styles.placeholderRow}
            data-tile
            data-tile-state="placeholder"
            data-placeholder
            data-index={first + i}
          />
        ) : grid ? (
          <AssetCard
            key={asset.id}
            asset={asset}
            index={first + i}
            selected={selected.has(asset.id)}
            onClick={onClick}
            onContextMenu={onContextMenu}
          />
        ) : (
          <AssetListRow
            key={asset.id}
            asset={asset}
            index={first + i}
            selected={selected.has(asset.id)}
            onClick={onClick}
            onContextMenu={onContextMenu}
          />
        )
      )}
    </div>
  );
}, rowPropsEqual);

/** Rows re-render only when one of their assets, the selection or the layout changed. */
function rowPropsEqual(a: ResultRowProps, b: ResultRowProps): boolean {
  return (
    a.viewMode === b.viewMode &&
    a.first === b.first &&
    a.top === b.top &&
    a.height === b.height &&
    a.columns === b.columns &&
    a.selected === b.selected &&
    a.onClick === b.onClick &&
    a.onContextMenu === b.onContextMenu &&
    a.items.length === b.items.length &&
    a.items.every((asset, i) => asset === b.items[i])
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
