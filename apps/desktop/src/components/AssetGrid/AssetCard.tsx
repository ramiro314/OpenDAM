import { memo, useCallback, useState, useEffect, useMemo } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import type { Asset, AssetKind } from '../../types';
import { useTagStore, useCollectionStore, useAssetStore, selectCollectionNames } from '../../store';
import {
  IconModel3D,
  IconImage,
  IconVideo,
  IconStar,
  IconCollection,
  IconHeart,
  IconHeartFilled,
} from '../Icons';
import { formatGroupBadge, formatSize } from '../../utils/format';
import { useImageScaling } from '../../hooks/useImageScaling';
import { tileImage, tileMedia, tileState, type TileMedia } from './tileState';
import styles from './AssetCard.module.css';

export interface AssetCardProps {
  asset: Asset;
  /** Position in the results, as `data-index` for the UI checks */
  index: number;
  selected: boolean;
  onClick: (asset: Asset, e: React.MouseEvent) => void;
  onContextMenu: (asset: Asset, e: React.MouseEvent) => void;
}

const KIND_ICON: Record<AssetKind, React.FC<{ size?: number }>> = {
  '3d_model': IconModel3D,
  material: IconImage,
  texture: IconImage,
  hdri: IconImage,
  ies: IconImage,
  video: IconVideo,
  image: IconImage,
  document: IconImage,
  other: IconImage,
};

const KIND_COLOR: Record<AssetKind, string> = {
  '3d_model': 'hsl(220 85% 55%)',
  material: 'hsl(268 72% 60%)',
  texture: 'hsl(178 64% 46%)',
  hdri: 'hsl( 38 92% 56%)',
  ies: 'hsl( 45 95% 50%)',
  video: 'hsl(  4 80% 55%)',
  image: 'hsl(140 64% 46%)',
  document: 'hsl(213 84% 55%)',
  other: 'hsl(220  8% 50%)',
};

export const AssetCard = memo(function AssetCard({
  asset,
  index,
  selected,
  onClick,
  onContextMenu,
}: AssetCardProps) {
  // The media that failed to load, so the card falls back to the next one
  const [failed, setFailed] = useState<TileMedia>();
  // `data-tile-state` reads "ready" once the image or video has loaded, or all media failed
  const [mediaLoaded, setMediaLoaded] = useState(false);
  const patchAsset = useAssetStore((s) => s.patchAsset);
  const scaling = useImageScaling('cover');

  useEffect(() => {
    setFailed(undefined);
    setMediaLoaded(false);
  }, [asset.bundledPreview, asset.fallbackRender, asset.thumbnailPath, asset.filePath]);

  const media = tileMedia(asset, { failed, withVideo: true });
  const image = tileImage(asset, media);

  const KindIcon = KIND_ICON[asset.kind] ?? IconImage;
  const kindColor = KIND_COLOR[asset.kind];
  const groupBadge = formatGroupBadge(asset);

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
      e.dataTransfer.setData('application/opendam-asset-id', asset.id);
      e.dataTransfer.setData('text/plain', asset.id);
      e.dataTransfer.effectAllowed = 'copy';
    },
    [asset.id]
  );

  const tagMap = useTagStore((s) => s.tagMap);
  const collectionMap = useCollectionStore(selectCollectionNames);

  const chips = useMemo(() => {
    const list: { id: string; name: string; type: 'collection' | 'tag'; color?: string }[] = [];
    if (asset.collections && asset.collections.length > 0) {
      for (const colId of asset.collections) {
        const name = collectionMap.get(colId);
        if (name) {
          list.push({ id: `col-${colId}`, name, type: 'collection' });
        }
      }
    }
    if (asset.tags && asset.tags.length > 0) {
      for (const tagId of asset.tags) {
        const tag = tagMap.get(tagId);
        if (tag) {
          list.push({ id: `tag-${tagId}`, name: tag.name, type: 'tag', color: tag.color });
        } else {
          list.push({ id: `tag-${tagId}`, name: tagId, type: 'tag' });
        }
      }
    }
    return list;
  }, [asset.collections, asset.tags, collectionMap, tagMap]);

  return (
    <div
      id={`asset-${asset.id}`}
      className={clsx(styles.card, selected && styles.selected)}
      onClick={handleClick}
      onContextMenu={handleContextMenu}
      draggable
      onDragStart={handleDragStart}
      title={asset.fileName}
      role="button"
      tabIndex={0}
      data-tile
      data-index={index}
      data-tile-state={tileState(media, mediaLoaded)}
      onKeyDown={(e) => e.key === 'Enter' && handleClick(e as unknown as React.MouseEvent)}
    >
      {/* Thumbnail / preview area */}
      <div className={styles.thumb}>
        {image ? (
          <img
            src={convertFileSrc(image)}
            alt={asset.fileName}
            className={styles.thumbImg}
            ref={scaling}
            decoding="async"
            onLoad={() => setMediaLoaded(true)}
            onError={() => setFailed(media)}
          />
        ) : media === 'video' ? (
          <div className={styles.videoThumbWrapper}>
            <video
              src={convertFileSrc(asset.filePath)}
              preload="metadata"
              muted
              playsInline
              className={styles.thumbVideo}
              onLoadedMetadata={(e) => {
                setMediaLoaded(true);
                try {
                  e.currentTarget.currentTime = 0.1;
                } catch {}
              }}
              onError={() => setFailed(media)}
            />
            <div className={styles.videoPlayOverlay}>
              <IconVideo size={13} />
            </div>
          </div>
        ) : (
          <div className={styles.thumbPlaceholder}>
            <KindIcon size={36} />
          </div>
        )}

        {/* Preview status badge */}
        {asset.previewStatus === 'generating' && (
          <div className={clsx(styles.previewBadge, styles.generating)}>
            <span className="animate-pulse">⟳</span>
          </div>
        )}
        {asset.previewStatus === 'error' && (
          <div className={clsx(styles.previewBadge, styles.error)}>!</div>
        )}

        {/* Favorite toggle button */}
        <button
          id={`card-fav-${asset.id}`}
          type="button"
          className={clsx(styles.favoriteBtn, asset.isFavorite && styles.favoriteActive)}
          onClick={handleToggleFavorite}
          title={asset.isFavorite ? 'Remove from favorites' : 'Add to favorites'}
          aria-label={asset.isFavorite ? 'Remove from favorites' : 'Add to favorites'}
        >
          {asset.isFavorite ? (
            <IconHeartFilled size={13} className={styles.favoriteHeartFilled} />
          ) : (
            <IconHeart size={13} />
          )}
        </button>

        {/* Color label stripe */}
        {asset.colorLabel !== 'none' && (
          <div
            className={styles.colorStripe}
            style={{ background: `var(--swatch-${asset.colorLabel})` }}
          />
        )}

        {/* Kind chip, or a format group's badge: `GLB +2` */}
        <div
          className={clsx(styles.kindChip, groupBadge && styles.groupBadge)}
          style={{ color: kindColor }}
          title={groupBadge?.title}
          data-group-badge={groupBadge?.label}
        >
          <KindIcon size={10} />
          <span>{groupBadge?.label ?? asset.extension.toUpperCase()}</span>
        </div>
      </div>

      {/* Card body */}
      <div className={styles.body}>
        <p className={clsx(styles.name, 'truncate')} title={asset.fileName}>
          {asset.fileName}
        </p>
        <div className={styles.meta}>
          {asset.rating > 0 && (
            <div className={styles.stars}>
              {Array.from({ length: 5 }).map((_, i) => (
                <IconStar
                  key={i}
                  size={10}
                  className={clsx(styles.star, i < asset.rating && styles.starFilled)}
                />
              ))}
            </div>
          )}
          <span className={styles.size}>{formatSize(asset.sizeBytes)}</span>
        </div>

        {/* Scrollable tags and collections chips on bottom, always reserved so every card has
            the same height */}
        <div
          className={styles.chipsScroll}
          onWheel={(e) => {
            if (e.deltaY !== 0) {
              e.currentTarget.scrollLeft += e.deltaY;
            }
          }}
        >
          {chips.map((chip) => (
            <span
              key={chip.id}
              className={clsx(
                styles.chip,
                chip.type === 'collection' ? styles.collectionChip : styles.tagChip
              )}
              title={chip.type === 'collection' ? `Collection: ${chip.name}` : `Tag: ${chip.name}`}
            >
              {chip.type === 'collection' ? (
                <IconCollection size={9} className={styles.chipIcon} />
              ) : (
                <span
                  className={styles.chipDot}
                  style={{
                    background: chip.color ? `var(--swatch-${chip.color})` : 'hsl(220 85% 60%)',
                  }}
                />
              )}
              <span className={styles.chipText}>{chip.name}</span>
            </span>
          ))}
        </div>
      </div>
    </div>
  );
});
