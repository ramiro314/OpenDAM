import { useState } from 'react';
import clsx from 'clsx';
import { useTagStore, useUIStore, useAssetStore } from '../../store';
import { IconEdit, IconTag, IconTrash } from '../Icons';
import { ConfirmDeleteDialog } from '../ConfirmDeleteDialog';
import { SidebarSection } from './SidebarSection';
import { InlineCreateForm } from './InlineCreateForm';
import { EditTagDialog } from './EditTagDialog';
import { isNameTaken } from './sidebarHelpers';
import type { Tag } from '../../types';
import styles from './Sidebar.module.css';

interface TagsSectionProps {
  clearSmartFilter: () => void;
}

export function TagsSection({ clearSmartFilter }: TagsSectionProps) {
  const tags = useTagStore((s) => s.tags);
  const createTag = useTagStore((s) => s.createTag);
  const deleteTag = useTagStore((s) => s.deleteTag);

  const setActiveCollection = useUIStore((s) => s.setActiveCollection);
  const replaceFilter = useAssetStore((s) => s.replaceFilter);
  const resetFilter = useAssetStore((s) => s.resetFilter);
  const clearFilter = useAssetStore((s) => s.clearFilter);
  const activeTagId = useAssetStore((s) => s.query.filter.tags?.[0] ?? null);
  const search = useAssetStore((s) => s.search);

  const [expanded, setExpanded] = useState(true);
  const [isCreating, setIsCreating] = useState(false);
  const [newName, setNewName] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<{ id: string; name: string } | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; tag: Tag } | null>(null);
  const [editTarget, setEditTarget] = useState<Tag | null>(null);

  const cancelCreate = () => {
    setIsCreating(false);
    setNewName('');
    setError(null);
  };

  const handleCreate = async () => {
    const name = newName.trim();
    if (!name) {
      setIsCreating(false);
      setError(null);
      return;
    }

    if (isNameTaken(tags, name)) {
      setError(`Tag "${name}" already exists`);
      return;
    }

    try {
      await createTag(name);
      setNewName('');
      setError(null);
      setIsCreating(false);
    } catch (err) {
      console.error('Failed to create tag:', err);
      setError('Failed to create tag');
    }
  };

  const handleTagFilter = (tagId: string) => {
    clearSmartFilter();
    setActiveCollection(null);
    if (activeTagId === tagId) {
      resetFilter();
    } else {
      replaceFilter({ tags: [tagId] });
    }
    search();
  };

  const executeDelete = async () => {
    if (!deleteTarget) return;
    const { id } = deleteTarget;
    setDeleteTarget(null);

    try {
      await deleteTag(id);
      if (activeTagId === id) {
        clearFilter('tags');
        search();
      }
    } catch (err) {
      console.error('Failed to delete tag:', err);
    }
  };

  return (
    <>
      <SidebarSection
        label="Tags"
        icon={<IconTag size={14} />}
        expanded={expanded}
        onToggle={() => setExpanded((e) => !e)}
        onAdd={() => {
          setExpanded(true);
          setIsCreating(true);
        }}
      >
        {isCreating && (
          <InlineCreateForm
            placeholder="Tag name…"
            saveTitle="Save tag"
            value={newName}
            error={error}
            onChange={(value) => {
              setNewName(value);
              if (error) setError(null);
            }}
            onSubmit={handleCreate}
            onCancel={cancelCreate}
          />
        )}

        {tags.length === 0 && !isCreating && <p className={styles.empty}>No tags yet</p>}
        {tags.map((tag) => {
          const isActiveFilter = activeTagId === tag.id;
          return (
            <div
              key={tag.id}
              id={`tag-${tag.id}`}
              className={clsx(styles.item, isActiveFilter && styles.active)}
              onClick={() => handleTagFilter(tag.id)}
              onContextMenu={(e) => {
                e.preventDefault();
                setMenu({ x: e.clientX, y: e.clientY, tag });
              }}
              title={isActiveFilter ? 'Click to clear filter' : 'Click to filter by tag'}
              role="button"
              tabIndex={0}
            >
              <span
                className={styles.tagDot}
                style={{
                  background: tag.color ? `var(--swatch-${tag.color})` : 'hsl(220 85% 60%)',
                }}
              />
              <span className={clsx(styles.itemName, 'truncate')}>{tag.name}</span>
              {isActiveFilter && <span className={styles.filterActiveChip}>●</span>}
              <span
                className={clsx(styles.itemActionBtn, styles.itemActionBtnDanger)}
                onClick={(e) => {
                  e.stopPropagation();
                  setDeleteTarget({ id: tag.id, name: tag.name });
                }}
                title="Delete tag"
                role="button"
                tabIndex={0}
              >
                <IconTrash size={11} />
              </span>
            </div>
          );
        })}
      </SidebarSection>

      {menu && (
        <div
          className={styles.contextBackdrop}
          onClick={() => setMenu(null)}
          onContextMenu={(e) => {
            e.preventDefault();
            setMenu(null);
          }}
        >
          <div
            className={styles.contextMenu}
            style={{ left: menu.x, top: menu.y }}
            role="menu"
            onClick={(e) => e.stopPropagation()}
          >
            <button
              className={styles.contextMenuItem}
              role="menuitem"
              autoFocus
              onKeyDown={(e) => e.key === 'Escape' && setMenu(null)}
              onClick={() => {
                setEditTarget(menu.tag);
                setMenu(null);
              }}
            >
              <IconEdit size={12} /> Edit tag…
            </button>
            <button
              className={clsx(styles.contextMenuItem, styles.contextMenuItemDanger)}
              role="menuitem"
              onKeyDown={(e) => e.key === 'Escape' && setMenu(null)}
              onClick={() => {
                setDeleteTarget({ id: menu.tag.id, name: menu.tag.name });
                setMenu(null);
              }}
            >
              <IconTrash size={12} /> Delete tag…
            </button>
          </div>
        </div>
      )}

      <EditTagDialog tag={editTarget} onClose={() => setEditTarget(null)} />

      <ConfirmDeleteDialog
        isOpen={Boolean(deleteTarget)}
        itemType="tag"
        itemName={deleteTarget?.name ?? ''}
        onConfirm={executeDelete}
        onCancel={() => setDeleteTarget(null)}
      />
    </>
  );
}
