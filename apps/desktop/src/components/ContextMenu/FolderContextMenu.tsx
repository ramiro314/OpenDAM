import { api } from '../../api';
import { useAutotagStore, useFolderActionStore } from '../../store';
import type { PackMark } from '../../types';
import { autotagAvailability } from '../../utils/autotag';
import { markChoices } from '../../utils/packEdits';
import { folderMenuItems } from '../FolderActions';
import { IconPackage, IconTag } from '../Icons';
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
  /** What you said the folder is, if you did */
  mark?: PackMark | null;
}

/**
 * The right-click menu of a tree node or a pack card (plan C22): auto-tag the items below it, add
 * everything below it to a collection or a tag, or reveal it in the file manager. It also
 * corrects pack detection (plan C8 rule 4): the backend reruns the structure pass and emits
 * `library:updated`, which refetches the tree, the Packs view and the panel.
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
  const { libraryId, path, name, isPack, itemCount, mark } = target;
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
        ...markChoices(isPack, mark).map((choice) => ({
          id: choice.id,
          label: choice.label,
          icon: <IconPackage size={12} />,
          onSelect: () =>
            api
              .setPackOverride(libraryId, path, { kind: 'mark', mark: choice.mark })
              .catch((err) =>
                useFolderActionStore
                  .getState()
                  .setFailure(`${choice.label} on ${name} failed: ${String(err)}`)
              ),
        })),
        ...folderMenuItems({ libraryId, path, name }),
      ]}
    />
  );
}
