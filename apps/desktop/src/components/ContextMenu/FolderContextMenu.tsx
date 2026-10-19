import { useAutotagStore } from '../../store';
import { autotagAvailability } from '../../utils/autotag';
import { folderMenuItems } from '../FolderActions';
import { IconTag } from '../Icons';
import { ContextMenu } from './ContextMenu';

export interface FolderMenuTarget {
  /** Where the right-click happened */
  x: number;
  y: number;
  libraryId: string;
  /** Relative to the library root; `''` is the root */
  path: string;
  name: string;
  isPack: boolean;
  /** Visible items below the folder */
  itemCount: number;
}

/**
 * The right-click menu of a tree node or a pack card (plan C22): auto-tag the items below it, add
 * everything below it to a collection or a tag, or reveal it in the file manager.
 */
export function FolderContextMenu({
  target,
  onClose,
}: {
  target: FolderMenuTarget;
  onClose: () => void;
}) {
  const providers = useAutotagStore((s) => s.providers);
  const openAutotagDialog = useAutotagStore((s) => s.openAutotagDialog);
  const { libraryId, path, name, isPack, itemCount } = target;
  return (
    <ContextMenu
      x={target.x}
      y={target.y}
      onClose={onClose}
      items={[
        {
          id: 'folder-menu-autotag',
          label: 'Auto-tag…',
          icon: <IconTag size={12} />,
          disabledReason: autotagAvailability(
            providers,
            itemCount,
            `This ${isPack ? 'pack' : 'folder'} has no visible items`
          ).reason,
          onSelect: () => openAutotagDialog({ kind: 'folder', libraryId, path, name }),
        },
        ...folderMenuItems({ libraryId, path, name }),
      ]}
    />
  );
}
