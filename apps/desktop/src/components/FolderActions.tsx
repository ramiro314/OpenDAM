import { useRef, useState } from 'react';
import clsx from 'clsx';
import {
  applyFolderAction,
  revealFolder,
  useCollectionStore,
  useFolderActionStore,
  useTagStore,
  type FolderTarget,
} from '../store';
import type { ContextMenuItem } from './ContextMenu/ContextMenu';
import { IconCollection, IconEye, IconTag, IconX } from './Icons';
import dialogStyles from './ConfirmDeleteDialog.module.css';
import styles from './Sidebar/EditTagDialog.module.css';

/**
 * The folder actions of a tree node, a pack card or a library row (the root, `path: ''`), for
 * its context menu: Add everything to a collection, Add a tag to everything, and Reveal in file
 * manager (plan C22).
 */
export function folderMenuItems(target: FolderTarget): ContextMenuItem[] {
  const { openDialog } = useFolderActionStore.getState();
  return [
    {
      id: 'folder-menu-collect',
      label: 'Add everything to a collection…',
      icon: <IconCollection size={12} />,
      onSelect: () => openDialog(target, 'collection'),
    },
    {
      id: 'folder-menu-tag',
      label: 'Add a tag to everything…',
      icon: <IconTag size={12} />,
      onSelect: () => openDialog(target, 'tag'),
    },
    {
      id: 'folder-menu-reveal',
      label: 'Reveal in file manager',
      icon: <IconEye size={12} />,
      onSelect: () =>
        revealFolder(target).catch((err) => console.error('Failed to reveal folder:', err)),
    },
  ];
}

/** Picks the tag or collection for the open folder action, then writes it to every file below. */
export function FolderActionDialog() {
  const dialog = useFolderActionStore((s) => s.dialog);
  if (!dialog) return null;
  const { target, action } = dialog;
  return <PickerDialog key={`${target.libraryId}\n${target.path}\n${action}`} {...dialog} />;
}

function PickerDialog({ target, action }: { target: FolderTarget; action: 'tag' | 'collection' }) {
  const closeDialog = useFolderActionStore((s) => s.closeDialog);
  const tags = useTagStore((s) => s.tags);
  const collections = useCollectionStore((s) => s.collections);
  const options = (action === 'tag' ? tags : collections.filter((c) => !c.isSmart))
    .map(({ id, name }) => ({ id, name }))
    .sort((a, b) => a.name.localeCompare(b.name));
  const [chosen, setChosen] = useState(options[0]?.id ?? '');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // A press that starts in the form and ends on the overlay still fires a click on the overlay
  const pressedOnOverlay = useRef(false);

  const noun = action === 'tag' ? 'tag' : 'collection';
  const title = action === 'tag' ? 'Add a tag to everything' : 'Add everything to a collection';
  // Closing while a write waits on the library's index lock leaves the write running; it
  // refetches the grid and the counts when it ends.
  const close = closeDialog;
  const dialog = useRef(useFolderActionStore.getState().dialog);
  const stillOpen = () => useFolderActionStore.getState().dialog === dialog.current;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!chosen) return;
    setSaving(true);
    try {
      await applyFolderAction(target, action, chosen);
      if (stillOpen()) closeDialog();
    } catch (err) {
      console.error(`Failed to ${title.toLowerCase()}:`, err);
      if (!stillOpen()) return;
      setError(String(err));
      setSaving(false);
    }
  };

  return (
    <div
      className={dialogStyles.overlay}
      onMouseDown={(e) => {
        pressedOnOverlay.current = e.target === e.currentTarget;
      }}
      onClick={(e) => {
        if (pressedOnOverlay.current && e.target === e.currentTarget) close();
        pressedOnOverlay.current = false;
      }}
      onKeyDown={(e) => e.key === 'Escape' && close()}
    >
      <div
        className={clsx(dialogStyles.dialog, styles.dialog)}
        role="dialog"
        aria-modal="true"
        aria-labelledby="folder-action-title"
        data-folder-action={action}
      >
        <div className={dialogStyles.header}>
          <h3 id="folder-action-title" className={clsx(dialogStyles.title, styles.title)}>
            {title}
          </h3>
          <button className={dialogStyles.closeBtn} onClick={close} title="Close">
            <IconX size={14} />
          </button>
        </div>

        <form className={clsx(dialogStyles.content, styles.form)} onSubmit={handleSubmit}>
          <p className={dialogStyles.subText}>
            Every file in <strong>{target.name}</strong>, hidden files included, and every format of
            the models among them.
          </p>
          {options.length === 0 ? (
            <p className={dialogStyles.subText}>
              There is no {noun} yet. Create one in the sidebar first.
            </p>
          ) : (
            <>
              <label className={styles.label} htmlFor="folder-action-choice">
                {action === 'tag' ? 'Tag' : 'Collection'}
              </label>
              <select
                id="folder-action-choice"
                className={clsx(styles.input, styles.select)}
                value={chosen}
                onChange={(e) => setChosen(e.target.value)}
                autoFocus
              >
                {options.map((o) => (
                  <option key={o.id} value={o.id}>
                    {o.name}
                  </option>
                ))}
              </select>
            </>
          )}

          {saving && (
            <p className={dialogStyles.subText} data-folder-action-running>
              Adding can wait for indexing to finish. Closing this leaves it running.
            </p>
          )}
          {error && (
            <p className={styles.error} role="alert">
              {error}
            </p>
          )}

          <div className={dialogStyles.actions}>
            <button
              type="button"
              className={clsx(dialogStyles.btn, dialogStyles.cancelBtn)}
              onClick={close}
            >
              {saving ? 'Close' : 'Cancel'}
            </button>
            <button
              type="submit"
              className={clsx(dialogStyles.btn, styles.saveBtn)}
              disabled={saving || !chosen}
            >
              {saving ? 'Adding…' : 'Add'}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
