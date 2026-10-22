import { useState } from 'react';
import clsx from 'clsx';
import { api } from '../../api';
import { useCollectionStore, useUIStore, useAssetStore, cachedAssets } from '../../store';
import type { Collection, ExportSettings } from '../../types';
import { exportMenuReason } from '../../utils/exportView';
import {
  IconCollection,
  IconChevronDown,
  IconChevronRight,
  IconExternal,
  IconPlus,
  IconTrash,
} from '../Icons';
import { ConfirmDeleteDialog } from '../ConfirmDeleteDialog';
import { ContextMenu } from '../ContextMenu/ContextMenu';
import { ExportDialog } from '../Export/ExportDialog';
import { SidebarSection } from './SidebarSection';
import { InlineCreateForm } from './InlineCreateForm';
import { isNameTaken, resolveDroppedAssetId } from './sidebarHelpers';
import styles from './Sidebar.module.css';

interface CollectionsSectionProps {
  clearSmartFilter: () => void;
}

export function CollectionsSection({ clearSmartFilter }: CollectionsSectionProps) {
  const collections = useCollectionStore((s) => s.collections);
  const createCollection = useCollectionStore((s) => s.createCollection);
  const deleteCollection = useCollectionStore((s) => s.deleteCollection);
  const addToCollection = useCollectionStore((s) => s.addToCollection);

  const activeCollectionId = useUIStore((s) => s.activeCollectionId);
  const setActiveCollection = useUIStore((s) => s.setActiveCollection);
  const resetFilter = useAssetStore((s) => s.resetFilter);
  const setQuery = useAssetStore((s) => s.setQuery);
  const search = useAssetStore((s) => s.search);

  const [expanded, setExpanded] = useState(true);

  // Inline creation: parentCollectionId null creates a root collection
  const [isCreating, setIsCreating] = useState(false);
  const [parentCollectionId, setParentCollectionId] = useState<string | null>(null);
  const [newName, setNewName] = useState('');
  const [error, setError] = useState<string | null>(null);

  // Track expanded/collapsed state for collections that have subcollections
  const [expandedColIds, setExpandedColIds] = useState<Record<string, boolean>>({});

  const [deleteTarget, setDeleteTarget] = useState<{ id: string; name: string } | null>(null);

  // The right-click menu, with the export settings it reads to tell a domain (plan C3)
  const [menu, setMenu] = useState<{ x: number; y: number; collection: Collection } | null>(null);
  const [exportSettings, setExportSettings] = useState<ExportSettings | null>(null);
  const [exportDomain, setExportDomain] = useState<Collection | null>(null);

  const openMenu = (e: React.MouseEvent, collection: Collection) => {
    e.preventDefault();
    setMenu({ x: e.clientX, y: e.clientY, collection });
    setExportSettings(null);
    api
      .getExportSettings()
      .then(setExportSettings)
      .catch((err) => console.error('Failed to load the export settings:', err));
  };

  const toggleCollectionExpanded = (id: string, e?: React.MouseEvent) => {
    if (e) e.stopPropagation();
    setExpandedColIds((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const cancelCreate = () => {
    setIsCreating(false);
    setParentCollectionId(null);
    setNewName('');
    setError(null);
  };

  const handleNameChange = (value: string) => {
    setNewName(value);
    if (error) setError(null);
  };

  const handleCreate = async () => {
    const name = newName.trim();
    if (!name) {
      setIsCreating(false);
      setParentCollectionId(null);
      setError(null);
      return;
    }

    // Check duplicates under the same parent (or root)
    const siblings = collections.filter(
      (c) => (c.parentId ?? null) === (parentCollectionId ?? null)
    );
    if (isNameTaken(siblings, name)) {
      setError(`Collection "${name}" already exists here`);
      return;
    }

    try {
      await createCollection(name, undefined, parentCollectionId ?? undefined);
      if (parentCollectionId) {
        setExpandedColIds((prev) => ({ ...prev, [parentCollectionId]: true }));
      }
      setNewName('');
      setParentCollectionId(null);
      setError(null);
      setIsCreating(false);
    } catch (err) {
      console.error('Failed to create collection:', err);
      setError('Failed to create collection');
    }
  };

  const handleCollectionClick = (id: string) => {
    clearSmartFilter();
    resetFilter();
    if (activeCollectionId === id) {
      setActiveCollection(null);
      setQuery({ collectionId: undefined });
    } else {
      setActiveCollection(id);
      setQuery({ collectionId: id, libraryId: undefined, folder: undefined });
    }
    search();
  };

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

  const handleDropOnCollection = async (e: React.DragEvent, colId: string) => {
    e.preventDefault();
    const rawData =
      e.dataTransfer.getData('application/opendam-asset-id') ||
      e.dataTransfer.getData('text/plain');
    if (!rawData) return;

    const assetId = resolveDroppedAssetId(rawData, cachedAssets(useAssetStore.getState()));

    try {
      await addToCollection(colId, [assetId]);
      await useCollectionStore.getState().fetchCollections();
      const curAsset = useAssetStore.getState().getById(assetId);
      if (curAsset) {
        const updatedCols = Array.from(new Set([...(curAsset.collections || []), colId]));
        useAssetStore.getState().patchAsset(assetId, { collections: updatedCols });
      }
    } catch (err) {
      console.error('Failed to add asset to collection:', err);
    }
  };

  const renderCollectionTree = (parentId: string | null = null, depth: number = 0) => {
    const items = collections.filter((c) => !c.isSmart && (c.parentId ?? null) === parentId);
    if (items.length === 0 && (!isCreating || parentCollectionId !== parentId)) {
      return null;
    }

    return (
      <div className={depth > 0 ? styles.subTree : undefined}>
        {items.map((col) => {
          const hasChildren = collections.some((c) => c.parentId === col.id);
          const isExpanded = expandedColIds[col.id] ?? false;
          const isCreatingSub = isCreating && parentCollectionId === col.id;

          return (
            <div key={col.id}>
              <div
                id={`col-${col.id}`}
                className={clsx(styles.item, activeCollectionId === col.id && styles.active)}
                onClick={() => handleCollectionClick(col.id)}
                onDragOver={(e) => {
                  e.preventDefault();
                  e.dataTransfer.dropEffect = 'copy';
                }}
                onDrop={(e) => handleDropOnCollection(e, col.id)}
                onContextMenu={(e) => openMenu(e, col)}
                title="Click to view; drag assets here to add"
                role="button"
                tabIndex={0}
              >
                {hasChildren ? (
                  <span
                    className={styles.chevronBtn}
                    onClick={(e) => toggleCollectionExpanded(col.id, e)}
                    title={isExpanded ? 'Collapse subcollections' : 'Expand subcollections'}
                  >
                    {isExpanded ? <IconChevronDown size={11} /> : <IconChevronRight size={11} />}
                  </span>
                ) : (
                  <IconCollection size={13} />
                )}

                <span className={clsx(styles.itemName, 'truncate')}>{col.name}</span>
                <span className={styles.badge}>{col.assetIds.length}</span>

                <div className={styles.itemActions}>
                  <span
                    className={styles.itemActionBtn}
                    onClick={(e) => {
                      e.stopPropagation();
                      setParentCollectionId(col.id);
                      setNewName('');
                      setError(null);
                      setIsCreating(true);
                      setExpandedColIds((prev) => ({ ...prev, [col.id]: true }));
                    }}
                    title="Add subcollection"
                  >
                    <IconPlus size={11} />
                  </span>
                  <span
                    className={clsx(styles.itemActionBtn, styles.itemActionBtnDanger)}
                    onClick={(e) => {
                      e.stopPropagation();
                      setDeleteTarget({ id: col.id, name: col.name });
                    }}
                    title="Delete collection"
                  >
                    <IconTrash size={11} />
                  </span>
                </div>
              </div>

              {/* Inline input for new subcollection inside this item */}
              {isCreatingSub && (
                <InlineCreateForm
                  className={styles.subTree}
                  placeholder="Subcollection name…"
                  saveTitle="Save subcollection"
                  value={newName}
                  error={error}
                  onChange={handleNameChange}
                  onSubmit={handleCreate}
                  onCancel={cancelCreate}
                />
              )}

              {/* Render subcollection children if expanded */}
              {isExpanded && renderCollectionTree(col.id, depth + 1)}
            </div>
          );
        })}
      </div>
    );
  };

  return (
    <>
      <SidebarSection
        label="Collections"
        icon={<IconCollection size={14} />}
        expanded={expanded}
        onToggle={() => setExpanded((e) => !e)}
        onAdd={() => {
          setExpanded(true);
          setIsCreating(true);
        }}
      >
        {isCreating && parentCollectionId === null && (
          <InlineCreateForm
            placeholder="Collection name…"
            saveTitle="Save collection"
            value={newName}
            error={error}
            onChange={handleNameChange}
            onSubmit={handleCreate}
            onCancel={cancelCreate}
          />
        )}

        {collections.length === 0 && !isCreating && (
          <p className={styles.empty}>No collections yet</p>
        )}

        {renderCollectionTree(null)}
      </SidebarSection>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={[
            {
              id: 'collection-menu-export',
              label: 'Export to game…',
              icon: <IconExternal size={12} />,
              disabledReason: exportMenuReason(menu.collection, exportSettings, collections),
              onSelect: () => setExportDomain(menu.collection),
            },
          ]}
        />
      )}

      {exportDomain && <ExportDialog domain={exportDomain} onClose={() => setExportDomain(null)} />}

      <ConfirmDeleteDialog
        isOpen={Boolean(deleteTarget)}
        itemType="collection"
        itemName={deleteTarget?.name ?? ''}
        onConfirm={executeDelete}
        onCancel={() => setDeleteTarget(null)}
      />
    </>
  );
}
