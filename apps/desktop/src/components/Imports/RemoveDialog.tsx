import { useEffect } from 'react';
import clsx from 'clsx';
import { removeSummary, usePreparationStore, type RemoveDraft } from '../../store/preparation';
import { useImportStore } from '../../store/imports';
import { IconX } from '../Icons';
import dialog from '../Settings/SettingsDialog.module.css';
import styles from './Imports.module.css';

/**
 * The Remove confirmation of the one pack whose Remove waits for it, whichever Preparation
 * asked: mounted once, beside the app's other dialogs
 */
export function RemoveDialogs() {
  const open = usePreparationStore((s) => Object.values(s.editors).find((e) => e.remove !== null));
  const imports = useImportStore((s) => s.imports);
  if (!open?.remove) return null;
  const pack = (imports[open.libraryId] ?? [])
    .flatMap((i) => i.packs)
    .find((p) => p.id === open.packId);
  return (
    <RemoveDialog
      packId={open.packId}
      draft={open.remove}
      saving={open.saving}
      folder={pack?.target ?? ''}
    />
  );
}

interface Props {
  packId: string;
  draft: RemoveDraft;
  saving: boolean;
  /** The pack folder, shown above the lists and cut from each path */
  folder: string;
}

/**
 * Before a Remove: the pack's files it deletes, naming the kept ones, and the outside changes
 * it loses or leaves
 */
export function RemoveDialog({ packId, draft, saving, folder }: Props) {
  const { closeRemove, confirmRemove } = usePreparationStore.getState();

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.preventDefault();
      usePreparationStore.getState().closeRemove(packId);
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [packId]);

  const prefix = folder ? `${folder}/` : '';
  const short = (path: string) => (path.startsWith(prefix) ? path.slice(prefix.length) : path);
  const { files } = draft.preview;
  const summary = removeSummary(draft.preview);
  const count = files.length === 1 ? '1 file' : `${files.length} files`;
  return (
    <div className={dialog.overlay} onClick={() => closeRemove(packId)}>
      <div
        className={clsx(dialog.dialog, styles.dialog)}
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-labelledby="remove-dialog-title"
        data-remove-dialog={packId}
      >
        <div className={dialog.header}>
          <div className={dialog.titleArea}>
            <h3 id="remove-dialog-title" className={dialog.title}>
              Remove import
            </h3>
          </div>
          <button
            className={dialog.closeBtn}
            onClick={() => closeRemove(packId)}
            disabled={saving}
            title="Close"
          >
            <IconX size={14} />
          </button>
        </div>
        <div className={dialog.content}>
          {folder && <span className={styles.path}>{folder}</span>}
          <span className={styles.hint} data-remove-what>
            {files.length === 0
              ? 'The pack has no files yet. Removing drops its import records.'
              : `Deletes the ${count} the import put in this folder, and the folders left empty, with their tags, ratings and collections. Then the pack's import records go.`}
          </span>
          <RemoveList
            label="Not from the original, deleted too"
            marker="data-remove-kept"
            paths={summary.kept.map(short)}
          />
          <RemoveList
            label="Changed outside OpenDAM: your changes are lost"
            marker="data-remove-lost"
            paths={summary.lost.map(short)}
            danger
          />
          <RemoveList
            label="Added outside OpenDAM: left in the folder"
            marker="data-remove-staying"
            paths={summary.staying.map(short)}
          />
          {summary.missing > 0 && (
            <span className={styles.hint} data-remove-missing>
              {summary.missing === 1
                ? '1 file is gone already.'
                : `${summary.missing} files are gone already.`}
            </span>
          )}
          {files.length > 0 && (
            <details className={styles.details} data-remove-files>
              <summary>The {count} it deletes</summary>
              <ul className={styles.kept}>
                {files.map((f) => (
                  <li key={f.path} className={styles.path}>
                    {short(f.path)}
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
        <div className={dialog.actions}>
          <span className={clsx(styles.footerNote, styles.errorText)} data-remove-notice>
            {draft.notice}
          </span>
          <button
            className={clsx(dialog.btn, dialog.secondaryBtn)}
            onClick={() => closeRemove(packId)}
            disabled={saving}
          >
            Cancel
          </button>
          <button
            className={clsx(dialog.btn, styles.dangerBtn)}
            disabled={saving}
            onClick={() => void confirmRemove(packId)}
            data-remove-confirm
          >
            {saving ? 'Queuing…' : 'Remove import'}
          </button>
        </div>
      </div>
    </div>
  );
}

function RemoveList({
  label,
  marker,
  paths,
  danger = false,
}: {
  label: string;
  marker: string;
  paths: string[];
  danger?: boolean;
}) {
  if (paths.length === 0) return null;
  return (
    <div className={styles.section} {...{ [marker]: '' }}>
      <span className={clsx(styles.sectionLabel, danger && styles.errorText)}>{label}</span>
      <ul className={styles.kept}>
        {paths.map((path) => (
          <li key={path} className={styles.path}>
            {path}
          </li>
        ))}
      </ul>
    </div>
  );
}
