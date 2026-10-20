import { memo, useCallback, useEffect, useState } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import type { Asset } from '../../types';
import { useAssetStore } from '../../store';
import { IconImage, IconVideo, IconStar, IconHeart, IconHeartFilled } from '../Icons';
import { formatGroupBadge, formatSize } from '../../utils/format';
import { useImageScaling } from '../../hooks/useImageScaling';
import { tileImage, tileMedia, tileState, type TileMedia } from './tileState';
import styles from './AssetListRow.module.css';

interface AssetListRowProps {
  asset: Asset;
  /** Position in the results, as `data-index` for the UI checks */
  index: number;
  selected: boolean;
  onClick: (asset: Asset, e: React.MouseEvent) => void;
  onContextMenu: (asset: Asset, e: React.MouseEvent) => void;
}

function formatDate(ms: number): string {
  const d = new Date(ms);
  return d.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

export const AssetListRow = memo(function AssetListRow({
  asset,
  index,
  selected,
  onClick,
  onContextMenu,
}: AssetListRowProps) {
  const patchAsset = useAssetStore((s) => s.patchAsset);
  const scaling = useImageScaling('cover');
  // The media that failed to load, so the row falls back to the next one
  const [failed, setFailed] = useState<TileMedia>();
  // The image has loaded; `data-tile-state` reads "ready" once it has, or all media failed
  const [mediaSettled, setMediaSettled] = useState(false);

  useEffect(() => {
    setFailed(undefined);
    setMediaSettled(false);
  }, [asset.bundledPreview, asset.fallbackRender, asset.thumbnailPath, asset.filePath]);

  const media = tileMedia(asset, { failed });
  const image = tileImage(asset, media);
  const groupBadge = formatGroupBadge(asset);
  const settle = () => setMediaSettled(true);

  const handleClick = useCallback(
    (e: React.MouseEvent) => {
      e.stopPropagation();
      onClick(asset, e);
    },
    [asset, onClick]
  );

  const handleContextMenu = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      e.stopPropagation();
      onContextMenu(asset, e);
    },
    [asset, onContextMenu]
  );

  const handleToggleFavorite = useCallback(
    (e: React.MouseEvent) => {
      e.stopPropagation();
      patchAsset(asset.id, { isFavorite: !asset.isFavorite });
    },
    [asset.id, asset.isFavorite, patchAsset]
  );

  const handleDragStart = useCallback(
    (e: React.DragEvent) => {
      e.dataTransfer.setData('text/plain', asset.filePath);
      e.dataTransfer.effectAllowed = 'copy';
    },
    [asset.filePath]
  );

  return (
    <div
      id={`asset-row-${asset.id}`}
      className={clsx(styles.row, selected && styles.selected)}
      onClick={handleClick}
      onContextMenu={handleContextMenu}
      draggable
      onDragStart={handleDragStart}
      role="button"
      tabIndex={0}
      data-tile
      data-index={index}
      data-tile-state={tileState(media, mediaSettled)}
    >
      {/* Favorite Button */}
      <button
        id={`row-fav-${asset.id}`}
        type="button"
        className={clsx(styles.favoriteBtn, asset.isFavorite && styles.favoriteActive)}
        onClick={handleToggleFavorite}
        title={asset.isFavorite ? 'Remove from favorites' : 'Add to favorites'}
        aria-label={asset.isFavorite ? 'Remove from favorites' : 'Add to favorites'}
      >
        {asset.isFavorite ? (
          <IconHeartFilled size={14} className={styles.favoriteHeartFilled} />
        ) : (
          <IconHeart size={14} />
        )}
      </button>

      {/* Thumbnail */}
      <div className={styles.thumb}>
        {image ? (
          <img
            src={convertFileSrc(image)}
            alt={asset.fileName}
            className={styles.thumbImg}
            ref={scaling}
            decoding="async"
            onLoad={settle}
            onError={() => setFailed(media)}
          />
        ) : (
          <div className={styles.thumbPlaceholder}>
            {asset.kind === 'video' ? <IconVideo size={14} /> : <IconImage size={14} />}
          </div>
        )}
      </div>

      {/* Color label */}
      {asset.colorLabel !== 'none' && (
        <div
          className={styles.colorDot}
          style={{ background: `var(--swatch-${asset.colorLabel})` }}
        />
      )}

      {/* Name */}
      <div className={styles.nameCol}>
        <span className={clsx(styles.name, 'truncate')}>{asset.fileName}</span>
        <span className={clsx(styles.path, 'truncate')}>{asset.filePath}</span>
      </div>

      {/* Extension, or a format group's badge: `GLB +2` */}
      <span
        className={clsx(styles.ext, groupBadge && styles.groupBadge)}
        title={groupBadge?.title}
        data-group-badge={groupBadge?.label}
      >
        {groupBadge?.label ?? asset.extension.toUpperCase()}
      </span>

      {/* Rating */}
      <div className={styles.stars}>
        {Array.from({ length: 5 }).map((_, i) => (
          <IconStar
            key={i}
            size={10}
            className={clsx(styles.star, i < asset.rating && styles.starFilled)}
          />
        ))}
      </div>

      {/* Size */}
      <span className={styles.size}>{formatSize(asset.sizeBytes)}</span>

      {/* Date */}
      <span className={styles.date}>{formatDate(asset.modifiedAt)}</span>
    </div>
  );
});
