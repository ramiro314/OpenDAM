import { useState } from 'react';
import clsx from 'clsx';
import { useCollectionStore, useUIStore, useAssetStore } from '../../store';
import type { Collection, FilterSpec } from '../../types';
import { IconFilter, IconTrash } from '../Icons';
import { ConfirmDeleteDialog } from '../ConfirmDeleteDialog';
import { SmartFilterModal } from '../SmartFilterModal';
import { SidebarSection } from './SidebarSection';
import styles from './Sidebar.module.css';

interface SmartFiltersSectionProps {
  activeId: string | null;
  setActiveId: (id: string | null) => void;
}

export function SmartFiltersSection({ activeId, setActiveId }: SmartFiltersSectionProps) {
  const collections = useCollectionStore((s) => s.collections);
  const deleteCollection = useCollectionStore((s) => s.deleteCollection);

  const activeCollectionId = useUIStore((s) => s.activeCollectionId);
  const setActiveCollection = useUIStore((s) => s.setActiveCollection);
  const replaceFilter = useAssetStore((s) => s.replaceFilter);
  const resetFilter = useAssetStore((s) => s.resetFilter);
  const setQuery = useAssetStore((s) => s.setQuery);
  const search = useAssetStore((s) => s.search);

  const [expanded, setExpanded] = useState(true);
  const [isCreating, setIsCreating] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<{ id: string; name: string } | null>(null);

  const smartFilters = collections.filter((c) => c.isSmart);

  const handleClick = (col: Collection) => {
    setActiveCollection(null);
    if (activeId === col.id) {
      setActiveId(null);
      resetFilter();
      search();
    } else {
      setActiveId(col.id);
      try {
        const spec: FilterSpec = col.filterSpec ? JSON.parse(col.filterSpec) : {};
        replaceFilter(spec);
        search();
      } catch (err) {
        console.error('Failed to parse smart filter spec:', err);
      }
    }
  };

  // A smart filter is a collection, so it is deleted as one
  const executeDelete = async () => {
    if (!deleteTarget) return;
    const { id } = deleteTarget;
    setDeleteTarget(null);

    try {
      await deleteCollection(id);
      if (activeCollectionId === id) {
        setActiveCollection(null);
        setQuery({ collectionId: undefined });
        search();
      }
    } catch (err) {
      console.error('Failed to delete collection:', err);
    }
  };

  return (
    <>
      <SidebarSection
        label="Smart Filters"
        icon={<IconFilter size={14} />}
        expanded={expanded}
        onToggle={() => setExpanded((e) => !e)}
        onAdd={() => setIsCreating(true)}
      >
        {smartFilters.length === 0 && <p className={styles.empty}>No smart filters yet</p>}
        {smartFilters.map((col) => {
          const isActive = activeId === col.id;
          return (
            <div
              key={col.id}
              id={`smart-filter-${col.id}`}
              className={clsx(styles.item, isActive && styles.active)}
              onClick={() => handleClick(col)}
              role="button"
              tabIndex={0}
            >
              <IconFilter size={13} />
              <span className={clsx(styles.itemName, 'truncate')}>{col.name}</span>
              {isActive && <span className={styles.filterActiveChip}>●</span>}
              <span
                className={clsx(styles.itemActionBtn, styles.itemActionBtnDanger)}
                onClick={(e) => {
                  e.stopPropagation();
                  setDeleteTarget({ id: col.id, name: col.name });
                }}
                title="Delete smart filter"
                role="button"
                tabIndex={0}
              >
                <IconTrash size={11} />
              </span>
            </div>
          );
        })}
      </SidebarSection>

      <ConfirmDeleteDialog
        isOpen={Boolean(deleteTarget)}
        itemType="collection"
        itemName={deleteTarget?.name ?? ''}
        onConfirm={executeDelete}
        onCancel={() => setDeleteTarget(null)}
      />

      <SmartFilterModal isOpen={isCreating} onClose={() => setIsCreating(false)} />
    </>
  );
}
