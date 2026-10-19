import { useState } from 'react';
import clsx from 'clsx';
import { convertFileSrc } from '@tauri-apps/api/core';
import { useAssetStore, useFolderTreeStore } from '../../store';
import type { Cover } from '../../types';
import { tileMedia } from '../AssetGrid/tileState';
import { IconChevronDown, IconChevronRight, IconFolder, IconPackage } from '../Icons';
import { folderCount, isFolderShown } from './sidebarHelpers';
import styles from './Sidebar.module.css';

interface FolderTreeProps {
  libraryId: string;
  /** The node whose children this lists; `''` is the library root */
  parentPath: string;
  onSelect: (libraryId: string, path: string) => void;
}

/**
 * One level of a library's folder tree, each expanded node listing its own children below it.
 * Packs show their cover, or a pack icon. With hidden files off, folders holding only support
 * files are left out.
 */
export function FolderTree({ libraryId, parentPath, onSelect }: FolderTreeProps) {
  const nodes = useFolderTreeStore((s) => s.children[libraryId]?.[parentPath]);
  const expanded = useFolderTreeStore((s) => s.expanded[libraryId]);
  const toggle = useFolderTreeStore((s) => s.toggle);
  const selected = useAssetStore((s) => s.query.folder);
  const showHidden = useAssetStore((s) => Boolean(s.query.filter.showHidden));

  const shown = nodes?.filter((node) => isFolderShown(node, showHidden));
  if (!shown || shown.length === 0) return null;
  return (
    <div className={styles.subTree} role="group">
      {shown.map((node) => {
        const isOpen = expanded?.includes(node.path) ?? false;
        const isSelected = selected?.libraryId === libraryId && selected.path === node.path;
        const hasChildren = showHidden ? node.hasChildren : node.hasVisibleChildren;
        const hidden = node.hiddenCount > 0 ? `, ${node.hiddenCount.toLocaleString()} hidden` : '';
        return (
          <div key={node.path}>
            <div
              className={clsx(styles.item, isSelected && styles.active)}
              onClick={() => onSelect(libraryId, node.path)}
              data-library-id={libraryId}
              data-folder-path={node.path}
              data-pack={node.isPack || undefined}
              role="treeitem"
              aria-selected={isSelected}
              aria-expanded={hasChildren ? isOpen : undefined}
              tabIndex={0}
              title={`${node.path}${node.isPack ? ' (pack)' : ''}: ${node.itemCount.toLocaleString()} items${hidden}`}
            >
              {hasChildren ? (
                <span
                  className={styles.chevronBtn}
                  onClick={(e) => {
                    e.stopPropagation();
                    void toggle(libraryId, node.path);
                  }}
                  data-folder-toggle
                  title={isOpen ? 'Collapse folder' : 'Expand folder'}
                >
                  {isOpen ? <IconChevronDown size={11} /> : <IconChevronRight size={11} />}
                </span>
              ) : (
                !node.isPack && <IconFolder size={13} />
              )}
              {node.isPack && <PackIcon cover={node.cover} />}
              <span className={clsx(styles.itemName, 'truncate')}>{node.name}</span>
              <span className={styles.badge}>{folderCount(node, showHidden).toLocaleString()}</span>
            </div>
            {isOpen && hasChildren && (
              <FolderTree libraryId={libraryId} parentPath={node.path} onSelect={onSelect} />
            )}
          </div>
        );
      })}
    </div>
  );
}

/** A pack node's cover as a small tile, or the pack icon when it has none or it fails to load. */
function PackIcon({ cover }: { cover?: Cover | null }) {
  const [failed, setFailed] = useState<string | null>(null);
  const media = cover
    ? tileMedia({ ...cover, thumbnailPath: cover.thumbnailPath ?? undefined })
    : 'none';
  const src =
    media === 'thumbnail' ? cover?.thumbnailPath : media === 'original' ? cover?.filePath : null;
  if (!src || failed === src) return <IconPackage size={13} className={styles.packIcon} />;
  return (
    <img
      src={convertFileSrc(src)}
      alt=""
      className={styles.packCover}
      decoding="async"
      loading="lazy"
      onError={() => setFailed(src)}
      data-pack-cover
    />
  );
}
