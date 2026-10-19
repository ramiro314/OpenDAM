import { useState } from 'react';
import clsx from 'clsx';
import { open } from '@tauri-apps/plugin-dialog';
import {
  useLibraryStore,
  useUIStore,
  useAssetStore,
  useAutotagStore,
  useFolderTreeStore,
  selectFolder,
} from '../../store';
import type { FolderNode, Library } from '../../types';
import { autotagAvailability } from '../../utils/autotag';
import { IconChevronDown, IconChevronRight, IconFolder, IconTag, IconTrash } from '../Icons';
import { ConfirmDeleteDialog } from '../ConfirmDeleteDialog';
import { ContextMenu } from '../ContextMenu/ContextMenu';
import { FolderContextMenu, type FolderMenuTarget } from '../ContextMenu/FolderContextMenu';
import { folderMenuItems } from '../FolderActions';
import { FolderTree } from './FolderTree';
import { SidebarSection } from './SidebarSection';
import { libraryNameFromPath } from './sidebarHelpers';
import styles from './Sidebar.module.css';

interface LibrariesSectionProps {
  clearSmartFilter: () => void;
}

export function LibrariesSection({ clearSmartFilter }: LibrariesSectionProps) {
  const libraries = useLibraryStore((s) => s.libraries);
  const addLibrary = useLibraryStore((s) => s.addLibrary);
  const removeLibrary = useLibraryStore((s) => s.removeLibrary);

  const {
    activeLibraryId,
    selectedLibraryIds,
    setActiveLibrary,
    setActiveCollection,
    toggleSelectedLibrary,
    clearLibrarySelection,
  } = useUIStore();
  const setQuery = useAssetStore((s) => s.setQuery);
  const search = useAssetStore((s) => s.search);
  const expandedFolders = useFolderTreeStore((s) => s.expanded);
  const toggleFolder = useFolderTreeStore((s) => s.toggle);

  const [expanded, setExpanded] = useState(true);
  const [removeTarget, setRemoveTarget] = useState<{ id: string; name: string } | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; library: Library } | null>(null);
  const [folderMenu, setFolderMenu] = useState<FolderMenuTarget | null>(null);
  const providers = useAutotagStore((s) => s.providers);
  const openAutotagDialog = useAutotagStore((s) => s.openAutotagDialog);

  const handleAddLibrary = async () => {
    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        title: 'Select Library Folder',
      });
      if (selectedPath && typeof selectedPath === 'string') {
        await addLibrary(libraryNameFromPath(selectedPath), [selectedPath]);
        search();
      }
    } catch (err) {
      console.error('Failed to add library:', err);
    }
  };

  const handleAllLibrariesClick = () => {
    clearSmartFilter();
    setActiveCollection(null);
    clearLibrarySelection();
    setQuery({
      libraryIds: undefined,
      libraryId: undefined,
      collectionId: undefined,
      folder: undefined,
    });
    search();
  };

  const handleLibraryClick = (id: string, e?: React.MouseEvent) => {
    clearSmartFilter();
    setActiveCollection(null);
    const isCheckbox = (e?.target as HTMLElement)?.tagName?.toLowerCase() === 'input';
    const isMulti = e ? Boolean(e.metaKey || e.ctrlKey || e.shiftKey || isCheckbox) : false;
    toggleSelectedLibrary(id, isMulti);
    const nextSelected = useUIStore.getState().selectedLibraryIds;
    setQuery({
      libraryIds: nextSelected.length > 0 ? nextSelected : undefined,
      libraryId: nextSelected.length === 1 ? nextSelected[0] : undefined,
      collectionId: undefined,
      folder: undefined,
    });
    search();
  };

  // A folder names its own library, so it replaces the library selection
  const handleFolderClick = (libraryId: string, path: string) => {
    clearSmartFilter();
    selectFolder({ libraryId, path });
  };

  const handleFolderMenu = (e: React.MouseEvent, libraryId: string, node: FolderNode) => {
    e.preventDefault();
    setFolderMenu({
      x: e.clientX,
      y: e.clientY,
      libraryId,
      path: node.path,
      name: node.name,
      isPack: node.isPack,
      itemCount: node.itemCount,
      mark: node.mark,
    });
  };

  const executeRemove = async () => {
    if (!removeTarget) return;
    const { id } = removeTarget;
    setRemoveTarget(null);

    try {
      await removeLibrary(id);
      if (activeLibraryId === id) {
        setActiveLibrary(null);
        setQuery({ libraryId: undefined });
      }
      if (useAssetStore.getState().query.folder?.libraryId === id) {
        setQuery({ folder: undefined });
      }
      search();
    } catch (err) {
      console.error('Failed to delete library:', err);
    }
  };

  return (
    <>
      <SidebarSection
        label="Libraries"
        icon={<IconFolder size={14} />}
        expanded={expanded}
        onToggle={() => setExpanded((e) => !e)}
        onAdd={handleAddLibrary}
      >
        {libraries.length === 0 && <p className={styles.empty}>No libraries yet</p>}
        {libraries.length > 1 && (
          <div
            id="lib-all"
            className={clsx(styles.item, selectedLibraryIds.length === 0 && styles.active)}
            onClick={handleAllLibrariesClick}
            role="button"
            tabIndex={0}
            title="Search across all libraries (none selected)"
          >
            <IconFolder size={13} />
            <span
              className={clsx(styles.itemName, 'truncate')}
              style={{ fontWeight: selectedLibraryIds.length === 0 ? 600 : 400 }}
            >
              All Libraries
            </span>
            <span className={styles.badge}>
              {libraries.reduce((sum, l) => sum + (l.assetCount || 0), 0).toLocaleString()}
            </span>
          </div>
        )}
        {libraries.map((lib) => {
          const isSelected = selectedLibraryIds.includes(lib.id);
          const isExpanded = expandedFolders[lib.id]?.includes('') ?? false;
          return (
            <div key={lib.id}>
              <div
                id={`lib-${lib.id}`}
                className={clsx(styles.item, isSelected && styles.active)}
                onClick={(e) => handleLibraryClick(lib.id, e)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  setMenu({ x: e.clientX, y: e.clientY, library: lib });
                }}
                role="button"
                tabIndex={0}
                title="Click to select library; Ctrl+Click to select multiple"
              >
                {libraries.length > 1 && (
                  <input
                    type="checkbox"
                    checked={isSelected}
                    onClick={(e) => {
                      e.stopPropagation();
                      handleLibraryClick(lib.id, e);
                    }}
                    onChange={() => {}}
                    style={{
                      width: 13,
                      height: 13,
                      accentColor: 'var(--color-brand-500)',
                      cursor: 'pointer',
                      marginRight: 6,
                    }}
                    title="Toggle this library in filter"
                  />
                )}
                <span
                  id={`lib-toggle-${lib.id}`}
                  className={styles.chevronBtn}
                  onClick={(e) => {
                    e.stopPropagation();
                    void toggleFolder(lib.id, '');
                  }}
                  title={isExpanded ? 'Collapse folders' : 'Expand folders'}
                >
                  {isExpanded ? <IconChevronDown size={11} /> : <IconChevronRight size={11} />}
                </span>
                <IconFolder size={13} />
                <span className={clsx(styles.itemName, 'truncate')}>{lib.name}</span>
                <span className={styles.badge}>{(lib.assetCount || 0).toLocaleString()}</span>
                <span
                  className={clsx(styles.itemActionBtn, styles.itemActionBtnDanger)}
                  onClick={(e) => {
                    e.stopPropagation();
                    setRemoveTarget({ id: lib.id, name: lib.name });
                  }}
                  title="Remove library"
                  role="button"
                  tabIndex={0}
                >
                  <IconTrash size={11} />
                </span>
              </div>
              {isExpanded && (
                <FolderTree
                  libraryId={lib.id}
                  parentPath=""
                  onSelect={handleFolderClick}
                  onMenu={handleFolderMenu}
                />
              )}
            </div>
          );
        })}
      </SidebarSection>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
          items={[
            {
              id: 'library-menu-autotag',
              label: 'Auto-tag library…',
              icon: <IconTag size={12} />,
              disabledReason: autotagAvailability(
                providers,
                menu.library.assetCount,
                'This library has no assets'
              ).reason,
              onSelect: () =>
                openAutotagDialog({
                  kind: 'library',
                  libraryId: menu.library.id,
                  libraryName: menu.library.name,
                }),
            },
            ...folderMenuItems({ libraryId: menu.library.id, path: '', name: menu.library.name }),
          ]}
        />
      )}

      {folderMenu && <FolderContextMenu target={folderMenu} onClose={() => setFolderMenu(null)} />}

      <ConfirmDeleteDialog
        isOpen={Boolean(removeTarget)}
        itemType="library"
        itemName={removeTarget?.name ?? ''}
        onConfirm={executeRemove}
        onCancel={() => setRemoveTarget(null)}
      />
    </>
  );
}
