import { useEffect } from 'react';
import clsx from 'clsx';
import { useAssetStore, useFolderTreeStore, useLiveCountsStore } from '../../store';
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
 * Packs get a pack icon. With hidden files off, folders holding only support files are left out.
 * While search text or a filter is on, each badge counts the matching items below its folder,
 * once they are counted, and a folder with none dims.
 */
export function FolderTree({ libraryId, parentPath, onSelect }: FolderTreeProps) {
  const nodes = useFolderTreeStore((s) => s.children[libraryId]?.[parentPath]);
  const expanded = useFolderTreeStore((s) => s.expanded[libraryId]);
  const toggle = useFolderTreeStore((s) => s.toggle);
  const selected = useAssetStore((s) => s.query.folder);
  const showHidden = useAssetStore((s) => Boolean(s.query.filter.showHidden));
  const live = useLiveCountsStore((s) => s.live);
  const liveCounts = useLiveCountsStore((s) => s.folders[libraryId]?.[parentPath]);
  const countFolders = useLiveCountsStore((s) => s.countFolders);

  // A node expanded while counts are live asks for its children's; a recount asks for them all
  useEffect(() => {
    if (live) countFolders(libraryId, [parentPath]);
  }, [live, libraryId, parentPath, countFolders]);

  const shown = nodes?.filter((node) => isFolderShown(node, showHidden));
  if (!shown || shown.length === 0) return null;
  return (
    <div className={styles.subTree} role="group">
      {shown.map((node) => {
        const isOpen = expanded?.includes(node.path) ?? false;
        const isSelected = selected?.libraryId === libraryId && selected.path === node.path;
        const hasChildren = showHidden ? node.hasChildren : node.hasVisibleChildren;
        const hidden = node.hiddenCount > 0 ? `, ${node.hiddenCount.toLocaleString()} hidden` : '';
        const liveCount = live && liveCounts ? (liveCounts[node.path] ?? 0) : undefined;
        const count = live ? liveCount : folderCount(node, showHidden);
        const title = live
          ? `${node.path}: ${liveCount === undefined ? 'counting' : liveCount.toLocaleString()} matching items`
          : `${node.path}${node.isPack ? ' (pack)' : ''}: ${node.itemCount.toLocaleString()} items${hidden}`;
        return (
          <div key={node.path}>
            <div
              className={clsx(
                styles.item,
                isSelected && styles.active,
                liveCount === 0 && styles.noMatches
              )}
              onClick={() => onSelect(libraryId, node.path)}
              data-library-id={libraryId}
              data-folder-path={node.path}
              data-pack={node.isPack || undefined}
              data-no-matches={liveCount === 0 || undefined}
              role="treeitem"
              aria-selected={isSelected}
              aria-expanded={hasChildren ? isOpen : undefined}
              tabIndex={0}
              title={title}
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
              {node.isPack && <IconPackage size={13} className={styles.packIcon} />}
              <span className={clsx(styles.itemName, 'truncate')}>{node.name}</span>
              {count !== undefined && (
                <span className={styles.badge} data-live-count={live || undefined}>
                  {count.toLocaleString()}
                </span>
              )}
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
