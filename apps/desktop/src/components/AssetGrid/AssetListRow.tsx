import { memo, useCallback, useEffect, useState } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import type { Asset } from '../../types';
import { useAssetStore } from '../../store';
import { IconImage, IconVideo, IconStar, IconHeart, IconHeartFilled } from '../Icons';
import { formatSize } from '../../utils/format';
import { tileMedia, tileState } from './tileState';
import styles from './AssetListRow.module.css';

interface AssetListRowProps {
  asset: Asset;
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
  selected,
  onClick,
  onContextMenu,
}: AssetListRowProps) {
  const patchAsset = useAssetStore((s) => s.patchAsset);
  // The thumbnail has loaded or failed; `data-tile-state` reads "ready" once it has
  const [mediaSettled, setMediaSettled] = useState(false);

  useEffect(() => {
    setMediaSettled(false);
  }, [asset.thumbnailPath, asset.filePath]);

  const media = tileMedia(asset);
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
        {media === 'thumbnail' && asset.thumbnailPath ? (
          <img
            src={convertFileSrc(asset.thumbnailPath)}
            alt={asset.fileName}
            className={styles.thumbImg}
            loading="lazy"
            onLoad={settle}
            onError={settle}
          />
        ) : media === 'original' ? (
          <img
            src={convertFileSrc(asset.filePath)}
            alt={asset.fileName}
            className={styles.thumbImg}
            loading="lazy"
            onLoad={settle}
            onError={settle}
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

      {/* Extension */}
      <span className={styles.ext}>{asset.extension.toUpperCase()}</span>

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
