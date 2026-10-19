import { memo, useEffect, useState } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import type { Cover, PackCard as Pack } from '../../types';
import { useImageScaling } from '../../hooks/useImageScaling';
import { IconPackage } from '../Icons';
import { tileMedia, tileState, type TileMedia } from './tileState';
import styles from './PackCard.module.css';

export interface PackCardProps {
  pack: Pack;
  /** Position in the Packs view, as `data-index` for the UI checks */
  index: number;
  /** The library's name, the subtitle of a pack at a library's root */
  libraryName: string;
  selected: boolean;
  onSelect: (pack: Pack) => void;
  onOpen: (pack: Pack) => void;
  onMenu: (e: React.MouseEvent, pack: Pack) => void;
}

/** A cover as the asset it is, for `tileMedia`. */
const coverAsset = (cover: Cover) => ({
  ...cover,
  thumbnailPath: cover.thumbnailPath ?? undefined,
});

/**
 * One pack: its cover, name and parent folder, and its visible items, or "No visible items" when
 * every file is hidden. A click selects it; a double-click or Enter opens it in the tree.
 */
export const PackCard = memo(function PackCard({
  pack,
  index,
  libraryName,
  selected,
  onSelect,
  onOpen,
  onMenu,
}: PackCardProps) {
  const [failed, setFailed] = useState<TileMedia>();
  const [loaded, setLoaded] = useState(false);
  const scaling = useImageScaling('cover');
  const cover = pack.cover ? coverAsset(pack.cover) : null;

  useEffect(() => {
    setFailed(undefined);
    setLoaded(false);
  }, [cover?.thumbnailPath, cover?.filePath]);

  const media = cover ? tileMedia(cover, { failed }) : 'none';
  const src =
    media === 'thumbnail' ? cover?.thumbnailPath : media === 'original' ? cover?.filePath : null;
  const items = pack.itemCount;

  return (
    <div
      className={clsx(styles.card, selected && styles.selected)}
      onClick={(e) => {
        e.stopPropagation();
        onSelect(pack);
      }}
      onDoubleClick={() => onOpen(pack)}
      onContextMenu={(e) => onMenu(e, pack)}
      onKeyDown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          onOpen(pack);
        }
      }}
      title={`${pack.name} (${pack.path})`}
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      data-tile
      data-pack-card
      data-pack-library={pack.libraryId}
      data-pack-path={pack.path}
      data-index={index}
      data-tile-state={tileState(media, loaded)}
    >
      <div className={styles.cover}>
        {src ? (
          <img
            src={convertFileSrc(src)}
            alt={pack.name}
            className={styles.coverImg}
            ref={scaling}
            decoding="async"
            onLoad={() => setLoaded(true)}
            onError={() => setFailed(media)}
          />
        ) : (
          <div className={styles.coverPlaceholder}>
            <IconPackage size={40} />
          </div>
        )}
      </div>
      <div className={styles.body}>
        <p className={clsx(styles.name, 'truncate')}>
          <IconPackage size={12} className={styles.packIcon} />
          <span className="truncate">{pack.name}</span>
        </p>
        <p className={clsx(styles.subtitle, 'truncate')}>{pack.parentPath || libraryName}</p>
        {items > 0 ? (
          <span className={styles.count}>
            {items.toLocaleString()} {items === 1 ? 'item' : 'items'}
          </span>
        ) : (
          <span className={styles.empty} data-no-visible-items>
            No visible items
          </span>
        )}
      </div>
    </div>
  );
});
