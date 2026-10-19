import { useCallback, useLayoutEffect, useRef, useState } from 'react';
import { useVirtualizer } from '@tanstack/react-virtual';
import { openPack, packKey, useLibraryStore, usePacksStore, useUIStore } from '../../store';
import type { PackCard as Pack } from '../../types';
import { ContextMenu } from '../ContextMenu/ContextMenu';
import { folderMenuItems } from '../FolderActions';
import { CARD_BODY_HEIGHT } from './cardMetrics';
import { FilterBar } from './FilterBar';
import {
  GRID_GAP,
  OVERSCAN_ROWS,
  SCROLL_PADDING,
  TILE_MIN,
  gridLayout,
  rowCount,
} from './gridGeometry';
import { PackCard } from './PackCard';
import styles from './AssetGrid.module.css';

/** Scrolls `pack`'s tree node, or its library's row for a root pack, into view. */
function revealNode(pack: Pick<Pack, 'libraryId' | 'path'>) {
  const id = CSS.escape(pack.libraryId);
  const selector =
    pack.path === ''
      ? `#lib-${id}`
      : `[data-library-id="${id}"][data-folder-path="${CSS.escape(pack.path)}"]`;
  requestAnimationFrame(() =>
    document.querySelector(selector)?.scrollIntoView({ block: 'nearest' })
  );
}

/**
 * The Packs view: a windowed grid of a card for every pack below the selected node, flat and in
 * name order (plan C17). Only the visible rows plus overscan are in the DOM.
 */
export function PacksView() {
  const packs = usePacksStore((s) => s.packs) ?? [];
  const selected = usePacksStore((s) => s.selected);
  const select = usePacksStore((s) => s.select);
  const libraries = useLibraryStore((s) => s.libraries);
  const gridSize = useUIStore((s) => s.gridSize);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  const [menu, setMenu] = useState<{ x: number; y: number; pack: Pack } | null>(null);

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, []);

  const { columns, rowHeight, rowPitch } = gridLayout(
    Math.max(0, width - 2 * SCROLL_PADDING),
    TILE_MIN[gridSize]
  );
  const rows = rowCount(packs.length, columns);
  const virtualizer = useVirtualizer({
    count: rows,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowPitch,
    overscan: OVERSCAN_ROWS,
    scrollMargin: SCROLL_PADDING,
  });
  useLayoutEffect(() => virtualizer.measure(), [rowPitch, virtualizer]);

  const handleSelect = useCallback((pack: Pack) => select(packKey(pack)), [select]);
  const handleOpen = useCallback(
    (pack: Pack) => void openPack(pack).then(() => revealNode(pack)),
    []
  );
  // Selects the card, so you see what the menu acts on
  const handleContextMenu = useCallback(
    (pack: Pack, e: React.MouseEvent) => {
      select(packKey(pack));
      setMenu({ x: e.clientX, y: e.clientY, pack });
    },
    [select]
  );
  const libraryName = (id: string) => libraries.find((l) => l.id === id)?.name ?? id;

  return (
    <div className={styles.container}>
      <FilterBar />
      <div className={styles.scrollFrame}>
        <div
          id="pack-scroll"
          className={styles.scrollArea}
          ref={scrollRef}
          onClick={(e) => e.target === e.currentTarget && select(null)}
          data-columns={columns}
          data-row-pitch={rowPitch}
          style={
            {
              '--scroll-padding': `${SCROLL_PADDING}px`,
              '--grid-gap': `${GRID_GAP}px`,
              '--card-body-height': `${CARD_BODY_HEIGHT}px`,
            } as React.CSSProperties
          }
        >
          <div
            className={styles.rows}
            style={{ height: rows > 0 ? (rows - 1) * rowPitch + rowHeight : 0 }}
          >
            {virtualizer.getVirtualItems().map((row) => {
              const first = row.index * columns;
              return (
                <div
                  key={row.key}
                  className={styles.gridRow}
                  style={{
                    transform: `translateY(${row.index * rowPitch}px)`,
                    height: rowHeight,
                    gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))`,
                  }}
                >
                  {packs.slice(first, first + columns).map((pack, i) => (
                    <PackCard
                      key={packKey(pack)}
                      pack={pack}
                      index={first + i}
                      libraryName={libraryName(pack.libraryId)}
                      selected={selected === packKey(pack)}
                      onSelect={handleSelect}
                      onOpen={handleOpen}
                      onContextMenu={handleContextMenu}
                    />
                  ))}
                </div>
              );
            })}
          </div>
        </div>
      </div>
      <div className={styles.footer}>
        <span id="pack-count" className={styles.footerCount}>
          {packs.length.toLocaleString()} {packs.length === 1 ? 'pack' : 'packs'}
        </span>
      </div>
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={folderMenuItems({
            libraryId: menu.pack.libraryId,
            path: menu.pack.path,
            name: menu.pack.name,
          })}
        />
      )}
    </div>
  );
}
