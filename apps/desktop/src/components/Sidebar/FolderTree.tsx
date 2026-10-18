import clsx from 'clsx';
import { useAssetStore, useFolderTreeStore } from '../../store';
import { IconChevronDown, IconChevronRight, IconFolder } from '../Icons';
import styles from './Sidebar.module.css';

interface FolderTreeProps {
  libraryId: string;
  /** The node whose children this lists; `''` is the library root */
  parentPath: string;
  onSelect: (libraryId: string, path: string) => void;
}

/** One level of a library's folder tree, each expanded node listing its own children below it. */
export function FolderTree({ libraryId, parentPath, onSelect }: FolderTreeProps) {
  const nodes = useFolderTreeStore((s) => s.children[libraryId]?.[parentPath]);
  const expanded = useFolderTreeStore((s) => s.expanded[libraryId]);
  const toggle = useFolderTreeStore((s) => s.toggle);
  const selected = useAssetStore((s) => s.query.folder);

  if (!nodes || nodes.length === 0) return null;
  return (
    <div className={styles.subTree} role="group">
      {nodes.map((node) => {
        const isOpen = expanded?.includes(node.path) ?? false;
        const isSelected = selected?.libraryId === libraryId && selected.path === node.path;
        return (
          <div key={node.path}>
            <div
              className={clsx(styles.item, isSelected && styles.active)}
              onClick={() => onSelect(libraryId, node.path)}
              data-library-id={libraryId}
              data-folder-path={node.path}
              role="treeitem"
              aria-selected={isSelected}
              aria-expanded={node.hasChildren ? isOpen : undefined}
              tabIndex={0}
              title={node.path}
            >
              {node.hasChildren ? (
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
                <IconFolder size={13} />
              )}
              <span className={clsx(styles.itemName, 'truncate')}>{node.name}</span>
              <span className={styles.badge}>{node.itemCount.toLocaleString()}</span>
            </div>
            {isOpen && (
              <FolderTree libraryId={libraryId} parentPath={node.path} onSelect={onSelect} />
            )}
          </div>
        );
      })}
    </div>
  );
}
